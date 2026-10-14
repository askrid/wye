//! OTLP/JSON encoding of usage records: attributes, log records, and spans.

use std::io::Write;

use ring::rand::{SecureRandom, SystemRandom};

use super::super::Line;

#[derive(Clone, Copy)]
pub(in crate::usage_log) enum Value<'a> {
    Str(&'a str),
    Int(u64),
    Double(f64),
}

/// One OTLP `KeyValue`. Strings are escaped by serde_json; 64-bit integers
/// are strings in the protobuf JSON mapping.
pub(in crate::usage_log) fn write_attribute(buf: &mut Vec<u8>, key: &str, value: Value<'_>) {
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

/// One usage record as an OTLP `LogRecord`.
pub(super) fn write_log_record(buf: &mut Vec<u8>, line: &Line<'_>) {
    let _ = write!(
        buf,
        r#"{{"timeUnixNano":"{}""#,
        line.unix_ms.saturating_mul(1_000_000)
    );
    buf.extend_from_slice(br#","severityNumber":9,"severityText":"INFO","body":{"stringValue":"#);
    let _ = serde_json::to_writer(&mut *buf, line.event);
    buf.extend_from_slice(br#"},"attributes":["#);
    write_record_attributes(buf, line);
    buf.extend_from_slice(b"]}");
}

/// The attributes a usage record shares between its log record and span.
fn write_record_attributes(buf: &mut Vec<u8>, line: &Line<'_>) {
    let mut first = true;
    let mut attribute = |key: &str, value: Value<'_>| {
        if !std::mem::take(&mut first) {
            buf.push(b',');
        }
        write_attribute(buf, key, value);
    };
    attribute("gen_ai.system", Value::Str("anthropic"));
    attribute("http.route", Value::Str(line.path));
    attribute(
        "http.response.status_code",
        Value::Int(u64::from(line.status)),
    );
    attribute("wye.outcome", Value::Str(line.outcome));
    attribute("wye.duration_ms", Value::Double(line.duration_ms));
    if let Some(ms) = line.headers_ms {
        attribute("wye.headers_ms", Value::Double(ms));
    }
    if let Some(subject) = line.subject {
        attribute("enduser.id", Value::Str(subject));
    }
    if let Some(session) = line.session_id {
        attribute("session.id", Value::Str(session));
    }
    if let Some(model) = line.model {
        attribute("gen_ai.response.model", Value::Str(model));
    }
    attribute("wye.usage_status", Value::Str(line.usage_status));
    let tokens = [
        ("gen_ai.usage.input_tokens", line.input_tokens),
        ("gen_ai.usage.output_tokens", line.output_tokens),
        (
            "gen_ai.usage.cache_creation.input_tokens",
            line.cache_creation_input_tokens,
        ),
        (
            "gen_ai.usage.cache_read.input_tokens",
            line.cache_read_input_tokens,
        ),
    ];
    for (key, count) in tokens {
        if let Some(count) = count {
            attribute(key, Value::Int(count));
        }
    }
    if let Some(prompt) = &line.prompt {
        // Nested and variable-length: one JSON string the backend can parse.
        let json = serde_json::to_string(prompt).unwrap_or_default();
        attribute("wye.prompt", Value::Str(&json));
    }
}

/// One usage record as an OTLP server `Span`: child of the request's W3C
/// `traceparent` when it has a valid one, else the root of a new trace.
pub(super) fn write_span(buf: &mut Vec<u8>, line: &Line<'_>, random: &SystemRandom) {
    let mut ids = [0u8; 24];
    // A failed draw leaves zeros, which collectors reject: the span is lost,
    // never the request.
    let _ = random.fill(&mut ids);
    let (trace_id, parent) = match line.traceparent.and_then(parse_traceparent) {
        Some((trace, parent)) => (trace, Some(parent)),
        None => (hex(ids.get(..16).unwrap_or_default()), None),
    };
    let span_id = hex(ids.get(16..).unwrap_or_default());
    let end_ns = line.unix_ms.saturating_mul(1_000_000);
    let start_ns = end_ns.saturating_sub((line.duration_ms * 1_000_000.0) as u64);
    let _ = write!(buf, r#"{{"traceId":"{trace_id}","spanId":"{span_id}","#);
    if let Some(parent) = parent {
        let _ = write!(buf, r#""parentSpanId":"{parent}","#);
    }
    let _ = write!(buf, r#""name":"POST "#);
    let route = serde_json::to_string(line.path).unwrap_or_default();
    buf.extend_from_slice(route.trim_matches('"').as_bytes());
    let _ = write!(
        buf,
        r#"","kind":2,"startTimeUnixNano":"{start_ns}","endTimeUnixNano":"{end_ns}","attributes":["#
    );
    write_record_attributes(buf, line);
    buf.extend_from_slice(br#"],"events":["#);
    if let Some(ms) = line.headers_ms {
        let at = start_ns.saturating_add((ms * 1_000_000.0) as u64);
        let _ = write!(
            buf,
            r#"{{"timeUnixNano":"{at}","name":"response headers"}}"#
        );
    }
    let failed = matches!(line.outcome, "failed" | "gateway_error") || line.status >= 500;
    // STATUS_CODE_ERROR = 2, STATUS_CODE_UNSET = 0.
    let _ = write!(
        buf,
        r#"],"status":{{"code":{}}}}}"#,
        if failed { 2 } else { 0 }
    );
}

/// The trace id and parent span id of a valid version-00 `traceparent`.
fn parse_traceparent(value: &str) -> Option<(String, String)> {
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
    valid.then(|| (trace.to_owned(), parent.to_owned()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_parses_only_valid_version_00() {
        let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(
            parse_traceparent(valid),
            Some((
                "4bf92f3577b34da6a3ce929d0e0e4736".into(),
                "00f067aa0ba902b7".into()
            ))
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
