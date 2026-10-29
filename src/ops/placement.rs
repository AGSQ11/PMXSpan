//! Node + storage auto-placement, ported from the module's
//! `CreateVmSelectNode` / `CreateVMSelectNodeStorage`.
//!
//! Nodes without enough free memory are skipped, then each remaining node's
//! storages are checked for the required content type and free space. The
//! survivors are ordered by the configured resource priority and picked
//! according to the strategy (dense / sparse / random).

use crate::api::PveClient;
use crate::api::types::{NodeInfo, NodeStorage};
use crate::config::{PlacementStrategy, Resource};
use anyhow::bail;
use rand::seq::SliceRandom;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

#[derive(Clone, Debug)]
pub struct PlacementRequest {
    pub memory_mb: u64,
    pub disk_gb: u64,
    /// `images` for QEMU disks, `rootdir` for LXC.
    pub content: &'static str,
    /// Use exactly this storage if set.
    pub storage: Option<String>,
    /// Restrict to these nodes (empty = all online nodes).
    pub nodes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub node: String,
    pub mem_free_mb: f64,
    pub cpu_free: f64,
    pub storage: NodeStorage,
}

impl Candidate {
    fn storage_free_gb(&self) -> f64 {
        self.storage.avail / GIB
    }
    fn key(&self, r: Resource) -> f64 {
        match r {
            Resource::Ram => self.mem_free_mb,
            Resource::Cpu => self.cpu_free,
            Resource::Disk => self.storage_free_gb(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Placement {
    pub node: String,
    pub storage: String,
    /// Why other nodes were rejected, for the job log.
    pub notes: Vec<String>,
}

/// Picks the storage on one node: active, enabled, right content, enough space.
pub fn pick_storage(storages: &[NodeStorage], req: &PlacementRequest, strategy: PlacementStrategy) -> Option<NodeStorage> {
    let need = req.disk_gb as f64;
    let mut ok: Vec<&NodeStorage> = storages
        .iter()
        .filter(|s| s.usable() && s.has_content(req.content))
        .filter(|s| req.storage.as_ref().map(|w| &s.storage == w).unwrap_or(true))
        .filter(|s| (s.avail / GIB).floor() >= need)
        .collect();
    ok.sort_by(|a, b| a.avail.total_cmp(&b.avail));
    match strategy {
        PlacementStrategy::Dense => ok.first().map(|s| (*s).clone()),
        PlacementStrategy::Sparse => ok.last().map(|s| (*s).clone()),
        PlacementStrategy::Random => ok.choose(&mut rand::thread_rng()).map(|s| (*s).clone()),
    }
}

/// Orders candidates by the priority list (ascending free resources) and picks one.
pub fn choose(mut avail: Vec<Candidate>, priority: &[Resource], strategy: PlacementStrategy) -> Option<Candidate> {
    let mut prio: Vec<Resource> = priority.to_vec();
    for r in [Resource::Ram, Resource::Cpu, Resource::Disk] {
        if !prio.contains(&r) {
            prio.push(r);
        }
    }
    avail.sort_by(|a, b| {
        for r in &prio {
            let o = a.key(*r).total_cmp(&b.key(*r));
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        a.node.cmp(&b.node)
    });
    match strategy {
        PlacementStrategy::Dense => avail.into_iter().next(),
        PlacementStrategy::Sparse => avail.into_iter().last(),
        PlacementStrategy::Random => avail.choose(&mut rand::thread_rng()).cloned(),
    }
}

pub async fn select(
    client: &PveClient,
    req: &PlacementRequest,
    strategy: PlacementStrategy,
    priority: &[Resource],
) -> anyhow::Result<Placement> {
    let nodes: Vec<NodeInfo> = client.get_list("/nodes").await?.iter().map(NodeInfo::from_value).collect();
    let mut notes = Vec::new();
    let mut avail = Vec::new();
    let mut enough_mem = false;
    for n in nodes {
        if !req.nodes.is_empty() && !req.nodes.contains(&n.node) {
            continue;
        }
        if !n.online() {
            notes.push(format!("{}: offline", n.node));
            continue;
        }
        let mem_free_mb = (n.maxmem - n.mem) / MIB;
        if mem_free_mb < req.memory_mb as f64 {
            notes.push(format!("{}: only {:.0} MiB RAM free, need {}", n.node, mem_free_mb, req.memory_mb));
            continue;
        }
        enough_mem = true;
        // A sick node is skipped, not fatal.
        let storages = match client.get_list(&format!("/nodes/{}/storage", n.node)).await {
            Ok(list) => list.iter().map(NodeStorage::from_value).collect::<Vec<_>>(),
            Err(e) => {
                notes.push(format!("{}: cannot read storage ({e:#})", n.node));
                continue;
            }
        };
        match pick_storage(&storages, req, strategy) {
            Some(storage) => avail.push(Candidate { node: n.node.clone(), mem_free_mb, cpu_free: 1.0 - n.cpu, storage }),
            None => notes.push(format!(
                "{}: no active storage with content '{}'{} and {} GiB free",
                n.node,
                req.content,
                req.storage.as_ref().map(|s| format!(" named '{s}'")).unwrap_or_default(),
                req.disk_gb
            )),
        }
    }
    if avail.is_empty() {
        if enough_mem {
            bail!("unable to find a node with enough free storage:\n  {}", notes.join("\n  "));
        }
        bail!("unable to find a node with enough free memory:\n  {}", notes.join("\n  "));
    }
    let c = choose(avail, priority, strategy).expect("non-empty");
    Ok(Placement { node: c.node, storage: c.storage.storage, notes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(name: &str, avail_gb: f64, content: &str) -> NodeStorage {
        NodeStorage {
            storage: name.into(),
            content: content.into(),
            active: true,
            enabled: true,
            avail: avail_gb * GIB,
            ..Default::default()
        }
    }

    fn req() -> PlacementRequest {
        PlacementRequest { memory_mb: 1024, disk_gb: 20, content: "images", storage: None, nodes: vec![] }
    }

    #[test]
    fn storage_filtering() {
        let s = vec![st("small", 10.0, "images"), st("iso", 500.0, "iso"), st("a", 50.0, "images,rootdir"), st("b", 300.0, "images")];
        assert_eq!(pick_storage(&s, &req(), PlacementStrategy::Dense).unwrap().storage, "a");
        assert_eq!(pick_storage(&s, &req(), PlacementStrategy::Sparse).unwrap().storage, "b");
        let lxc = PlacementRequest { content: "rootdir", ..req() };
        assert_eq!(pick_storage(&s, &lxc, PlacementStrategy::Sparse).unwrap().storage, "a");
        let named = PlacementRequest { storage: Some("small".into()), ..req() };
        assert!(pick_storage(&s, &named, PlacementStrategy::Sparse).is_none());
    }

    #[test]
    fn node_ordering() {
        let c =
            |n: &str, mem: f64, cpu: f64| Candidate { node: n.into(), mem_free_mb: mem, cpu_free: cpu, storage: st("x", 100.0, "images") };
        let avail = vec![c("n1", 4000.0, 0.5), c("n2", 16000.0, 0.1), c("n3", 8000.0, 0.9)];
        let prio = [Resource::Ram, Resource::Cpu, Resource::Disk];
        assert_eq!(choose(avail.clone(), &prio, PlacementStrategy::Dense).unwrap().node, "n1");
        assert_eq!(choose(avail.clone(), &prio, PlacementStrategy::Sparse).unwrap().node, "n2");
        let cpu_first = [Resource::Cpu];
        assert_eq!(choose(avail, &cpu_first, PlacementStrategy::Sparse).unwrap().node, "n3");
    }
}
