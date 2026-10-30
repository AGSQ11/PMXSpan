//! Local IP address management.
//!
//! A small local database of pools and assignments, with these guarantees:
//! - an explicitly requested ("preferred") IP is honoured exactly or the
//!   operation fails; there is no silent fallback to a random address;
//! - allocation failures say *why* (no pools, no matching pool, exhausted).

use crate::config::write_atomic;
use anyhow::{Context, anyhow, bail};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Pool {
    pub id: String,
    pub name: String,
    /// Network in CIDR notation, e.g. `203.0.113.0/24` or `2001:db8::/64`.
    pub cidr: String,
    pub gateway: String,
    /// Optional first/last usable host addresses; whole subnet if empty.
    pub range_start: String,
    pub range_end: String,
    /// Restrict the pool to guests attached to this bridge (empty = any).
    pub bridge: String,
    pub vlan: Option<u16>,
    pub dns: String,
    /// Restrict to one Proxmox server profile (empty = any).
    pub server_id: String,
    pub notes: String,
}

impl Default for Pool {
    fn default() -> Self {
        Self {
            id: crate::config::new_id(),
            name: String::new(),
            cidr: String::new(),
            gateway: String::new(),
            range_start: String::new(),
            range_end: String::new(),
            bridge: String::new(),
            vlan: None,
            dns: String::new(),
            server_id: String::new(),
            notes: String::new(),
        }
    }
}

impl Pool {
    pub fn net(&self) -> anyhow::Result<IpNet> {
        IpNet::from_str(self.cidr.trim()).with_context(|| format!("pool '{}' has invalid CIDR '{}'", self.name, self.cidr))
    }

    pub fn is_v6(&self) -> bool {
        self.net().map(|n| matches!(n, IpNet::V6(_))).unwrap_or(false)
    }

    pub fn prefix_len(&self) -> u8 {
        self.net().map(|n| n.prefix_len()).unwrap_or(0)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let net = self.net()?;
        if self.name.trim().is_empty() {
            bail!("pool name is required");
        }
        for (label, v) in [("gateway", &self.gateway), ("range start", &self.range_start), ("range end", &self.range_end)] {
            if !v.trim().is_empty() {
                let ip = IpAddr::from_str(v.trim()).map_err(|_| anyhow!("{label} '{v}' is not an IP address"))?;
                if !net.contains(&ip) {
                    bail!("{label} {ip} is outside {net}");
                }
            }
        }
        Ok(())
    }

    /// Candidate host addresses in order, skipping network/broadcast (v4) and the gateway.
    fn candidates(&self) -> anyhow::Result<Box<dyn Iterator<Item = IpAddr>>> {
        let net = self.net()?;
        let start = if self.range_start.trim().is_empty() { None } else { Some(IpAddr::from_str(self.range_start.trim())?) };
        let end = if self.range_end.trim().is_empty() { None } else { Some(IpAddr::from_str(self.range_end.trim())?) };
        let gw = IpAddr::from_str(self.gateway.trim()).ok();
        let iter: Box<dyn Iterator<Item = IpAddr>> = match net {
            IpNet::V4(n) => Box::new(n.hosts().map(IpAddr::V4)),
            // v6 subnets are huge; cap the scan.
            IpNet::V6(n) => Box::new(n.hosts().skip(1).take(1 << 16).map(IpAddr::V6)),
        };
        Ok(Box::new(
            iter.filter(move |ip| start.map(|s| *ip >= s).unwrap_or(true) && end.map(|e| *ip <= e).unwrap_or(true) && Some(*ip) != gw),
        ))
    }

    pub fn capacity(&self) -> u64 {
        match self.candidates() {
            Ok(it) => it.take(1 << 20).count() as u64,
            Err(_) => 0,
        }
    }

