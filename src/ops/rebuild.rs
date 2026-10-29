//! Reinstall ("rebuild") a guest in place, ported from the module's admin
//! reinstall flow.
//!
//! The guest keeps its VMID, name, resources, NICs (including MAC addresses,
//! so DHCP reservations and IPAM bindings stay valid), cloud-init network
//! config and pool membership. Its disks are replaced by a fresh copy of the
//! chosen template. This is destructive and the UI makes the operator type
//! the VMID to confirm.

use super::create::{Env, encode_sshkeys};
use super::guest::{self, PowerAction, run_task};
use crate::api::Params;
use crate::api::types::{GuestKind, GuestRef, is_net_key};
use crate::jobs::JobCtx;
use crate::util::{PropString, ValueExt};
use anyhow::{Context, bail};
use serde_json::Value;

#[derive(Clone, Debug)]
pub enum RebuildSource {
    QemuTemplate { node: String, vmid: u64 },
    LxcTemplate { volid: String },
}

#[derive(Clone, Debug)]
pub struct RebuildSpec {
    pub guest: GuestRef,
    pub source: RebuildSource,
    pub password: String,
    pub ssh_keys: String,
    /// Start after rebuild (default: if it was running before).
    pub start: Option<bool>,
}

/// QEMU keys carried over from the old VM.
const QEMU_KEEP: &[&str] = &[
    "name",
    "cores",
    "sockets",
    "memory",
    "balloon",
    "cpu",
    "cpulimit",
    "cpuunits",
    "numa",
    "onboot",
    "tags",
    "description",
    "agent",
    "nameserver",
    "searchdomain",
    "ciuser",
    "sshkeys",
    "protection",
    "startup",
];

/// LXC keys carried over from the old container.
const LXC_KEEP: &[&str] = &[
    "hostname",
    "cores",
    "cpulimit",
    "cpuunits",
    "memory",
    "swap",
    "onboot",
    "tags",
    "description",
    "nameserver",
    "searchdomain",
    "features",
    "startup",
    "protection",
];

pub async fn run(env: Env, spec: RebuildSpec, ctx: JobCtx) -> anyhow::Result<String> {
    let client = &env.client;
    let g = spec.guest.clone();
    let cfg = guest::config(client, &g).await.context("reading current config")?;
    let st = guest::status(client, &g).await?;
    let was_running = st.s("status") == "running";
    if cfg.get("protection").map(|v| v.as_str() == Some("1") || v.as_u64() == Some(1)).unwrap_or(false) {
        bail!("guest has protection enabled; disable it before rebuilding");
    }
    let pool = client
        .get_q("/cluster/resources", &Params::new().set("type", "vm"))
        .await?
        .arr()
        .into_iter()
        .find(|r| r.u("vmid") == g.vmid)
        .map(|r| r.s("pool"))
        .filter(|p| !p.is_empty());
    let boot = guest::boot_disk(g.kind, &cfg).ok_or_else(|| anyhow::anyhow!("cannot determine the boot disk"))?;
    let boot_size = guest::disk_size_bytes(&cfg, &boot).unwrap_or(0);
    let boot_storage = cfg
        .get(&boot)
        .and_then(Value::as_str)
        .and_then(|s| PropString::parse(s).positional().and_then(|v| v.split(':').next()).map(str::to_string))
        .ok_or_else(|| anyhow::anyhow!("cannot determine the storage of {boot}"))?;
    let extra_disks: Vec<String> = guest::disk_keys(g.kind, &cfg).into_iter().filter(|d| *d != boot).collect();
    if !extra_disks.is_empty() {
        ctx.warn(format!("additional disks {} will be destroyed with the guest", extra_disks.join(", ")));
    }
    ctx.info(format!(
        "boot disk {boot} on {boot_storage} ({}); pool {}",
        crate::util::fmt_bytes(boot_size as f64),
        pool.as_deref().unwrap_or("-")
    ));

    if was_running || st.s("status") == "paused" {
        guest::power(client, &ctx, &g, PowerAction::Stop).await?;
    }
    ctx.progress(0.15);
    let r = client.delete(&g.path(), &Params::new().set("purge", 0).set("destroy-unreferenced-disks", 1)).await?;
    run_task(client, &ctx, &format!("remove old {} {}", g.kind.label(), g.vmid), r).await?;
    ctx.progress(0.35);

    let rebuilt: anyhow::Result<GuestRef> = match (&spec.source, g.kind) {
        (RebuildSource::QemuTemplate { node: tnode, vmid: tid }, GuestKind::Qemu) => {
            rebuild_qemu(&env, &ctx, &g, &cfg, tnode, *tid, &boot_storage, boot_size, pool.as_deref(), &spec).await
        }
        (RebuildSource::LxcTemplate { volid }, GuestKind::Lxc) => {
            rebuild_lxc(&env, &ctx, &g, &cfg, volid, &boot_storage, boot_size, pool.as_deref(), &spec).await
        }
        _ => bail!("rebuild source does not match the guest type"),
    };
    let g = rebuilt.with_context(|| {
        format!(
            "the old {} {} was removed but recreating it failed; the VMID is free - recreate it from the Create wizard",
            g.kind.label(),
            g.vmid
        )
    })?;

    if spec.start.unwrap_or(was_running) {
        ctx.progress(0.9);
        guest::power(client, &ctx, &g, PowerAction::Start).await?;
    }
    Ok(format!("{} {} rebuilt on {}", g.kind.label(), g.vmid, g.node))
}

