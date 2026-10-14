//! OTLP metrics aggregated from usage records in the writer task.
//!
//! Totals are cumulative since process start, so a lost export loses nothing:
//! the next one carries the totals forward. Each metric keeps at most
//! [`MAX_SERIES`] attribute sets; records beyond that are counted in
//! `wye.metrics.overflow` instead of growing memory. Per-process telemetry is
//! not shared state: replicas never compare or merge these numbers.

use std::io::Write;

use super::{Line, otlp::Value, otlp::write_attribute};

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
    status: u16,
    outcome: &'static str,
    count: u64,
}

struct DurationSeries {
    route: Box<str>,
    outcome: &'static str,
    buckets: [u64; DURATION_BOUNDS_MS.len() + 1],
    count: u64,
    sum: f64,
}

struct TokenSeries {
    kind: &'static str,
    model: Box<str>,
    count: u64,
}

/// Cumulative totals since `start_ns`.
pub(super) struct Metrics {
    start_ns: u64,
    requests: Vec<RequestSeries>,
    durations: Vec<DurationSeries>,
    tokens: Vec<TokenSeries>,
    overflow: u64,
}

impl Metrics {
    pub(super) fn new(start_ns: u64) -> Self {
        Self {
            start_ns,
            requests: Vec::new(),
            durations: Vec::new(),
            tokens: Vec::new(),
            overflow: 0,
        }
    }

    /// Adds one usage record. Series are few (routes × statuses × outcomes,
    /// models × token kinds), so a linear search beats hashing here.
    pub(super) fn record(&mut self, line: &Line<'_>) {
        let found = self.requests.iter().position(|s| {
            &*s.route == line.path && s.status == line.status && s.outcome == line.outcome
        });
        if let Some(series) = found.and_then(|i| self.requests.get_mut(i)) {
            series.count += 1;
        } else if self.requests.len() < MAX_SERIES {
            self.requests.push(RequestSeries {
                route: line.path.into(),
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
        let Some(model) = line.model else { return };
        for (kind, count) in [
            ("input", line.input_tokens),
            ("output", line.output_tokens),
            ("cache_read", line.cache_read_input_tokens),
            ("cache_creation", line.cache_creation_input_tokens),
        ] {
            let Some(count) = count.filter(|&c| c > 0) else {
                continue;
            };
            let found = self
                .tokens
                .iter()
                .position(|s| s.kind == kind && &*s.model == model);
            if let Some(series) = found.and_then(|i| self.tokens.get_mut(i)) {
                series.count = series.count.saturating_add(count);
            } else if self.tokens.len() < MAX_SERIES {
                self.tokens.push(TokenSeries {
                    kind,
                    model: model.into(),
                    count,
                });
            } else {
                self.overflow += 1;
            }
        }
    }

    /// The totals as an OTLP `ExportMetricsServiceRequest` (JSON) at `now_ns`.
    pub(super) fn encode(&self, resource: &[u8], now_ns: u64, active: usize, lost: u64) -> Vec<u8> {
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
                    ("http.route", Value::Str(&s.route)),
                    ("http.response.status_code", Value::Int(u64::from(s.status))),
                    ("wye.outcome", Value::Str(s.outcome)),
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
                    ("http.route", Value::Str(&s.route)),
                    ("wye.outcome", Value::Str(s.outcome)),
                ],
            );
        }
        buf.extend_from_slice(b"]}},");

        sum_header(
            &mut buf,
            "wye.tokens",
            "{token}",
            "Tokens the provider reported, by kind and model",
        );
        for (i, s) in self.tokens.iter().enumerate() {
            point_start(&mut buf, i, &times, s.count);
            attributes(
                &mut buf,
                &[
                    ("gen_ai.token.type", Value::Str(s.kind)),
                    ("gen_ai.response.model", Value::Str(&s.model)),
                ],
            );
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

fn attributes(buf: &mut Vec<u8>, list: &[(&str, Value<'_>)]) {
    buf.extend_from_slice(br#""attributes":["#);
    for (i, (key, value)) in list.iter().enumerate() {
        if i > 0 {
            buf.push(b',');
        }
        write_attribute(buf, key, *value);
    }
    buf.extend_from_slice(b"]}");
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn line(
        path: &'static str,
        status: u16,
        ms: f64,
        model: Option<&'static str>,
    ) -> Line<'static> {
        Line {
            event: "request",
            unix_ms: 0,
            path,
            status,
            outcome: "completed",
            duration_ms: ms,
            headers_ms: None,
            subject: None,
            session_id: None,
            traceparent: None,
            model,
            usage_status: "complete",
            input_tokens: Some(10),
            output_tokens: Some(5),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(0),
            prompt: None,
        }
    }

    #[test]
    fn totals_are_cumulative_and_encode_as_otlp_json() {
        let mut metrics = Metrics::new(1);
        metrics.record(&line("/v1/messages", 200, 3.0, Some("m")));
        metrics.record(&line("/v1/messages", 200, 700.0, Some("m")));
        metrics.record(&line("/v1/messages", 529, 1.0, None));
        let json: serde_json::Value =
            serde_json::from_slice(&metrics.encode(br#"{"attributes":[]}"#, 9, 4, 2)).unwrap();
        let list = json["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .clone();
        let metric = |name: &str| list.iter().find(|m| m["name"] == name).unwrap().clone();

        let requests = metric("wye.requests")["sum"]["dataPoints"].clone();
        assert_eq!(requests.as_array().unwrap().len(), 2);
        assert_eq!(requests[0]["asInt"], "2");
        assert_eq!(requests[0]["startTimeUnixNano"], "1");
        assert_eq!(requests[1]["attributes"][1]["value"]["intValue"], "529");

        let duration = metric("wye.request.duration")["histogram"]["dataPoints"][0].clone();
        assert_eq!(duration["count"], "3");
        assert_eq!(duration["sum"], 704.0);
        assert_eq!(
            duration["bucketCounts"][0], "2",
            "1 and 3 ms fall in the first bucket"
        );
        assert_eq!(
            duration["bucketCounts"][7], "1",
            "700 ms falls in the 1 s bucket"
        );

        let tokens = metric("wye.tokens")["sum"]["dataPoints"].clone();
        // Zero counts are skipped: input and output only.
        assert_eq!(tokens.as_array().unwrap().len(), 2);
        assert_eq!(tokens[0]["asInt"], "20");
        assert_eq!(
            metric("wye.telemetry.lost")["sum"]["dataPoints"][0]["asInt"],
            "2"
        );
        assert_eq!(
            metric("wye.requests.active")["gauge"]["dataPoints"][0]["asInt"],
            "4"
        );
    }

    #[test]
    fn series_beyond_the_limit_are_counted_not_kept() {
        let mut metrics = Metrics::new(0);
        let routes: Vec<&'static str> = (0..MAX_SERIES + 5)
            .map(|i| &*Box::leak(format!("/r{i}").into_boxed_str()))
            .collect();
        for route in &routes {
            metrics.record(&line(route, 200, 1.0, None));
        }
        assert_eq!(metrics.requests.len(), MAX_SERIES);
        assert_eq!(metrics.overflow, 10, "5 request and 5 duration series");
    }
}
