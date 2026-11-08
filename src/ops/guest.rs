//! Single-guest operations: power, config, disks, snapshots, backups,
//! migration, cloning, deletion and console access.

use crate::api::types::{GuestKind, GuestRef, Snapshot, StorageItem, is_lxc_disk_key, is_qemu_disk_key};
use crate::api::{Params, PveClient};
use crate::jobs::JobCtx;
use crate::util::{PropString, ValueExt, parse_size};
use anyhow::{Context, anyhow, bail};
use serde_json::{Map, Value};
use std::time::Duration;

pub const TASK_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Start,
    Shutdown,
    Stop,
    Reboot,
    Reset,
    Suspend,
    Resume,
}

impl PowerAction {
    pub fn label(self) -> &'static str {
        match self {
            PowerAction::Start => "Start",
            PowerAction::Shutdown => "Shutdown",
            PowerAction::Stop => "Stop (hard)",
            PowerAction::Reboot => "Reboot",
            PowerAction::Reset => "Reset",
            PowerAction::Suspend => "Suspend",
            PowerAction::Resume => "Resume",
        }
    }
    fn endpoint(self) -> &'static str {
        match self {
            PowerAction::Start => "start",
            PowerAction::Shutdown => "shutdown",
            PowerAction::Stop => "stop",
            PowerAction::Reboot => "reboot",
            PowerAction::Reset => "reset",
            PowerAction::Suspend => "suspend",
            PowerAction::Resume => "resume",
        }
    }
    pub fn supported(self, kind: GuestKind) -> bool {
        !(kind == GuestKind::Lxc && self == PowerAction::Reset)
    }
}

/// Runs a task-returning call and waits for it, logging to the job.
pub async fn run_task(client: &PveClient, ctx: &JobCtx, what: &str, upid: Value) -> anyhow::Result<()> {
    let Some(upid) = upid.as_str().map(str::to_string) else {
        // Some endpoints complete synchronously and return null.
        ctx.info(format!("{what}: done"));
        return Ok(());
    };
    ctx.info(format!("{what}: task {upid}"));
    client.wait_task(&upid, TASK_TIMEOUT, |_| {}).await.with_context(|| what.to_string())?;
    ctx.info(format!("{what}: OK"));
    Ok(())
}

pub async fn power(client: &PveClient, ctx: &JobCtx, g: &GuestRef, action: PowerAction) -> anyhow::Result<String> {
    if !action.supported(g.kind) {
        bail!("{} is not supported for containers", action.label());
    }
    let mut p = Params::new();
    if action == PowerAction::Shutdown {
        // Fall back to a hard stop if the guest ignores ACPI for too long.
        p.push("forceStop", 1);
        p.push("timeout", 180);
    }
    let upid = client.post(&format!("{}/status/{}", g.path(), action.endpoint()), &p).await?;
    run_task(client, ctx, &format!("{} {} {}", action.label(), g.kind.label(), g.vmid), upid).await?;
    Ok(format!("{} {} {}: done", action.label(), g.kind.label(), g.vmid))
}

pub async fn status(client: &PveClient, g: &GuestRef) -> anyhow::Result<Value> {
    client.get(&format!("{}/status/current", g.path())).await
}

pub async fn config(client: &PveClient, g: &GuestRef) -> anyhow::Result<Map<String, Value>> {
    let v = client.get(&format!("{}/config", g.path())).await?;
    Ok(v.as_object().cloned().unwrap_or_default())
}

/// Writes config. QEMU uses POST (async, returns a task for hotplug), LXC PUT.
pub async fn set_config(client: &PveClient, ctx: &JobCtx, g: &GuestRef, params: Params) -> anyhow::Result<()> {
    if params.is_empty() {
        return Ok(());
    }
    let keys: Vec<&str> = params.0.iter().map(|(k, _)| k.as_str()).collect();
    ctx.info(format!("updating config: {}", keys.join(", ")));
    match g.kind {
        GuestKind::Qemu => {
            let r = client.post(&format!("{}/config", g.path()), &params).await?;
            run_task(client, ctx, "apply config", r).await
        }
        GuestKind::Lxc => {
            client.put(&format!("{}/config", g.path()), &params).await?;
            Ok(())
        }
    }
}

