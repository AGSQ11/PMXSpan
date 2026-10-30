//! Guest provisioning: containers from a template, VMs from a cloned
//! template, an installer ISO or a catalog image.
//!
//! Every step is logged to the job; a failure after IPs were allocated
//! releases them again, and a failure after the guest exists says so
//! explicitly instead of pretending nothing happened.

use super::catalog::{self, CatalogEntry};
use super::guest::{self, run_task};
use super::placement::{self, PlacementRequest};
use crate::api::types::{GuestKind, GuestRef, NodeStorage};
use crate::api::{Params, PveClient};
use crate::config::{ServerProfile, Settings};
use crate::ipam::{AllocRequest, Allocation, IpamDb};
use crate::jobs::JobCtx;
use crate::util::{PropString, ValueExt, generate_mac, sanitize_hostname, validate_mac};
use anyhow::{Context, bail};
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// Clone a QEMU template (`template: 1`).
    QemuTemplate { node: String, vmid: u64, full: bool },
    /// Empty VM with an installer ISO attached (volid or none).
    QemuIso { iso: Option<String> },
    /// LXC from a `vztmpl` volume.
    LxcTemplate { volid: String },
    /// Download an image from the OS catalog: QEMU via `import-from`, LXC as `ostemplate`.
    Catalog { entry: Box<CatalogEntry> },
}

impl Source {
    pub fn kind(&self) -> GuestKind {
        match self {
            Source::LxcTemplate { .. } => GuestKind::Lxc,
            Source::Catalog { entry } if entry.is_lxc() => GuestKind::Lxc,
            _ => GuestKind::Qemu,
        }
    }
}

