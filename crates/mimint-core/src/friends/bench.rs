//! Performance budgets B1–B8 (Phase 7, §10.2): the measurement
//! harness. Every budget is measured for real — a number that
//! was not measured is recorded as unmeasured, never filled
//! in. Over budget is recorded with the real number + cause;
//! the budget itself is never edited.
//!
//! * B1 handshake p95 < 500 ms, 20 runs.
//! * B2 ceremony machine part < 2 s, both flows (excludes the
//!   time the user spends looking at the SAS).
//! * B3 sync of 100 records < 2 s.
//! * B4 one 64 KiB record < 500 ms.
//! * B5 one message < 5 ms, 1000 messages.
//! * B6 hub start < 1 s.
//! * B7 RSS increase < 50 MiB.
//! * B8 idle CPU ~0% over 60 s.

use crate::error::MimintError;
use std::time::{Duration, Instant};

/// The budgets (§10.2). Never edited to fit a measurement.
pub const B1_HANDSHAKE_P95_MS: f64 = 500.0;
pub const B2_CEREMONY_MS: f64 = 2000.0;
pub const B3_SYNC_100_MS: f64 = 2000.0;
pub const B4_ONE_RECORD_64K_MS: f64 = 500.0;
pub const B5_ONE_MESSAGE_MS: f64 = 5.0;
pub const B6_HUB_START_MS: f64 = 1000.0;
pub const B7_RSS_MIB: f64 = 50.0;
pub const B8_IDLE_CPU_PCT: f64 = 1.0;

/// One measured budget: the number, the budget, and whether
/// it passed.
#[derive(Debug, Clone)]
pub struct BudgetMeasurement {
    pub id: &'static str,
    pub value: f64,
    pub unit: &'static str,
    pub budget: f64,
    pub passed: bool,
    pub method: String,
}

/// p95 of a sample (nearest-rank).
pub fn p95(mut samples: Vec<f64>) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = ((samples.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    samples[index.min(samples.len() - 1)]
}

/// Mean of a sample.
pub fn mean(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().sum::<f64>() / samples.len() as f64
}

/// Time one closure (once). Returns milliseconds.
pub fn time_once<F>(run: F) -> Duration
where
    F: FnOnce(),
{
    let start = Instant::now();
    run();
    start.elapsed()
}

/// RSS of the current process, in MiB, from /proc/self/status.
pub fn rss_mib() -> Result<f64, MimintError> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|err| MimintError::Io(format!("cannot read /proc/self/status: {err}")))?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            return Ok(kb / 1024.0);
        }
    }
    Err(MimintError::Config(
        "VmRSS not found in /proc/self/status".to_string(),
    ))
}

/// Idle CPU of the current process over `window`: (utime +
/// stime) delta / wall time, in percent. Reads
/// /proc/self/stat (fields 14 + 15, in clock ticks).
pub fn idle_cpu_percent(window: Duration) -> Result<f64, MimintError> {
    let ticks_per_second = 100.0; // Linux USER_HZ on x86_64.
    let read_cpu_seconds = || -> Result<f64, MimintError> {
        let stat = std::fs::read_to_string("/proc/self/stat")
            .map_err(|err| MimintError::Io(format!("cannot read /proc/self/stat: {err}")))?;
        let after_paren = stat.rsplit(')').next().unwrap_or("");
        let fields: Vec<&str> = after_paren.split_whitespace().collect();
        // After "(comm)", field 3 is state; utime is field
        // 14, stime field 15 → indices 11 and 12 here.
        let utime: f64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let stime: f64 = fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        Ok((utime + stime) / ticks_per_second)
    };
    let before = read_cpu_seconds()?;
    std::thread::sleep(window);
    let after = read_cpu_seconds()?;
    let wall = window.as_secs_f64();
    if wall <= 0.0 {
        return Ok(0.0);
    }
    Ok((after - before) / wall * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p95_and_mean_are_correct() {
        let samples: Vec<f64> = (1..=100).map(|v| v as f64).collect();
        assert_eq!(p95(samples.clone()), 95.0);
        assert_eq!(mean(&samples), 50.5);
        assert_eq!(p95(vec![]), 0.0);
        assert_eq!(p95(vec![7.0]), 7.0);
    }

    #[test]
    fn rss_reads_from_proc() {
        // Must be a plausible number on Linux.
        let rss = rss_mib().unwrap();
        assert!(rss > 1.0 && rss < 10_000.0, "rss {rss}");
    }
}