pub fn disk_keys(kind: GuestKind, cfg: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<String> = cfg
        .keys()
        .filter(|k| match kind {
            GuestKind::Qemu => is_qemu_disk_key(k),
            GuestKind::Lxc => is_lxc_disk_key(k),
        })
        .filter(|k| {
            let v = cfg.get(*k).and_then(Value::as_str).unwrap_or("");
            !v.contains("media=cdrom") && !v.contains("cloudinit")
        })
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// The disk the guest boots from: first disk in `boot: order=...`, else the first disk.
pub fn boot_disk(kind: GuestKind, cfg: &Map<String, Value>) -> Option<String> {
    if kind == GuestKind::Lxc {
        return cfg.contains_key("rootfs").then(|| "rootfs".to_string());
    }
    let disks = disk_keys(kind, cfg);
    if let Some(boot) = cfg.get("boot").and_then(Value::as_str) {
        let order = PropString::parse(boot);
        if let Some(o) = order.get("order") {
            for dev in o.split(';') {
                if disks.iter().any(|d| d == dev) {
                    return Some(dev.to_string());
                }
            }
        }
    }
    if let Some(b) = cfg.get("bootdisk").and_then(Value::as_str)
        && disks.iter().any(|d| d == b)
    {
        return Some(b.to_string());
    }
    disks.into_iter().next()
}

pub fn disk_size_bytes(cfg: &Map<String, Value>, key: &str) -> Option<u64> {
    let v = cfg.get(key)?.as_str()?;
    PropString::parse(v).get("size").and_then(parse_size)
}

/// Grows a disk to `size_gb` (never shrinks - Proxmox cannot shrink).
pub async fn grow_disk(client: &PveClient, ctx: &JobCtx, g: &GuestRef, disk: &str, size_gb: u64) -> anyhow::Result<()> {
    let cfg = config(client, g).await?;
    let current = disk_size_bytes(&cfg, disk).unwrap_or(0);
    let want = size_gb * 1024 * 1024 * 1024;
    if want <= current {
        ctx.info(format!("{disk} is already {} (requested {size_gb} GiB), not resizing", crate::util::fmt_bytes(current as f64)));
        return Ok(());
    }
    let r = client.put(&format!("{}/resize", g.path()), &Params::new().set("disk", disk).set("size", format!("{size_gb}G"))).await?;
    run_task(client, ctx, &format!("resize {disk} to {size_gb}G"), r).await
}

pub async fn move_disk(
    client: &PveClient,
    ctx: &JobCtx,
    g: &GuestRef,
    disk: &str,
    storage: &str,
    delete_source: bool,
) -> anyhow::Result<String> {
    let (endpoint, key) = match g.kind {
        GuestKind::Qemu => ("move_disk", "disk"),
        GuestKind::Lxc => ("move_volume", "volume"),
    };
    let r = client
        .post(&format!("{}/{endpoint}", g.path()), &Params::new().set(key, disk).set("storage", storage).flag("delete", delete_source))
        .await?;
    run_task(client, ctx, &format!("move {disk} to {storage}"), r).await?;
    Ok(format!("moved {disk} to {storage}"))
}

pub async fn snapshots(client: &PveClient, g: &GuestRef) -> anyhow::Result<Vec<Snapshot>> {
    let mut list: Vec<Snapshot> = client.get_list(&format!("{}/snapshot", g.path())).await?.iter().map(Snapshot::from_value).collect();
    // "current" is the live state pseudo-entry.
    list.retain(|s| s.name != "current");
    list.sort_by_key(|s| s.snaptime);
    Ok(list)
}

pub async fn snapshot_create(
    client: &PveClient,
    ctx: &JobCtx,
    g: &GuestRef,
    name: &str,
    description: &str,
    vmstate: bool,
) -> anyhow::Result<String> {
    validate_snapshot_name(name)?;
    let mut p = Params::new().set("snapname", name).opt("description", Some(description));
    if g.kind == GuestKind::Qemu && vmstate {
        p.push("vmstate", 1);
    }
    let r = client.post(&format!("{}/snapshot", g.path()), &p).await?;
    run_task(client, ctx, &format!("snapshot {name}"), r).await?;
    Ok(format!("snapshot '{name}' created"))
}

pub fn validate_snapshot_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        bail!("snapshot name must start with a letter and contain only letters, digits, '-' and '_' (max 40)");
    }
    Ok(())
}

