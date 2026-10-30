//! Exports guest configurations to a JSON file as a recovery copy: the raw
//! Proxmox config, firewall (options, rules, aliases, ipsets), HA settings,
//! pool, snapshot names and the guest's IPAM records.

use super::rebuild::FirewallSnapshot;
use crate::api::PveClient;
use crate::api::types::GuestRef;
use crate::config::{ServerProfile, write_atomic};
use crate::ipam::IpamDb;
use crate::jobs::JobCtx;
use crate::util::ValueExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

/// One guest to export, with what the resource list already knows.
#[derive(Clone, Debug)]
pub struct ExportItem {
    pub guest: GuestRef,
    pub name: String,
    pub pool: String,
}

pub async fn export(
    client: PveClient,
    ctx: JobCtx,
    profile: ServerProfile,
    ipam: Arc<Mutex<IpamDb>>,
    items: Vec<ExportItem>,
    path: PathBuf,
) -> anyhow::Result<String> {
    let mut guests = Vec::new();
    let mut failed = 0;
    for (i, it) in items.iter().enumerate() {
        ctx.progress(i as f32 / items.len().max(1) as f32);
        let g = &it.guest;
        let config = match client.get(&format!("{}/config", g.path())).await {
            Ok(c) => c,
            Err(e) => {
                ctx.warn(format!("{} {}: config not readable, skipped: {e:#}", g.kind.label(), g.vmid));
                failed += 1;
                continue;
            }
        };
        let firewall = match FirewallSnapshot::take(&client, g).await {
            Ok(f) => serde_json::to_value(f)?,
            Err(e) => {
                ctx.warn(format!("{}: firewall not readable: {e:#}", g.vmid));
                Value::Null
            }
        };
        let sid = format!("{}:{}", if g.kind == crate::api::types::GuestKind::Lxc { "ct" } else { "vm" }, g.vmid);
        let ha = client.get(&format!("/cluster/ha/resources/{sid}")).await.ok();
        let snapshots: Vec<String> = client
            .get_list(&format!("{}/snapshot", g.path()))
            .await
            .unwrap_or_default()
            .iter()
            .map(|s| s.s("name"))
            .filter(|n| n != "current")
            .collect();
        let ipam_records: Vec<_> = ipam.lock().guest_ips(&profile.id, g.vmid).into_iter().cloned().collect();
        guests.push(json!({
            "vmid": g.vmid,
            "type": g.kind.api(),
            "node": g.node,
            "name": it.name,
            "pool": it.pool,
            "config": config,
            "firewall": firewall,
            "ha": ha,
            "snapshots": snapshots,
            "ipam": ipam_records,
        }));
    }
    let doc = json!({
        "format": "pmxspan-guest-export",
        "version": 1,
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "app": concat!("PMXSpan ", env!("CARGO_PKG_VERSION")),
        "server": {"name": profile.display_name(), "host": profile.host, "port": profile.port},
        "guests": guests,
    });
    write_atomic(&path, serde_json::to_string_pretty(&doc)?.as_bytes())?;
    let n = doc["guests"].as_array().map(Vec::len).unwrap_or(0);
    let skipped = if failed > 0 { format!(" ({failed} skipped)") } else { String::new() };
    Ok(format!("exported {n} guest config(s){skipped} to {}", path.display()))
}

/// A default file name for an export.
pub fn file_name(profile: &ServerProfile, what: &str) -> String {
    let host: String = profile.display_name().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    format!("pmxspan-{host}-{what}-{}.json", chrono::Local::now().format("%Y%m%d-%H%M"))
}
