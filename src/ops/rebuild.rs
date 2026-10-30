//! Reinstall ("rebuild") a guest in place.
//!
//! The guest keeps its VMID, name, resources, NICs (including MAC addresses,
//! so DHCP reservations and IPAM bindings stay valid), cloud-init network
//! config and pool membership. Its disks are replaced by a fresh copy of the
//! chosen template or OS catalog image. This is destructive and the UI makes
//! the operator type the VMID to confirm.
//!
//! Catalog rebuilds download the image *before* anything is deleted, so an
//! unreachable mirror or a full storage leaves the guest untouched; the
//! download is always cleaned up.

use super::catalog::{self, Artifact, CatalogEntry};
use super::create::{Env, apply_catalog_overrides, encode_sshkeys};
use super::guest::{self, PowerAction, run_task};
use crate::api::Params;
use crate::api::types::{GuestKind, GuestRef, is_lxc_disk_key, is_net_key, is_qemu_disk_key};
use crate::jobs::JobCtx;
use crate::util::{PropString, ValueExt};
use anyhow::{Context, bail};
use serde_json::Value;

#[derive(Clone, Debug)]
pub enum RebuildSource {
    QemuTemplate {
        node: String,
        vmid: u64,
    },
    LxcTemplate {
        volid: String,
    },
    /// An OS catalog image (VM cloud image or container template).
    Catalog {
        entry: Box<CatalogEntry>,
    },
}

#[derive(Clone, Debug)]
pub struct RebuildSpec {
    pub guest: GuestRef,
    pub source: RebuildSource,
    pub password: String,
    pub ssh_keys: String,
    /// Start after rebuild (default: if it was running before).
    pub start: Option<bool>,
    /// Back the guest up to this storage before anything is changed.
    pub backup_storage: Option<String>,
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
    "cicustom",
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

    // Destroying a guest also deletes its firewall config (rules, options,
    // aliases, ipsets), so it is carried over explicitly. Without a readable
    // snapshot the rebuilt guest would come up unprotected: stop here.
    let firewall = FirewallSnapshot::take(client, &g)
        .await
        .context("could not read the guest's firewall config, so it could not be carried over - the guest was NOT modified")?;

    if let Some(storage) = &spec.backup_storage {
        ctx.info(format!("backing up {} to {storage} before the rebuild", g.vmid));
        guest::backup_now(client, &ctx, &g, storage, "snapshot", "zstd", "PMXSpan: before rebuild of {{guestname}}", false)
            .await
            .context("the backup before the rebuild failed - the guest was NOT modified")?;
    }

    // Catalog: download first. Nothing has been touched yet if this fails.
    let artifact = match &spec.source {
        RebuildSource::Catalog { entry } => {
            if entry.is_lxc() != (g.kind == GuestKind::Lxc) {
                bail!("catalog entry `{}` is for {}, not this {}", entry.label, entry.virt, g.kind.label());
            }
            if !entry.nodes.is_empty() && !entry.nodes.contains(&g.node) {
                bail!(
                    "catalog entry `{}` is restricted to nodes {}; the guest is on {} (nothing was changed)",
                    entry.label,
                    entry.nodes.join(", "),
                    g.node
                );
            }
            let size = catalog::estimate_size(entry, &env.settings).await;
            let cands = catalog::node_candidates(client, &g.node, entry, &env.profile, size).await?;
            if cands.is_empty() {
                bail!("{} on {} - the guest was NOT modified", catalog::describe_missing(&env.profile, entry), g.node);
            }
            let a = catalog::download(client, &ctx, &g.node, entry, g.vmid, &cands, &env.settings)
                .await
                .context("downloading the catalog image failed - the guest was NOT modified")?;
            Some(a)
        }
        _ => None,
    };
    let result =
        destroy_and_recreate(&env, &ctx, &spec, &g, &cfg, &st, &boot, &boot_storage, boot_size, pool.as_deref(), artifact.as_ref()).await;
    if let Some(a) = &artifact {
        catalog::cleanup(client, &ctx, a).await;
    }
    let (g, was_running) = result?;
    let firewall_ok = firewall.restore(client, &ctx, &g).await;
    reapply_ipam(&env, &ctx, &g).await;
    if !firewall_ok {
        bail!(
            "{} {} was rebuilt but its firewall config could not be fully restored (see the warnings above); \
             it was left stopped - fix its firewall, then start it",
            g.kind.label(),
            g.vmid
        );
    }