pub async fn snapshot_rollback(client: &PveClient, ctx: &JobCtx, g: &GuestRef, name: &str, start: bool) -> anyhow::Result<String> {
    let mut p = Params::new();
    if start {
        p.push("start", 1);
    }
    let r = client.post(&format!("{}/snapshot/{name}/rollback", g.path()), &p).await?;
    run_task(client, ctx, &format!("rollback to {name}"), r).await?;
    Ok(format!("rolled back to '{name}'"))
}

pub async fn snapshot_delete(client: &PveClient, ctx: &JobCtx, g: &GuestRef, name: &str) -> anyhow::Result<String> {
    let r = client.delete(&format!("{}/snapshot/{name}", g.path()), &Params::new()).await?;
    run_task(client, ctx, &format!("delete snapshot {name}"), r).await?;
    Ok(format!("snapshot '{name}' deleted"))
}

/// Snippet volumes (`storage:snippets/file`) on storages with `snippets`
/// content across `nodes`. Proxmox's API cannot upload snippets; they are
/// placed on the node (e.g. /var/lib/vz/snippets) by other means.
pub async fn list_snippets(client: &PveClient, nodes: Vec<String>) -> anyhow::Result<Vec<String>> {
    let mut out = std::collections::BTreeSet::new();
    for n in nodes {
        let storages = client.get_q(&format!("/nodes/{n}/storage"), &Params::new().set("content", "snippets").set("enabled", 1)).await?;
        for s in storages.arr() {
            let name = s.s("storage");
            if let Ok(items) = client.get_q(&format!("/nodes/{n}/storage/{name}/content"), &Params::new().set("content", "snippets")).await
            {
                out.extend(items.arr().iter().map(|i| i.s("volid")).filter(|v| !v.is_empty()));
            }
        }
    }
    Ok(out.into_iter().collect())
}

/// The parts of a `cicustom` value: user, vendor, network and meta files.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CiCustom {
    pub user: Option<String>,
    pub vendor: Option<String>,
    pub network: Option<String>,
    pub meta: Option<String>,
}

impl CiCustom {
    pub fn parse(s: &str) -> Self {
        let p = PropString::parse(s);
        let get = |k: &str| p.get(k).map(str::to_string).filter(|v| !v.is_empty());
        Self { user: get("user"), vendor: get("vendor"), network: get("network"), meta: get("meta") }
    }

    /// The config value (empty = no custom files).
    pub fn value(&self) -> String {
        [("user", &self.user), ("vendor", &self.vendor), ("network", &self.network), ("meta", &self.meta)]
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={v}")))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Which guest type a backup volume holds, from its format or file name.
pub fn backup_kind(b: &StorageItem) -> Option<GuestKind> {
    if b.format == "pbs-vm" || b.volid.contains("vzdump-qemu-") {
        Some(GuestKind::Qemu)
    } else if b.format == "pbs-ct" || b.volid.contains("vzdump-lxc-") || b.volid.contains("vzdump-openvz-") {
        Some(GuestKind::Lxc)
    } else {
        None
    }
}

/// A Proxmox Backup Server snapshot, whose files can be browsed.
pub fn is_pbs_backup(b: &StorageItem) -> bool {
    b.format.starts_with("pbs-")
}

/// One entry of a PBS snapshot during single-file restore.
#[derive(Clone, Debug)]
pub struct RestoreEntry {
    /// Base64 path, as the API wants it back.
    pub filepath: String,
    pub text: String,
    /// `d` directory, `f` file, `v` virtual (archive / disk / partition), `l` symlink, `h` hardlink.
    pub kind: String,
    pub leaf: bool,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}

impl RestoreEntry {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind.as_str(), "d" | "v") || !self.leaf
    }
}

