//! Resource limits per tenant pool. Proxmox has no pool quotas, so PMXSpan
//! enforces them itself when it creates a guest, assigns addresses or moves a
//! guest into a pool. Limits live in the server profile; 0 means unlimited.

use crate::api::PveClient;
use crate::api::types::Resource;
use crate::config::PoolLimits;
use crate::ipam::IpamDb;
use crate::util::ValueExt;
use anyhow::bail;
use parking_lot::Mutex;
use std::sync::Arc;

const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// What a pool uses (or what an operation adds).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub guests: u64,
    pub cores: u64,
    pub memory_mb: u64,
    pub disk_gb: u64,
    pub ips: u64,
}

/// Current usage of `pool`: its guests (templates excluded), their CPU,
/// memory and root disk sizes, and their IPAM addresses.
pub fn usage(pool: &str, resources: &[Resource], ipam: &IpamDb, server_id: &str) -> Usage {
    let mut u = Usage::default();
    for r in resources.iter().filter(|r| r.kind().is_some() && !r.template && r.pool == pool) {
        u.guests += 1;
        u.cores += r.maxcpu.round() as u64;
        u.memory_mb += (r.maxmem / MIB).round() as u64;
        u.disk_gb += (r.maxdisk / GIB).ceil() as u64;
        u.ips += ipam.guest_ips(server_id, r.vmid).iter().filter(|a| !a.reserved).count() as u64;
    }
    u
}

/// Fails, naming every exceeded limit, when `current + add` goes over `limits`.
pub fn check(pool: &str, limits: &PoolLimits, current: Usage, add: Usage) -> anyhow::Result<()> {
    let mut over = Vec::new();
    let mut test = |what: &str, limit: u64, cur: u64, add: u64, unit: &str| {
        if limit > 0 && add > 0 && cur + add > limit {
            over.push(format!("{what}: {cur}{unit} used + {add}{unit} > limit {limit}{unit}"));
        }
    };
    test("guests", limits.guests, current.guests, add.guests, "");
    test("vCPUs", limits.cores, current.cores, add.cores, "");
    test("memory", limits.memory_mb, current.memory_mb, add.memory_mb, " MiB");
    test("disk", limits.disk_gb, current.disk_gb, add.disk_gb, " GiB");
    test("IP addresses", limits.ips, current.ips, add.ips, "");
    if !over.is_empty() {
        bail!("pool {pool} would exceed its limits (Users · Tenants → Pools):\n  {}", over.join("\n  "));
    }
    Ok(())
}

/// Reads the cluster fresh and checks one operation against a pool's limits.
/// Does nothing for pools without limits.
pub async fn enforce(
    client: &PveClient,
    limits: Option<&PoolLimits>,
    pool: &str,
    ipam: &Arc<Mutex<IpamDb>>,
    server_id: &str,
    add: Usage,
) -> anyhow::Result<()> {
    let Some(limits) = limits.filter(|l| !l.is_unlimited()) else { return Ok(()) };
    let resources: Vec<Resource> = client.get_list("/cluster/resources").await?.iter().map(Resource::from_value).collect();
    let current = usage(pool, &resources, &ipam.lock(), server_id);
    check(pool, limits, current, add)
}

/// The pool a guest belongs to, from a fresh resource list.
pub async fn pool_of(client: &PveClient, vmid: u64) -> anyhow::Result<Option<String>> {
    Ok(client.get_list("/cluster/resources").await?.iter().find(|r| r.u("vmid") == vmid).map(|r| r.s("pool")).filter(|p| !p.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits() {
        let l = PoolLimits { guests: 2, cores: 4, memory_mb: 4096, disk_gb: 0, ips: 2 };
        let cur = Usage { guests: 1, cores: 2, memory_mb: 2048, disk_gb: 500, ips: 1 };
        assert!(check("t", &l, cur, Usage { guests: 1, cores: 2, memory_mb: 2048, disk_gb: 50, ips: 1 }).is_ok());
        let e = check("t", &l, cur, Usage { guests: 1, cores: 4, memory_mb: 1024, disk_gb: 50, ips: 2 }).unwrap_err().to_string();
        assert!(e.contains("vCPUs: 2 used + 4 > limit 4") && e.contains("IP addresses") && !e.contains("memory") && !e.contains("disk"));
        // Adding nothing never fails, even when already over.
        assert!(check("t", &l, Usage { guests: 9, ..cur }, Usage { ips: 1, ..Default::default() }).is_ok());
        assert!(PoolLimits::default().is_unlimited());
    }
}
