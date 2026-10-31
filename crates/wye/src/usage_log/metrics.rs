//! OTLP metrics aggregated from usage records in the writer task.
//!
//! Totals are cumulative since process start, so a lost export loses nothing:
//! the next one carries the totals forward. Each metric keeps at most
//! [`MAX_SERIES`] attribute sets; records beyond that are counted in
//! `wye.telemetry.lost` instead of growing memory. Per-process telemetry is
//! not shared state: replicas never compare or merge these numbers.

use gateway_core::{agent::CacheMiss, route::FailoverReason};

use super::{Line, Outcome};

/// Attribute sets kept per metric.
const MAX_SERIES: usize = 256;
/// Upper bounds of the request duration buckets, in milliseconds. Model
/// requests run from milliseconds (cached errors) to minutes (long streams).
const DURATION_BOUNDS_MS: [f64; 14] = [
    5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 30_000.0,
    60_000.0, 300_000.0,
];

/// One attribute set and its total.
struct Counted<K> {
    key: K,
    count: u64,
}

struct RequestKey {
    route: Box<str>,
    upstream: Option<Box<str>>,
    status: u16,
    outcome: Outcome,
}

struct DurationSeries {
    route: Box<str>,
    outcome: Outcome,
    buckets: [u64; DURATION_BOUNDS_MS.len() + 1],
    count: u64,
    sum: f64,
}

/// What tokens and costs are counted by.
struct ModelKey {
    model: Box<str>,
    upstream: Option<Box<str>>,
    /// The agent's request class: Claude Code's hint header, or Codex's
    /// subagent header.
    request_class: Option<&'static str>,
}

struct UpgradeKey {
    target: Box<str>,
    rule: &'static str,
    subject: Option<Box<str>>,
}

struct FailoverKey {
    /// The upstream that refused.
    upstream: Option<Box<str>>,
    reason: FailoverReason,
}

/// Cumulative totals since `start_ns`.
pub(super) struct Metrics {
    start_ns: u64,
    requests: Vec<Counted<RequestKey>>,
    durations: Vec<DurationSeries>,
    /// By token kind, then model.
    tokens: Vec<Counted<(&'static str, ModelKey)>>,
    /// Priced cost, in pico-USD, exported as USD.
    costs: Vec<Counted<ModelKey>>,
    /// Checked requests a target model would reject.
    upgrades: Vec<Counted<UpgradeKey>>,
    /// Refusals that moved a request to the route's next upstream.
    failovers: Vec<Counted<FailoverKey>>,
    /// Tool calls responses requested, by tool name.
    tool_calls: Vec<Counted<Box<str>>>,
    /// Requests that wrote the prompt cache without reading it, per reason in
    /// [`CacheMiss::ALL`] order.
    cache_misses: [u64; CacheMiss::ALL.len()],
    overflow: u64,
}

impl Metrics {
    pub(super) fn new(start_ns: u64) -> Self {
        Self {
            start_ns,
            requests: Vec::new(),
            durations: Vec::new(),
            tokens: Vec::new(),
            costs: Vec::new(),
            upgrades: Vec::new(),
            failovers: Vec::new(),
            tool_calls: Vec::new(),
            cache_misses: [0; CacheMiss::ALL.len()],
            overflow: 0,
        }
    }

