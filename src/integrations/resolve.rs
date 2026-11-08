//! Where and as whom to connect to a guest or node over SSH.
//!
//! Host: IPAM main IPv4, else IPAM main IPv6, else an address reported by the
//! guest agent (no loopback / link-local, IPv4 first), else a container's
//! static `ip=` / `ip6=`. User: the VM's cloud-init `ciuser`, else the
//! catalog image's default user, else `root`; containers use `root`.

use crate::api::PveClient;
use crate::api::types::{GuestKind, GuestRef};
use crate::ipam::IpamDb;
use crate::util::{PropString, ValueExt};
use serde_json::{Map, Value};
use std::net::IpAddr;

/// The main address of one family, from the lowest interface that has one.
pub fn ipam_main(db: &IpamDb, server_id: &str, vmid: u64, v6: bool) -> Option<String> {
    let mut list: Vec<_> = db.guest_ips(server_id, vmid).into_iter().filter(|a| a.main && a.is_v6() == v6).collect();
    list.sort_by_key(|a| a.iface());
    list.first().map(|a| a.ip.clone())
}

fn routable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !(v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast()),
        IpAddr::V6(v6) => {
            let link_local = (v6.segments()[0] & 0xffc0) == 0xfe80;
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || link_local)
        }
    }
}

/// Usable addresses from the agent's `network-get-interfaces` result, IPv4 first.
pub fn agent_addresses(result: &Value) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    for iface in result.arr() {
        if iface.s("name") == "lo" {
            continue;
        }
        for a in iface.get("ip-addresses").map(|a| a.arr()).unwrap_or_default() {
            if let Ok(ip) = a.s("ip-address").parse::<IpAddr>()
                && routable(&ip)
                && !out.contains(&ip)
            {
                out.push(ip);
            }
        }
    }
    // Stable: keeps the agent's order within each family.
    out.sort_by_key(|ip| ip.is_ipv6());
    out
}

/// A container's static addresses from `netN` (`ip=` then `ip6=`), IPv4 first.
pub fn lxc_static(cfg: &Map<String, Value>) -> Vec<String> {
    let mut nets: Vec<(&String, &Value)> = cfg.iter().filter(|(k, _)| crate::api::types::is_net_key(k)).collect();
    nets.sort_by_key(|(k, _)| k.trim_start_matches("net").parse::<u32>().unwrap_or(u32::MAX));
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for (_, v) in nets {
        let p = PropString::parse(v.as_str().unwrap_or(""));
        for (key, list) in [("ip", &mut v4), ("ip6", &mut v6)] {
            let Some(val) = p.get(key) else { continue };
            let addr = val.split('/').next().unwrap_or("");
            if let Ok(ip) = addr.parse::<IpAddr>()
                && routable(&ip)
            {
                list.push(ip.to_string());
            }
        }
    }
    v4.extend(v6);
    v4
}

/// The host to connect to and where it came from.
pub fn pick_host(ipam4: Option<&str>, ipam6: Option<&str>, agent: &[IpAddr], lxc: &[String]) -> Option<(String, &'static str)> {
    if let Some(ip) = ipam4.filter(|s| !s.is_empty()) {
        return Some((ip.to_string(), "IPAM main IPv4"));
    }
    if let Some(ip) = ipam6.filter(|s| !s.is_empty()) {
        return Some((ip.to_string(), "IPAM main IPv6"));
    }
    if let Some(ip) = agent.first() {
        return Some((ip.to_string(), "reported by the guest agent"));
    }
    lxc.first().map(|ip| (ip.clone(), "container network config"))
}

pub fn pick_user(kind: GuestKind, ciuser: &str, catalog_user: Option<&str>) -> String {
    if kind == GuestKind::Lxc {
        return "root".into();
    }
    let ciuser = ciuser.trim();
    if !ciuser.is_empty() {
        return ciuser.to_string();
    }
    catalog_user.map(str::trim).filter(|u| !u.is_empty()).unwrap_or("root").to_string()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    pub host: Option<String>,
    /// Where the host came from (for the confirmation card).
    pub host_source: &'static str,
    pub user: String,
}

/// Resolves a guest, asking the API only for what IPAM does not answer.
pub async fn resolve_guest(
    client: &PveClient,
    g: &GuestRef,
    running: bool,
    ipam4: Option<String>,
    ipam6: Option<String>,
    catalog_user: Option<String>,
) -> Resolved {
    let cfg = crate::ops::guest::config(client, g).await.unwrap_or_default();
    let ciuser = cfg.get("ciuser").and_then(Value::as_str).unwrap_or("").to_string();
    let user = pick_user(g.kind, &ciuser, catalog_user.as_deref());
    let mut agent = Vec::new();
    if ipam4.is_none() && ipam6.is_none() && running && g.kind == GuestKind::Qemu {
        // Bounded: a missing agent must not hold the dialog up.
        let call = crate::ops::guest::agent_get(client, g, "network-get-interfaces");
        if let Ok(Ok(v)) = tokio::time::timeout(std::time::Duration::from_secs(8), call).await {
            agent = agent_addresses(&v);
        }
    }
    let lxc = if g.kind == GuestKind::Lxc { lxc_static(&cfg) } else { Vec::new() };
    let picked = pick_host(ipam4.as_deref(), ipam6.as_deref(), &agent, &lxc);
    Resolved { host_source: picked.as_ref().map(|p| p.1).unwrap_or(""), host: picked.map(|p| p.0), user }
}

