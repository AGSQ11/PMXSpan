//! Runs one schedule: an automatic snapshot (with retention) or a power action.

use super::guest::{self, PowerAction};
use crate::api::PveClient;
use crate::api::types::GuestRef;
use crate::automation::{Schedule, ScheduleAction, auto_snapshot_name};
use crate::jobs::JobCtx;
use crate::util::ValueExt;
use anyhow::{Context, anyhow};

pub async fn run(client: &PveClient, ctx: &JobCtx, s: &Schedule) -> anyhow::Result<String> {
    // The guest may have migrated since the schedule was made: find its node now.
    let node = client
        .get_list("/cluster/resources")
        .await?
        .iter()
        .find(|r| r.u("vmid") == s.vmid && r.s("type") == s.kind.api())
        .map(|r| r.s("node"))
        .ok_or_else(|| anyhow!("{} {} no longer exists", s.kind.label(), s.vmid))?;
    let g = GuestRef { node, kind: s.kind, vmid: s.vmid };
    match &s.action {
        ScheduleAction::Snapshot { keep } => {
            let name = auto_snapshot_name(chrono::Local::now());
            guest::snapshot_create(client, ctx, &g, &name, "Automatic snapshot (PMXSpan schedule)", false).await?;
            let mut autos: Vec<_> = guest::snapshots(client, &g).await?.into_iter().filter(|x| x.name.starts_with("auto-")).collect();
            autos.sort_by_key(|x| x.snaptime);
            let excess = autos.len().saturating_sub((*keep).max(1) as usize);
            let mut pruned = Vec::new();
            for old in autos.into_iter().take(excess) {
                guest::snapshot_delete(client, ctx, &g, &old.name).await.with_context(|| format!("pruning snapshot {}", old.name))?;
                pruned.push(old.name);
            }
            Ok(if pruned.is_empty() {
                format!("snapshot {name} taken")
            } else {
                format!("snapshot {name} taken; pruned {}", pruned.join(", "))
            })
        }
        ScheduleAction::Power(a) => {
            let action = match a.as_str() {
                "start" => PowerAction::Start,
                "shutdown" => PowerAction::Shutdown,
                "reboot" => PowerAction::Reboot,
                "stop" => PowerAction::Stop,
                other => anyhow::bail!("unknown power action {other}"),
            };
            let st = guest::status(client, &g).await?.s("status");
            // Nothing to do is a success, not an error.
            match (action, st.as_str()) {
                (PowerAction::Start, "running") => return Ok("already running".into()),
                (PowerAction::Shutdown | PowerAction::Stop | PowerAction::Reboot, "stopped") => return Ok("already stopped".into()),
                _ => {}
            }
            guest::power(client, ctx, &g, action).await
        }
    }
}