    /// Adds one usage record. Series are few (routes × statuses × outcomes,
    /// models × token kinds), so a linear search beats hashing here.
    pub(super) fn record(&mut self, line: &Line<'_>) {
        let overflow = &mut self.overflow;
        bump(
            &mut self.requests,
            overflow,
            1,
            |k| {
                &*k.route == line.path
                    && k.upstream.as_deref() == line.upstream
                    && k.status == line.status
                    && k.outcome == line.outcome
            },
            || RequestKey {
                route: line.path.into(),
                upstream: line.upstream.map(Into::into),
                status: line.status,
                outcome: line.outcome,
            },
        );
        let bucket = DURATION_BOUNDS_MS
            .iter()
            .position(|&bound| line.duration_ms <= bound)
            .unwrap_or(DURATION_BOUNDS_MS.len());
        let duration = series(
            &mut self.durations,
            overflow,
            |s| &*s.route == line.path && s.outcome == line.outcome,
            || DurationSeries {
                route: line.path.into(),
                outcome: line.outcome,
                buckets: [0; DURATION_BOUNDS_MS.len() + 1],
                count: 0,
                sum: 0.0,
            },
        );
        if let Some(s) = duration {
            if let Some(count) = s.buckets.get_mut(bucket) {
                *count += 1;
            }
            s.count += 1;
            s.sum += line.duration_ms;
        }
        if let Some(count) = line
            .cache_miss
            .and_then(|miss| CacheMiss::ALL.iter().position(|&r| r == miss))
            .and_then(|i| self.cache_misses.get_mut(i))
        {
            *count += 1;
        }
        for (name, count) in line.tool_calls.into_iter().flat_map(|t| t.iter()) {
            bump(
                &mut self.tool_calls,
                overflow,
                u64::from(count),
                |n| &**n == name,
                || name.into(),
            );
        }
        for (target, rule) in line.upgrade_issues.iter().flat_map(|i| i.iter()) {
            bump(
                &mut self.upgrades,
                overflow,
                1,
                |k| &*k.target == target && k.rule == rule && k.subject.as_deref() == line.subject,
                || UpgradeKey {
                    target: target.into(),
                    rule,
                    subject: line.subject.map(Into::into),
                },
            );
        }
        for failover in line.failovers {
            let upstream = failover.upstream.as_deref();
            bump(
                &mut self.failovers,
                overflow,
                1,
                |k| k.upstream.as_deref() == upstream && k.reason == failover.reason,
                || FailoverKey {
                    upstream: upstream.map(Into::into),
                    reason: failover.reason,
                },
            );
        }
        let Some(model) = line.model else { return };
        let same = |k: &ModelKey| {
            &*k.model == model
                && k.request_class == line.request_class
                && k.upstream.as_deref() == line.upstream
        };
        let key = || ModelKey {
            model: model.into(),
            upstream: line.upstream.map(Into::into),
            request_class: line.request_class,
        };
        if let Some(pico) = line.cost.filter(|&c| c > 0) {
            bump(&mut self.costs, overflow, pico, same, key);
        }
        for (kind, count) in [
            ("input", line.input_tokens),
            ("output", line.output_tokens),
            ("cache_read", line.cache_read_input_tokens),
            ("cache_creation", line.cache_creation_input_tokens),
        ] {
            let Some(count) = count.filter(|&c| c > 0) else {
                continue;
            };
            bump(
                &mut self.tokens,
                overflow,
                count,
                |(k, model)| *k == kind && same(model),
                || (kind, key()),
            );
        }
    }
}

/// Adds `by` to the total of the key that `matches`, or of a new one from
/// `new` (see [`series`]).
fn bump<K>(
    list: &mut Vec<Counted<K>>,
    overflow: &mut u64,
    by: u64,
    matches: impl Fn(&K) -> bool,
    new: impl FnOnce() -> K,
) {
    let counted = series(
        list,
        overflow,
        |s| matches(&s.key),
        || Counted {
            key: new(),
            count: 0,
        },
    );
    if let Some(s) = counted {
        s.count = s.count.saturating_add(by);
    }
}

/// The series in `list` that `matches`, or a new one from `new` while fewer
/// than [`MAX_SERIES`] exist; beyond that `None`, counted in `overflow`.
fn series<'s, S>(
    list: &'s mut Vec<S>,
    overflow: &mut u64,
    matches: impl Fn(&S) -> bool,
    new: impl FnOnce() -> S,
) -> Option<&'s mut S> {
    match list.iter().position(matches) {
        Some(i) => list.get_mut(i),
        None if list.len() < MAX_SERIES => {
            list.push(new());
            list.last_mut()
        }
        None => {
            *overflow += 1;
            None
        }
    }
}

mod encode;

#[cfg(test)]
mod tests;
