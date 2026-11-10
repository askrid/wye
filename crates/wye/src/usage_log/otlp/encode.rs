//! OTLP/JSON encoding of usage records: attributes, log records, and spans.

use std::{fmt, io::Write};

use ring::rand::{SecureRandom, SystemRandom};

use gateway_core::pricing::Usd;

use super::{
    super::{Line, Outcome},
    attr,
};

#[derive(Clone, Copy)]
pub(in crate::usage_log) enum Value<'a> {
    Str(&'a str),
    Int(u64),
    Double(f64),
}

/// The attributes that are present, comma-separated.
pub(in crate::usage_log) fn write_attributes(
    buf: &mut Vec<u8>,
    attributes: &[(&str, Option<Value<'_>>)],
) {
    let present = attributes
        .iter()
        .filter_map(|(key, value)| Some((key, (*value)?)));
    for (i, (key, value)) in present.enumerate() {
        if i > 0 {
            buf.push(b',');
        }
        write_attribute(buf, key, value);
    }
}

/// One OTLP `KeyValue`. Strings are escaped by serde_json; 64-bit integers
/// are strings in the protobuf JSON mapping.
fn write_attribute(buf: &mut Vec<u8>, key: &str, value: Value<'_>) {
    buf.extend_from_slice(br#"{"key":"#);
    let _ = serde_json::to_writer(&mut *buf, key);
    buf.extend_from_slice(br#","value":{"#);
    match value {
        Value::Str(s) => {
            buf.extend_from_slice(br#""stringValue":"#);
            let _ = serde_json::to_writer(&mut *buf, s);
        }
        Value::Int(n) => {
            let _ = write!(buf, r#""intValue":"{n}""#);
        }
        Value::Double(x) => {
            buf.extend_from_slice(br#""doubleValue":"#);
            let _ = serde_json::to_writer(&mut *buf, &x);
        }
    }
    buf.extend_from_slice(b"}}");
}

/// One usage record as an OTLP `LogRecord`, with its `attributes` already
/// encoded ([`write_record_attributes`]).
pub(super) fn write_log_record(buf: &mut Vec<u8>, line: &Line<'_>, attributes: &[u8]) {
    let _ = write!(
        buf,
        r#"{{"timeUnixNano":"{}""#,
        line.unix_ms.saturating_mul(1_000_000)
    );
    buf.extend_from_slice(br#","severityNumber":9,"severityText":"INFO","body":{"stringValue":"#);
    let _ = serde_json::to_writer(&mut *buf, line.event);
    buf.extend_from_slice(br#"},"attributes":["#);
    buf.extend_from_slice(attributes);
    buf.extend_from_slice(b"]}");
}

/// The attributes a usage record shares between its log record and span,
/// encoded once for both.
pub(super) fn write_record_attributes(buf: &mut Vec<u8>, line: &Line<'_>) {
    fn json(value: &impl serde::Serialize) -> String {
        serde_json::to_string(value).unwrap_or_default()
    }
    // A string keeps the exact decimal.
    let cost = line.cost.map(|cost| Usd(cost).to_string());
    let tool_calls = line.tool_calls.as_ref().map(json);
    let upgrade_issues = line.upgrade_issues.as_ref().map(json);
    let failovers = (!line.failovers.is_empty()).then(|| json(&line.failovers));
    // Nested and variable-length: one JSON string the backend can parse.
    let prompt_hashes = line.prompt_hashes.as_ref().map(json);
    write_attributes(
        buf,
        &[
            ("gen_ai.provider.name", Some(Value::Str(line.provider_name))),
            (attr::ROUTE, Some(Value::Str(line.endpoint))),
            (attr::STATUS, Some(Value::Int(u64::from(line.status)))),
            (attr::OUTCOME, Some(Value::Str(line.outcome.as_str()))),
            ("wye.upstream_error", line.upstream_error.map(Value::Str)),
            ("wye.duration_ms", Some(Value::Double(line.duration_ms))),
            (
                "wye.response_headers_ms",
                line.response_headers_ms.map(Value::Double),
            ),
            (attr::UPSTREAM, line.upstream.map(Value::Str)),
            (attr::SUBJECT, line.subject.map(Value::Str)),
            ("session.id", line.session_id.map(Value::Str)),
            (attr::REQUEST_CLASS, line.request_class.map(Value::Str)),
            ("gen_ai.agent.name", line.agent_type.map(Value::Str)),
            ("gen_ai.agent.id", line.agent_id.map(Value::Str)),
            ("wye.parent_agent_id", line.parent_agent_id.map(Value::Str)),
            ("wye.prompt_id", line.prompt_id.map(Value::Str)),
            ("wye.compaction", line.compaction.map(Value::Str)),
            (
                "wye.after_compaction",
                line.after_compaction.map(Value::Str),
            ),
            ("gen_ai.request.model", line.requested_model.map(Value::Str)),
            (attr::MODEL, line.model.map(Value::Str)),
            ("wye.usage_status", Some(Value::Str(line.usage_status))),
            (
                "gen_ai.usage.input_tokens",
                line.input_tokens.map(Value::Int),
            ),
            (
                "gen_ai.usage.output_tokens",
                line.output_tokens.map(Value::Int),
            ),
            (
                "gen_ai.usage.cache_creation.input_tokens",
                line.cache_creation_input_tokens.map(Value::Int),
            ),
            (
                "gen_ai.usage.cache_read.input_tokens",
                line.cache_read_input_tokens.map(Value::Int),
            ),
            ("wye.cost_usd", cost.as_deref().map(Value::Str)),
            ("wye.cost_status", line.cost_status.map(Value::Str)),
            ("wye.pricing_version", line.pricing_version.map(Value::Str)),
            ("wye.tool_calls", tool_calls.as_deref().map(Value::Str)),
            (
                "wye.upgrade_issues",
                upgrade_issues.as_deref().map(Value::Str),
            ),
            ("wye.failovers", failovers.as_deref().map(Value::Str)),
            (
                attr::CACHE_MISS,
                line.cache_miss.map(|m| Value::Str(m.as_str())),
            ),
            (
                "wye.prompt_hashes",
                prompt_hashes.as_deref().map(Value::Str),
            ),
        ],
    );
}

/// The OpenTelemetry GenAI operation of a tool span, and its name's prefix.
const EXECUTE_TOOL: &str = "execute_tool";

/// One usage record as an OTLP server `Span`, followed by one internal span
/// per tool call it reports in `x-claude-code-prev-tool-durations`.
///
/// The trace: the caller's, when the request has a valid W3C `traceparent`
/// (the span is its child); else the Claude Code session's, a hash of the
/// session id that every replica computes alike, so a backend shows a whole
/// session as one trace with no coordination; else a new random trace.
/// Tool spans are siblings of the request span: they ran before it, ending
/// at the latest when it started.
pub(super) fn write_span(
    buf: &mut Vec<u8>,
    line: &Line<'_>,
    attributes: &[u8],
    random: &SystemRandom,
) {
    let mut ids = [0u8; 24];
    // A failed draw leaves zeros, which collectors reject: the span is lost,
    // never the request.
    let _ = random.fill(&mut ids);
    let (drawn, span_id) = ids.split_at(16);
    let (trace_id, parent) = match line.traceparent.and_then(parse_traceparent) {
        Some((trace, parent)) => (TraceId::Given(trace), Some(parent)),
        None => match line.session_id {
            Some(session) => (TraceId::Derived(session_trace_id(session)), None),
            None => (TraceId::Derived(drawn.try_into().unwrap_or_default()), None),
        },
    };
    let end_ns = line.unix_ms.saturating_mul(1_000_000);
    let start_ns = end_ns.saturating_sub((line.duration_ms * 1_000_000.0) as u64);
    let open_span = |buf: &mut Vec<u8>, span_id: &[u8]| {
        let _ = write!(
            buf,
            r#"{{"traceId":"{trace_id}","spanId":"{}","#,
            Hex(span_id)
        );
        if let Some(parent) = parent {
            let _ = write!(buf, r#""parentSpanId":"{parent}","#);
        }
    };
    open_span(buf, span_id);
    // The endpoint is one of the API's own, so it needs no escaping.
    let _ = write!(
        buf,
        r#""name":"POST {}","kind":2,"startTimeUnixNano":"{start_ns}","endTimeUnixNano":"{end_ns}","attributes":["#,
        line.endpoint
    );
    buf.extend_from_slice(attributes);
    buf.extend_from_slice(br#"],"events":["#);
    if let Some(ms) = line.response_headers_ms {
        let at = start_ns.saturating_add((ms * 1_000_000.0) as u64);
        let _ = write!(
            buf,
            r#"{{"timeUnixNano":"{at}","name":"response headers"}}"#
        );
    }
    // A refusal is the client's to fix, as OpenTelemetry leaves a server
    // span's 4xx unset; Wye's own overload (`529`, `503`) is one too.
    let failed = match line.outcome {
        Outcome::Refused => false,
        Outcome::UpstreamError(_) | Outcome::GatewayError | Outcome::Truncated => true,
        Outcome::Completed | Outcome::Cancelled => line.status >= 500,
    };
    // STATUS_CODE_ERROR = 2, STATUS_CODE_UNSET = 0.
    let _ = write!(
        buf,
        r#"],"status":{{"code":{}}}}}"#,
        if failed { 2 } else { 0 }
    );
    for (name, ms) in line.tool_durations.into_iter().flat_map(|t| t.iter()) {
        let mut id = [0u8; 8];
        let _ = random.fill(&mut id);
        buf.push(b',');
        open_span(buf, &id);
        let tool_start = start_ns.saturating_sub(ms.saturating_mul(1_000_000));
        // SPAN_KIND_INTERNAL = 1.
        buf.extend_from_slice(br#""name":"#);
        let _ = serde_json::to_writer(&mut *buf, &format!("{EXECUTE_TOOL} {name}"));
        let _ = write!(
            buf,
            r#","kind":1,"startTimeUnixNano":"{tool_start}","endTimeUnixNano":"{start_ns}","attributes":["#
        );
        write_attributes(
            buf,
            &[
                ("gen_ai.operation.name", Some(Value::Str(EXECUTE_TOOL))),
                (attr::TOOL, Some(Value::Str(&name))),
                ("wye.tool.duration_ms", Some(Value::Int(ms))),
                ("session.id", line.session_id.map(Value::Str)),
                ("gen_ai.agent.id", line.agent_id.map(Value::Str)),
                ("wye.prompt_id", line.prompt_id.map(Value::Str)),
            ],
        );
        buf.extend_from_slice(br#"],"status":{"code":0}}"#);
    }
}

/// A span's trace id: the caller's, from `traceparent`, or 16 bytes Wye
/// derived from the session or drew.
enum TraceId<'a> {
    Given(&'a str),
    Derived([u8; 16]),
}

impl fmt::Display for TraceId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Given(id) => f.write_str(id),
            Self::Derived(id) => Hex(id).fmt(f),
        }
    }
}

/// Bytes as lowercase hex, written without allocating.
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

/// The trace id of a Claude Code session: the first 16 bytes of
/// SHA-256(`wye.session` NUL session id), so every replica derives the same id.
fn session_trace_id(session: &str) -> [u8; 16] {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(b"wye.session\0");
    ctx.update(session.as_bytes());
    ctx.finish()
        .as_ref()
        .first_chunk()
        .copied()
        .unwrap_or_default()
}

/// The trace id and parent span id of a valid version-00 `traceparent`.
fn parse_traceparent(value: &str) -> Option<(&str, &str)> {
    let mut parts = value.trim().split('-');
    let (version, trace, parent, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let hex_of = |s: &str, len: usize| {
        s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    let valid = version == "00"
        && parts.next().is_none()
        && hex_of(trace, 32)
        && hex_of(parent, 16)
        && hex_of(flags, 2)
        && trace.bytes().any(|b| b != b'0')
        && parent.bytes().any(|b| b != b'0');
    valid.then_some((trace, parent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_parses_only_valid_version_00() {
        let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(
            parse_traceparent(valid),
            Some(("4bf92f3577b34da6a3ce929d0e0e4736", "00f067aa0ba902b7"))
        );
        for bad in [
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-4bf92f-00f067aa0ba902b7-01",
            "garbage",
        ] {
            assert_eq!(parse_traceparent(bad), None, "{bad}");
        }
    }

    fn line<'a>(session: Option<&'a str>, tools: Option<&'a [u8]>) -> Line<'a> {
        Line {
            unix_ms: 10_000,
            duration_ms: 2_000.0,
            session_id: session,
            agent_id: Some("a-1"),
            tool_durations: tools.map(crate::usage_log::agent::ToolDurations),
            ..Line::blank()
        }
    }

    fn spans(line: &Line<'_>) -> Vec<serde_json::Value> {
        let mut attributes = Vec::new();
        write_record_attributes(&mut attributes, line);
        let mut buf = vec![b'['];
        write_span(&mut buf, line, &attributes, &SystemRandom::new());
        buf.push(b']');
        serde_json::from_slice::<serde_json::Value>(&buf)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    #[test]
    fn a_session_is_one_trace_with_tool_spans_before_each_call() {
        let first = spans(&line(Some("s-1"), None));
        let second = spans(&line(Some("s-1"), Some(b"Bash=500;Read=9")));
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 3, "the request span and two tool spans");
        let trace = first[0]["traceId"].as_str().unwrap();
        assert_eq!(trace, Hex(&session_trace_id("s-1")).to_string());
        assert!(second.iter().all(|s| s["traceId"] == trace));
        assert_ne!(first[0]["spanId"], second[0]["spanId"]);

        let bash = &second[1];
        assert_eq!(bash["name"], "execute_tool Bash");
        assert_eq!(bash["kind"], 1);
        assert!(bash.get("parentSpanId").is_none(), "a sibling of the call");
        // The call ran from 8 s to 10 s; Bash ended by 8 s after 500 ms.
        assert_eq!(bash["startTimeUnixNano"], "7500000000");
        assert_eq!(bash["endTimeUnixNano"], "8000000000");
        let attribute = |key: &str| {
            bash["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == key)
                .unwrap_or_else(|| panic!("no {key}"))["value"]
                .clone()
        };
        assert_eq!(
            attribute("gen_ai.operation.name")["stringValue"],
            "execute_tool"
        );
        assert_eq!(attribute("gen_ai.tool.name")["stringValue"], "Bash");
        assert_eq!(attribute("wye.tool.duration_ms")["intValue"], "500");
        assert_eq!(attribute("gen_ai.agent.id")["stringValue"], "a-1");

        let random = spans(&line(None, None));
        assert_ne!(random[0]["traceId"], trace);
    }

    #[test]
    fn a_span_is_an_error_unless_wye_refused_the_request() {
        use crate::usage_log::UpstreamError;
        let status = |outcome, status| {
            let line = Line {
                outcome,
                status,
                ..Line::blank()
            };
            spans(&line)[0]["status"]["code"].as_u64().unwrap()
        };
        assert_eq!(status(Outcome::Completed, 200), 0);
        assert_eq!(status(Outcome::Completed, 429), 0);
        assert_eq!(status(Outcome::Completed, 500), 2);
        assert_eq!(status(Outcome::Refused, 403), 0);
        assert_eq!(status(Outcome::Refused, 529), 0, "Wye's own overload");
        assert_eq!(status(Outcome::Refused, 503), 0, "Wye's own overload");
        assert_eq!(
            status(Outcome::UpstreamError(UpstreamError::Timeout), 504),
            2
        );
        assert_eq!(status(Outcome::GatewayError, 500), 2);
        assert_eq!(status(Outcome::Truncated, 200), 2);
        assert_eq!(status(Outcome::Cancelled, 499), 0);
    }

    #[test]
    fn session_trace_ids_are_stable_and_distinct() {
        let a = session_trace_id("s-1");
        assert_eq!(Hex(&a).to_string().len(), 32);
        assert_eq!(a, session_trace_id("s-1"));
        assert_ne!(a, session_trace_id("s-2"));
    }

    #[test]
    fn attributes_follow_the_protobuf_json_mapping() {
        let mut buf = Vec::new();
        write_attribute(&mut buf, "a", Value::Int(7));
        buf.push(b',');
        write_attribute(&mut buf, "b\"", Value::Str("x\ny"));
        let parsed: serde_json::Value =
            serde_json::from_slice(&[b"[", &buf[..], b"]"].concat()).unwrap();
        assert_eq!(parsed[0]["value"]["intValue"], "7");
        assert_eq!(parsed[1]["key"], "b\"");
        assert_eq!(parsed[1]["value"]["stringValue"], "x\ny");
    }
}
