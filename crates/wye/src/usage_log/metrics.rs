//! OTLP metrics aggregated from usage records in the writer task.
//!
//! Totals are cumulative since process start, so a lost export loses nothing:
//! the next one carries the totals forward. Each metric keeps at most
//! [`MAX_SERIES`] attribute sets; records beyond that are counted in
//! `wye.telemetry.lost` instead of growing memory. Per-process telemetry is
//! not shared state: replicas never compare or merge these numbers.

use gateway_core::agent::CacheMiss;

use super::{Line, Outcome};

/// Attribute sets kept per metric.
const MAX_SERIES: usize = 256;
/// Upper bounds of the request duration buckets, in milliseconds. Model
/// requests run from milliseconds (cached errors) to minutes (long streams).
const DURATION_BOUNDS_MS: [f64; 14] = [
    5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 30_000.0,
    60_000.0, 300_000.0,
];

struct RequestSeries {
    route: Box<str>,
    upstream: Option<Box<str>>,
    status: u16,
    outcome: Outcome,
    count: u64,
}

struct DurationSeries {
    route: Box<str>,
    outcome: Outcome,
    buckets: [u64; DURATION_BOUNDS_MS.len() + 1],
    count: u64,
    sum: f64,
}

struct TokenSeries {
    kind: &'static str,
    model: Box<str>,
    upstream: Option<Box<str>>,
    /// The agent's request class: Claude Code's hint header, or Codex's
    /// subagent header.
    request_class: Option<&'static str>,
    count: u64,
}

/// Priced cost, in pico-USD, exported as USD.
struct CostSeries {
    model: Box<str>,
    upstream: Option<Box<str>>,
    request_class: Option<&'static str>,
    pico: u64,
}

/// Checked requests a target model would reject, by rule and subject.
struct UpgradeSeries {
    target: Box<str>,
    rule: &'static str,
    subject: Option<Box<str>>,
    count: u64,
}

/// Refusals that moved a request to the route's next upstream.
struct FailoverSeries {
    /// The upstream that refused.
    upstream: Option<Box<str>>,
    reason: &'static str,
    count: u64,
}

/// Cumulative totals since `start_ns`.
pub(super) struct Metrics {
    start_ns: u64,
    requests: Vec<RequestSeries>,
    durations: Vec<DurationSeries>,
    tokens: Vec<TokenSeries>,
    costs: Vec<CostSeries>,
    upgrades: Vec<UpgradeSeries>,
    failovers: Vec<FailoverSeries>,
    /// Tool calls responses requested, by tool name.
    tool_calls: Vec<(Box<str>, u64)>,
    /// Requests that wrote the prompt cache without reading it, by reason.
    /// Per reason, in [`CacheMiss::ALL`] order.
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
        let found = self.requests.iter().position(|s| {
            &*s.route == line.path
                && s.upstream.as_deref() == line.upstream
                && s.status == line.status
                && s.outcome == line.outcome
        });
        if let Some(series) = found.and_then(|i| self.requests.get_mut(i)) {
            series.count += 1;
        } else if self.requests.len() < MAX_SERIES {
            self.requests.push(RequestSeries {
                route: line.path.into(),
                upstream: line.upstream.map(Into::into),
                status: line.status,
                outcome: line.outcome,
                count: 1,
            });
        } else {
            self.overflow += 1;
        }
        let bucket = DURATION_BOUNDS_MS
            .iter()
            .position(|&bound| line.duration_ms <= bound)
            .unwrap_or(DURATION_BOUNDS_MS.len());
        let series = match self
            .durations
            .iter()
            .position(|s| &*s.route == line.path && s.outcome == line.outcome)
        {
            Some(i) => self.durations.get_mut(i),
            None if self.durations.len() < MAX_SERIES => {
                self.durations.push(DurationSeries {
                    route: line.path.into(),
                    outcome: line.outcome,
                    buckets: [0; DURATION_BOUNDS_MS.len() + 1],
                    count: 0,
                    sum: 0.0,
                });
                self.durations.last_mut()
            }
            None => None,
        };
        match series {
            Some(series) => {
                if let Some(count) = series.buckets.get_mut(bucket) {
                    *count += 1;
                }
                series.count += 1;
                series.sum += line.duration_ms;
            }
            None => self.overflow += 1,
        }
        if let Some(count) = line
            .cache_miss
            .and_then(|miss| CacheMiss::ALL.iter().position(|&r| r == miss))
            .and_then(|i| self.cache_misses.get_mut(i))
        {
            *count += 1;
        }
        for (name, count) in line.tool_calls.into_iter().flat_map(|t| t.iter()) {
            let count = u64::from(count);
            if let Some(series) = self.tool_calls.iter_mut().find(|(n, _)| &**n == name) {
                series.1 = series.1.saturating_add(count);
            } else if self.tool_calls.len() < MAX_SERIES {
                self.tool_calls.push((name.into(), count));
            } else {
                self.overflow += 1;
            }
        }
        for (target, rule) in line.upgrade_issues.iter().flat_map(|i| i.iter()) {
            let found = self.upgrades.iter().position(|s| {
                &*s.target == target && s.rule == rule && s.subject.as_deref() == line.subject
            });
            if let Some(series) = found.and_then(|i| self.upgrades.get_mut(i)) {
                series.count += 1;
            } else if self.upgrades.len() < MAX_SERIES {
                self.upgrades.push(UpgradeSeries {
                    target: target.into(),
                    rule,
                    subject: line.subject.map(Into::into),
                    count: 1,
                });
            } else {
                self.overflow += 1;
            }
        }
        for failover in line.failovers {
            let reason = match failover.status {
                Some(429) => "429",
                Some(529) => "529",
                Some(_) => "other",
                None => "connect",
            };
            let upstream = failover.upstream.as_deref();
            let found = self
                .failovers
                .iter()
                .position(|s| s.upstream.as_deref() == upstream && s.reason == reason);
            if let Some(series) = found.and_then(|i| self.failovers.get_mut(i)) {
                series.count += 1;
            } else if self.failovers.len() < MAX_SERIES {
                self.failovers.push(FailoverSeries {
                    upstream: upstream.map(Into::into),
                    reason,
                    count: 1,
                });
            } else {
                self.overflow += 1;
            }
        }
        let Some(model) = line.model else { return };
        if let Some(pico) = line.cost.filter(|&c| c > 0) {
            let found = self.costs.iter().position(|s| {
                &*s.model == model
                    && s.request_class == line.request_class
                    && s.upstream.as_deref() == line.upstream
            });
            if let Some(series) = found.and_then(|i| self.costs.get_mut(i)) {
                series.pico = series.pico.saturating_add(pico);
            } else if self.costs.len() < MAX_SERIES {
                self.costs.push(CostSeries {
                    model: model.into(),
                    upstream: line.upstream.map(Into::into),
                    request_class: line.request_class,
                    pico,
                });
            } else {
                self.overflow += 1;
            }
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
            let found = self.tokens.iter().position(|s| {
                s.kind == kind
                    && &*s.model == model
                    && s.request_class == line.request_class
                    && s.upstream.as_deref() == line.upstream
            });
            if let Some(series) = found.and_then(|i| self.tokens.get_mut(i)) {
                series.count = series.count.saturating_add(count);
            } else if self.tokens.len() < MAX_SERIES {
                self.tokens.push(TokenSeries {
                    kind,
                    model: model.into(),
                    upstream: line.upstream.map(Into::into),
                    request_class: line.request_class,
                    count,
                });
            } else {
                self.overflow += 1;
            }
        }
    }
}

mod encode;

#[cfg(test)]
mod tests;