/// Applies a catalog entry's ostype and config overrides. Keys outside the
/// catalog allowlist are skipped here too, so an entry stored before the
/// allowlist existed (or edited on disk) cannot slip them through.
pub fn apply_catalog_overrides(p: &mut Params, ctx: &JobCtx, entry: &CatalogEntry) {
    if !entry.ostype.is_empty() {
        p.push("ostype", catalog::normalize_ostype(&entry.virt, &entry.ostype));
    }
    for (k, v) in &entry.config {
        match catalog::check_config_key(&entry.virt, k, v) {
            Ok(()) => p.push(k, v),
            Err(e) => ctx.warn(format!("catalog entry `{}`: skipped {e:#}", entry.id)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum IpMode {
    #[default]
    None,
    Dhcp,
    /// IPv6 SLAAC.
    Auto,
    Static {
        cidr: String,
        gateway: String,
    },
    /// Take an address from local IPAM; `preferred` must be honoured exactly.
    Ipam {
        pool_id: Option<String>,
        preferred: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct NetSpec {
    pub bridge: String,
    pub vlan: Option<u16>,
    pub rate_mbps: Option<f64>,
    pub firewall: bool,
    /// QEMU NIC model (virtio, e1000, ...). Ignored for LXC.
    pub model: String,
    /// Fixed MAC, or generated with the configured prefix.
    pub mac: Option<String>,
    pub ipv4: IpMode,
    pub ipv6: IpMode,
}

impl Default for NetSpec {
    fn default() -> Self {
        Self {
            bridge: "vmbr0".into(),
            vlan: None,
            rate_mbps: None,
            firewall: true,
            model: "virtio".into(),
            mac: None,
            ipv4: IpMode::Dhcp,
            ipv6: IpMode::None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CreateSpec {
    pub source: Source,
    /// None = auto-place.
    pub node: Option<String>,
    /// None = next free id.
    pub vmid: Option<u64>,
    pub hostname: String,
    pub cores: u32,
    pub sockets: u32,
    pub memory_mb: u64,
    pub swap_mb: u64,
    pub disk_gb: u64,
    /// None = auto-pick a storage with the right content type.
    pub storage: Option<String>,
    pub disk_bus: String,
    pub cpu_type: String,
    pub ostype: String,
    pub cpulimit: Option<f64>,
    pub net: NetSpec,
    pub nameserver: String,
    pub searchdomain: String,
    pub ciuser: String,
    pub password: String,
    pub ssh_keys: String,
    pub pool: Option<String>,
    pub tags: String,
    pub description: String,
    pub unprivileged: bool,
    pub nesting: bool,
    pub agent: bool,
    pub onboot: bool,
    pub start: bool,
    /// Add to HA (with this group; empty = none).
    pub ha: Option<String>,
}

impl Default for CreateSpec {
    fn default() -> Self {
        Self {
            source: Source::QemuIso { iso: None },
            node: None,
            vmid: None,
            hostname: String::new(),
            cores: 2,
            sockets: 1,
            memory_mb: 2048,
            swap_mb: 512,
            disk_gb: 20,
            storage: None,
            disk_bus: "scsi".into(),
            cpu_type: "x86-64-v2-AES".into(),
            ostype: "l26".into(),
            cpulimit: None,
            net: NetSpec::default(),
            nameserver: String::new(),
            searchdomain: String::new(),
            ciuser: String::new(),
            password: String::new(),
            ssh_keys: String::new(),
            pool: None,
            tags: String::new(),
            description: String::new(),
            unprivileged: true,
            nesting: true,
            agent: true,
            onboot: true,
            start: true,
            ha: None,
        }
    }
}

/// Everything a job needs besides the spec.
#[derive(Clone)]
pub struct Env {
    pub client: PveClient,
    pub server_id: String,
    pub profile: ServerProfile,
    pub settings: Settings,
    pub ipam: Arc<Mutex<IpamDb>>,
}

/// Static address info resolved for one interface.
#[derive(Clone, Debug, Default)]
pub struct ResolvedIps {
    pub ip4: Option<String>,
    pub gw4: Option<String>,
    pub ip6: Option<String>,
    pub gw6: Option<String>,
    pub dns: Vec<String>,
    /// Addresses taken from IPAM by this operation (to roll back on failure).
    pub allocated: Vec<String>,
    /// VLAN of the IPAM pool the address came from.
    pub vlan: Option<u16>,
}

impl ResolvedIps {
    pub fn summary(&self) -> String {
        let mut s = Vec::new();
        if let Some(ip) = &self.ip4 {
            s.push(ip.clone());
        }
        if let Some(ip) = &self.ip6 {
            s.push(ip.clone());
        }
        s.join(", ")
    }
}

fn resolve_mode(
    mode: &IpMode,
    v6: bool,
    env: &Env,
    vmid: u64,
    hostname: &str,
    bridge: &str,
    out: &mut ResolvedIps,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    Ok(match mode {
        IpMode::None => (None, None),
        IpMode::Dhcp => (Some("dhcp".into()), None),
        IpMode::Auto => (Some(if v6 { "auto".into() } else { "dhcp".into() }), None),
        IpMode::Static { cidr, gateway } => {
            let cidr = cidr.trim();
            if !cidr.contains('/') {
                bail!("static address '{cidr}' must include a prefix length, e.g. 192.0.2.10/24");
            }
            (Some(cidr.to_string()), Some(gateway.trim().to_string()).filter(|g| !g.is_empty()))
        }
        IpMode::Ipam { pool_id, preferred } => {
            let req = AllocRequest {
                server_id: env.server_id.clone(),
                vmid,
                hostname: hostname.to_string(),
                bridge: bridge.to_string(),
                v6,
                preferred: preferred.clone(),
                pool_id: pool_id.clone(),
                iface: 0,
            };
            let a: Allocation = env.ipam.lock().allocate(&req)?;
            out.allocated.push(a.ip.clone());
            out.vlan = out.vlan.or(a.vlan);
            out.dns.extend(a.dns.split([' ', ',']).filter(|s| !s.is_empty()).map(str::to_string));
            (Some(a.cidr), Some(a.gateway).filter(|g| !g.is_empty()))
        }
    })
}

pub fn resolve_ips(env: &Env, net: &NetSpec, vmid: u64, hostname: &str) -> anyhow::Result<ResolvedIps> {
    let mut out = ResolvedIps::default();
    let r4 = resolve_mode(&net.ipv4, false, env, vmid, hostname, &net.bridge, &mut out);
    let (ip4, gw4) = match r4 {
        Ok(v) => v,
        Err(e) => return Err(e.context("IPv4 assignment failed")),
    };
    let (ip6, gw6) = match resolve_mode(&net.ipv6, true, env, vmid, hostname, &net.bridge, &mut out) {
        Ok(v) => v,
        Err(e) => {
            release(env, &out.allocated);
            return Err(e.context("IPv6 assignment failed"));
        }
    };
    out.ip4 = ip4;
    out.gw4 = gw4;
    out.ip6 = ip6;
    out.gw6 = gw6;
    Ok(out)
}

fn release(env: &Env, ips: &[String]) {
    let mut db = env.ipam.lock();
    for ip in ips {
        let _ = db.release_ip(ip);
    }
}

/// `net0` for an LXC container.
pub fn lxc_net_line(net: &NetSpec, ips: &ResolvedIps, mac: &str, ifname: &str) -> String {
    let mut p = PropString::default();
    p.set("name", ifname);
    p.set("bridge", &net.bridge);
    p.set("hwaddr", mac);
    p.set("type", "veth");
    if let Some(ip) = &ips.ip4 {
        p.set("ip", ip);
    }
    if let Some(gw) = &ips.gw4 {
        p.set("gw", gw);
    }
    if let Some(ip) = &ips.ip6 {
        p.set("ip6", ip);
    }
    if let Some(gw) = &ips.gw6 {
        p.set("gw6", gw);
    }
    if net.firewall {
        p.set("firewall", "1");
    }
    if let Some(t) = net.vlan {
        p.set("tag", t.to_string());
    }
    if let Some(r) = net.rate_mbps.filter(|r| *r > 0.0) {
        // Proxmox rate is MB/s.
        p.set("rate", format!("{:.3}", r / 8.0));
    }
    p.to_string()
}

/// `net0` for a QEMU VM.
pub fn qemu_net_line(net: &NetSpec, mac: &str) -> String {
    let mut p = PropString::default();
    p.set(if net.model.is_empty() { "virtio" } else { &net.model }, mac);
    p.set("bridge", &net.bridge);
    if net.firewall {
        p.set("firewall", "1");
    }
    if let Some(t) = net.vlan {
        p.set("tag", t.to_string());
    }
    if let Some(r) = net.rate_mbps.filter(|r| *r > 0.0) {
        p.set("rate", format!("{:.3}", r / 8.0));
    }
    p.to_string()
}

/// cloud-init `ipconfig0`. Empty when nothing is configured.
pub fn ipconfig_line(ips: &ResolvedIps) -> String {
    let mut p = PropString::default();
    if let Some(ip) = &ips.ip4 {
        p.set("ip", ip);
    }
    if let Some(gw) = &ips.gw4 {
        p.set("gw", gw);
    }
    if let Some(ip) = &ips.ip6 {
        p.set("ip6", ip);
    }
    if let Some(gw) = &ips.gw6 {
        p.set("gw6", gw);
    }
    p.to_string()
}

/// Proxmox wants `sshkeys` URL-encoded (spaces as %20) inside the form value.
pub fn encode_sshkeys(keys: &str) -> Option<String> {
    let cleaned: Vec<&str> = keys.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    if cleaned.is_empty() {
        return None;
    }
    Some(urlencoding::encode(&cleaned.join("\n")).into_owned())
}

pub fn validate(spec: &CreateSpec) -> anyhow::Result<()> {
    let mut errs = Vec::new();
    if sanitize_hostname(&spec.hostname).is_empty() {
        errs.push("hostname is blank after sanitization".to_string());
    }
    if spec.memory_mb < 16 {
        errs.push("memory must be at least 16 MiB".into());
    }
    if spec.cores == 0 {
        errs.push("CPU cores cannot be 0".into());
    }
    if spec.disk_gb < 1 {
        errs.push("disk size cannot be lower than 1 GiB".into());
    }
    if spec.source.kind() == GuestKind::Lxc && spec.password.is_empty() && spec.ssh_keys.trim().is_empty() {
        errs.push("containers need a root password or at least one SSH key".into());
    }
    if !spec.password.is_empty() && spec.password.len() < 5 {
        errs.push("password must be at least 5 characters".into());
    }
    if let Some(mac) = &spec.net.mac
        && !mac.is_empty()
        && !validate_mac(mac)
    {
        errs.push(format!("'{mac}' is not a valid MAC address"));
    }
    if let Source::LxcTemplate { volid } = &spec.source
        && volid.is_empty()
    {
        errs.push("choose a container template".into());
    }
    if let Source::Catalog { entry } = &spec.source
        && let Err(e) = entry.validate()
    {
        errs.push(format!("{e:#}"));
    }
    if !errs.is_empty() {
        bail!("validation failed:\n  {}", errs.join("\n  "));
    }
    Ok(())
}

fn description(spec: &CreateSpec, settings: &Settings) -> String {
    let mut d = spec.description.trim().to_string();
    if settings.tag_managed_guests {
        let marker = format!("Managed by PMXSpan ({})", chrono::Local::now().format("%Y-%m-%d"));
        d = if d.is_empty() { marker } else { format!("{d}\n\n{marker}") };
    }
    d
}

fn tags(spec: &CreateSpec, settings: &Settings) -> String {
    let mut t: Vec<String> = spec.tags.split([',', ';', ' ']).filter(|s| !s.is_empty()).map(|s| s.to_ascii_lowercase()).collect();
    // The managed tag keeps its case (`PMXSpan`); Proxmox tags allow it.
    let mt = settings.managed_tag.trim();
    if settings.tag_managed_guests && !mt.is_empty() && !t.iter().any(|x| x.eq_ignore_ascii_case(mt)) {
        t.push(mt.to_string());
    }
    t.join(";")
}

fn nameservers(spec: &CreateSpec, settings: &Settings, ips: &ResolvedIps) -> Option<String> {
    let src = if !spec.nameserver.trim().is_empty() {
        spec.nameserver.clone()
    } else if !ips.dns.is_empty() {
        ips.dns.join(" ")
    } else {
        settings.default_dns.clone()
    };
    let list: Vec<&str> = src.split([' ', ',']).filter(|s| !s.is_empty()).collect();
    (!list.is_empty()).then(|| list.join(" "))
}

pub async fn run(env: Env, mut spec: CreateSpec, ctx: JobCtx) -> anyhow::Result<String> {
    let client = &env.client;
    spec.hostname = sanitize_hostname(&spec.hostname);
    validate(&spec)?;
    let kind = spec.source.kind();

    // 1. VMID
    let range = env.profile.vmid_range();
    let vmid = match spec.vmid {
        Some(id) => {
            if let Some((lo, hi)) = range
                && !(lo..=hi).contains(&id)
            {
                bail!("VMID {id} is outside this server's range {lo}–{hi} (Servers → Edit)");
            }
            if !guest::vmid_free(client, id).await? {
                bail!("VMID {id} is already in use");
            }
            id
        }
        None => guest::next_vmid_in(client, range).await?,
    };
    ctx.info(format!("VMID {vmid}"));
    ctx.progress(0.05);

    // 2. Placement. Template clones default to the template's node so local
    //    template disks can be cloned; other nodes are handled by migrating after.
    //    Catalog images restrict placement to nodes that can download them.
    let content = if kind == GuestKind::Lxc { "rootdir" } else { "images" };
    let catalog_candidates = match &spec.source {
        Source::Catalog { entry } => Some(catalog_nodes(&env, &ctx, entry, spec.node.as_deref()).await?),
        _ => None,
    };
    let allowed_nodes: Vec<String> = catalog_candidates.as_ref().map(|m| m.keys().cloned().collect()).unwrap_or_default();
    let (node, storage) = match (&spec.node, &spec.source) {
        (Some(n), _) => {
            let storage = match &spec.storage {
                Some(s) => s.clone(),
                None => {
                    let req = PlacementRequest {
                        memory_mb: spec.memory_mb,
                        disk_gb: spec.disk_gb,
                        content,
                        storage: None,
                        nodes: vec![n.clone()],
                        exclude: vec![],
                    };
                    placement::select(client, &req, env.settings.placement, &env.settings.resource_priority).await?.storage
                }
            };
            (n.clone(), storage)
        }
        (None, _) => {
            let req = PlacementRequest {
                memory_mb: spec.memory_mb,
                disk_gb: spec.disk_gb,
                content,
                storage: spec.storage.clone(),
                nodes: allowed_nodes.clone(),
                exclude: env.profile.maintenance_nodes.clone(),
            };
            let p = placement::select(client, &req, env.settings.placement, &env.settings.resource_priority).await?;
            for n in &p.notes {
                ctx.info(format!("placement: {n}"));
            }
            (p.node, p.storage)
        }
    };
    if env.profile.in_maintenance(&node) {
        ctx.warn(format!("{node} is in maintenance; creating there because it was chosen explicitly"));
    }
    ctx.info(format!("target node {node}, storage {storage}"));
    ctx.progress(0.1);

    // 3. Addresses
    let ips = resolve_ips(&env, &spec.net, vmid, &spec.hostname)?;
    if !ips.allocated.is_empty() {
        ctx.info(format!("IPAM allocated {}", ips.allocated.join(", ")));
    }
    if spec.net.vlan.is_none()
        && let Some(v) = ips.vlan
    {
        ctx.info(format!("using VLAN {v} of the IPAM pool"));
        spec.net.vlan = Some(v);
    }
    let mac = spec.net.mac.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| generate_mac(&env.settings.mac_prefix));

    let result = match &spec.source {
        Source::LxcTemplate { volid } => create_lxc(&env, &ctx, &spec, vmid, &node, &storage, volid, &ips, &mac, None).await,
        Source::QemuIso { iso } => create_qemu_iso(&env, &ctx, &spec, vmid, &node, &storage, iso.as_deref(), &mac).await,
        Source::QemuTemplate { node: tnode, vmid: tid, full } => {
            create_qemu_clone(&env, &ctx, &spec, vmid, &node, &storage, tnode, *tid, *full, &ips, &mac).await
        }
        Source::Catalog { entry } => {
            let cands = catalog_candidates.as_ref().and_then(|m| m.get(&node)).cloned().unwrap_or_default();
            match catalog::download(client, &ctx, &node, entry, vmid, &cands, &env.settings).await {
                Ok(art) => {
                    let r = if entry.is_lxc() {
                        create_lxc(&env, &ctx, &spec, vmid, &node, &storage, &art.volid, &ips, &mac, Some(entry)).await
                    } else {
                        create_qemu_catalog(&env, &ctx, &spec, vmid, &node, &storage, entry, &art.volid, &ips, &mac).await
                    };
                    // The downloaded image is never left behind, success or not.
                    catalog::cleanup(client, &ctx, &art).await;
                    r
                }
                Err(e) => Err(e.context("downloading the catalog image failed")),
            }
        }
    };

    if let Err(e) = result {
        release(&env, &ips.allocated);
        if !ips.allocated.is_empty() {
            ctx.warn(format!("released IPAM addresses {}", ips.allocated.join(", ")));
        }
        // Tell the operator whether a half-built guest was left behind.
        let exists = client.get(&format!("/nodes/{node}/{}/{vmid}/status/current", kind.api())).await.is_ok();
        if exists {
            ctx.warn(format!("{} {vmid} exists on {node} but provisioning did not complete; inspect or destroy it", kind.label()));
        }
        return Err(e);
    }

    let g = GuestRef { node: node.clone(), kind, vmid };
    if spec.start {
        ctx.progress(0.9);
        guest::power(client, &ctx, &g, guest::PowerAction::Start).await.context("guest was created but failed to start")?;
    }
    if let Some(group) = &spec.ha {
        let sid = format!("{}:{vmid}", if kind == GuestKind::Lxc { "ct" } else { "vm" });
        let p = Params::new().set("sid", &sid).set("state", if spec.start { "started" } else { "stopped" }).opt("group", Some(group));
        match client.post("/cluster/ha/resources", &p).await {
            Ok(_) => ctx.info(format!("added {sid} to HA{}", if group.is_empty() { String::new() } else { format!(" (group {group})") })),
            Err(e) => ctx.warn(format!("the guest was created but adding it to HA failed: {e:#}")),
        }
    }
    let ipdesc = ips.summary();
    Ok(format!(
        "{} {vmid} '{}' created on {node}{}",
        kind.label(),
        spec.hostname,
        if ipdesc.is_empty() { String::new() } else { format!(" ({ipdesc})") }
    ))
}

#[allow(clippy::too_many_arguments)]
async fn create_lxc(
    env: &Env,
    ctx: &JobCtx,
    spec: &CreateSpec,
    vmid: u64,
    node: &str,
    storage: &str,
    volid: &str,
    ips: &ResolvedIps,
    mac: &str,
    catalog_entry: Option<&CatalogEntry>,
) -> anyhow::Result<()> {
    let mut p = Params::new()
        .set("vmid", vmid)
        .set("hostname", &spec.hostname)
        .set("ostemplate", volid)
        .set("rootfs", format!("{storage}:{}", spec.disk_gb))
        .set("memory", spec.memory_mb)
        .set("swap", spec.swap_mb)
        .set("cores", spec.cores)
        .set("net0", lxc_net_line(&spec.net, ips, mac, "eth0"))
        .flag("unprivileged", spec.unprivileged)
        .flag("onboot", spec.onboot)
        .opt("pool", spec.pool.as_deref())
        .opt("tags", Some(tags(spec, &env.settings)))
        .opt("description", Some(description(spec, &env.settings)))
        .opt("nameserver", nameservers(spec, &env.settings, ips))
        .opt("searchdomain", Some(spec.searchdomain.trim()).filter(|s| !s.is_empty()).or(Some(env.settings.default_search_domain.trim())))
        .opt("password", Some(&spec.password));
    if let Some(l) = spec.cpulimit.filter(|l| *l > 0.0) {
        p.push("cpulimit", format!("{l}"));
    }
    if spec.nesting {
        p.push("features", "nesting=1");
    }
    let keys: Vec<&str> = spec.ssh_keys.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    if !keys.is_empty() {
        p.push("ssh-public-keys", keys.join("\n"));
    }
    if let Some(e) = catalog_entry {
        apply_catalog_overrides(&mut p, ctx, e);
    }
    ctx.info(format!("creating container {vmid} from {volid}"));
    let r = env.client.post(&format!("/nodes/{node}/lxc"), &p).await?;
    run_task(&env.client, ctx, "create container", r).await
}

#[allow(clippy::too_many_arguments)]
async fn create_qemu_iso(
    env: &Env,
    ctx: &JobCtx,
    spec: &CreateSpec,
    vmid: u64,
    node: &str,
    storage: &str,
    iso: Option<&str>,
    mac: &str,
) -> anyhow::Result<()> {
    let bus = if spec.disk_bus.is_empty() { "scsi" } else { spec.disk_bus.as_str() };
    let disk_key = format!("{bus}0");
    let mut disk = format!("{storage}:{}", spec.disk_gb);
    if bus == "scsi" || bus == "virtio" {
        disk.push_str(",iothread=1,discard=on");
    }
    let cd = match iso {
        Some(i) if !i.is_empty() => format!("{i},media=cdrom"),
        _ => "none,media=cdrom".into(),
    };
    let mut p = Params::new()
        .set("vmid", vmid)
        .set("name", &spec.hostname)
        .set("memory", spec.memory_mb)
        .set("cores", spec.cores)
        .set("sockets", spec.sockets.max(1))
        .set("cpu", &spec.cpu_type)
        .set("ostype", &spec.ostype)
        .set("scsihw", "virtio-scsi-single")
        .set(&disk_key, disk)
        .set("ide2", cd)
        .set("boot", format!("order={disk_key};ide2;net0"))
        .set("net0", qemu_net_line(&spec.net, mac))
        .flag("onboot", spec.onboot)
        .opt("pool", spec.pool.as_deref())
        .opt("tags", Some(tags(spec, &env.settings)))
        .opt("description", Some(description(spec, &env.settings)));
    if spec.agent {
        p.push("agent", "1");
    }
    if let Some(l) = spec.cpulimit.filter(|l| *l > 0.0) {
        p.push("cpulimit", format!("{l}"));
    }
    if spec.ostype.starts_with('w') {
        // Windows guests want a TPM-less, UEFI-less safe default; keep VGA standard.
        p.push("vga", "std");
    }
    ctx.info(format!("creating VM {vmid}"));
    let r = env.client.post(&format!("/nodes/{node}/qemu"), &p).await?;
    run_task(&env.client, ctx, "create VM", r).await
}

/// Nodes that may receive a catalog image, with their usable download storages.
async fn catalog_nodes(
    env: &Env,
    ctx: &JobCtx,
    entry: &CatalogEntry,
    fixed: Option<&str>,
) -> anyhow::Result<std::collections::BTreeMap<String, Vec<NodeStorage>>> {
    let client = &env.client;
    let size = catalog::estimate_size(entry, &env.settings).await;
    ctx.info(format!("catalog image `{}`: needs ~{} of download space", entry.label, crate::util::fmt_bytes(size as f64 * 1.2)));
    let nodes: Vec<String> = match fixed {
        Some(n) => vec![n.to_string()],
        None => client.get_list("/nodes").await?.iter().filter(|n| n.s("status") == "online").map(|n| n.s("node")).collect(),
    };
    let mut out = std::collections::BTreeMap::new();
    let mut why = Vec::new();
    for n in nodes {
        if !entry.nodes.is_empty() && !entry.nodes.contains(&n) {
            why.push(format!("{n}: not in the entry's node list"));
            continue;
        }
        match catalog::node_candidates(client, &n, entry, &env.profile, size).await {
            Ok(c) if !c.is_empty() => {
                ctx.info(format!("{n}: download storage {}", c.iter().map(|s| s.storage.clone()).collect::<Vec<_>>().join(" -> ")));
                out.insert(n, c);
            }
            Ok(_) => why.push(format!("{n}: {}", catalog::describe_missing(&env.profile, entry))),
            Err(e) => why.push(format!("{n}: cannot read storage ({e:#})")),
        }
    }
    if out.is_empty() {
        bail!("no node can download `{}`:\n  {}", entry.label, why.join("\n  "));
    }
    Ok(out)
}

/// A VM whose boot disk is imported from a downloaded cloud image.
#[allow(clippy::too_many_arguments)]
async fn create_qemu_catalog(
    env: &Env,
    ctx: &JobCtx,
    spec: &CreateSpec,
    vmid: u64,
    node: &str,
    storage: &str,
    entry: &CatalogEntry,
    image: &str,
    ips: &ResolvedIps,
    mac: &str,
) -> anyhow::Result<()> {
    let bus = if spec.disk_bus.is_empty() { "scsi" } else { spec.disk_bus.as_str() };
    let disk_key = format!("{bus}0");
    let mut disk = format!("{storage}:0,import-from={image}");
    if bus == "scsi" || bus == "virtio" {
        disk.push_str(",iothread=1,discard=on");
    }
    let mut p = Params::new()
        .set("vmid", vmid)
        .set("name", &spec.hostname)
        .set("memory", spec.memory_mb)
        .set("cores", spec.cores)
        .set("sockets", spec.sockets.max(1))
        .set("cpu", &spec.cpu_type)
        .set("ostype", catalog::normalize_ostype("qemu", &entry.ostype))
        .set("scsihw", "virtio-scsi-single")
        .set(&disk_key, disk)
        .set("ide2", format!("{storage}:cloudinit"))
        // Cloud images expect a serial console.
        .set("serial0", "socket")
        .set("vga", "serial0")
        .set("boot", format!("order={disk_key};net0"))
        .set("net0", qemu_net_line(&spec.net, mac))
        .flag("onboot", spec.onboot)
        .opt("pool", spec.pool.as_deref())
        .opt("tags", Some(tags(spec, &env.settings)))
        .opt("description", Some(description(spec, &env.settings)));
    if spec.agent {
        p.push("agent", "1");
    }
    if let Some(l) = spec.cpulimit.filter(|l| *l > 0.0) {
        p.push("cpulimit", format!("{l}"));
    }
    let ipc = ipconfig_line(ips);
    if !ipc.is_empty() {
        p.push("ipconfig0", ipc);
    }
    let user = if !spec.ciuser.is_empty() { spec.ciuser.clone() } else { entry.cloud_user.clone() };
    if !user.is_empty() {
        p.push("ciuser", user);
    }
    if !spec.password.is_empty() {
        p.push("cipassword", &spec.password);
    }
    if let Some(k) = encode_sshkeys(&spec.ssh_keys) {
        p.push("sshkeys", k);
    }
    if let Some(ns) = nameservers(spec, &env.settings, ips) {
        p.push("nameserver", ns);
    }
    let sd = if spec.searchdomain.trim().is_empty() { env.settings.default_search_domain.trim() } else { spec.searchdomain.trim() };
    if !sd.is_empty() {
        p.push("searchdomain", sd);
    }
    apply_catalog_overrides(&mut p, ctx, entry);
    ctx.info(format!("creating VM {vmid} with its disk imported from {image}"));
    let r = env.client.post(&format!("/nodes/{node}/qemu"), &p).await?;
    run_task(&env.client, ctx, "create VM (import disk)", r).await?;
    let g = GuestRef { node: node.to_string(), kind: GuestKind::Qemu, vmid };
    guest::grow_disk(&env.client, ctx, &g, &disk_key, spec.disk_gb).await
}

#[allow(clippy::too_many_arguments)]
async fn create_qemu_clone(
    env: &Env,
    ctx: &JobCtx,
    spec: &CreateSpec,
    vmid: u64,
    node: &str,
    storage: &str,
    tnode: &str,
    tid: u64,
    full: bool,
    ips: &ResolvedIps,
    mac: &str,
) -> anyhow::Result<()> {
    let client = &env.client;
    let template = GuestRef { node: tnode.to_string(), kind: GuestKind::Qemu, vmid: tid };
    let tcfg = guest::config(client, &template).await.context("reading template config")?;
    if !tcfg.get("template").map(|v| v.b_any()).unwrap_or(false) {
        ctx.warn(format!("source {tid} is not marked as a template; cloning it anyway"));
    }
    // Clone on the template's node; Proxmox only allows `target` when the
    // template's disks are on shared storage, so migrate afterwards otherwise.
    let shared = template_disks_shared(client, tnode, &tcfg).await.unwrap_or(false);
    let direct = tnode == node || shared;
    let p = Params::new()
        .set("newid", vmid)
        .set("name", &spec.hostname)
        .opt("pool", spec.pool.as_deref())
        .opt("target", (direct && tnode != node).then_some(node))
        .opt("storage", full.then_some(storage))
        .set("full", if full { 1 } else { 0 });
    ctx.info(format!("cloning template {tid} ({}) on {tnode}", if full { "full" } else { "linked" }));
    let r = client.post(&format!("{}/clone", template.path()), &p).await?;
    run_task(client, ctx, "clone template", r).await?;
    ctx.progress(0.5);

    let mut g = GuestRef { node: tnode.to_string(), kind: GuestKind::Qemu, vmid };
    if direct && tnode != node {
        g.node = node.to_string();
    }
    if g.node != node {
        ctx.info(format!("template disks are local to {tnode}; migrating {vmid} to {node}"));
        guest::migrate(client, ctx, &g, node, false, Some(storage)).await?;
        g.node = node.to_string();
    }

    configure_cloned(env, ctx, &g, spec, ips, mac, &tcfg, storage).await?;
    if let Some(boot) = guest::boot_disk(GuestKind::Qemu, &guest::config(client, &g).await?) {
        guest::grow_disk(client, ctx, &g, &boot, spec.disk_gb).await?;
    }
    Ok(())
}

/// Applies resources, NIC, and cloud-init settings to a freshly cloned VM.
#[allow(clippy::too_many_arguments)]
pub async fn configure_cloned(
    env: &Env,
    ctx: &JobCtx,
    g: &GuestRef,
    spec: &CreateSpec,
    ips: &ResolvedIps,
    mac: &str,
    tcfg: &serde_json::Map<String, serde_json::Value>,
    storage: &str,
) -> anyhow::Result<()> {
    let client = &env.client;
    // Keep the template's NIC model unless one was chosen explicitly.
    let mut net = spec.net.clone();
    if let Some(model) =
        tcfg.get("net0").and_then(|v| v.as_str()).and_then(|s| PropString::parse(s).qemu_nic_model().map(|(m, _)| m.to_string()))
        && spec.net.model.is_empty()
    {
        net.model = model;
    }
    let mut p = Params::new()
        .set("cores", spec.cores)
        .set("sockets", spec.sockets.max(1))
        .set("memory", spec.memory_mb)
        .set("net0", qemu_net_line(&net, mac))
        .flag("onboot", spec.onboot)
        .opt("tags", Some(tags(spec, &env.settings)))
        .opt("description", Some(description(spec, &env.settings)));
    if spec.agent {
        p.push("agent", "1");
    }
    if let Some(l) = spec.cpulimit.filter(|l| *l > 0.0) {
        p.push("cpulimit", format!("{l}"));
    }
    let has_ci = tcfg.values().any(|v| v.as_str().map(|s| s.contains("cloudinit")).unwrap_or(false));
    let wants_ci =
        !spec.ciuser.is_empty() || !spec.password.is_empty() || !spec.ssh_keys.trim().is_empty() || ips.ip4.is_some() || ips.ip6.is_some();
    if wants_ci {
        if !has_ci {
            let cfg = guest::config(client, g).await?;
            let slot =
                if !cfg.contains_key("ide2") { "ide2".to_string() } else { guest::free_disk_slot(&cfg, "ide").unwrap_or("sata5".into()) };
            ctx.info(format!("template has no cloud-init drive, adding one at {slot}"));
            p.push(&slot, format!("{storage}:cloudinit"));
        }
        let ipc = ipconfig_line(ips);
        if !ipc.is_empty() {
            p.push("ipconfig0", ipc);
        }
        if !spec.ciuser.is_empty() {
            p.push("ciuser", &spec.ciuser);
        }
        if !spec.password.is_empty() {
            p.push("cipassword", &spec.password);
        }
        if let Some(k) = encode_sshkeys(&spec.ssh_keys) {
            p.push("sshkeys", k);
        }
        if let Some(ns) = nameservers(spec, &env.settings, ips) {
            p.push("nameserver", ns);
        }
        let sd = if spec.searchdomain.trim().is_empty() { env.settings.default_search_domain.trim() } else { spec.searchdomain.trim() };
        if !sd.is_empty() {
            p.push("searchdomain", sd);
        }
    }
    guest::set_config(client, ctx, g, p).await
}

async fn template_disks_shared(client: &PveClient, node: &str, cfg: &serde_json::Map<String, serde_json::Value>) -> anyhow::Result<bool> {
    let storages = client.get_list(&format!("/nodes/{node}/storage")).await?;
    let shared: std::collections::HashSet<String> = storages.iter().filter(|s| s.b("shared")).map(|s| s.s("storage")).collect();
    let disks = guest::disk_keys(GuestKind::Qemu, cfg);
    if disks.is_empty() {
        return Ok(false);
    }
    Ok(disks
        .iter()
        .all(|k| cfg.get(k).and_then(|v| v.as_str()).and_then(|s| s.split(':').next()).map(|st| shared.contains(st)).unwrap_or(false)))
}

trait AnyBool {
    fn b_any(&self) -> bool;
}

impl AnyBool for serde_json::Value {
    fn b_any(&self) -> bool {
        match self {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
            serde_json::Value::String(s) => s == "1",
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_lines() {
        let net = NetSpec { vlan: Some(20), rate_mbps: Some(100.0), ..Default::default() };
        let ips =
            ResolvedIps { ip4: Some("192.0.2.5/24".into()), gw4: Some("192.0.2.1".into()), ip6: Some("auto".into()), ..Default::default() };
        assert_eq!(
            lxc_net_line(&net, &ips, "BC:24:11:00:00:01", "eth0"),
            "name=eth0,bridge=vmbr0,hwaddr=BC:24:11:00:00:01,type=veth,ip=192.0.2.5/24,gw=192.0.2.1,ip6=auto,firewall=1,tag=20,rate=12.500"
        );
        assert_eq!(qemu_net_line(&net, "BC:24:11:00:00:01"), "virtio=BC:24:11:00:00:01,bridge=vmbr0,firewall=1,tag=20,rate=12.500");
        assert_eq!(ipconfig_line(&ips), "ip=192.0.2.5/24,gw=192.0.2.1,ip6=auto");
    }

    #[test]
    fn managed_tag_keeps_its_case() {
        let mut spec = CreateSpec::default();
        let settings = Settings::default();
        spec.tags = "Web, prod".into();
        assert_eq!(tags(&spec, &settings), "web;prod;PMXSpan");
        spec.tags = "pmxspan".into();
        assert_eq!(tags(&spec, &settings), "pmxspan", "no duplicate in another case");
    }

    #[test]
    fn sshkeys_encoding() {
        let k = encode_sshkeys("ssh-ed25519 AAAA user@host\n\n# comment\nssh-rsa BBBB x").unwrap();
        assert_eq!(k, "ssh-ed25519%20AAAA%20user%40host%0Assh-rsa%20BBBB%20x");
        assert!(encode_sshkeys("  \n").is_none());
    }

    #[test]
    fn validation() {
        let mut s = CreateSpec { hostname: "web".into(), ..Default::default() };
        assert!(validate(&s).is_ok());
        s.source = Source::LxcTemplate { volid: "local:vztmpl/debian.tar.zst".into() };
        assert!(validate(&s).is_err(), "LXC needs password or key");
        s.password = "secret123".into();
        assert!(validate(&s).is_ok());
        s.hostname = "!!!".into();
        assert!(validate(&s).is_err());
    }
}