    /// `ip/prefix` for guest network config.
    pub fn cidr_for(&self, ip: &str) -> String {
        format!("{}/{}", ip, self.prefix_len())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Assignment {
    pub ip: String,
    pub pool_id: String,
    pub server_id: String,
    pub vmid: Option<u64>,
    pub hostname: String,
    /// Reserved addresses are never handed out automatically.
    pub reserved: bool,
    pub note: String,
    pub assigned_at: i64,
    /// Guest interface index (`net<N>`) the address belongs to.
    pub iface: Option<u32>,
    /// The address pushed into the guest's network config for its family
    /// (LXC `netN ip=`, QEMU cloud-init `ipconfigN`). Others are additional
    /// addresses: tracked, allowed by the IP filter, configured in the guest.
    pub main: bool,
}

impl Assignment {
    pub fn is_v6(&self) -> bool {
        self.ip.contains(':')
    }
    pub fn iface(&self) -> u32 {
        self.iface.unwrap_or(0)
    }
    fn same_slot(&self, server_id: &str, vmid: u64, iface: u32, v6: bool) -> bool {
        self.server_id == server_id && self.vmid == Some(vmid) && self.iface() == iface && self.is_v6() == v6
    }
}

/// One change to an address, for the IP assignment history.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct IpEvent {
    pub at: i64,
    /// `assigned`, `adopted`, `released`, `main`, `reserved` or `unreserved`.
    pub action: String,
    pub ip: String,
    pub pool_id: String,
    pub server_id: String,
    pub vmid: Option<u64>,
    pub iface: Option<u32>,
    pub hostname: String,
    pub note: String,
    /// The local OS user who made the change.
    pub by: String,
}

/// Oldest history entries are dropped beyond this many.
const HISTORY_MAX: usize = 20_000;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IpamDb {
    pub pools: Vec<Pool>,
    pub assignments: Vec<Assignment>,
    /// Every change to an assignment, oldest first.
    pub history: Vec<IpEvent>,
    #[serde(skip)]
    path: PathBuf,
}

/// What a guest needs to get an address.
#[derive(Clone, Debug, Default)]
pub struct AllocRequest {
    pub server_id: String,
    pub vmid: u64,
    pub hostname: String,
    pub bridge: String,
    pub v6: bool,
    /// Exact address wanted; no fallback if it is unavailable.
    pub preferred: Option<String>,
    /// Restrict to one pool.
    pub pool_id: Option<String>,
    /// Guest interface index the address is for.
    pub iface: u32,
}

#[derive(Clone, Debug)]
pub struct Allocation {
    pub ip: String,
    pub cidr: String,
    pub gateway: String,
    pub pool_id: String,
    pub dns: String,
    pub vlan: Option<u16>,
    pub main: bool,
}

impl IpamDb {
    pub fn load(path: &Path) -> Self {
        let mut db: IpamDb = std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        db.path = path.to_path_buf();
        db.normalize();
        db
    }