    if spec.start.unwrap_or(was_running) {
        ctx.progress(0.9);
        guest::power(client, &ctx, &g, PowerAction::Start).await?;
    }
    Ok(format!("{} {} rebuilt on {}", g.kind.label(), g.vmid, g.node))
}

/// A guest's firewall config, as read from the API.
#[derive(serde::Serialize)]
pub(crate) struct FirewallSnapshot {
    options: serde_json::Map<String, Value>,
    rules: Vec<Value>,
    aliases: Vec<Value>,
    ipsets: Vec<(Value, Vec<Value>)>,
}

impl FirewallSnapshot {
    pub(crate) async fn take(client: &crate::api::PveClient, g: &GuestRef) -> anyhow::Result<Self> {
        let base = format!("{}/firewall", g.path());
        let options = client.get(&format!("{base}/options")).await?.as_object().cloned().unwrap_or_default();
        let mut rules = client.get_list(&format!("{base}/rules")).await?;
        rules.sort_by_key(|r| r.u("pos"));
        let aliases = client.get_list(&format!("{base}/aliases")).await?;
        let mut ipsets = Vec::new();
        for set in client.get_list(&format!("{base}/ipset")).await? {
            let entries = client.get_list(&format!("{base}/ipset/{}", set.s("name"))).await?;
            ipsets.push((set, entries));
        }
        Ok(Self { options, rules, aliases, ipsets })
    }

    fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.aliases.is_empty() && self.ipsets.is_empty() && self.options.keys().all(|k| k == "digest")
    }

    /// Writes everything back. Returns `false` when anything failed (each
    /// failure is logged); the caller then must not start the guest.
    async fn restore(&self, client: &crate::api::PveClient, ctx: &JobCtx, g: &GuestRef) -> bool {
        if self.is_empty() {
            return true;
        }
        let base = format!("{}/firewall", g.path());
        let params = |v: &Value, skip: &[&str]| {
            let mut p = Params::new();
            for (k, v) in v.as_object().into_iter().flatten() {
                if !skip.contains(&k.as_str()) && !v.is_null() {
                    p.push(k, value_str(v));
                }
            }
            p
        };
        let mut failed = Vec::new();
        for a in &self.aliases {
            if let Err(e) = client.post(&format!("{base}/aliases"), &params(a, &["digest", "ipversion"])).await {
                failed.push(format!("alias {}: {e:#}", a.s("name")));
            }
        }
        for (set, entries) in &self.ipsets {
            let name = set.s("name");
            if let Err(e) = client.post(&format!("{base}/ipset"), &params(set, &["digest"])).await {
                failed.push(format!("ipset {name}: {e:#}"));
                continue;
            }
            for e in entries {
                if let Err(err) = client.post(&format!("{base}/ipset/{name}"), &params(e, &["digest"])).await {
                    failed.push(format!("{name} {}: {err:#}", e.s("cidr")));
                }
            }
        }
        // New rules are inserted at the top, so add them bottom-up.
        for r in self.rules.iter().rev() {
            if let Err(e) = client.post(&format!("{base}/rules"), &params(r, &["pos", "digest", "ipversion"])).await {
                failed.push(format!("rule {}: {e:#}", r.u("pos")));
            }
        }
        let opts = params(&Value::Object(self.options.clone()), &["digest"]);
        if !opts.is_empty()
            && let Err(e) = client.put(&format!("{base}/options"), &opts).await
        {
            failed.push(format!("options: {e:#}"));
        }
        if failed.is_empty() {
            ctx.info(format!(
                "firewall restored: {} rule(s), {} alias(es), {} ipset(s)",
                self.rules.len(),
                self.aliases.len(),
                self.ipsets.len()
            ));
        } else {
            ctx.warn(format!("firewall only partly restored: {}", failed.join("; ")));
        }
        failed.is_empty()
    }
}

