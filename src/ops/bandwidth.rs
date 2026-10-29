//! Traffic accounting from RRD data, ported from the module's
//! `netUsageRrdData`: RRD `netin`/`netout` are average bytes/second per
//! sample, so traffic = rate x sample interval, summed from the period start.

use crate::api::types::GuestRef;
use crate::api::{Params, PveClient};
use crate::util::ValueExt;

#[derive(Clone, Debug, Default)]
pub struct Traffic {
    pub received: f64,
    pub sent: f64,
    pub samples: usize,
    pub timeframe: &'static str,
    /// Earliest sample actually covered (RRD retention may be shorter than asked).
    pub covered_from: i64,
}

impl Traffic {
    pub fn total(&self) -> f64 {
        self.received + self.sent
    }
}

/// Picks the finest RRD timeframe that still covers `since`.
pub fn timeframe_for(since: i64, now: i64) -> (&'static str, i64) {
    let age = (now - since).max(0);
    if age <= 3600 {
        ("hour", 60)
    } else if age <= 86_400 {
        ("day", 60)
    } else if age <= 7 * 86_400 {
        ("week", 1800)
    } else if age <= 31 * 86_400 {
        ("month", 21_600)
    } else {
        ("year", 604_800)
    }
}

/// Sums traffic from `(time, netin, netout)` samples newer than `since`.
pub fn integrate(samples: &[(i64, f64, f64)], since: i64, default_step: i64) -> (f64, f64, usize, i64) {
    let mut rx = 0.0;
    let mut tx = 0.0;
    let mut n = 0;
    let mut first = 0;
    let mut last: Option<i64> = None;
    for &(t, netin, netout) in samples {
        if t < since {
            last = Some(t);
            continue;
        }
        let delta = match last {
            Some(prev) if t > prev => t - prev,
            _ => default_step,
        } as f64;
        if netin.is_finite() {
            rx += netin * delta;
        }
        if netout.is_finite() {
            tx += netout * delta;
        }
        if first == 0 {
            first = t;
        }
        n += 1;
        last = Some(t);
    }
    (rx, tx, n, first)
}

pub async fn guest_traffic(client: &PveClient, g: &GuestRef, since: i64) -> anyhow::Result<Traffic> {
    let now = chrono::Utc::now().timestamp();
    let (tf, step) = timeframe_for(since, now);
    let data = client.get_q(&format!("{}/rrddata", g.path()), &Params::new().set("timeframe", tf).set("cf", "AVERAGE")).await?;
    let samples: Vec<(i64, f64, f64)> = data
        .arr()
        .iter()
        // Missing samples (guest stopped) come back without netin/netout.
        .filter(|s| s.get("netin").is_some() || s.get("netout").is_some())
        .map(|s| (s.f("time") as i64, s.f("netin"), s.f("netout")))
        .collect();
    let (received, sent, n, first) = integrate(&samples, since, step);
    Ok(Traffic { received, sent, samples: n, timeframe: tf, covered_from: first })
}

/// Start of the current calendar month (local time), the default billing period.
pub fn month_start() -> i64 {
    use chrono::{Datelike, Local, TimeZone};
    let now = Local::now();
    Local
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .map(|t| t.timestamp())
        .unwrap_or_else(|| now.timestamp() - 30 * 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrates_rate_times_interval() {
        // 100 B/s in, 50 B/s out over two 60s intervals after `since`.
        let s = vec![(0, 999.0, 999.0), (60, 100.0, 50.0), (120, 100.0, 50.0)];
        let (rx, tx, n, first) = integrate(&s, 30, 60);
        assert_eq!(n, 2);
        assert_eq!(first, 60);
        assert_eq!(rx, 12_000.0);
        assert_eq!(tx, 6_000.0);
    }

    #[test]
    fn timeframes() {
        assert_eq!(timeframe_for(1000, 1000 + 3000).0, "hour");
        assert_eq!(timeframe_for(0, 3 * 86_400).0, "week");
        assert_eq!(timeframe_for(0, 20 * 86_400).0, "month");
        assert_eq!(timeframe_for(0, 200 * 86_400).0, "year");
    }
}