    /// Upgrades older databases: guest addresses without an interface belong
    /// to net0, and every (guest, interface, family) gets exactly one main address.
    fn normalize(&mut self) {
        for a in self.assignments.iter_mut() {
            if a.vmid.is_some() && a.iface.is_none() {
                a.iface = Some(0);
            }
        }
        let mut seen_main = std::collections::HashSet::new();
        for a in self.assignments.iter_mut().filter(|a| a.vmid.is_some() && a.main) {
            let key = (a.server_id.clone(), a.vmid, a.iface(), a.is_v6());
            if !seen_main.insert(key) {
                a.main = false;
            }
        }
        let n = self.assignments.len();
        for i in 0..n {
            let a = &self.assignments[i];
            let Some(vmid) = a.vmid else { continue };
            let key = (a.server_id.clone(), Some(vmid), a.iface(), a.is_v6());
            if !seen_main.contains(&key) {
                self.assignments[i].main = true;
                seen_main.insert(key);
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        write_atomic(&self.path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn pool(&self, id: &str) -> Option<&Pool> {
        self.pools.iter().find(|p| p.id == id)
    }

    pub fn used_in_pool(&self, pool_id: &str) -> usize {
        self.assignments.iter().filter(|a| a.pool_id == pool_id).count()
    }

    fn is_taken(&self, ip: &str) -> bool {
        self.assignments.iter().any(|a| a.ip == ip)
    }

    fn matching_pools(&self, req: &AllocRequest) -> Vec<&Pool> {
        self.pools
            .iter()
            .filter(|p| p.is_v6() == req.v6)
            .filter(|p| p.server_id.is_empty() || p.server_id == req.server_id)
            .filter(|p| p.bridge.is_empty() || req.bridge.is_empty() || p.bridge == req.bridge)
            .filter(|p| req.pool_id.as_ref().map(|id| &p.id == id).unwrap_or(true))
            .collect()
    }

    /// Picks an address without recording it.
    pub fn plan(&self, req: &AllocRequest) -> anyhow::Result<Allocation> {
        let family = if req.v6 { "IPv6" } else { "IPv4" };
        if self.pools.iter().all(|p| p.is_v6() != req.v6) {
            bail!("no {family} pools are defined in IPAM");
        }
        let pools = self.matching_pools(req);
        if pools.is_empty() {
            bail!("no {family} pool matches this guest (server/bridge '{}' restrictions exclude all pools)", req.bridge);
        }
        if let Some(pref) = req.preferred.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let ip = IpAddr::from_str(pref).map_err(|_| anyhow!("preferred address '{pref}' is not a valid IP"))?;
            let pool = pools
                .iter()
                .find(|p| p.net().map(|n| n.contains(&ip)).unwrap_or(false))
                .ok_or_else(|| anyhow!("preferred address {ip} is not inside any matching {family} pool"))?;
            if let Some(a) = self.assignments.iter().find(|a| a.ip == ip.to_string())
                && (a.vmid != Some(req.vmid) || a.server_id != req.server_id)
            {
                bail!(
                    "preferred address {ip} is already {} {}",
                    if a.reserved { "reserved" } else { "assigned to" },
                    a.vmid.map(|v| format!("VMID {v}")).unwrap_or_else(|| a.note.clone())
                );
            }
            return Ok(self.allocation(pool, ip.to_string()));
        }
        for pool in &pools {
            for ip in pool.candidates()? {
                let s = ip.to_string();
                if !self.is_taken(&s) {
                    return Ok(self.allocation(pool, s));
                }
            }
        }
        bail!("all matching {family} pools are exhausted ({})", pools.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "))
    }

    fn allocation(&self, pool: &Pool, ip: String) -> Allocation {
        Allocation {
            cidr: pool.cidr_for(&ip),
            gateway: pool.gateway.clone(),
            pool_id: pool.id.clone(),
            dns: pool.dns.clone(),
            vlan: pool.vlan,
            main: false,
            ip,
        }
    }

    fn has_main(&self, server_id: &str, vmid: u64, iface: u32, v6: bool) -> bool {
        self.assignments.iter().any(|a| a.main && a.same_slot(server_id, vmid, iface, v6))
    }

    fn record(&mut self, req: &AllocRequest, a: Allocation) -> Allocation {
        self.record_as(req, a, "assigned")
    }

    fn record_as(&mut self, req: &AllocRequest, mut a: Allocation, action: &str) -> Allocation {
        if let Some(existing) = self.assignments.iter().find(|x| x.ip == a.ip) {
            a.main = existing.main;
            return a;
        }
        a.main = !self.has_main(&req.server_id, req.vmid, req.iface, req.v6);
        let assignment = Assignment {
            ip: a.ip.clone(),
            pool_id: a.pool_id.clone(),
            server_id: req.server_id.clone(),
            vmid: Some(req.vmid),
            hostname: req.hostname.clone(),
            reserved: false,
            note: String::new(),
            assigned_at: chrono::Utc::now().timestamp(),
            iface: Some(req.iface),
            main: a.main,
        };
        self.log(action, &assignment, "");
        self.assignments.push(assignment);
        a
    }

    /// Appends to the history (bounded).
    fn log(&mut self, action: &str, a: &Assignment, note: &str) {
        let by = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
        self.history.push(IpEvent {
            at: chrono::Utc::now().timestamp(),
            action: action.to_string(),
            ip: a.ip.clone(),
            pool_id: a.pool_id.clone(),
            server_id: a.server_id.clone(),
            vmid: a.vmid,
            iface: a.iface,
            hostname: a.hostname.clone(),
            note: note.to_string(),
            by,
        });
        if self.history.len() > HISTORY_MAX {
            let drop = self.history.len() - HISTORY_MAX;
            self.history.drain(..drop);
        }
    }

    /// Removes the assignments matching `hit` and logs each as released.
    fn remove_where(&mut self, hit: impl Fn(&Assignment) -> bool, note: &str) -> Vec<Assignment> {
        let (gone, kept): (Vec<Assignment>, Vec<Assignment>) = std::mem::take(&mut self.assignments).into_iter().partition(|a| hit(a));
        self.assignments = kept;
        for a in &gone {
            self.log(if a.reserved { "unreserved" } else { "released" }, a, note);
        }
        gone
    }

    /// Plans and records an allocation. The first address of a family on an
    /// interface becomes its main address.
    pub fn allocate(&mut self, req: &AllocRequest) -> anyhow::Result<Allocation> {
        let a = self.plan(req)?;
        let a = self.record(req, a);
        self.save()?;
        Ok(a)
    }

    /// Allocates `count` addresses atomically: either all are recorded or none.
    /// A preferred address (if any) is the first one; the rest come from the same pool rules.
    pub fn allocate_many(&mut self, req: &AllocRequest, count: usize) -> anyhow::Result<Vec<Allocation>> {
        let snapshot = (self.assignments.clone(), self.history.len());
        let mut out = Vec::new();
        for i in 0..count.max(1) {
            let r = if i == 0 { req.clone() } else { AllocRequest { preferred: None, ..req.clone() } };
            match self.plan(&r) {
                Ok(a) => out.push(self.record(&r, a)),
                Err(e) => {
                    self.assignments = snapshot.0;
                    self.history.truncate(snapshot.1);
                    let done = if i > 0 { format!(" (only {i} of {count} were available)") } else { String::new() };
                    return Err(e.context(format!("could not allocate {count} address(es){done}")));
                }
            }
        }
        self.save()?;
        Ok(out)
    }

    /// Releases an address. If it was the main address of its interface/family,
    /// the next address of the same slot is promoted so the guest keeps a main IP.
    pub fn release_ip(&mut self, ip: &str) -> anyhow::Result<()> {
        let removed = self.remove_where(|a| a.ip == ip && !a.reserved, "");
        for r in removed.iter().filter(|r| r.main) {
            if let Some(vmid) = r.vmid
                && let Some(next) = self.assignments.iter_mut().find(|a| a.same_slot(&r.server_id, vmid, r.iface(), r.is_v6()))
            {
                next.main = true;
            }
        }
        self.save()
    }

    /// Makes `ip` the main address of its interface/family.
    pub fn set_main(&mut self, ip: &str) -> anyhow::Result<()> {
        let target = self.assignments.iter().find(|a| a.ip == ip).cloned().ok_or_else(|| anyhow!("{ip} is not assigned"))?;
        let vmid = target.vmid.ok_or_else(|| anyhow!("{ip} is not assigned to a guest"))?;
        for a in self.assignments.iter_mut().filter(|a| a.same_slot(&target.server_id, vmid, target.iface(), target.is_v6())) {
            a.main = a.ip == ip;
        }
        if !target.main {
            self.log("main", &target, "");
        }
        self.save()
    }

    /// Releases every address on one interface of a guest.
    pub fn release_iface(&mut self, server_id: &str, vmid: u64, iface: u32) -> anyhow::Result<Vec<String>> {
        let gone = self.remove_where(|a| a.server_id == server_id && a.vmid == Some(vmid) && a.iface() == iface && !a.reserved, "");
        self.save()?;
        Ok(gone.into_iter().map(|a| a.ip).collect())
    }

    /// Records an address the guest already uses (adoption). It must lie in a
    /// pool and be free.
    pub fn register_existing(&mut self, server_id: &str, vmid: u64, iface: u32, ip: &str, hostname: &str) -> anyhow::Result<Allocation> {
        let addr = IpAddr::from_str(ip.trim()).map_err(|_| anyhow!("'{ip}' is not an IP address"))?;
        let pool = self
            .pools
            .iter()
            .find(|p| p.net().map(|n| n.contains(&addr)).unwrap_or(false))
            .cloned()
            .ok_or_else(|| anyhow!("{addr} is not inside any IPAM pool"))?;
        if let Some(a) = self.assignments.iter().find(|a| a.ip == addr.to_string())
            && (a.vmid != Some(vmid) || a.server_id != server_id)
        {
            bail!(
                "{addr} is already {}",
                if a.reserved { "reserved".to_string() } else { format!("assigned to VMID {}", a.vmid.unwrap_or(0)) }
            );
        }
        let req =
            AllocRequest { server_id: server_id.into(), vmid, hostname: hostname.into(), v6: addr.is_ipv6(), iface, ..Default::default() };
        let a = self.allocation(&pool, addr.to_string());
        let a = self.record_as(&req, a, "adopted");
        self.save()?;
        Ok(a)
    }

    /// The main address (with its pool) of one family on one guest interface.
    pub fn main_for(&self, server_id: &str, vmid: u64, iface: u32, v6: bool) -> Option<(Assignment, Option<Pool>)> {
        let a = self.assignments.iter().find(|a| a.main && a.same_slot(server_id, vmid, iface, v6))?;
        Some((a.clone(), self.pool(&a.pool_id).cloned()))
    }

    /// Releases every non-reserved address held by a guest.
    /// The note is recorded in the history.
    pub fn release_guest_note(&mut self, server_id: &str, vmid: u64, note: &str) -> anyhow::Result<Vec<String>> {
        let gone = self.remove_where(|a| a.server_id == server_id && a.vmid == Some(vmid) && !a.reserved, note);
        self.save()?;
        Ok(gone.into_iter().map(|a| a.ip).collect())
    }

    /// Drops a reservation.
    pub fn unreserve(&mut self, ip: &str) -> anyhow::Result<()> {
        self.remove_where(|a| a.ip == ip && a.reserved, "");
        self.save()
    }

    /// Deletes a pool and forgets every assignment in it.
    pub fn delete_pool(&mut self, id: &str) -> anyhow::Result<()> {
        self.remove_where(|a| a.pool_id == id, "pool deleted");
        self.pools.retain(|p| p.id != id);
        self.save()
    }

    pub fn guest_ips(&self, server_id: &str, vmid: u64) -> Vec<&Assignment> {
        self.assignments.iter().filter(|a| a.server_id == server_id && a.vmid == Some(vmid)).collect()
    }

    pub fn reserve(&mut self, pool_id: &str, ip: &str, note: &str) -> anyhow::Result<()> {
        let pool = self.pool(pool_id).ok_or_else(|| anyhow!("unknown pool"))?;
        let addr = IpAddr::from_str(ip.trim()).map_err(|_| anyhow!("'{ip}' is not an IP address"))?;
        if !pool.net()?.contains(&addr) {
            bail!("{addr} is outside {}", pool.cidr);
        }
        if self.is_taken(&addr.to_string()) {
            bail!("{addr} is already in use");
        }
        let a = Assignment {
            ip: addr.to_string(),
            pool_id: pool_id.to_string(),
            reserved: true,
            note: note.to_string(),
            assigned_at: chrono::Utc::now().timestamp(),
            ..Default::default()
        };
        self.log("reserved", &a, note);
        self.assignments.push(a);
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> IpamDb {
        let mut db = IpamDb { path: std::env::temp_dir().join(format!("pxp-ipam-{}.json", crate::config::new_id())), ..Default::default() };
        db.pools.push(Pool {
            id: "p4".into(),
            name: "public".into(),
            cidr: "192.0.2.0/29".into(),
            gateway: "192.0.2.1".into(),
            bridge: "vmbr0".into(),
            ..Default::default()
        });
        db
    }

    #[test]
    fn history_records_every_change() {
        let mut db = db();
        let req = AllocRequest { server_id: "s".into(), vmid: 100, bridge: "vmbr0".into(), ..Default::default() };
        db.allocate_many(&req, 2).unwrap();
        db.set_main("192.0.2.3").unwrap();
        db.reserve("p4", "192.0.2.6", "router").unwrap();
        db.release_ip("192.0.2.2").unwrap();
        // A failed multi-allocation leaves no trace.
        assert!(db.allocate_many(&AllocRequest { vmid: 101, ..req.clone() }, 9).is_err());
        db.release_guest_note("s", 100, "guest destroyed").unwrap();
        db.unreserve("192.0.2.6").unwrap();
        let actions: Vec<(&str, &str)> = db.history.iter().map(|e| (e.action.as_str(), e.ip.as_str())).collect();
        assert_eq!(
            actions,
            vec![
                ("assigned", "192.0.2.2"),
                ("assigned", "192.0.2.3"),
                ("main", "192.0.2.3"),
                ("reserved", "192.0.2.6"),
                ("released", "192.0.2.2"),
                ("released", "192.0.2.3"),
                ("unreserved", "192.0.2.6"),
            ]
        );
        assert_eq!(db.history[5].note, "guest destroyed");
        assert_eq!(db.history[0].vmid, Some(100));
        // It survives a reload.
        let again = IpamDb::load(&db.path);
        assert_eq!(again.history.len(), 7);
        let _ = std::fs::remove_file(&db.path);
    }

    #[test]
    fn allocates_sequentially_and_skips_gateway() {
        let mut db = db();
        let req = AllocRequest { server_id: "s".into(), vmid: 100, bridge: "vmbr0".into(), ..Default::default() };
        let a = db.allocate(&req).unwrap();
        assert_eq!(a.ip, "192.0.2.2");
        assert_eq!(a.cidr, "192.0.2.2/29");
        let b = db.allocate(&AllocRequest { vmid: 101, ..req.clone() }).unwrap();
        assert_eq!(b.ip, "192.0.2.3");
        // .1 is the gateway, .2..=.6 are hosts -> 5 usable.
        assert_eq!(db.pools[0].capacity(), 5);
        let _ = std::fs::remove_file(&db.path);
    }

    #[test]
    fn preferred_is_exact_or_error() {
        let mut db = db();
        let req = AllocRequest {
            server_id: "s".into(),
            vmid: 100,
            bridge: "vmbr0".into(),
            preferred: Some("192.0.2.5".into()),
            ..Default::default()
        };
        assert_eq!(db.allocate(&req).unwrap().ip, "192.0.2.5");
        let other = AllocRequest { vmid: 200, ..req.clone() };
        assert!(db.plan(&other).is_err());
        let outside = AllocRequest { preferred: Some("10.0.0.1".into()), ..req.clone() };
        assert!(db.plan(&outside).is_err());
        let _ = std::fs::remove_file(&db.path);
    }

    #[test]
    fn explains_failures() {
        let db = db();
        let v6 = AllocRequest { v6: true, ..Default::default() };
        assert!(db.plan(&v6).unwrap_err().to_string().contains("no IPv6 pools"));
        let wrong_bridge = AllocRequest { bridge: "vmbr9".into(), ..Default::default() };
        assert!(db.plan(&wrong_bridge).unwrap_err().to_string().contains("no IPv4 pool matches"));
    }

    #[test]
    fn main_address_and_promotion() {
        let mut db = db();
        let req = AllocRequest { server_id: "s".into(), vmid: 100, bridge: "vmbr0".into(), ..Default::default() };
        let ips = db.allocate_many(&req, 3).unwrap();
        assert_eq!(ips.iter().map(|a| a.main).collect::<Vec<_>>(), vec![true, false, false]);
        // A second interface gets its own main address.
        let n1 = db.allocate(&AllocRequest { iface: 1, ..req.clone() }).unwrap();
        assert!(n1.main);
        db.set_main(&ips[2].ip).unwrap();
        assert_eq!(db.main_for("s", 100, 0, false).unwrap().0.ip, ips[2].ip);
        // Releasing the main promotes another address on the same interface.
        db.release_ip(&ips[2].ip).unwrap();
        let m = db.main_for("s", 100, 0, false).unwrap().0.ip;
        assert!(m == ips[0].ip || m == ips[1].ip);
        assert_eq!(db.main_for("s", 100, 1, false).unwrap().0.ip, n1.ip);
        let _ = std::fs::remove_file(&db.path);
    }

    #[test]
    fn allocate_many_is_atomic() {
        let mut db = db();
        let req = AllocRequest { server_id: "s".into(), vmid: 100, bridge: "vmbr0".into(), ..Default::default() };
        // Only 5 usable addresses in a /29 without the gateway.
        let err = db.allocate_many(&req, 6).unwrap_err();
        assert!(format!("{err:#}").contains("only 5 of 6"));
        assert!(db.assignments.is_empty(), "partial allocation must be rolled back");
        let _ = std::fs::remove_file(&db.path);
    }

    #[test]
    fn adopt_existing_and_normalize_legacy() {
        let mut db = db();
        let a = db.register_existing("s", 100, 0, "192.0.2.4", "web").unwrap();
        assert!(a.main);
        assert!(db.register_existing("s", 101, 0, "192.0.2.4", "x").is_err());
        assert!(db.register_existing("s", 101, 0, "10.9.9.9", "x").is_err());
        // Legacy rows (no iface/main) are upgraded on load.
        db.assignments.push(Assignment { ip: "192.0.2.5".into(), server_id: "s".into(), vmid: Some(7), ..Default::default() });
        db.normalize();
        let legacy = db.assignments.iter().find(|x| x.ip == "192.0.2.5").unwrap();
        assert_eq!(legacy.iface, Some(0));
        assert!(legacy.main);
        let _ = std::fs::remove_file(&db.path);
    }
}
