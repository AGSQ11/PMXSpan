//! Drift between the local IPAM database and the cluster: addresses kept for
//! guests that no longer exist, static addresses guests use that IPAM does not
//! know about, main addresses that differ, and app-created guests without any
//! IPAM record.

use super::guest;
use super::ipassign::static_ips;
use crate::api::PveClient;
use crate::api::types::{GuestRef, Resource};
use crate::ipam::IpamDb;

/// A guest's static addresses as configured on the cluster.
#[derive(Clone, Debug)]
pub struct GuestNet {
    pub guest: GuestRef,
    pub name: String,
    pub tags: String,
    /// (interface, address); `None` when its config could not be read.
    pub ips: Option<Vec<(u32, String)>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DriftKind {
    /// IPAM holds an address for a VMID that no longer exists.
    Orphan,
    /// The guest uses a static address IPAM has no record of (for it).
    Untracked,
    /// IPAM's main address for an interface is not what the guest has.
    Mismatch { guest_has: String },
    /// Created by the app (managed tag) but has no IPAM record at all.
    NoIpam,
}

#[derive(Clone, Debug)]
pub struct DriftItem {
    pub kind: DriftKind,
    pub vmid: u64,
    /// Set for guests that exist.
    pub guest: Option<GuestRef>,
    pub name: String,
    pub iface: Option<u32>,
    pub ip: String,
}

/// Reads every guest's config (templates skipped).
pub async fn scan(client: &PveClient, guests: Vec<Resource>) -> Vec<GuestNet> {
    let mut out = Vec::new();
    for r in guests.into_iter().filter(|r| !r.template) {
        let Some(kind) = r.kind() else { continue };
        let g = GuestRef { node: r.node.clone(), kind, vmid: r.vmid };
        let ips = guest::config(client, &g).await.ok().map(|cfg| static_ips(kind, &cfg));
        out.push(GuestNet { guest: g, name: r.name.clone(), tags: r.tags.clone(), ips });
    }
    out
}

/// Compares IPAM with what the cluster reports. `all_vmids` must hold every
/// VMID on the cluster (templates too) so nothing is called an orphan by mistake.
pub fn compare(server_id: &str, ipam: &IpamDb, guests: &[GuestNet], all_vmids: &[u64], managed_tag: &str) -> Vec<DriftItem> {
    let mut out = Vec::new();
    for a in ipam.assignments.iter().filter(|a| a.server_id == server_id) {
        if let Some(v) = a.vmid
            && !all_vmids.contains(&v)
        {
            out.push(DriftItem {
                kind: DriftKind::Orphan,
                vmid: v,
                guest: None,
                name: a.hostname.clone(),
                iface: a.iface,
                ip: a.ip.clone(),
            });
        }
    }
    for g in guests {
        let tracked = ipam.guest_ips(server_id, g.guest.vmid);
        let Some(ips) = &g.ips else { continue };
        for (n, ip) in ips {
            if !tracked.iter().any(|a| &a.ip == ip) {
                out.push(DriftItem {
                    kind: DriftKind::Untracked,
                    vmid: g.guest.vmid,
                    guest: Some(g.guest.clone()),
                    name: g.name.clone(),
                    iface: Some(*n),
                    ip: ip.clone(),
                });
            }
        }
        for a in tracked.iter().filter(|a| a.main) {
            let v6 = a.is_v6();
            let on_guest: Vec<&String> = ips.iter().filter(|(n, ip)| *n == a.iface() && ip.contains(':') == v6).map(|(_, ip)| ip).collect();
            if let Some(has) = on_guest.first()
                && !on_guest.contains(&&a.ip)
            {
                out.push(DriftItem {
                    kind: DriftKind::Mismatch { guest_has: (*has).clone() },
                    vmid: g.guest.vmid,
                    guest: Some(g.guest.clone()),
                    name: g.name.clone(),
                    iface: a.iface,
                    ip: a.ip.clone(),
                });
            }
        }
        let managed = !managed_tag.is_empty() && g.tags.split([';', ',', ' ']).any(|t| t.eq_ignore_ascii_case(managed_tag));
        if managed && tracked.is_empty() && ips.is_empty() {
            out.push(DriftItem {
                kind: DriftKind::NoIpam,
                vmid: g.guest.vmid,
                guest: Some(g.guest.clone()),
                name: g.name.clone(),
                iface: None,
                ip: String::new(),
            });
        }
    }
    out.sort_by_key(|d| d.vmid);
    out
}

pub fn kind_label(k: &DriftKind) -> &'static str {
    match k {
        DriftKind::Orphan => "guest gone",
        DriftKind::Untracked => "not in IPAM",
        DriftKind::Mismatch { .. } => "differs",
        DriftKind::NoIpam => "no IPAM record",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::GuestKind;
    use crate::ipam::{AllocRequest, Pool};

    #[test]
    fn finds_every_kind() {
        let mut db = IpamDb::load(&std::env::temp_dir().join(format!("pxp-drift-{}.json", crate::config::new_id())));
        db.pools.push(Pool {
            id: "p".into(),
            name: "p".into(),
            cidr: "192.0.2.0/24".into(),
            gateway: "192.0.2.1".into(),
            ..Default::default()
        });
        let req = |vmid: u64, ip: &str| AllocRequest { server_id: "s".into(), vmid, preferred: Some(ip.into()), ..Default::default() };
        db.allocate(&req(100, "192.0.2.10")).unwrap(); // guest 100: matches
        db.allocate(&req(101, "192.0.2.11")).unwrap(); // guest 101: has another IP
        db.allocate(&req(199, "192.0.2.99")).unwrap(); // guest 199: gone
        let g = |vmid: u64, ips: Vec<(u32, &str)>, tags: &str| GuestNet {
            guest: GuestRef { node: "n".into(), kind: GuestKind::Lxc, vmid },
            name: format!("g{vmid}"),
            tags: tags.into(),
            ips: Some(ips.into_iter().map(|(n, i)| (n, i.to_string())).collect()),
        };
        let guests = vec![
            g(100, vec![(0, "192.0.2.10")], ""),
            g(101, vec![(0, "192.0.2.50")], ""),
            g(102, vec![], "web;PMXSpan"),
            g(103, vec![], "web"),
        ];
        let d = compare("s", &db, &guests, &[100, 101, 102, 103, 9000], "PMXSpan");
        let kinds: Vec<(u64, &str)> = d.iter().map(|i| (i.vmid, kind_label(&i.kind))).collect();
        assert_eq!(kinds, vec![(101, "not in IPAM"), (101, "differs"), (102, "no IPAM record"), (199, "guest gone")]);
        // Other servers' records are ignored.
        assert!(compare("other", &db, &guests, &[], "PMXSpan").iter().all(|i| i.kind != DriftKind::Orphan));
    }
}
