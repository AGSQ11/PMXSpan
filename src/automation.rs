//! Local automation: guest schedules (snapshots with retention, timed power
//! actions) and usage alert rules, stored in `automation.json`.
//!
//! Proxmox has no scheduler for either, and PMXSpan has no agent on the
//! nodes, so schedules run while the app is open or from
//! `pmxspan --run-schedules` started by cron / Task Scheduler.

use crate::api::types::GuestKind;
use crate::config::write_atomic;
use chrono::{DateTime, Datelike, Local, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScheduleAction {
    /// Take a snapshot named `auto-<date>-<time>`, keeping the newest `keep` of them.
    Snapshot { keep: u32 },
    /// `start`, `shutdown`, `reboot` or `stop`.
    Power(String),
}

impl ScheduleAction {
    pub fn label(&self) -> String {
        match self {
            ScheduleAction::Snapshot { keep } => format!("snapshot (keep {keep})"),
            ScheduleAction::Power(a) => a.clone(),
        }
    }

    /// How late a missed run may still happen. A power action hours late
    /// does more harm than good; a late snapshot is still useful.
    fn grace_secs(&self) -> i64 {
        match self {
            ScheduleAction::Snapshot { .. } => 6 * 3600,
            ScheduleAction::Power(_) => 30 * 60,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum When {
    EveryHours(u32),
    /// At `at` (HH:MM, local time) on `days` (0 = Monday … 6 = Sunday; empty = every day).
    Daily {
        at: String,
        days: Vec<u8>,
    },
}

impl When {
    pub fn label(&self) -> String {
        match self {
            When::EveryHours(1) => "every hour".into(),
            When::EveryHours(h) => format!("every {h} hours"),
            When::Daily { at, days } if days.is_empty() || days.len() == 7 => format!("daily at {at}"),
            When::Daily { at, days } => {
                const N: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
                let d: Vec<&str> = days.iter().filter_map(|d| N.get(*d as usize).copied()).collect();
                format!("{} at {at}", d.join(", "))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub id: String,
    pub enabled: bool,
    pub server_id: String,
    pub kind: GuestKind,
    pub vmid: u64,
    /// Guest name when the schedule was made (display only).
    pub name: String,
    pub action: ScheduleAction,
    pub when: When,
    pub created: i64,
    pub last_run: i64,
    pub last_result: String,
    pub last_ok: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            id: crate::config::new_id(),
            enabled: true,
            server_id: String::new(),
            kind: GuestKind::Qemu,
            vmid: 0,
            name: String::new(),
            action: ScheduleAction::Snapshot { keep: 7 },
            when: When::Daily { at: "03:00".into(), days: Vec::new() },
            created: chrono::Utc::now().timestamp(),
            last_run: 0,
            last_result: String::new(),
            last_ok: true,
        }
    }
}

impl Schedule {
    /// The newest scheduled moment at or before `now`, if any.
    fn last_slot(&self, now: DateTime<Local>) -> Option<i64> {
        match &self.when {
            When::EveryHours(h) => {
                let every = i64::from((*h).max(1)) * 3600;
                let anchor = self.last_run.max(self.created);
                let next = anchor + every;
                (now.timestamp() >= next).then_some(next)
            }
            When::Daily { at, days } => {
                let t = NaiveTime::parse_from_str(at.trim(), "%H:%M").ok()?;
                for back in 0..8 {
                    let date = (now - chrono::Duration::days(back)).date_naive();
                    let wd = date.weekday().num_days_from_monday() as u8;
                    if !days.is_empty() && !days.contains(&wd) {
                        continue;
                    }
                    let slot = Local.from_local_datetime(&date.and_time(t)).earliest()?;
                    if slot <= now {
                        return Some(slot.timestamp());
                    }
                }
                None
            }
        }
    }

    /// Whether the schedule should run now: a slot has passed since the last
    /// run (and since it was made), and not so long ago that running late
    /// would surprise anyone.
    pub fn due(&self, now: DateTime<Local>) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(slot) = self.last_slot(now) else { return false };
        slot > self.last_run && slot >= self.created && now.timestamp() - slot <= self.action.grace_secs().max(self.min_grace())
    }

    /// Every-N-hours schedules are never "too late" for their own interval.
    fn min_grace(&self) -> i64 {
        match self.when {
            When::EveryHours(h) => i64::from(h.max(1)) * 3600,
            When::Daily { .. } => 0,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.vmid == 0 || self.server_id.is_empty() {
            anyhow::bail!("pick a guest");
        }
        match &self.when {
            When::EveryHours(h) if *h == 0 => anyhow::bail!("the interval must be at least one hour"),
            When::Daily { at, .. } if NaiveTime::parse_from_str(at.trim(), "%H:%M").is_err() => {
                anyhow::bail!("time must look like 03:30")
            }
            _ => {}
        }
        match &self.action {
            ScheduleAction::Snapshot { keep } if *keep == 0 => anyhow::bail!("keep at least one snapshot"),
            ScheduleAction::Power(a) if !["start", "shutdown", "reboot", "stop"].contains(&a.as_str()) => {
                anyhow::bail!("unknown power action {a}")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Metric {
    /// Percent of the guest's vCPUs.
    Cpu,
    /// Percent of the guest's memory.
    Memory,
    /// MB/s received.
    NetIn,
    /// MB/s sent.
    NetOut,
    /// MB/s read from disk.
    DiskRead,
    /// MB/s written to disk.
    DiskWrite,
}

impl Metric {
    pub const ALL: [Metric; 6] = [Metric::Cpu, Metric::Memory, Metric::NetIn, Metric::NetOut, Metric::DiskRead, Metric::DiskWrite];

    pub fn label(self) -> &'static str {
        match self {
            Metric::Cpu => "CPU",
            Metric::Memory => "Memory",
            Metric::NetIn => "Network in",
            Metric::NetOut => "Network out",
            Metric::DiskRead => "Disk read",
            Metric::DiskWrite => "Disk write",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Metric::Cpu | Metric::Memory => "%",
            _ => "MB/s",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertRule {
    pub id: String,
    pub enabled: bool,
    pub metric: Metric,
    pub threshold: f64,
    /// Fire once the value has stayed above the threshold this long.
    pub minutes: u32,
    /// Only these guests (empty = every running guest).
    pub vmids: Vec<u64>,
    /// Only on this server (empty = any).
    pub server_id: String,
}

impl Default for AlertRule {
    fn default() -> Self {
        Self {
            id: crate::config::new_id(),
            enabled: true,
            metric: Metric::Cpu,
            threshold: 90.0,
            minutes: 5,
            vmids: Vec::new(),
            server_id: String::new(),
        }
    }
}

impl AlertRule {
    pub fn label(&self) -> String {
        format!("{} above {}{} for {} min", self.metric.label(), self.threshold, self.metric.unit(), self.minutes)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomationStore {
    pub schedules: Vec<Schedule>,
    pub alert_rules: Vec<AlertRule>,
    #[serde(skip)]
    path: PathBuf,
}

impl AutomationStore {
    pub fn load(path: &Path) -> Self {
        let mut s: Self = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        s.path = path.to_path_buf();
        s
    }

    pub fn save(&self) -> anyhow::Result<()> {
        write_atomic(&self.path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    /// Takes newer run results from the file (written by `--run-schedules`).
    pub fn merge_runs_from_disk(&mut self) {
        let disk = Self::load(&self.path);
        for d in disk.schedules {
            if let Some(s) = self.schedules.iter_mut().find(|s| s.id == d.id)
                && d.last_run > s.last_run
            {
                s.last_run = d.last_run;
                s.last_ok = d.last_ok;
                s.last_result = d.last_result;
            }
        }
    }

    /// Records a run's outcome.
    pub fn record(&mut self, id: &str, at: i64, result: &anyhow::Result<String>) {
        if let Some(s) = self.schedules.iter_mut().find(|s| s.id == id) {
            s.last_run = at;
            s.last_ok = result.is_ok();
            s.last_result = match result {
                Ok(m) => m.clone(),
                Err(e) => format!("{e:#}"),
            };
        }
    }
}

/// Snapshot name for an automatic snapshot taken at `at`.
pub fn auto_snapshot_name(at: DateTime<Local>) -> String {
    format!("auto-{}", at.format("%Y%m%d-%H%M"))
}

/// Live usage of one guest, from two consecutive resource readings.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub netin: f64,
    pub netout: f64,
    pub diskread: f64,
    pub diskwrite: f64,
}

impl Sample {
    pub fn get(&self, m: Metric) -> f64 {
        match m {
            Metric::Cpu => self.cpu_pct,
            Metric::Memory => self.mem_pct,
            Metric::NetIn => self.netin,
            Metric::NetOut => self.netout,
            Metric::DiskRead => self.diskread,
            Metric::DiskWrite => self.diskwrite,
        }
    }
}

/// An alert that is (or was) firing.
#[derive(Clone, Debug)]
pub struct Alert {
    pub rule_id: String,
    pub rule: String,
    pub vmid: u64,
    pub name: String,
    pub value: f64,
    pub unit: &'static str,
    pub since: i64,
    pub resolved: Option<i64>,
}

/// Evaluates alert rules against samples taken while the app is open.
#[derive(Default)]
pub struct AlertMonitor {
    /// (vmid) → (time, netin, netout, diskread, diskwrite) of the previous reading.
    counters: std::collections::HashMap<u64, (f64, f64, f64, f64, f64)>,
    /// (rule, vmid) → when the value first went over the threshold.
    over_since: std::collections::HashMap<(String, u64), f64>,
    pub active: Vec<Alert>,
    /// Resolved alerts, newest last (bounded).
    pub recent: Vec<Alert>,
}

impl AlertMonitor {
    /// Feeds one resource reading taken at `now` (seconds). Returns alerts that
    /// started firing with this reading.
    pub fn feed(&mut self, server_id: &str, now: f64, guests: &[crate::api::types::Resource], rules: &[AlertRule]) -> Vec<Alert> {
        let mut fired = Vec::new();
        const MB: f64 = 1_000_000.0;
        let mut seen = std::collections::HashSet::new();
        for g in guests.iter().filter(|g| g.kind().is_some() && !g.template && g.status == "running") {
            seen.insert(g.vmid);
            let prev = self.counters.insert(g.vmid, (now, g.netin, g.netout, g.diskread, g.diskwrite));
            let rate = |cur: f64, old: f64, dt: f64| if dt > 0.0 && cur >= old { (cur - old) / dt / MB } else { 0.0 };
            let s = match prev {
                Some((t, i, o, r, w)) => Sample {
                    cpu_pct: g.cpu * 100.0,
                    mem_pct: if g.maxmem > 0.0 { g.mem / g.maxmem * 100.0 } else { 0.0 },
                    netin: rate(g.netin, i, now - t),
                    netout: rate(g.netout, o, now - t),
                    diskread: rate(g.diskread, r, now - t),
                    diskwrite: rate(g.diskwrite, w, now - t),
                },
                // Rates need two readings; CPU and memory work right away.
                None => Sample {
                    cpu_pct: g.cpu * 100.0,
                    mem_pct: if g.maxmem > 0.0 { g.mem / g.maxmem * 100.0 } else { 0.0 },
                    ..Default::default()
                },
            };
            for r in rules.iter().filter(|r| r.enabled && (r.server_id.is_empty() || r.server_id == server_id)) {
                if !r.vmids.is_empty() && !r.vmids.contains(&g.vmid) {
                    continue;
                }
                let key = (r.id.clone(), g.vmid);
                let value = s.get(r.metric);
                let firing = self.active.iter().position(|a| a.rule_id == r.id && a.vmid == g.vmid);
                if value > r.threshold {
                    let since = *self.over_since.entry(key).or_insert(now);
                    if let Some(i) = firing {
                        self.active[i].value = value;
                    } else if now - since >= f64::from(r.minutes) * 60.0 {
                        let a = Alert {
                            rule_id: r.id.clone(),
                            rule: r.label(),
                            vmid: g.vmid,
                            name: g.display_name(),
                            value,
                            unit: r.metric.unit(),
                            since: since as i64,
                            resolved: None,
                        };
                        self.active.push(a.clone());
                        fired.push(a);
                    }
                } else {
                    self.over_since.remove(&key);
                    if let Some(i) = firing {
                        self.resolve(i, now);
                    }
                }
            }
        }
        // Guests that stopped or vanished resolve their alerts.
        let gone: Vec<usize> = (0..self.active.len()).rev().filter(|i| !seen.contains(&self.active[*i].vmid)).collect();
        for i in gone {
            self.resolve(i, now);
        }
        self.counters.retain(|v, _| seen.contains(v));
        // Rules that were deleted or disabled resolve too.
        let live: Vec<&str> = rules.iter().filter(|r| r.enabled).map(|r| r.id.as_str()).collect();
        let dead: Vec<usize> = (0..self.active.len()).rev().filter(|i| !live.contains(&self.active[*i].rule_id.as_str())).collect();
        for i in dead {
            self.resolve(i, now);
        }
        fired
    }

    fn resolve(&mut self, i: usize, now: f64) {
        let mut a = self.active.remove(i);
        a.resolved = Some(now as i64);
        tracing::info!("alert resolved: {} on {} ({})", a.rule, a.vmid, a.name);
        self.recent.push(a);
        if self.recent.len() > 200 {
            self.recent.remove(0);
        }
    }

    /// Forgets everything (another server was connected).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::Resource;

    fn at(s: &str) -> DateTime<Local> {
        Local.from_local_datetime(&chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()).earliest().unwrap()
    }

    #[test]
    fn daily_schedules() {
        // 2026-11-02 is a Monday.
        let mut s = Schedule {
            created: at("2026-11-01 12:00").timestamp(),
            when: When::Daily { at: "03:00".into(), days: vec![] },
            ..Default::default()
        };
        assert!(!s.due(at("2026-11-02 02:59")));
        assert!(s.due(at("2026-11-02 03:00")));
        assert!(s.due(at("2026-11-02 08:00")), "a missed snapshot still runs within 6 h");
        assert!(!s.due(at("2026-11-02 10:00")), "but not much later");
        s.last_run = at("2026-11-02 03:01").timestamp();
        assert!(!s.due(at("2026-11-02 04:00")));
        assert!(s.due(at("2026-11-03 03:00")));
        // Weekdays only, and power actions are not run late.
        s.when = When::Daily { at: "22:00".into(), days: vec![0, 1, 2, 3, 4] };
        s.action = ScheduleAction::Power("shutdown".into());
        s.last_run = 0;
        assert!(!s.due(at("2026-11-07 22:05")), "Saturday");
        assert!(s.due(at("2026-11-06 22:10")), "Friday");
        assert!(!s.due(at("2026-11-06 23:00")), "30 minutes late at most");
        s.enabled = false;
        assert!(!s.due(at("2026-11-06 22:10")));
    }

    #[test]
    fn hourly_schedules() {
        let mut s = Schedule { created: at("2026-11-02 10:00").timestamp(), when: When::EveryHours(6), ..Default::default() };
        assert!(!s.due(at("2026-11-02 15:59")));
        assert!(s.due(at("2026-11-02 16:00")));
        assert!(s.due(at("2026-11-02 20:00")));
        s.last_run = at("2026-11-02 16:01").timestamp();
        assert!(!s.due(at("2026-11-02 21:00")));
        assert!(s.due(at("2026-11-02 22:02")));
        assert_eq!(auto_snapshot_name(at("2026-11-02 22:02")), "auto-20261102-2202");
        assert!(Schedule { vmid: 100, server_id: "s".into(), when: When::EveryHours(0), ..s.clone() }.validate().is_err());
        assert!(Schedule { vmid: 100, server_id: "s".into(), ..s }.validate().is_ok());
    }

    #[test]
    fn alerts_fire_after_their_duration_and_resolve() {
        let rule = AlertRule { metric: Metric::Cpu, threshold: 80.0, minutes: 2, ..Default::default() };
        let rate = AlertRule { metric: Metric::NetOut, threshold: 10.0, minutes: 0, ..Default::default() };
        let g = |cpu: f64, netout: f64| Resource {
            rtype: "qemu".into(),
            vmid: 100,
            name: "web".into(),
            status: "running".into(),
            cpu,
            netout,
            ..Default::default()
        };
        let rules = vec![rule.clone(), rate.clone()];
        let mut m = AlertMonitor::default();
        assert!(m.feed("s", 0.0, &[g(0.95, 0.0)], &rules).is_empty());
        assert!(m.feed("s", 60.0, &[g(0.95, 60e6)], &rules).is_empty(), "1 MB/s is under 10");
        let fired = m.feed("s", 120.0, &[g(0.95, 1260e6)], &rules);
        assert_eq!(fired.len(), 2, "CPU over for 2 min, 20 MB/s out");
        assert_eq!(m.active.len(), 2);
        assert!(m.feed("s", 180.0, &[g(0.95, 1260e6)], &rules).is_empty(), "no repeat while firing");
        assert_eq!(m.active.len(), 1, "network went quiet");
        m.feed("s", 240.0, &[g(0.10, 1260e6)], &rules);
        assert!(m.active.is_empty());
        assert_eq!(m.recent.len(), 2);
        // A stopped guest resolves its alerts.
        m.feed("s", 300.0, &[g(0.99, 0.0)], &rules);
        m.feed("s", 500.0, &[g(0.99, 0.0)], &rules);
        assert_eq!(m.active.len(), 1);
        m.feed("s", 560.0, &[], &rules);
        assert!(m.active.is_empty());
    }
}
