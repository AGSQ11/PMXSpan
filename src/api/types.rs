//! Typed views over Proxmox API responses. Construction is lenient: missing
//! or oddly-typed fields fall back to defaults instead of failing the view.

use crate::util::ValueExt;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum GuestKind {
    Qemu,
    Lxc,
}

impl GuestKind {
    pub fn api(self) -> &'static str {
        match self {
            GuestKind::Qemu => "qemu",
            GuestKind::Lxc => "lxc",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            GuestKind::Qemu => "VM",
            GuestKind::Lxc => "CT",
        }
    }
    pub fn from_api(s: &str) -> Option<Self> {
        match s {
            "qemu" => Some(GuestKind::Qemu),
            "lxc" => Some(GuestKind::Lxc),
            _ => None,
        }
    }
}

/// Identifies a guest. `node` is where it was last seen; it changes on migration.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GuestRef {
    pub node: String,
    pub kind: GuestKind,
    pub vmid: u64,
}

impl GuestRef {
    pub fn path(&self) -> String {
        format!("/nodes/{}/{}/{}", self.node, self.kind.api(), self.vmid)
    }
}

/// An entry of `/cluster/resources`.
#[derive(Clone, Debug, Default)]
pub struct Resource {
    pub rtype: String,
    pub node: String,
    pub vmid: u64,
    pub name: String,
    pub status: String,
    pub cpu: f64,
    pub maxcpu: f64,
    pub mem: f64,
    pub maxmem: f64,
    pub disk: f64,
    pub maxdisk: f64,
    pub uptime: u64,
    pub template: bool,
    pub tags: String,
    pub hastate: String,
    pub lock: String,
    pub storage: String,
    pub plugintype: String,
    pub content: String,
    pub shared: bool,
    pub pool: String,
}

impl Resource {
    pub fn from_value(v: &Value) -> Self {
        Self {
            rtype: v.s("type"),
            node: v.s("node"),
            vmid: v.u("vmid"),
            name: v.s("name"),
            status: v.s("status"),
            cpu: v.f("cpu"),
            maxcpu: v.f("maxcpu"),
            mem: v.f("mem"),
            maxmem: v.f("maxmem"),
            disk: v.f("disk"),
            maxdisk: v.f("maxdisk"),
            uptime: v.u("uptime"),
            template: v.b("template"),
            tags: v.s("tags"),
            hastate: v.s("hastate"),
            lock: v.s("lock"),
            storage: v.s("storage"),
            plugintype: v.s("plugintype"),
            content: v.s("content"),
            shared: v.b("shared"),
            pool: v.s("pool"),
        }
    }

    pub fn kind(&self) -> Option<GuestKind> {
        GuestKind::from_api(&self.rtype)
    }

    pub fn guest_ref(&self) -> Option<GuestRef> {
        Some(GuestRef { node: self.node.clone(), kind: self.kind()?, vmid: self.vmid })
    }

    pub fn display_name(&self) -> String {
        if self.name.is_empty() { format!("{} {}", self.rtype, self.vmid) } else { self.name.clone() }
    }

    pub fn has_content(&self, ct: &str) -> bool {
        self.content.split(',').any(|c| c.trim() == ct)
    }

    pub fn tag_list(&self) -> Vec<String> {
        self.tags.split([';', ',', ' ']).filter(|t| !t.is_empty()).map(str::to_string).collect()
    }
}

/// A parsed task id: `UPID:node:pid:pstart:starttime:type:id:user:`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upid {
    pub node: String,
    pub starttime: i64,
    pub task_type: String,
    pub id: String,
    pub user: String,
}

impl Upid {
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() < 8 || parts[0] != "UPID" {
            return None;
        }
        Some(Self {
            node: parts[1].to_string(),
            starttime: i64::from_str_radix(parts[4], 16).unwrap_or(0),
            task_type: parts[5].to_string(),
            id: parts[6].to_string(),
            user: parts[7].to_string(),
        })
    }
}

