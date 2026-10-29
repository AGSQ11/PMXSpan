//! Connection diagnostics: not just "did login work" but cluster health, storage capability coverage
//! and the effective permissions of the credential being used.

use crate::api::{Params, PveClient};
use crate::util::ValueExt;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct NodeLine {
    pub name: String,
    pub online: bool,
    pub ip: String,
}

#[derive(Clone, Debug, Default)]
pub struct StorageLine {
    pub storage: String,
    pub stype: String,
    pub content: String,
    pub shared: bool,
    pub nodes: String,
    pub disabled: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub version: String,
    pub release: String,
    pub cluster_name: Option<String>,
    pub quorate: Option<bool>,
    pub nodes: Vec<NodeLine>,
    pub storages: Vec<StorageLine>,
    /// content type -> storages providing it
    pub content_matrix: BTreeMap<String, Vec<String>>,
    /// privilege -> granted on `/`
    pub privileges: Vec<(String, bool)>,
    pub warnings: Vec<String>,
    pub userid: String,
}

pub const CONTENT_TYPES: [(&str, &str); 6] = [
    ("images", "VM disks"),
    ("rootdir", "container volumes"),
    ("iso", "ISO images"),
    ("vztmpl", "container templates"),
    ("backup", "backups"),
    ("snippets", "snippets / hookscripts"),
];

/// Privileges the panel's features rely on, checked on `/`.
pub const PRIVILEGES: [&str; 12] = [
    "Sys.Audit",
    "Sys.Modify",
    "VM.Allocate",
    "VM.Audit",
    "VM.PowerMgmt",
    "VM.Console",
    "VM.Config.Disk",
    "VM.Snapshot",
    "VM.Backup",
    "Datastore.AllocateSpace",
    "Pool.Allocate",
    "Permissions.Modify",
];

pub async fn run(client: &PveClient) -> anyhow::Result<Report> {
    let mut r = Report { userid: client.userid().to_string(), ..Default::default() };
    let v = client.get("/version").await?;
    r.version = v.s("version");
    r.release = v.s("release");

    match client.get_list("/cluster/status").await {
        Ok(items) => {
            for it in items {
                match it.s("type").as_str() {
                    "cluster" => {
                        r.cluster_name = Some(it.s("name"));
                        r.quorate = Some(it.b("quorate"));
                    }
                    "node" => r.nodes.push(NodeLine { name: it.s("name"), online: it.b("online"), ip: it.s("ip") }),
                    _ => {}
                }
            }
        }
        Err(e) => r.warnings.push(format!("cannot read /cluster/status (needs Sys.Audit): {e:#}")),
    }
    if r.nodes.is_empty()
        && let Ok(nodes) = client.get_list("/nodes").await
    {
        r.nodes = nodes.iter().map(|n| NodeLine { name: n.s("node"), online: n.s("status") == "online", ..Default::default() }).collect();
    }
    for n in r.nodes.iter().filter(|n| !n.online) {
        r.warnings.push(format!("node {} is offline", n.name));
    }
    if r.quorate == Some(false) {
        r.warnings.push("cluster is NOT quorate - most write operations will fail".into());
    }

    match client.get_list("/storage").await {
        Ok(list) => {
            for s in list {
                let line = StorageLine {
                    storage: s.s("storage"),
                    stype: s.s("type"),
                    content: s.s("content"),
                    shared: s.b("shared"),
                    nodes: s.opt_s("nodes").unwrap_or_else(|| "all".into()),
                    disabled: s.b("disable"),
                };
                if !line.disabled {
                    for c in line.content.split(',').map(str::trim).filter(|c| !c.is_empty()) {
                        r.content_matrix.entry(c.to_string()).or_default().push(line.storage.clone());
                    }
                }
                r.storages.push(line);
            }
        }
        Err(e) => r.warnings.push(format!("cannot read storage configuration: {e:#}")),
    }
    for (ct, what) in CONTENT_TYPES {
        if !r.content_matrix.contains_key(ct) {
            r.warnings.push(format!("no enabled storage accepts '{ct}' ({what})"));
        }
    }

    match client.get_q("/access/permissions", &Params::new().set("path", "/")).await {
        Ok(perms) => {
            let root = perms.get("/").cloned().unwrap_or_default();
            for p in PRIVILEGES {
                let granted = root.get(p).map(|v| !v.is_null()).unwrap_or(false);
                r.privileges.push((p.to_string(), granted));
            }
            let missing: Vec<&str> = r.privileges.iter().filter(|(_, g)| !g).map(|(p, _)| p.as_str()).collect();
            if !missing.is_empty() {
                r.warnings.push(format!("credential lacks on '/': {} (related features will fail)", missing.join(", ")));
            }
        }
        Err(e) => r.warnings.push(format!("cannot read effective permissions: {e:#}")),
    }
    if client.uses_token() {
        r.warnings.push("API tokens cannot open browser consoles (noVNC needs a ticket); use password auth for console access".into());
    }
    Ok(r)
}
