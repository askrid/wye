//! The cumulative totals as an OTLP `ExportMetricsServiceRequest` (JSON).

use std::io::Write;

use super::{CACHE_MISS_REASONS, DURATION_BOUNDS_MS, Metrics};
use crate::usage_log::otlp::{Value, write_attribute};

impl Metrics {
    /// The totals as an OTLP `ExportMetricsServiceRequest` (JSON) at `now_ns`.
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

        sum_header(
            &mut buf,
            "wye.requests",
            "{request}",
            "Proxied requests by outcome",
        );
        for (i, s) in self.requests.iter().enumerate() {
            point_start(&mut buf, i, &times, s.count);
            attributes(
                &mut buf,
                &[
                    ("http.route", Some(Value::Str(&s.route))),
                    ("wye.upstream", s.upstream.as_deref().map(Value::Str)),
                    (
                        "http.response.status_code",
                        Some(Value::Int(u64::from(s.status))),
                    ),
                    ("wye.outcome", Some(Value::Str(s.outcome))),
                ],
            );
        }
        buf.extend_from_slice(b"]}},");

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
            attributes(
                &mut buf,
                &[
                    ("http.route", Some(Value::Str(&s.route))),
                    ("wye.outcome", Some(Value::Str(s.outcome))),
                ],
            );
        }
        buf.extend_from_slice(b"]}},");

        sum_header(
            &mut buf,
            "wye.tokens",
            "{token}",
            "Tokens the provider reported, by kind, model, and Claude Code request class",
        );
        for (i, s) in self.tokens.iter().enumerate() {
            point_start(&mut buf, i, &times, s.count);
            attributes(
                &mut buf,
                &[
                    ("gen_ai.token.type", Some(Value::Str(s.kind))),
                    ("gen_ai.response.model", Some(Value::Str(&s.model))),
                    ("wye.request_class", s.request_class.map(Value::Str)),
                    ("wye.upstream", s.upstream.as_deref().map(Value::Str)),
                ],
            );
        }
        buf.extend_from_slice(b"]}},");

        // Costs are exact integers here; the export is for display, in USD.
        sum_header(
            &mut buf,
            "wye.cost",
            "USD",
            "Priced cost of requests (complete and partial costs), by model and Claude Code request class",
        );
        for (i, s) in self.costs.iter().enumerate() {
            if i > 0 {
                buf.push(b',');
            }
            let _ = write!(buf, r#"{{{times},"asDouble":"#);
            let _ = serde_json::to_writer(&mut buf, &(s.pico as f64 / 1e12));
            buf.push(b',');
            attributes(
                &mut buf,
                &[
                    ("gen_ai.response.model", Some(Value::Str(&s.model))),
                    ("wye.request_class", s.request_class.map(Value::Str)),
                    ("wye.upstream", s.upstream.as_deref().map(Value::Str)),
                ],
            );
        }
        buf.extend_from_slice(b"]}},");

        sum_header(
            &mut buf,
            "wye.upgrade.issues",
            "{request}",
            "Checked requests a target model would reject, by target, rule, and subject",
        );
        for (i, s) in self.upgrades.iter().enumerate() {
            point_start(&mut buf, i, &times, s.count);
            attributes(
                &mut buf,
                &[
                    ("wye.upgrade.target", Some(Value::Str(&s.target))),
                    ("wye.upgrade.rule", Some(Value::Str(s.rule))),
                    ("enduser.id", s.subject.as_deref().map(Value::Str)),
                ],
            );
        }
        buf.extend_from_slice(b"]}},");

        sum_header(
            &mut buf,
            "wye.failovers",
            "{request}",
            "Requests moved to the route's next upstream, by the upstream that refused and why",
        );
        for (i, s) in self.failovers.iter().enumerate() {
            point_start(&mut buf, i, &times, s.count);
            attributes(
                &mut buf,
                &[
                    ("wye.upstream", s.upstream.as_deref().map(Value::Str)),
                    ("wye.failover.reason", Some(Value::Str(s.reason))),
                ],
            );
        }
        buf.extend_from_slice(b"]}},");

        sum_header(
            &mut buf,
            "wye.tool.calls",
            "{call}",
            "Tool calls responses requested, by tool name",
        );
        for (i, (name, count)) in self.tool_calls.iter().enumerate() {
            point_start(&mut buf, i, &times, *count);
            attributes(&mut buf, &[("gen_ai.tool.name", Some(Value::Str(name)))]);
        }
        buf.extend_from_slice(b"]}},");

        sum_header(
            &mut buf,
            "wye.cache.misses",
            "{request}",
            "Requests that wrote the prompt cache without reading it, by reason",
        );
        for (i, (reason, count)) in CACHE_MISS_REASONS.iter().zip(self.cache_misses).enumerate() {
            point_start(&mut buf, i, &times, count);
            attributes(&mut buf, &[("wye.cache_miss", Some(Value::Str(reason)))]);
        }
        buf.extend_from_slice(b"]}},");

        sum_header(
            &mut buf,
            "wye.telemetry.lost",
            "{record}",
            "Usage records lost to full queues or failed exports, and records beyond the series limit",
        );
        point_start(&mut buf, 0, &times, lost.saturating_add(self.overflow));
        attributes(&mut buf, &[]);
        buf.extend_from_slice(b"]}},");

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

fn sum_header(buf: &mut Vec<u8>, name: &str, unit: &str, description: &str) {
    let _ = write!(
        buf,
        r#"{{"name":"{name}","unit":"{unit}","description":"{description}","sum":{{"aggregationTemporality":2,"isMonotonic":true,"dataPoints":["#
    );
}

/// Opens a sum data point; [`attributes`] closes it.
fn point_start(buf: &mut Vec<u8>, index: usize, times: &str, value: u64) {
    if index > 0 {
        buf.push(b',');
    }
    let _ = write!(buf, r#"{{{times},"asInt":"{value}","#);
}

/// Writes the present attributes and closes the data point.
fn attributes(buf: &mut Vec<u8>, list: &[(&str, Option<Value<'_>>)]) {
    buf.extend_from_slice(br#""attributes":["#);
    let present = list
        .iter()
        .filter_map(|(key, value)| value.map(|v| (key, v)));
    for (i, (key, value)) in present.enumerate() {
        if i > 0 {
            buf.push(b',');
        }
        write_attribute(buf, key, value);
    }
    buf.extend_from_slice(b"]}");
}