/// A node's address: its cluster IP from `/cluster/status`, else the profile
/// host when that is the node (a single-node setup or the same name).
pub fn node_host(status: &[Value], node: &str, profile_host: &str, node_count: usize) -> Option<String> {
    let ip = status.iter().find(|it| it.s("type") == "node" && it.s("name") == node).map(|it| it.s("ip")).filter(|ip| !ip.is_empty());
    if ip.is_some() {
        return ip;
    }
    let h = profile_host.trim();
    let same = h.eq_ignore_ascii_case(node) || h.split('.').next().is_some_and(|first| first.eq_ignore_ascii_case(node));
    (!h.is_empty() && (node_count == 1 || same)).then(|| h.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn host_resolution_order() {
        let agent = vec!["10.0.0.9".parse().unwrap()];
        let lxc = vec!["192.168.5.5".to_string()];
        assert_eq!(pick_host(Some("10.1.1.1"), Some("2001:db8::1"), &agent, &lxc).unwrap().0, "10.1.1.1");
        assert_eq!(pick_host(None, Some("2001:db8::1"), &agent, &lxc).unwrap().0, "2001:db8::1");
        assert_eq!(pick_host(None, None, &agent, &lxc).unwrap(), ("10.0.0.9".into(), "reported by the guest agent"));
        assert_eq!(pick_host(None, None, &[], &lxc).unwrap().0, "192.168.5.5");
        assert_eq!(pick_host(Some(""), None, &[], &[]), None);
    }

    #[test]
    fn agent_addresses_skip_loopback_and_link_local_ipv4_first() {
        let r = json!([
            {"name": "lo", "ip-addresses": [{"ip-address": "127.0.0.1"}, {"ip-address": "::1"}]},
            {"name": "eth0", "ip-addresses": [
                {"ip-address": "fe80::1"}, {"ip-address": "2001:db8::7"}, {"ip-address": "169.254.3.3"},
                {"ip-address": "10.0.0.7"}]},
            {"name": "eth1", "ip-addresses": [{"ip-address": "10.0.0.7"}, {"ip-address": "172.16.0.2"}]}
        ]);
        let got: Vec<String> = agent_addresses(&r).iter().map(|i| i.to_string()).collect();
        assert_eq!(got, ["10.0.0.7", "172.16.0.2", "2001:db8::7"]);
    }

    #[test]
    fn lxc_static_addresses_from_config() {
        let cfg: Map<String, Value> = serde_json::from_value(json!({
            "net1": "name=eth1,bridge=vmbr1,ip=192.168.9.9/24",
            "net0": "name=eth0,bridge=vmbr0,ip=dhcp,ip6=2001:db8::5/64",
            "net2": "name=eth2,bridge=vmbr0,ip=10.0.0.2/24,ip6=auto",
            "hostname": "ct1"
        }))
        .unwrap();
        assert_eq!(lxc_static(&cfg), ["192.168.9.9", "10.0.0.2", "2001:db8::5"]);
    }

    #[test]
    fn user_resolution_order() {
        assert_eq!(pick_user(GuestKind::Qemu, "ubuntu", Some("debian")), "ubuntu");
        assert_eq!(pick_user(GuestKind::Qemu, " ", Some("debian")), "debian");
        assert_eq!(pick_user(GuestKind::Qemu, "", None), "root");
        assert_eq!(pick_user(GuestKind::Lxc, "ubuntu", Some("debian")), "root");
    }

    #[test]
    fn ipam_main_prefers_lowest_interface() {
        let dir = std::env::temp_dir().join(format!("pmxspan-res-{}", crate::config::new_id()));
        let path = dir.join("ipam.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{"pools":[],"assignments":[
              {"ip":"10.0.1.5","server_id":"s","vmid":100,"iface":1,"main":true},
              {"ip":"10.0.0.5","server_id":"s","vmid":100,"iface":0,"main":true},
              {"ip":"10.0.0.6","server_id":"s","vmid":100,"iface":0,"main":false},
              {"ip":"2001:db8::5","server_id":"s","vmid":100,"iface":0,"main":true},
              {"ip":"10.9.9.9","server_id":"other","vmid":100,"iface":0,"main":true}]}"#,
        )
        .unwrap();
        let db = IpamDb::load(&path);
        assert_eq!(ipam_main(&db, "s", 100, false).as_deref(), Some("10.0.0.5"));
        assert_eq!(ipam_main(&db, "s", 100, true).as_deref(), Some("2001:db8::5"));
        assert_eq!(ipam_main(&db, "s", 101, false), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_address_from_cluster_status() {
        let st = vec![json!({"type": "cluster", "name": "lab"}), json!({"type": "node", "name": "pve1", "ip": "192.168.1.10"})];
        assert_eq!(node_host(&st, "pve1", "pve.example", 2).as_deref(), Some("192.168.1.10"));
        assert_eq!(node_host(&st, "pve2", "pve.example", 2), None);
        assert_eq!(node_host(&st, "pve2", "pve2.example.com", 2).as_deref(), Some("pve2.example.com"));
        assert_eq!(node_host(&[], "solo", "10.0.0.1", 1).as_deref(), Some("10.0.0.1"));
    }
}