#[allow(clippy::too_many_arguments)]
async fn rebuild_qemu(
    env: &Env,
    ctx: &JobCtx,
    g: &GuestRef,
    old: &serde_json::Map<String, Value>,
    tnode: &str,
    tid: u64,
    storage: &str,
    boot_size: u64,
    pool: Option<&str>,
    spec: &RebuildSpec,
) -> anyhow::Result<GuestRef> {
    let client = &env.client;
    let template = GuestRef { node: tnode.to_string(), kind: GuestKind::Qemu, vmid: tid };
    let p = Params::new()
        .set("newid", g.vmid)
        .set("full", 1)
        .set("storage", storage)
        .opt("pool", pool)
        .opt("name", old.get("name").and_then(Value::as_str));
    let r = client.post(&format!("{}/clone", template.path()), &p).await?;
    run_task(client, ctx, &format!("clone template {tid}"), r).await?;
    let mut ng = GuestRef { node: tnode.to_string(), kind: GuestKind::Qemu, vmid: g.vmid };
    if ng.node != g.node {
        guest::migrate(client, ctx, &ng, &g.node, false, Some(storage)).await?;
        ng.node = g.node.clone();
    }
    ctx.progress(0.6);

    let newcfg = guest::config(client, &ng).await?;
    let mut p = Params::new();
    for k in QEMU_KEEP {
        if let Some(v) = old.get(*k) {
            p.push(k, value_str(v));
        }
    }
    for (k, v) in old {
        if is_net_key(k) || k.starts_with("ipconfig") {
            p.push(k, value_str(v));
        }
    }
    // Drop NICs the template has but the old VM did not.
    let stale: Vec<String> = newcfg.keys().filter(|k| is_net_key(k) && !old.contains_key(*k)).cloned().collect();
    if !stale.is_empty() {
        p.push("delete", stale.join(","));
    }
    let has_ci = newcfg.values().any(|v| v.as_str().map(|s| s.contains("cloudinit")).unwrap_or(false));
    let wants_ci = !spec.password.is_empty() || !spec.ssh_keys.trim().is_empty() || old.keys().any(|k| k.starts_with("ipconfig"));
    if wants_ci && !has_ci {
        let slot =
            if !newcfg.contains_key("ide2") { "ide2".into() } else { guest::free_disk_slot(&newcfg, "ide").unwrap_or("sata5".into()) };
        ctx.info(format!("template has no cloud-init drive, adding one at {slot}"));
        p.push(&slot, format!("{storage}:cloudinit"));
    }
    if !spec.password.is_empty() {
        p.push("cipassword", &spec.password);
    }
    if let Some(k) = encode_sshkeys(&spec.ssh_keys) {
        p.push("sshkeys", k);
    }
    guest::set_config(client, ctx, &ng, p).await?;
    if let Some(boot) = guest::boot_disk(GuestKind::Qemu, &guest::config(client, &ng).await?) {
        let gb = boot_size.div_ceil(1024 * 1024 * 1024);
        if gb > 0 {
            guest::grow_disk(client, ctx, &ng, &boot, gb).await?;
        }
    }
    Ok(ng)
}

#[allow(clippy::too_many_arguments)]
async fn rebuild_lxc(
    env: &Env,
    ctx: &JobCtx,
    g: &GuestRef,
    old: &serde_json::Map<String, Value>,
    volid: &str,
    storage: &str,
    boot_size: u64,
    pool: Option<&str>,
    spec: &RebuildSpec,
) -> anyhow::Result<GuestRef> {
    if spec.password.is_empty() && spec.ssh_keys.trim().is_empty() {
        bail!("a root password or SSH key is required to rebuild a container");
    }
    let gb = boot_size.div_ceil(1024 * 1024 * 1024).max(1);
    let unprivileged = old.get("unprivileged").map(|v| value_str(v) == "1").unwrap_or(true);
    let mut p = Params::new()
        .set("vmid", g.vmid)
        .set("ostemplate", volid)
        .set("rootfs", format!("{storage}:{gb}"))
        .flag("unprivileged", unprivileged)
        .opt("pool", pool)
        .opt("password", Some(&spec.password));
    for k in LXC_KEEP {
        if let Some(v) = old.get(*k) {
            p.push(k, value_str(v));
        }
    }
    for (k, v) in old {
        if is_net_key(k) {
            p.push(k, value_str(v));
        }
    }
    let keys: Vec<&str> = spec.ssh_keys.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    if !keys.is_empty() {
        p.push("ssh-public-keys", keys.join("\n"));
    }
    let r = env.client.post(&format!("/nodes/{}/lxc", g.node), &p).await?;
    run_task(&env.client, ctx, &format!("recreate container from {volid}"), r).await?;
    Ok(g.clone())
}

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