/// Lists a directory of a PBS backup (`filepath` = "/" for the top).
/// The first call can take a minute while Proxmox starts its restore helper.
pub async fn file_restore_list(
    client: &PveClient,
    node: &str,
    storage: &str,
    volume: &str,
    filepath: &str,
) -> anyhow::Result<Vec<RestoreEntry>> {
    let p = Params::new().set("volume", volume).set("filepath", filepath);
    let v = client.get_q(&format!("/nodes/{node}/storage/{storage}/file-restore/list"), &p).await?;
    let mut out: Vec<RestoreEntry> = v
        .arr()
        .iter()
        .map(|e| RestoreEntry {
            filepath: e.s("filepath"),
            text: e.s("text"),
            kind: e.s("type"),
            leaf: e.b("leaf"),
            size: e.get("size").and_then(Value::as_u64),
            mtime: e.get("mtime").and_then(Value::as_i64),
        })
        .collect();
    out.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.text.to_lowercase().cmp(&b.text.to_lowercase())));
    Ok(out)
}

/// Downloads one file, or a directory as a zip (or tar.zst), from a PBS backup.
pub async fn file_restore_download(
    client: &PveClient,
    node: &str,
    storage: &str,
    volume: &str,
    filepath: &str,
    tar: bool,
    dest: &std::path::Path,
) -> anyhow::Result<String> {
    let p = Params::new().set("volume", volume).set("filepath", filepath).flag("tar", tar);
    let n = client.download(&format!("/nodes/{node}/storage/{storage}/file-restore/download"), &p, dest).await?;
    Ok(format!("saved {} to {}", crate::util::fmt_bytes(n as f64), dest.display()))
}

/// Backups of this guest across every backup-capable storage visible on its
/// node; with `all`, backups of every guest of the same type (to restore
/// another guest's backup over this one).
pub async fn backups_on_node(client: &PveClient, g: &GuestRef, all: bool) -> anyhow::Result<Vec<(String, StorageItem)>> {
    let storages = client.get_q(&format!("/nodes/{}/storage", g.node), &Params::new().set("content", "backup")).await?;
    let mut out = Vec::new();
    for s in storages.arr() {
        let name = s.s("storage");
        if s.get("active").is_some() && !s.b("active") {
            continue;
        }
        let mut p = Params::new().set("content", "backup");
        if !all {
            p.push("vmid", g.vmid);
        }
        let items = client.get_q(&format!("/nodes/{}/storage/{name}/content", g.node), &p).await;
        match items {
            Ok(items) => out.extend(
                items
                    .arr()
                    .iter()
                    .map(|i| (name.clone(), StorageItem::from_value(i)))
                    .filter(|(_, b)| !all || backup_kind(b) == Some(g.kind)),
            ),
            Err(e) => tracing::warn!("listing backups on {name}: {e:#}"),
        }
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.1.ctime));
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub async fn backup_now(
    client: &PveClient,
    ctx: &JobCtx,
    g: &GuestRef,
    storage: &str,
    mode: &str,
    compress: &str,
    notes: &str,
    protected: bool,
) -> anyhow::Result<String> {
    let p = Params::new()
        .set("vmid", g.vmid)
        .set("storage", storage)
        .set("mode", mode)
        .set("compress", compress)
        .opt("notes-template", Some(notes))
        .flag("protected", protected);
    let r = client.post(&format!("/nodes/{}/vzdump", g.node), &p).await?;
    run_task(client, ctx, &format!("backup {} to {storage}", g.vmid), r).await?;
    Ok(format!("backup of {} written to {storage}", g.vmid))
}

/// Restores a backup over the existing guest (it must be stopped).
pub async fn restore(client: &PveClient, ctx: &JobCtx, g: &GuestRef, volid: &str, target_storage: Option<&str>) -> anyhow::Result<String> {
    let st = status(client, g).await?;
    if st.s("status") == "running" {
        ctx.info("guest is running, stopping it first");
        power(client, ctx, g, PowerAction::Stop).await?;
    }
    let p = match g.kind {
        GuestKind::Qemu => Params::new().set("vmid", g.vmid).set("archive", volid).set("force", 1).opt("storage", target_storage),
        GuestKind::Lxc => {
            Params::new().set("vmid", g.vmid).set("ostemplate", volid).set("restore", 1).set("force", 1).opt("storage", target_storage)
        }
    };
    let r = client.post(&format!("/nodes/{}/{}", g.node, g.kind.api()), &p).await?;
    run_task(client, ctx, &format!("restore {volid}"), r).await?;
    Ok(format!("restored {} from {volid}", g.vmid))
}

