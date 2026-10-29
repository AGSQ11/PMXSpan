//! Local IP address management.
//!
//! The HostBill module delegated IP allocation to HostBill's IPAM plugin. As a
//! standalone app we keep a small local database of pools and assignments,
//! with the same guarantees the module was hardened to provide:
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
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IpamDb {
    pub pools: Vec<Pool>,
    pub assignments: Vec<Assignment>,
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
}

#[derive(Clone, Debug)]
pub struct Allocation {
    pub ip: String,
    pub cidr: String,
    pub gateway: String,
    pub pool_id: String,
    pub dns: String,
}

impl IpamDb {
    pub fn load(path: &Path) -> Self {
        let mut db: IpamDb = std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        db.path = path.to_path_buf();
        db
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
        Allocation { cidr: pool.cidr_for(&ip), gateway: pool.gateway.clone(), pool_id: pool.id.clone(), dns: pool.dns.clone(), ip }
    }

    /// Plans and records an allocation.
    pub fn allocate(&mut self, req: &AllocRequest) -> anyhow::Result<Allocation> {
        let a = self.plan(req)?;
        if !self.assignments.iter().any(|x| x.ip == a.ip) {
            self.assignments.push(Assignment {
                ip: a.ip.clone(),
                pool_id: a.pool_id.clone(),
                server_id: req.server_id.clone(),
                vmid: Some(req.vmid),
                hostname: req.hostname.clone(),
                reserved: false,
                note: String::new(),
                assigned_at: chrono::Utc::now().timestamp(),
            });
        }
        self.save()?;
        Ok(a)
    }

    pub fn release_ip(&mut self, ip: &str) -> anyhow::Result<()> {
        self.assignments.retain(|a| a.ip != ip || a.reserved);
        self.save()
    }

    /// Releases every non-reserved address held by a guest.
    pub fn release_guest(&mut self, server_id: &str, vmid: u64) -> anyhow::Result<Vec<String>> {
        let mut released = Vec::new();
        self.assignments.retain(|a| {
            let hit = a.server_id == server_id && a.vmid == Some(vmid) && !a.reserved;
            if hit {
                released.push(a.ip.clone());
            }
            !hit
        });
        self.save()?;
        Ok(released)
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
        self.assignments.push(Assignment {
            ip: addr.to_string(),
            pool_id: pool_id.to_string(),
            reserved: true,
            note: note.to_string(),
            assigned_at: chrono::Utc::now().timestamp(),
            ..Default::default()
        });
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
}
