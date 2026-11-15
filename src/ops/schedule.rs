//! Runs one schedule: an automatic snapshot (with retention) or a power action.

use super::guest::{self, PowerAction};
use crate::api::PveClient;
use crate::api::types::GuestRef;
use crate::automation::{GuestReplaced, Schedule, ScheduleAction, auto_snapshot_name, auto_snapshot_prefix};
use crate::jobs::JobCtx;
use crate::util::ValueExt;
use anyhow::{Context, anyhow};

/// A VMID can be reused after a destroy (also outside PMXSpan). The schedule
/// is bound to the guest name it was made for, which a rebuild keeps; a live
/// guest under another name is a different guest and must not be acted on.
fn ensure_same_guest(s: &Schedule, live_name: &str) -> anyhow::Result<()> {
    let live = if live_name.is_empty() { format!("{} {}", s.kind.api(), s.vmid) } else { live_name.to_string() };
    if s.name.is_empty() || s.name == live {
        return Ok(());
    }
    Err(GuestReplaced(format!(
        "{} {} is now '{live}', not '{}' (destroyed, replaced or renamed): schedule disabled, nothing was done",
        s.kind.label(),
        s.vmid,
        s.name
    ))
    .into())
}

pub async fn run(client: &PveClient, ctx: &JobCtx, s: &Schedule) -> anyhow::Result<String> {
    // The guest may have migrated since the schedule was made: find its node now.
    let resources = client.get_list("/cluster/resources").await?;
    let row = resources
        .iter()
        .find(|r| r.u("vmid") == s.vmid && r.s("type") == s.kind.api())
        .ok_or_else(|| anyhow!("{} {} no longer exists", s.kind.label(), s.vmid))?;
    ensure_same_guest(s, &row.s("name"))?;
    let node = row.s("node");
    let g = GuestRef { node, kind: s.kind, vmid: s.vmid };
    match &s.action {
        ScheduleAction::Snapshot { keep } => {
            let name = auto_snapshot_name(&s.id, chrono::Local::now());
            guest::snapshot_create(client, ctx, &g, &name, "Automatic snapshot (PMXSpan schedule)", false).await?;
            // Only this schedule's own snapshots count towards its retention.
            let prefix = auto_snapshot_prefix(&s.id);
            let mut autos: Vec<_> = guest::snapshots(client, &g).await?.into_iter().filter(|x| x.name.starts_with(&prefix)).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::AutomationStore;

    fn sched(vmid: u64, name: &str) -> Schedule {
        Schedule { server_id: "s1".into(), vmid, name: name.into(), ..Default::default() }
    }

    #[test]
    fn destroyed_guest_schedules_are_revoked() {
        let mut st = AutomationStore::default();
        st.schedules = vec![sched(100, "web"), sched(101, "db"), Schedule { server_id: "s2".into(), ..sched(100, "other") }];
        assert_eq!(st.forget_guest("s1", 100), 1);
        let left: Vec<_> = st.schedules.iter().map(|s| (s.server_id.as_str(), s.vmid)).collect();
        assert_eq!(left, vec![("s1", 101), ("s2", 100)]);
    }

    #[test]
    fn reused_vmid_is_refused_and_schedule_disabled() {
        let s = sched(100, "web");
        assert!(ensure_same_guest(&s, "web").is_ok());
        let err = ensure_same_guest(&s, "prod-db").unwrap_err();
        assert!(err.downcast_ref::<GuestReplaced>().is_some(), "{err:#}");
        // A nameless guest is matched by the fallback display name.
        assert!(ensure_same_guest(&sched(100, "qemu 100"), "").is_ok());
        let mut st = AutomationStore::default();
        st.schedules = vec![s.clone()];
        st.record(&s.id, 5, &Err(err));
        assert!(!st.schedules[0].enabled && !st.schedules[0].last_ok);
        // Any other failure leaves the schedule on.
        let mut st = AutomationStore::default();
        st.schedules = vec![s.clone()];
        st.record(&s.id, 5, &Err(anyhow!("network down")));
        assert!(st.schedules[0].enabled);
    }
}