pub async fn delete_volume(client: &PveClient, ctx: &JobCtx, node: &str, storage: &str, volid: &str) -> anyhow::Result<String> {
    let r = client.delete(&format!("/nodes/{node}/storage/{storage}/content/{}", urlencoding::encode(volid)), &Params::new()).await?;
    run_task(client, ctx, &format!("delete {volid}"), r).await?;
    Ok(format!("deleted {volid}"))
}

pub async fn set_volume_protected(client: &PveClient, node: &str, storage: &str, volid: &str, protected: bool) -> anyhow::Result<()> {
    client
        .put(
            &format!("/nodes/{node}/storage/{storage}/content/{}", urlencoding::encode(volid)),
            &Params::new().flag("protected", protected),
        )
        .await?;
    Ok(())
}

pub async fn migrate(
    client: &PveClient,
    ctx: &JobCtx,
    g: &GuestRef,
    target: &str,
    online: bool,
    target_storage: Option<&str>,
) -> anyhow::Result<String> {
    let mut p = Params::new().set("target", target);
    match g.kind {
        GuestKind::Qemu => {
            if online {
                p.push("online", 1);
                p.push("with-local-disks", 1);
            }
        }
        GuestKind::Lxc => {
            if online {
                // Containers cannot live-migrate; restart mode is the closest.
                p.push("restart", 1);
            }
        }
    }
    if let Some(s) = target_storage.filter(|s| !s.is_empty()) {
        p.push("targetstorage", s);
    }
    let r = client.post(&format!("{}/migrate", g.path()), &p).await?;
    run_task(client, ctx, &format!("migrate {} to {target}", g.vmid), r).await?;
    Ok(format!("{} migrated to {target}", g.vmid))
}

#[allow(clippy::too_many_arguments)]
pub async fn clone_guest(
    client: &PveClient,
    ctx: &JobCtx,
    g: &GuestRef,
    newid: u64,
    name: &str,
    full: bool,
    target: Option<&str>,
    storage: Option<&str>,
    pool: Option<&str>,
) -> anyhow::Result<String> {
    let name_key = if g.kind == GuestKind::Qemu { "name" } else { "hostname" };
    let mut p = Params::new().set("newid", newid).opt(name_key, Some(name)).opt("pool", pool).opt("target", target);
    if full {
        p.push("full", 1);
        if let Some(s) = storage.filter(|s| !s.is_empty()) {
            p.push("storage", s);
        }
    } else if g.kind == GuestKind::Qemu {
        p.push("full", 0);
    }
    let r = client.post(&format!("{}/clone", g.path()), &p).await?;
    run_task(client, ctx, &format!("clone {} -> {newid}", g.vmid), r).await?;
    Ok(format!("cloned {} to {newid}", g.vmid))
}

pub async fn convert_to_template(client: &PveClient, ctx: &JobCtx, g: &GuestRef) -> anyhow::Result<String> {
    let r = client.post(&format!("{}/template", g.path()), &Params::new()).await?;
    run_task(client, ctx, "convert to template", r).await?;
    Ok(format!("{} is now a template", g.vmid))
}

/// Stops (if needed) and destroys a guest, then releases its IPAM addresses.
pub async fn destroy(
    client: &PveClient,
    ctx: &JobCtx,
    g: &GuestRef,
    purge: bool,
    ipam: Option<(&parking_lot::Mutex<crate::ipam::IpamDb>, &str)>,
) -> anyhow::Result<String> {
    let st = status(client, g).await?;
    if st.s("status") == "running" || st.s("status") == "paused" {
        ctx.info("guest is running, stopping it first");
        power(client, ctx, g, PowerAction::Stop).await?;
    }
    let p = Params::new().flag("purge", purge).set("destroy-unreferenced-disks", 1);
    let r = client.delete(&g.path(), &p).await?;
    run_task(client, ctx, &format!("destroy {}", g.vmid), r).await?;
    if let Some((db, server_id)) = ipam {
        let released = db.lock().release_guest_note(server_id, g.vmid, "guest destroyed")?;
        if !released.is_empty() {
            ctx.info(format!("released IPAM addresses: {}", released.join(", ")));
        }
        crate::hostkeys::forget(server_id, g.vmid);
    }
    Ok(format!("{} {} destroyed", g.kind.label(), g.vmid))
}