/// `OK` and `WARNINGS: n` both mean the task did its job.
pub fn task_exit_ok(exit: &str) -> bool {
    exit == "OK" || exit.starts_with("WARNINGS")
}

#[derive(Clone, Debug, Default)]
pub struct TaskInfo {
    pub upid: String,
    pub node: String,
    pub task_type: String,
    pub id: String,
    pub user: String,
    pub starttime: i64,
    pub endtime: i64,
    pub status: String,
}

impl TaskInfo {
    pub fn from_value(v: &Value) -> Self {
        Self {
            upid: v.s("upid"),
            node: v.s("node"),
            task_type: v.s("type"),
            id: v.s("id"),
            user: v.s("user"),
            starttime: v.f("starttime") as i64,
            endtime: v.f("endtime") as i64,
            status: v.s("status"),
        }
    }

    pub fn running(&self) -> bool {
        self.endtime == 0 && (self.status.is_empty() || self.status == "running")
    }

    pub fn ok(&self) -> bool {
        task_exit_ok(&self.status)
    }

    /// Human description, e.g. "qmstart 100".
    pub fn describe(&self) -> String {
        let what = describe_task_type(&self.task_type);
        if self.id.is_empty() { what.to_string() } else { format!("{what} {}", self.id) }
    }
}

pub fn describe_task_type(t: &str) -> &str {
    match t {
        "qmstart" | "vzstart" => "Start",
        "qmstop" | "vzstop" => "Stop",
        "qmshutdown" | "vzshutdown" => "Shutdown",
        "qmreboot" | "vzreboot" => "Reboot",
        "qmreset" => "Reset",
        "qmsuspend" | "vzsuspend" => "Suspend",
        "qmresume" | "vzresume" => "Resume",
        "qmcreate" | "vzcreate" => "Create",
        "qmclone" | "vzclone" => "Clone",
        "qmdestroy" | "vzdestroy" => "Destroy",
        "qmigrate" | "vzmigrate" => "Migrate",
        "qmsnapshot" | "vzsnapshot" => "Snapshot",
        "qmrollback" | "vzrollback" => "Rollback",
        "qmdelsnapshot" | "vzdelsnapshot" => "Delete snapshot",
        "qmtemplate" | "vztemplate" => "Convert to template",
        "qmmove" | "move_volume" => "Move disk",
        "resize" => "Resize disk",
        "vzdump" => "Backup",
        "qmrestore" => "Restore",
        "imgcopy" => "Copy image",
        "download" => "Download",
        "aptupdate" => "Update package database",
        "vncproxy" | "vncshell" => "Console",
        "spiceproxy" => "SPICE console",
        "termproxy" => "Terminal",
        "startall" => "Start all guests",
        "stopall" => "Stop all guests",
        "imgdel" => "Erase data",
        _ => t,
    }
}

/// Node from `/nodes`.
#[derive(Clone, Debug, Default)]
pub struct NodeInfo {
    pub node: String,
    pub status: String,
    pub cpu: f64,
    pub mem: f64,
    pub maxmem: f64,
}

impl NodeInfo {
    pub fn from_value(v: &Value) -> Self {
        Self { node: v.s("node"), status: v.s("status"), cpu: v.f("cpu"), mem: v.f("mem"), maxmem: v.f("maxmem") }
    }
    pub fn online(&self) -> bool {
        self.status == "online"
    }
}

/// Per-node storage from `/nodes/{node}/storage`.
#[derive(Clone, Debug, Default)]
pub struct NodeStorage {
    pub storage: String,
    pub stype: String,
    pub content: String,
    pub active: bool,
    pub enabled: bool,
    pub shared: bool,
    pub avail: f64,
    pub used: f64,
    pub total: f64,
}

