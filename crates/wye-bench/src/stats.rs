//! Percentiles, medians, and metric bookkeeping.

use std::{collections::BTreeMap, fmt::Display, future::Future, time::Instant};

pub(crate) const PERCENTILES: [(&str, f64); 2] = [("p50", 50.0), ("p99", 99.0)];

pub(crate) fn put(metrics: &mut BTreeMap<String, f64>, prefix: &str, name: &str, value: f64) {
    metrics.insert(format!("{prefix}{name}"), (value * 100.0).round() / 100.0);
}

pub(crate) fn median(runs: &[BTreeMap<String, f64>]) -> BTreeMap<String, f64> {
    runs[0]
        .keys()
        .map(|name| {
            let mut values: Vec<f64> = runs.iter().map(|r| r[name]).collect();
            values.sort_by(f64::total_cmp);
            (name.clone(), values[values.len() / 2])
        })
        .collect()
}

/// `NaN` (written as `null`) when a gateway produced no samples, e.g. every request failed.
pub(crate) fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((p / 100.0) * (sorted.len() - 1) as f64).round() as usize] as f64
}

pub(crate) fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

/// Runs `work` and reports its duration on stderr, to show where benchmark time goes.
pub(crate) async fn phase<T>(name: impl Display, work: impl Future<Output = T>) -> T {
    let start = Instant::now();
    let out = work.await;
    eprintln!("  phase {name}: {:.1}s", start.elapsed().as_secs_f64());
    out
}