/// Clears a stale lock (e.g. left by an interrupted backup or create).
/// Proxmox only honours `skiplock` for root@pam.
pub async fn unlock(client: &PveClient, ctx: &JobCtx, g: &GuestRef) -> anyhow::Result<String> {
    if !client.is_root() {
        bail!("clearing a lock requires root@pam (password login); the API rejects skiplock for other users and tokens");
    }
    let p = Params::new().set("delete", "lock").set("skiplock", 1);
    match g.kind {
        GuestKind::Qemu => {
            let r = client.post(&format!("{}/config", g.path()), &p).await?;
            run_task(client, ctx, "clear lock", r).await?;
        }
        GuestKind::Lxc => {
            client.put(&format!("{}/config", g.path()), &p).await?;
        }
    }
    Ok(format!("lock cleared on {}", g.vmid))
}

pub async fn next_vmid(client: &PveClient) -> anyhow::Result<u64> {
    let v = client.get("/cluster/nextid").await?;
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).ok_or_else(|| anyhow!("unexpected /cluster/nextid response: {v}"))
}

/// The lowest free VMID inside `range` (or Proxmox's next free ID without one).
pub async fn next_vmid_in(client: &PveClient, range: Option<(u64, u64)>) -> anyhow::Result<u64> {
    let Some((lo, hi)) = range else { return next_vmid(client).await };
    let used: std::collections::HashSet<u64> =
        client.get_q("/cluster/resources", &Params::new().set("type", "vm")).await?.arr().iter().map(|r| r.u("vmid")).collect();
    let mut checked = 0;
    for id in lo..=hi {
        if used.contains(&id) {
            continue;
        }
        // Double-check with Proxmox (it also knows IDs reserved by running tasks).
        if vmid_free(client, id).await? {
            return Ok(id);
        }
        checked += 1;
        if checked > 50 {
            break;
        }
    }
    bail!("no free VMID in this server's range {lo}–{hi}")
}

pub async fn vmid_free(client: &PveClient, vmid: u64) -> anyhow::Result<bool> {
    match client.get_q("/cluster/nextid", &Params::new().set("vmid", vmid)).await {
        Ok(_) => Ok(true),
        Err(e) => {
            if e.downcast_ref::<crate::api::ApiError>().map(|a| a.status == 400).unwrap_or(false) {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// Browser console URL (noVNC for VMs, xterm.js for containers / node shell).
pub fn console_url(client: &PveClient, g: &GuestRef, name: &str) -> String {
    let base = format!("https://{}:{}/", client.host(), client.port());
    let (node, name) = (urlencoding::encode(&g.node), urlencoding::encode(name));
    match g.kind {
        GuestKind::Qemu => format!("{base}?console=kvm&novnc=1&vmid={}&vmname={name}&node={node}&resize=off&cmd=", g.vmid),
        GuestKind::Lxc => format!("{base}?console=lxc&xtermjs=1&vmid={}&vmname={name}&node={node}&cmd=", g.vmid),
    }
}

pub fn node_shell_url(client: &PveClient, node: &str) -> String {
    format!("https://{}:{}/?console=shell&xtermjs=1&vmid=0&node={}&cmd=", client.host(), client.port(), urlencoding::encode(node))
}

/// Builds a SPICE `.vv` file and opens it with the system's viewer (virt-viewer).
pub async fn open_spice(client: &PveClient, g: &GuestRef) -> anyhow::Result<String> {
    if g.kind != GuestKind::Qemu {
        bail!("SPICE is only available for VMs");
    }
    let data = client.post(&format!("{}/spiceproxy", g.path()), &Params::new().set("proxy", client.host())).await?;
    let obj = data.as_object().ok_or_else(|| anyhow!("unexpected spiceproxy response"))?;
    let mut vv = String::from("[virt-viewer]\n");
    for (k, v) in obj {
        let val = match v {
            Value::String(s) => s.replace('\n', "\\n"),
            other => other.to_string(),
        };
        vv.push_str(&format!("{k}={val}\n"));
    }
    // The file holds a short-lived SPICE ticket: random name, private, and
    // removed again once the viewer has had time to read it.
    let path = std::env::temp_dir().join(format!("pmxspan-spice-{}.vv", crate::config::new_id()));
    {
        use std::io::Write;
        let mut f = crate::config::create_private(&path)?;
        f.write_all(vv.as_bytes())?;
    }
    if let Err(e) = open_path(&path) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(60));
        let _ = std::fs::remove_file(&path);
    });
    Ok(format!("opened SPICE session for {} (requires virt-viewer / remote-viewer)", g.vmid))
}

pub fn open_path(path: &std::path::Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        // explorer opens the file with its associated app, without cmd.exe
        // re-parsing the path (`&`, `^` in a profile path).
        std::process::Command::new("explorer").arg(path).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
    }
    Ok(())
}