impl NodeStorage {
    pub fn from_value(v: &Value) -> Self {
        Self {
            storage: v.s("storage"),
            stype: v.s("type"),
            content: v.s("content"),
            active: v.get("active").map(|_| v.b("active")).unwrap_or(true),
            enabled: v.get("enabled").map(|_| v.b("enabled")).unwrap_or(true),
            shared: v.b("shared"),
            avail: v.f("avail"),
            used: v.f("used"),
            total: v.f("total"),
        }
    }
    pub fn has_content(&self, ct: &str) -> bool {
        self.content.split(',').any(|c| c.trim() == ct)
    }
    pub fn usable(&self) -> bool {
        self.active && self.enabled
    }
}

/// An item of `/nodes/{node}/storage/{storage}/content`.
#[derive(Clone, Debug, Default)]
pub struct StorageItem {
    pub volid: String,
    pub content: String,
    pub format: String,
    pub size: f64,
    pub ctime: i64,
    pub vmid: u64,
    pub notes: String,
    pub protected: bool,
}

impl StorageItem {
    pub fn from_value(v: &Value) -> Self {
        Self {
            volid: v.s("volid"),
            content: v.s("content"),
            format: v.s("format"),
            size: v.f("size"),
            ctime: v.f("ctime") as i64,
            vmid: v.u("vmid"),
            notes: v.s("notes"),
            protected: v.b("protected"),
        }
    }
    /// The file part of the volume id.
    pub fn file_name(&self) -> &str {
        let after_colon = self.volid.split_once(':').map(|(_, r)| r).unwrap_or(&self.volid);
        after_colon.rsplit('/').next().unwrap_or(after_colon)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub name: String,
    pub description: String,
    pub snaptime: i64,
    pub vmstate: bool,
}

impl Snapshot {
    pub fn from_value(v: &Value) -> Self {
        Self { name: v.s("name"), description: v.s("description"), snaptime: v.f("snaptime") as i64, vmstate: v.b("vmstate") }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FirewallRule {
    pub pos: u64,
    pub rtype: String,
    pub action: String,
    pub enable: bool,
    pub source: String,
    pub dest: String,
    pub proto: String,
    pub dport: String,
    pub macro_: String,
    pub comment: String,
}

impl FirewallRule {
    pub fn from_value(v: &Value) -> Self {
        Self {
            pos: v.u("pos"),
            rtype: v.s("type"),
            action: v.s("action"),
            enable: v.b("enable"),
            source: v.s("source"),
            dest: v.s("dest"),
            proto: v.s("proto"),
            dport: v.s("dport"),
            macro_: v.s("macro"),
            comment: v.s("comment"),
        }
    }
}

pub fn is_qemu_disk_key(k: &str) -> bool {
    ["ide", "sata", "scsi", "virtio"]
        .iter()
        .any(|b| k.strip_prefix(b).map(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit())).unwrap_or(false))
}

pub fn is_lxc_disk_key(k: &str) -> bool {
    k == "rootfs" || k.strip_prefix("mp").map(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit())).unwrap_or(false)
}

pub fn is_net_key(k: &str) -> bool {
    k.strip_prefix("net").map(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit())).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upid_parse() {
        let u = Upid::parse("UPID:pve1:0012ABCD:01234567:65A1B2C3:qmstart:100:root@pam:").unwrap();
        assert_eq!(u.node, "pve1");
        assert_eq!(u.task_type, "qmstart");
        assert_eq!(u.id, "100");
        assert_eq!(u.starttime, 0x65A1B2C3);
        assert!(Upid::parse("nope").is_none());
    }

    #[test]
    fn keys() {
        assert!(is_qemu_disk_key("scsi0"));
        assert!(!is_qemu_disk_key("scsihw"));
        assert!(is_lxc_disk_key("mp3"));
        assert!(is_net_key("net0"));
        assert!(!is_net_key("network"));
        assert!(task_exit_ok("WARNINGS: 2"));
        assert!(!task_exit_ok("unexpected status"));
    }
}
