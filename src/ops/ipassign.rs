//! Assigning IP addresses to guests.
//!
//! Model: every guest interface (`net<N>`) holds any number of IPv4/IPv6
//! addresses from IPAM. One address per family is the *main* address and is
//! written into the guest's network config:
//! - LXC: `net<N>: ...,ip=<cidr>,gw=<gw>,ip6=...,gw6=...` (applied live by Proxmox)
//! - QEMU: cloud-init `ipconfig<N>` (applied at the next boot after regeneration)
//!
//! Additional addresses are tracked in IPAM and allowed by the anti-spoofing IP
//! filter (`ipfilter-net<N>` ipset); the guest OS configures them itself.

use super::create::Env;
use super::guest::{self, PowerAction};
use crate::api::types::{GuestKind, GuestRef, is_net_key};
use crate::api::{Params, PveClient};
use crate::ipam::AllocRequest;
use crate::jobs::JobCtx;
use crate::util::{PropString, ValueExt, generate_mac};
use anyhow::{Context, anyhow, bail};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IpFilter {
    /// Leave the ipsets as they are.
    #[default]
    Keep,
    /// Create/sync `ipfilter-net<N>` with the assigned addresses.
    On,
    /// Remove the ipsets and disable the option.
    Off,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ApplyOpts {
    /// Reboot a running VM so cloud-init picks up new addresses.
    pub reboot: bool,
    pub ipfilter: IpFilter,
}

fn net_index(key: &str) -> Option<u32> {
    key.strip_prefix("net").and_then(|n| n.parse().ok())
}

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Interfaces of a guest, by index.
pub fn interfaces(cfg: &Map<String, Value>) -> BTreeMap<u32, PropString> {
    cfg.iter().filter(|(k, _)| is_net_key(k)).filter_map(|(k, v)| Some((net_index(k)?, PropString::parse(&value_str(v))))).collect()
}

pub fn bridge_of(cfg: &Map<String, Value>, iface: u32) -> String {
    interfaces(cfg).get(&iface).and_then(|p| p.get("bridge").map(str::to_string)).unwrap_or_default()
}

/// What the main addresses of one interface translate to.
#[derive(Clone, Debug, Default, PartialEq)]
struct MainIps {
    ip4: Option<(String, Option<String>)>,
    ip6: Option<(String, Option<String>)>,
    vlan: Option<u16>,
}

fn main_ips(env: &Env, vmid: u64, iface: u32) -> MainIps {
    let db = env.ipam.lock();
    let mut m = MainIps::default();
    for v6 in [false, true] {
        if let Some((a, pool)) = db.main_for(&env.server_id, vmid, iface, v6) {
            let cidr = pool.as_ref().map(|p| p.cidr_for(&a.ip)).unwrap_or_else(|| format!("{}/{}", a.ip, if v6 { 128 } else { 32 }));
            let gw = pool.as_ref().map(|p| p.gateway.clone()).filter(|g| !g.trim().is_empty());
            if m.vlan.is_none() {
                m.vlan = pool.as_ref().and_then(|p| p.vlan);
            }
            if v6 {
                m.ip6 = Some((cidr, gw));
            } else {
                m.ip4 = Some((cidr, gw));
            }
        }
    }
    m
}

/// Rewrites addressing keys of a property string (`ip/gw/ip6/gw6`).
/// A family without a main address keeps its current value, unless that value
/// is an address that was just released: then v4 falls back to DHCP and v6 is
/// removed, so the guest never keeps using an address IPAM gave away.
fn rewrite_addressing(p: &mut PropString, m: &MainIps, released: &BTreeSet<String>) -> bool {
    let before = p.to_string();
    let host = |cidr: &str| cidr.split('/').next().unwrap_or("").to_string();
    match &m.ip4 {
        Some((cidr, gw)) => {
            p.set("ip", cidr);
            p.set_opt("gw", gw.clone());
        }
        None => {
            if p.get("ip").map(|v| released.contains(&host(v))).unwrap_or(false) {
                p.set("ip", "dhcp");
                p.remove("gw");
            }
        }
    }
    match &m.ip6 {
        Some((cidr, gw)) => {
            p.set("ip6", cidr);
            p.set_opt("gw6", gw.clone());
        }
        None => {
            if p.get("ip6").map(|v| released.contains(&host(v))).unwrap_or(false) {
                p.remove("ip6");
                p.remove("gw6");
            }
        }
    }
    p.to_string() != before
}

/// Writes the main addresses of every interface into the guest config,
/// syncs the IP filter, and (for VMs) regenerates cloud-init.
pub async fn apply(env: &Env, ctx: &JobCtx, g: &GuestRef, opts: ApplyOpts, released: &BTreeSet<String>) -> anyhow::Result<String> {
    let client = &env.client;
    let cfg = guest::config(client, g).await.context("reading guest config")?;
    let ifaces = interfaces(&cfg);
    if ifaces.is_empty() {
        bail!("the guest has no network interfaces");
    }
    let mut p = Params::new();
    let mut ci_changed = false;
    for (n, nic) in &ifaces {
        let m = main_ips(env, g.vmid, *n);
        let mut line = nic.clone();
        if let Some(t) = m.vlan
            && line.get("tag").is_none()
        {
            line.set("tag", t.to_string());
        }
        if opts.ipfilter == IpFilter::On && line.get("firewall") != Some("1") {
            // The ipset is only consulted on interfaces with the firewall flag.
            line.set("firewall", "1");
        }
        match g.kind {
            GuestKind::Lxc => {
                rewrite_addressing(&mut line, &m, released);
            }
            GuestKind::Qemu => {
                let key = format!("ipconfig{n}");
                let mut ipc = PropString::parse(&cfg.get(&key).map(value_str).unwrap_or_default());
                if rewrite_addressing(&mut ipc, &m, released) {
                    ctx.info(format!("{key} = {ipc}"));
                    p.push(&key, ipc.to_string());
                    ci_changed = true;
                }
            }
        }
        if line != *nic {
            ctx.info(format!("net{n} = {line}"));
            p.push(&format!("net{n}"), line.to_string());
        }
    }
    if !p.is_empty() {
        guest::set_config(client, ctx, g, p).await?;
    } else {
        ctx.info("network config already matches IPAM");
    }

    match opts.ipfilter {
        IpFilter::Keep => {
            // An enabled filter must follow the addresses, or new ones are blocked.
            let on = client.get(&format!("{}/firewall/options", g.path())).await.map(|o| o.b("ipfilter")).unwrap_or(false);
            if on {
                for n in ifaces.keys() {
                    sync_ipset(env, ctx, g, *n, &cfg).await?;
                }
            }
        }
        IpFilter::On => {
            for n in ifaces.keys() {
                sync_ipset(env, ctx, g, *n, &cfg).await?;
            }
            client.put(&format!("{}/firewall/options", g.path()), &Params::new().set("ipfilter", 1)).await.context("enabling IP filter")?;
            ctx.info("IP filter enabled (enforced while the guest firewall is on)");
        }
        IpFilter::Off => {
            for n in ifaces.keys() {
                delete_ipset(client, g, &format!("ipfilter-net{n}")).await?;
            }
            client
                .put(&format!("{}/firewall/options", g.path()), &Params::new().set("ipfilter", 0))
                .await
                .context("disabling IP filter")?;
            ctx.info("IP filter disabled");
        }
    }

    let mut msg = format!("addresses applied to {} {}", g.kind.label(), g.vmid);
    if g.kind == GuestKind::Qemu && ci_changed {
        let has_ci = cfg.values().any(|v| value_str(v).contains("cloudinit"));
        if has_ci {
            guest::regenerate_cloudinit(client, g).await?;
            ctx.info("cloud-init image regenerated");
            let running = guest::status(client, g).await.map(|s| s.s("status") == "running").unwrap_or(false);
            if running && opts.reboot {
                guest::power(client, ctx, g, PowerAction::Reboot).await?;
                msg.push_str(" (rebooted)");
            } else if running {
                msg.push_str("; reboot the VM to activate them");
            }
        } else {
            ctx.warn("the VM has no cloud-init drive: configure the new addresses inside the guest OS");
            msg.push_str(" (no cloud-init drive - configure inside the guest)");
        }
    }
    Ok(msg)
}

/// Keeps one `ipfilter-net<N>` ipset per
/// interface listing every assigned address. A family without addresses gets
/// a loopback placeholder so it is blocked rather than unrestricted; SLAAC
/// (`ip6=auto`) needs the whole v6 space allowed.
async fn sync_ipset(env: &Env, ctx: &JobCtx, g: &GuestRef, n: u32, cfg: &Map<String, Value>) -> anyhow::Result<()> {
    let client = &env.client;
    let name = format!("ipfilter-net{n}");
    let base = format!("{}/firewall/ipset", g.path());
    let mut wanted: BTreeMap<String, String> = env
        .ipam
        .lock()
        .guest_ips(&env.server_id, g.vmid)
        .into_iter()
        .filter(|a| a.iface() == n)
        .map(|a| (a.ip.clone(), if a.main { "Assigned (main)".to_string() } else { "Assigned".to_string() }))
        .collect();
    let slaac = match g.kind {
        GuestKind::Qemu => cfg.get(&format!("ipconfig{n}")).map(value_str),
        GuestKind::Lxc => cfg.get(&format!("net{n}")).map(value_str),
    }
    .map(|s| PropString::parse(&s).get("ip6") == Some("auto"))
    .unwrap_or(false);
    if slaac {
        wanted.insert("::/1".into(), "SLAAC: allow all [1/2]".into());
        wanted.insert("8000::/1".into(), "SLAAC: allow all [2/2]".into());
    }
    if !wanted.keys().any(|ip| !ip.contains(':')) {
        wanted.insert("127.0.0.1".into(), "Interface with no v4 IPs".into());
    }
    if !wanted.keys().any(|ip| ip.contains(':')) {
        wanted.insert("::1".into(), "Interface with no v6 IPs".into());
    }

    let sets = client.get_list(&base).await.context("listing ipsets")?;
    if !sets.iter().any(|s| s.s("name") == name) {
        client
            .post(&base, &Params::new().set("name", &name).set("comment", format!("Only allow assigned IPs on net{n} (PMXSpan)")))
            .await
            .with_context(|| format!("creating ipset {name}"))?;
    }
    let existing: Vec<String> = client.get_list(&format!("{base}/{name}")).await?.iter().map(|e| e.s("cidr")).collect();
    let norm = |c: &str| c.trim_end_matches("/32").trim_end_matches("/128").to_string();
    for (ip, comment) in &wanted {
        if !existing.iter().any(|e| norm(e) == *ip) {
            client
                .post(&format!("{base}/{name}"), &Params::new().set("cidr", ip).set("comment", comment))
                .await
                .with_context(|| format!("adding {ip} to {name}"))?;
        }
    }
    for e in existing.iter().filter(|e| !wanted.contains_key(&norm(e))) {
        client
            .delete(&format!("{base}/{name}/{}", urlencoding::encode(e)), &Params::new())
            .await
            .with_context(|| format!("removing {e} from {name}"))?;
    }
    ctx.info(format!("{name}: {}", wanted.keys().cloned().collect::<Vec<_>>().join(", ")));
    Ok(())
}

async fn delete_ipset(client: &PveClient, g: &GuestRef, name: &str) -> anyhow::Result<()> {
    let base = format!("{}/firewall/ipset", g.path());
    let sets = client.get_list(&base).await?;
    if !sets.iter().any(|s| s.s("name") == name) {
        return Ok(());
    }
    for e in client.get_list(&format!("{base}/{name}")).await? {
        client.delete(&format!("{base}/{name}/{}", urlencoding::encode(&e.s("cidr"))), &Params::new()).await?;
    }
    client.delete(&format!("{base}/{name}"), &Params::new()).await?;
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct AssignSpec {
    pub iface: u32,
    pub v6: bool,
    pub pool_id: Option<String>,
    /// Exact address for the first allocation; never silently replaced.
    pub preferred: Option<String>,
    pub count: usize,
    /// Make the (first) new address the interface's main address.
    pub make_main: bool,
}

/// Checks the guest's tenant pool IP limit before `count` more addresses.
async fn enforce_ip_limit(env: &Env, vmid: u64, count: u64) -> anyhow::Result<()> {
    if env.profile.pool_limits.is_empty() || count == 0 {
        return Ok(());
    }
    let Some(pool) = super::limits::pool_of(&env.client, vmid).await? else { return Ok(()) };
    let add = super::limits::Usage { ips: count, ..Default::default() };
    super::limits::enforce(&env.client, env.profile.pool_limits.get(&pool), &pool, &env.ipam, &env.server_id, add).await
}

/// Allocates addresses from IPAM for one interface and applies them.
pub async fn assign(env: Env, ctx: JobCtx, g: GuestRef, spec: AssignSpec, opts: ApplyOpts) -> anyhow::Result<String> {
    let cfg = guest::config(&env.client, &g).await?;
    if !interfaces(&cfg).contains_key(&spec.iface) {
        bail!("net{} does not exist on this guest", spec.iface);
    }
    let name = cfg.get("name").or_else(|| cfg.get("hostname")).map(value_str).unwrap_or_default();
    enforce_ip_limit(&env, g.vmid, spec.count.max(1) as u64).await?;
    let req = AllocRequest {
        server_id: env.server_id.clone(),
        vmid: g.vmid,
        hostname: name,
        bridge: bridge_of(&cfg, spec.iface),
        v6: spec.v6,
        preferred: spec.preferred.clone().filter(|s| !s.trim().is_empty()),
        pool_id: spec.pool_id.clone(),
        iface: spec.iface,
    };
    let allocs = env.ipam.lock().allocate_many(&req, spec.count.max(1))?;
    let list = allocs.iter().map(|a| a.ip.clone()).collect::<Vec<_>>().join(", ");
    ctx.info(format!("IPAM assigned {list} to net{}", spec.iface));
    if spec.make_main
        && let Some(first) = allocs.first()
        && !first.main
    {
        env.ipam.lock().set_main(&first.ip)?;
        ctx.info(format!("{} is now the main {} address of net{}", first.ip, if spec.v6 { "IPv6" } else { "IPv4" }, spec.iface));
    }
    match apply(&env, &ctx, &g, opts, &BTreeSet::new()).await {
        Ok(m) => Ok(format!("assigned {list}; {m}")),
        Err(e) => {
            // Keep IPAM truthful: the guest never got these addresses.
            let mut db = env.ipam.lock();
            for a in &allocs {
                let _ = db.release_ip(&a.ip);
            }
            ctx.warn(format!("released {list} again because applying failed"));
            Err(e)
        }
    }
}

pub async fn release(env: Env, ctx: JobCtx, g: GuestRef, ips: Vec<String>, opts: ApplyOpts) -> anyhow::Result<String> {
    {
        let mut db = env.ipam.lock();
        for ip in &ips {
            db.release_ip(ip)?;
        }
    }
    ctx.info(format!("released {}", ips.join(", ")));
    let released: BTreeSet<String> = ips.iter().cloned().collect();
    let m = apply(&env, &ctx, &g, opts, &released).await?;
    Ok(format!("released {}; {m}", ips.join(", ")))
}

pub async fn make_main(env: Env, ctx: JobCtx, g: GuestRef, ip: String, opts: ApplyOpts) -> anyhow::Result<String> {
    env.ipam.lock().set_main(&ip)?;
    ctx.info(format!("{ip} is now a main address"));
    apply(&env, &ctx, &g, opts, &BTreeSet::new()).await
}

#[derive(Clone, Debug)]
pub struct NewInterface {
    /// Explicit bridge; otherwise the pool's bridge.
    pub bridge: Option<String>,
    pub pool4: Option<String>,
    pub pool6: Option<String>,
    pub count4: usize,
    pub count6: usize,
    pub preferred4: Option<String>,
    pub preferred6: Option<String>,
    pub model: String,
    pub firewall: bool,
}

/// Adds `net<N>` with addresses from IPAM.
pub async fn add_interface(env: Env, ctx: JobCtx, g: GuestRef, spec: NewInterface, opts: ApplyOpts) -> anyhow::Result<String> {
    let client = &env.client;
    let cfg = guest::config(client, &g).await?;
    let n = (0..32).find(|i| !cfg.contains_key(&format!("net{i}"))).ok_or_else(|| anyhow!("no free interface slot"))?;
    let pool_bridge =
        |id: &Option<String>| id.as_ref().and_then(|id| env.ipam.lock().pool(id).map(|p| p.bridge.clone())).filter(|b| !b.is_empty());
    let bridge = spec
        .bridge
        .clone()
        .filter(|b| !b.is_empty())
        .or_else(|| pool_bridge(&spec.pool4))
        .or_else(|| pool_bridge(&spec.pool6))
        .unwrap_or_else(|| env.settings.default_bridge.clone());
    let name = cfg.get("name").or_else(|| cfg.get("hostname")).map(value_str).unwrap_or_default();
    enforce_ip_limit(&env, g.vmid, (spec.count4 + spec.count6) as u64).await?;
    let mut allocated = Vec::new();
    for (v6, count, pool, pref) in [(false, spec.count4, &spec.pool4, &spec.preferred4), (true, spec.count6, &spec.pool6, &spec.preferred6)]
    {
        if count == 0 {
            continue;
        }
        let req = AllocRequest {
            server_id: env.server_id.clone(),
            vmid: g.vmid,
            hostname: name.clone(),
            bridge: bridge.clone(),
            v6,
            preferred: pref.clone().filter(|s| !s.trim().is_empty()),
            pool_id: pool.clone(),
            iface: n,
        };
        match env.ipam.lock().allocate_many(&req, count) {
            Ok(a) => allocated.extend(a.into_iter().map(|a| a.ip)),
            Err(e) => {
                let mut db = env.ipam.lock();
                for ip in &allocated {
                    let _ = db.release_ip(ip);
                }
                return Err(e);
            }
        }
    }
    let mac = generate_mac(&env.settings.mac_prefix);
    let mut line = PropString::default();
    match g.kind {
        GuestKind::Qemu => {
            line.set(if spec.model.is_empty() { "virtio" } else { &spec.model }, &mac);
            line.set("bridge", &bridge);
        }
        GuestKind::Lxc => {
            line.set("name", format!("eth{n}"));
            line.set("bridge", &bridge);
            line.set("hwaddr", &mac);
            line.set("type", "veth");
        }
    }
    if spec.firewall {
        line.set("firewall", "1");
    }
    ctx.info(format!("adding net{n} on {bridge}"));
    if let Err(e) = guest::set_config(client, &ctx, &g, Params::new().set(&format!("net{n}"), line.to_string())).await {
        let mut db = env.ipam.lock();
        for ip in &allocated {
            let _ = db.release_ip(ip);
        }
        return Err(e.context("adding the interface failed; its addresses were released"));
    }
    let m = apply(&env, &ctx, &g, opts, &BTreeSet::new()).await?;
    Ok(format!("net{n} added{}; {m}", if allocated.is_empty() { String::new() } else { format!(" with {}", allocated.join(", ")) }))
}

/// Removes `net<N>` and releases its addresses.
pub async fn remove_interface(env: Env, ctx: JobCtx, g: GuestRef, n: u32) -> anyhow::Result<String> {
    let client = &env.client;
    let mut del = vec![format!("net{n}")];
    if g.kind == GuestKind::Qemu {
        let cfg = guest::config(client, &g).await?;
        if cfg.contains_key(&format!("ipconfig{n}")) {
            del.push(format!("ipconfig{n}"));
        }
    }
    guest::set_config(client, &ctx, &g, Params::new().set("delete", del.join(","))).await?;
    let released = env.ipam.lock().release_iface(&env.server_id, g.vmid, n)?;
    if !released.is_empty() {
        ctx.info(format!("released {}", released.join(", ")));
    }
    if let Err(e) = delete_ipset(client, &g, &format!("ipfilter-net{n}")).await {
        ctx.warn(format!("could not remove ipset ipfilter-net{n}: {e:#}"));
    }
    Ok(format!("net{n} removed"))
}

/// The static addresses in a guest's config, per interface (LXC `netN ip=`,
/// QEMU cloud-init `ipconfigN ip=`), without prefix length.
pub fn static_ips(kind: GuestKind, cfg: &Map<String, Value>) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for n in interfaces(cfg).keys() {
        let src = match kind {
            GuestKind::Lxc => cfg.get(&format!("net{n}")).map(value_str),
            GuestKind::Qemu => cfg.get(&format!("ipconfig{n}")).map(value_str),
        }
        .unwrap_or_default();
        let p = PropString::parse(&src);
        for key in ["ip", "ip6"] {
            let Some(v) = p.get(key) else { continue };
            if matches!(v, "dhcp" | "auto" | "manual" | "") {
                continue;
            }
            out.push((*n, v.split('/').next().unwrap_or(v).to_string()));
        }
    }
    out
}

/// Records the static addresses already configured on the guest into IPAM,
/// so guests created elsewhere can be managed here.
pub async fn adopt(env: Env, ctx: JobCtx, g: GuestRef) -> anyhow::Result<String> {
    let cfg = guest::config(&env.client, &g).await?;
    let name = cfg.get("name").or_else(|| cfg.get("hostname")).map(value_str).unwrap_or_default();
    let mut adopted = Vec::new();
    let mut skipped = Vec::new();
    for (n, ip) in static_ips(g.kind, &cfg) {
        match env.ipam.lock().register_existing(&env.server_id, g.vmid, n, &ip, &name) {
            Ok(_) => adopted.push(format!("{ip} (net{n})")),
            Err(e) => skipped.push(format!("{ip}: {e:#}")),
        }
    }
    for s in &skipped {
        ctx.warn(format!("not adopted: {s}"));
    }
    if adopted.is_empty() {
        return Ok(if skipped.is_empty() { "no static addresses found in the guest config".into() } else { "nothing adopted".into() });
    }
    Ok(format!("adopted {}", adopted.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressing_rewrite() {
        let m = MainIps { ip4: Some(("192.0.2.5/24".into(), Some("192.0.2.1".into()))), ip6: None, vlan: None };
        let mut p = PropString::parse("name=eth0,bridge=vmbr0,ip=dhcp,ip6=2001:db8::9/64,type=veth");
        assert!(rewrite_addressing(&mut p, &m, &BTreeSet::new()));
        assert_eq!(p.to_string(), "name=eth0,bridge=vmbr0,ip=192.0.2.5/24,ip6=2001:db8::9/64,type=veth,gw=192.0.2.1");
        // Unchanged when already correct.
        assert!(!rewrite_addressing(&mut p, &m, &BTreeSet::new()));
        // A released address is not kept: v4 -> dhcp, v6 removed.
        let none = MainIps::default();
        let released: BTreeSet<String> = ["192.0.2.5".to_string(), "2001:db8::9".to_string()].into();
        assert!(rewrite_addressing(&mut p, &none, &released));
        assert_eq!(p.to_string(), "name=eth0,bridge=vmbr0,ip=dhcp,type=veth");
        // A working config with no IPAM data is never emptied.
        let mut q = PropString::parse("ip=198.51.100.7/24,gw=198.51.100.1");
        assert!(!rewrite_addressing(&mut q, &none, &BTreeSet::new()));
    }
}