// ---- guest agent ----------------------------------------------------------

pub async fn agent_get(client: &PveClient, g: &GuestRef, cmd: &str) -> anyhow::Result<Value> {
    let v = client.get(&format!("{}/agent/{cmd}", g.path())).await?;
    Ok(v.get("result").cloned().unwrap_or(v))
}

pub async fn agent_ping(client: &PveClient, g: &GuestRef) -> anyhow::Result<()> {
    client.post(&format!("{}/agent/ping", g.path()), &Params::new()).await?;
    Ok(())
}

pub async fn agent_set_password(client: &PveClient, g: &GuestRef, user: &str, password: &str) -> anyhow::Result<()> {
    client.post(&format!("{}/agent/set-user-password", g.path()), &Params::new().set("username", user).set("password", password)).await?;
    Ok(())
}

/// Runs a command through the guest agent and waits for its output.
pub async fn agent_exec(client: &PveClient, g: &GuestRef, argv: &[String], input: Option<&str>) -> anyhow::Result<Value> {
    if argv.is_empty() {
        bail!("no command given");
    }
    let mut p = Params::new();
    for a in argv {
        p.0.push(("command".into(), a.clone()));
    }
    if let Some(i) = input.filter(|s| !s.is_empty()) {
        p.0.push(("input-data".into(), i.to_string()));
    }
    let r = client.post(&format!("{}/agent/exec", g.path()), &p).await?;
    let pid = r.u("pid");
    for _ in 0..120 {
        let st = client.get_q(&format!("{}/agent/exec-status", g.path()), &Params::new().set("pid", pid)).await?;
        if st.b("exited") {
            return Ok(st);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("command did not finish within 60s (pid {pid})")
}

/// Resets the guest's root/admin password.
/// - QEMU with a responding agent: set it live via the agent.
/// - QEMU without agent: write `cipassword` and regenerate cloud-init (applies on next boot).
/// - LXC: the API offers no way to change a running container's password.
pub async fn reset_password(client: &PveClient, ctx: &JobCtx, g: &GuestRef, user: &str, password: &str) -> anyhow::Result<String> {
    if password.len() < 5 {
        bail!("password is too short");
    }
    match g.kind {
        GuestKind::Lxc => bail!(
            "Proxmox has no API to change a container's password. Use the console (passwd), or rebuild the container with a new password."
        ),
        GuestKind::Qemu => {
            if agent_ping(client, g).await.is_ok() {
                ctx.info("guest agent responding, setting password live");
                agent_set_password(client, g, user, password).await?;
                return Ok(format!("password for '{user}' changed via guest agent"));
            }
            ctx.warn("guest agent not responding, falling back to cloud-init (applies on next boot)");
            let cfg = config(client, g).await?;
            if !cfg.values().any(|v| v.as_str().map(|s| s.contains("cloudinit")).unwrap_or(false)) {
                bail!("guest agent is not responding and the VM has no cloud-init drive; cannot reset the password");
            }
            set_config(client, ctx, g, Params::new().set("ciuser", user).set("cipassword", password)).await?;
            regenerate_cloudinit(client, g).await?;
            Ok("cloud-init password updated; reboot the VM to apply it".into())
        }
    }
}

pub async fn regenerate_cloudinit(client: &PveClient, g: &GuestRef) -> anyhow::Result<()> {
    client.put(&format!("{}/cloudinit", g.path()), &Params::new()).await?;
    Ok(())
}

/// Sets the CD-ROM (`ide2` unless another cdrom slot exists) to an ISO or empties it.
pub async fn set_cdrom(client: &PveClient, ctx: &JobCtx, g: &GuestRef, iso: Option<&str>, boot_first: bool) -> anyhow::Result<String> {
    let cfg = config(client, g).await?;
    let slot = cdrom_slot(&cfg).ok_or_else(|| anyhow!("no free IDE/SATA slot for a CD-ROM drive"))?;
    let value = match iso {
        Some(i) => format!("{i},media=cdrom"),
        None => "none,media=cdrom".into(),
    };
    let mut p = Params::new().set(&slot, value);
    if boot_first && iso.is_some() {
        let mut order: Vec<String> = cfg
            .get("boot")
            .and_then(Value::as_str)
            .and_then(|b| PropString::parse(b).get("order").map(str::to_string))
            .map(|o| o.split(';').map(str::to_string).collect())
            .unwrap_or_default();
        order.retain(|d| d != &slot);
        order.insert(0, slot.clone());
        p.push("boot", format!("order={}", order.join(";")));
    }
    set_config(client, ctx, g, p).await?;
    Ok(match iso {
        Some(i) => format!("{i} inserted in {slot}"),
        None => format!("{slot} ejected"),
    })
}

/// The existing CD-ROM drive, or the slot a new one should use. The cloud-init
/// drive is also `media=cdrom` and must never be mistaken for it.
pub fn cdrom_slot(cfg: &Map<String, Value>) -> Option<String> {
    let is_cdrom = |v: &Value| v.as_str().map(|s| s.contains("media=cdrom") && !s.contains("cloudinit")).unwrap_or(false);
    let mut existing: Vec<&String> = cfg.iter().filter(|(k, v)| is_qemu_disk_key(k) && is_cdrom(v)).map(|(k, _)| k).collect();
    existing.sort();
    if let Some(k) = existing.first() {
        return Some((*k).clone());
    }
    if !cfg.contains_key("ide2") {
        return Some("ide2".into());
    }
    free_disk_slot(cfg, "ide").or_else(|| free_disk_slot(cfg, "sata"))
}

/// Free disk slot for a given bus, e.g. `scsi1`.
pub fn free_disk_slot(cfg: &Map<String, Value>, bus: &str) -> Option<String> {
    let max = match bus {
        "ide" => 4,
        "sata" => 6,
        "scsi" => 31,
        "virtio" => 16,
        "mp" => 256,
        _ => return None,
    };
    (0..max).map(|i| format!("{bus}{i}")).find(|k| !cfg.contains_key(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn boot_disk_detection() {
        let cfg = json!({
            "boot": "order=ide2;virtio1;net0",
            "ide2": "local:iso/x.iso,media=cdrom",
            "scsi0": "local-lvm:vm-1-disk-0,size=10G",
            "virtio1": "local-lvm:vm-1-disk-1,size=20G",
            "ide0": "local-lvm:vm-1-cloudinit,media=cdrom",
        });
        let cfg = cfg.as_object().unwrap();
        assert_eq!(disk_keys(GuestKind::Qemu, cfg), vec!["scsi0", "virtio1"]);
        assert_eq!(boot_disk(GuestKind::Qemu, cfg).as_deref(), Some("virtio1"));
        assert_eq!(disk_size_bytes(cfg, "scsi0"), Some(10 * 1024 * 1024 * 1024));
        assert_eq!(free_disk_slot(cfg, "scsi").as_deref(), Some("scsi1"));
        assert_eq!(cdrom_slot(cfg).as_deref(), Some("ide2"));
        let ci_on_ide2 = json!({"ide2": "local-lvm:vm-1-cloudinit,media=cdrom", "scsi0": "x,size=1G"});
        assert_eq!(cdrom_slot(ci_on_ide2.as_object().unwrap()).as_deref(), Some("ide0"));
    }

    #[test]
    fn snapshot_names() {
        assert!(validate_snapshot_name("before_upgrade-1").is_ok());
        assert!(validate_snapshot_name("1abc").is_err());
        assert!(validate_snapshot_name("has space").is_err());
    }
}
