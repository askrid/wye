//! The cumulative totals as an OTLP `ExportMetricsServiceRequest` (JSON).

use std::io::Write;

use gateway_core::{agent::CacheMiss, pricing::PICO_PER_USD};

use super::{DURATION_BOUNDS_MS, Metrics};
use crate::usage_log::otlp::{Value, attr, write_attributes};

impl Metrics {
    pub(in crate::usage_log) fn encode(
        &self,
        resource: &[u8],
        now_ns: u64,
        active: usize,
        lost: u64,
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4096);
        buf.extend_from_slice(br#"{"resourceMetrics":[{"resource":"#);
        buf.extend_from_slice(resource);
        buf.extend_from_slice(br#","scopeMetrics":[{"scope":{"name":"wye"},"metrics":["#);
        let times = format!(
            r#""startTimeUnixNano":"{}","timeUnixNano":"{now_ns}""#,
            self.start_ns
        );

        sum(
            &mut buf,
            &times,
            "wye.requests",
            "{request}",
            "Proxied requests by route, upstream, status, and outcome",
            &self.requests,
            |s| {
                (
                    Value::Int(s.count),
                    [
                        (attr::ROUTE, Some(Value::Str(&s.key.route))),
                        (attr::UPSTREAM, s.key.upstream.as_deref().map(Value::Str)),
                        (attr::STATUS, Some(Value::Int(u64::from(s.key.status)))),
                        (attr::OUTCOME, Some(Value::Str(s.key.outcome.as_str()))),
                    ],
                )
            },
        );

        buf.extend_from_slice(
            br#"{"name":"wye.request.duration","unit":"ms","description":"Time from request to the end of the response body","histogram":{"aggregationTemporality":2,"dataPoints":["#,
        );
        for (i, s) in self.durations.iter().enumerate() {
            if i > 0 {
                buf.push(b',');
            }
            let _ = write!(buf, r#"{{{times},"count":"{}","sum":"#, s.count);
            let _ = serde_json::to_writer(&mut buf, &s.sum);
            buf.extend_from_slice(br#","bucketCounts":["#);
            for (j, count) in s.buckets.iter().enumerate() {
                if j > 0 {
                    buf.push(b',');
                }
                let _ = write!(buf, r#""{count}""#);
            }
            buf.extend_from_slice(br#"],"explicitBounds":"#);
            let _ = serde_json::to_writer(&mut buf, &DURATION_BOUNDS_MS);
            buf.push(b',');
            close_point(
                &mut buf,
                &[
                    (attr::ROUTE, Some(Value::Str(&s.route))),
                    (attr::OUTCOME, Some(Value::Str(s.outcome.as_str()))),
                ],
            );
        }
        buf.extend_from_slice(b"]}},");

        sum(
            &mut buf,
            &times,
            "wye.tokens",
            "{token}",
            "Tokens the provider reported, by kind, model, request class, and upstream",
            &self.tokens,
            |s| {
                let (kind, key) = &s.key;
                (
                    Value::Int(s.count),
                    [
                        ("gen_ai.token.type", Some(Value::Str(kind))),
                        (attr::MODEL, Some(Value::Str(&key.model))),
                        (attr::REQUEST_CLASS, key.request_class.map(Value::Str)),
                        (attr::UPSTREAM, key.upstream.as_deref().map(Value::Str)),
                    ],
                )
            },
        );

        // Costs are exact integers here; the export is for display, in USD.
        sum(
            &mut buf,
            &times,
            "wye.cost",
            "USD",
            "Priced cost of requests (complete and partial costs), by model, request class, and upstream",
            &self.costs,
            |s| {
                (
                    Value::Double(s.count as f64 / PICO_PER_USD as f64),
                    [
                        (attr::MODEL, Some(Value::Str(&s.key.model))),
                        (attr::REQUEST_CLASS, s.key.request_class.map(Value::Str)),
                        (attr::UPSTREAM, s.key.upstream.as_deref().map(Value::Str)),
                    ],
                )
            },
        );

        sum(
            &mut buf,
            &times,
            "wye.upgrade.issues",
            "{request}",
            "Checked requests a target model would reject, by target, rule, and subject",
            &self.upgrades,
            |s| {
                (
                    Value::Int(s.count),
                    [
                        ("wye.upgrade.target", Some(Value::Str(&s.key.target))),
                        ("wye.upgrade.rule", Some(Value::Str(s.key.rule))),
                        (attr::SUBJECT, s.key.subject.as_deref().map(Value::Str)),
                    ],
                )
            },
        );

        sum(
            &mut buf,
            &times,
            "wye.failovers",
            "{request}",
            "Requests moved to the route's next upstream, by the upstream that refused and why",
            &self.failovers,
            |s| {
                (
                    Value::Int(s.count),
                    [
                        (attr::UPSTREAM, s.key.upstream.as_deref().map(Value::Str)),
                        (
                            "wye.failover.reason",
                            Some(Value::Str(s.key.reason.as_str())),
                        ),
                    ],
                )
            },
        );

        sum(
            &mut buf,
            &times,
            "wye.tool.calls",
            "{call}",
            "Tool calls responses requested, by tool name",
            &self.tool_calls,
            |s| {
                (
                    Value::Int(s.count),
                    [(attr::TOOL, Some(Value::Str(&s.key)))],
                )
            },
        );

        sum(
            &mut buf,
            &times,
            "wye.cache.misses",
            "{request}",
            "Requests that wrote the prompt cache without reading it, by reason",
            CacheMiss::ALL.iter().zip(self.cache_misses),
            |(reason, count)| {
                (
                    Value::Int(count),
                    [(attr::CACHE_MISS, Some(Value::Str(reason.as_str())))],
                )
            },
        );

        sum(
            &mut buf,
            &times,
            "wye.telemetry.lost",
            "{record}",
            "Usage records lost to full queues or failed exports, and records beyond the series limit",
            [lost.saturating_add(self.overflow)],
            |total| (Value::Int(total), []),
        );

        buf.extend_from_slice(
            br#"{"name":"wye.requests.active","unit":"{request}","description":"Proxied requests whose response has not ended","gauge":{"dataPoints":["#,
        );
        let _ = write!(
            buf,
            r#"{{"timeUnixNano":"{now_ns}","asInt":"{active}","attributes":[]}}"#
        );
        buf.extend_from_slice(b"]}}]}]}]}");
        buf
    }
}

/// One monotonic sum metric: a data point per item, which `point` turns into
/// the point's number and attributes.
fn sum<'a, T, const N: usize>(
    buf: &mut Vec<u8>,
    times: &str,
    name: &str,
    unit: &str,
    description: &str,
    items: impl IntoIterator<Item = T>,
    point: impl Fn(T) -> (Value<'a>, [(&'a str, Option<Value<'a>>); N]),
) {
    let _ = write!(
        buf,
        r#"{{"name":"{name}","unit":"{unit}","description":"{description}","sum":{{"aggregationTemporality":2,"isMonotonic":true,"dataPoints":["#
    );
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            buf.push(b',');
        }
        let _ = write!(buf, "{{{times},");
        let (value, attributes) = point(item);
        match value {
            Value::Int(n) => {
                let _ = write!(buf, r#""asInt":"{n}","#);
            }
            Value::Double(x) => {
                buf.extend_from_slice(br#""asDouble":"#);
                let _ = serde_json::to_writer(&mut *buf, &x);
                buf.push(b',');
            }
            // A point's value is a number.
            Value::Str(_) => {}
        }
        close_point(buf, &attributes);
    }
    buf.extend_from_slice(b"]}},");
}

/// Writes the present attributes and closes the data point.
fn close_point(buf: &mut Vec<u8>, attributes: &[(&str, Option<Value<'_>>)]) {
    buf.extend_from_slice(br#""attributes":["#);
    write_attributes(buf, attributes);
    buf.extend_from_slice(b"]}");
}