/// Re-applies the guest's IPAM addresses after a rebuild (they survive it).
async fn reapply_ipam(env: &Env, ctx: &JobCtx, g: &GuestRef) {
    if env.ipam.lock().guest_ips(&env.server_id, g.vmid).is_empty() {
        return;
    }
    let opts = super::ipassign::ApplyOpts::default();
    if let Err(e) = super::ipassign::apply(env, ctx, g, opts, &Default::default()).await {
        ctx.warn(format!("could not re-apply IPAM addresses: {e:#} (use the IP addresses tab)"));
    }
}

#[allow(clippy::too_many_arguments)]
async fn destroy_and_recreate(
    env: &Env,
    ctx: &JobCtx,
    spec: &RebuildSpec,
    g: &GuestRef,
    cfg: &serde_json::Map<String, Value>,
    st: &Value,
    boot: &str,
    boot_storage: &str,
    boot_size: u64,
    pool: Option<&str>,
    artifact: Option<&Artifact>,
) -> anyhow::Result<(GuestRef, bool)> {
    let client = &env.client;
    let was_running = st.s("status") == "running";
    if was_running || st.s("status") == "paused" {
        guest::power(client, ctx, g, PowerAction::Stop).await?;
    }
    ctx.progress(0.15);
    let r = client.delete(&g.path(), &Params::new().set("purge", 0).set("destroy-unreferenced-disks", 1)).await?;
    run_task(client, ctx, &format!("remove old {} {}", g.kind.label(), g.vmid), r).await?;
    ctx.progress(0.35);

    let rebuilt: anyhow::Result<GuestRef> = match (&spec.source, g.kind, artifact) {
        (RebuildSource::QemuTemplate { node: tnode, vmid: tid }, GuestKind::Qemu, _) => {
            rebuild_qemu(env, ctx, g, cfg, tnode, *tid, boot_storage, boot_size, pool, spec).await
        }
        (RebuildSource::LxcTemplate { volid }, GuestKind::Lxc, _) => {
            rebuild_lxc(env, ctx, g, cfg, volid, boot_storage, boot_size, pool, spec, None).await
        }
        (RebuildSource::Catalog { entry }, GuestKind::Lxc, Some(a)) => {
            rebuild_lxc(env, ctx, g, cfg, &a.volid, boot_storage, boot_size, pool, spec, Some(entry)).await
        }
        (RebuildSource::Catalog { entry }, GuestKind::Qemu, Some(a)) => {
            rebuild_qemu_catalog(env, ctx, g, cfg, boot, boot_storage, boot_size, pool, spec, entry, &a.volid).await
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
    Ok((g, was_running))
}

/// Options of a disk line without its volume and size (`iothread=1,discard=on,...`).
fn disk_options(line: &str) -> PropString {
    let mut p = PropString::parse(line);
    p.remove("");
    p.remove("size");
    p.remove("import-from");
    p
}

fn storage_of(line: &str) -> Option<String> {
    PropString::parse(line).positional().and_then(|v| v.split(':').next()).map(str::to_string).filter(|s| !s.is_empty() && s != "none")
}

fn with_volume(volume: String, opts: PropString) -> String {
    let mut p = PropString { parts: vec![(String::new(), volume)] };
    p.parts.extend(opts.parts);
    p.to_string()
}

/// QEMU reinstall from a catalog image:
/// the boot disk is re-created with `import-from`, other disks are re-created
/// empty with the same size, NICs/cloud-init network config are kept.
#[allow(clippy::too_many_arguments)]
async fn rebuild_qemu_catalog(
    env: &Env,
    ctx: &JobCtx,
    g: &GuestRef,
    old: &serde_json::Map<String, Value>,
    boot: &str,
    boot_storage: &str,
    boot_size: u64,
    pool: Option<&str>,
    spec: &RebuildSpec,
    entry: &CatalogEntry,
    image: &str,
) -> anyhow::Result<GuestRef> {
    let client = &env.client;
    let mut p = Params::new().set("vmid", g.vmid).opt("pool", pool);
    for k in QEMU_KEEP.iter().chain(["scsihw", "bios", "machine", "boot", "vga", "serial0", "hookscript"].iter()) {
        if let Some(v) = old.get(*k) {
            p.push(k, value_str(v));
        }
    }
    let mut ci_slot = None;
    for (k, v) in old {
        let line = value_str(v);
        if is_net_key(k) || k.starts_with("ipconfig") {
            p.push(k, line);
        } else if is_qemu_disk_key(k) {
            if line.contains("cloudinit") {
                ci_slot = Some(k.clone());
            } else if line.contains("media=cdrom") {
                p.push(k, "none,media=cdrom");
            } else if k == boot {
                p.push(
                    k,
                    with_volume(format!("{boot_storage}:0"), disk_options(&line)).replacen(
                        &format!("{boot_storage}:0"),
                        &format!("{boot_storage}:0,import-from={image}"),
                        1,
                    ),
                );
            } else {
                let gb =
                    PropString::parse(&line).get("size").and_then(crate::util::parse_size).unwrap_or(0).div_ceil(1024 * 1024 * 1024).max(1);
                let st = storage_of(&line).unwrap_or_else(|| boot_storage.to_string());
                ctx.info(format!("{k} is re-created empty ({gb} GiB on {st})"));
                p.push(k, with_volume(format!("{st}:{gb}"), disk_options(&line)));
            }
        } else if k == "efidisk0" || k == "tpmstate0" {
            let st = storage_of(&line).unwrap_or_else(|| boot_storage.to_string());
            p.push(k, with_volume(format!("{st}:1"), disk_options(&line)));
        }
    }
    let ci_slot = ci_slot.unwrap_or_else(|| {
        let taken: serde_json::Map<String, Value> = p.0.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        if !taken.contains_key("ide2") { "ide2".into() } else { guest::free_disk_slot(&taken, "ide").unwrap_or("sata5".into()) }
    });
    p.push(&ci_slot, format!("{boot_storage}:cloudinit"));
    if p.get("serial0").is_none() {
        p.push("serial0", "socket");
        p.push("vga", "serial0");
    }
    if p.get("boot").is_none() {
        p.push("boot", format!("order={boot};net0"));
    }
    if !entry.cloud_user.is_empty() {
        p.push("ciuser", &entry.cloud_user);
    }
    if !spec.password.is_empty() {
        p.push("cipassword", &spec.password);
    }
    if let Some(k) = encode_sshkeys(&spec.ssh_keys) {
        p.push("sshkeys", k);
    }
    apply_catalog_overrides(&mut p, ctx, entry);
    ctx.info(format!("recreating VM {} with {boot} imported from {image}", g.vmid));
    let r = client.post(&format!("/nodes/{}/qemu", g.node), &p).await?;
    run_task(client, ctx, "recreate VM (import disk)", r).await?;
    let gb = boot_size.div_ceil(1024 * 1024 * 1024);
    if gb > 0 {
        guest::grow_disk(client, ctx, g, boot, gb).await?;
    }
    Ok(g.clone())
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
    entry: Option<&CatalogEntry>,
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
        let line = value_str(v);
        if is_net_key(k) {
            p.push(k, line);
        } else if is_lxc_disk_key(k) && k != "rootfs" {
            // Mount points are re-created empty with the same size, path and options.
            let gb = PropString::parse(&line).get("size").and_then(crate::util::parse_size).unwrap_or(0).div_ceil(1024 * 1024 * 1024);
            match storage_of(&line) {
                Some(st) if gb > 0 => {
                    ctx.info(format!("{k} is re-created empty ({gb} GiB on {st})"));
                    p.push(k, with_volume(format!("{st}:{gb}"), disk_options(&line)));
                }
                _ => ctx.warn(format!("{k} ({line}) is a bind mount or has no size; it is not re-created")),
            }
        }
    }
    let keys: Vec<&str> = spec.ssh_keys.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    if !keys.is_empty() {
        p.push("ssh-public-keys", keys.join("\n"));
    }
    if let Some(e) = entry {
        apply_catalog_overrides(&mut p, ctx, e);
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
