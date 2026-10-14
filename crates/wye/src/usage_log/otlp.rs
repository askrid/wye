//! OTLP export of usage records: OTLP/HTTP with JSON encoding, logs signal.
//!
//! The writer task appends each record to a batch as an OTLP log record and
//! hands full (or one-second-old) batches to an exporter task through a
//! bounded queue. A full queue or a failed export drops the batch and counts
//! its records; nothing here can slow a request. Attribute names follow the
//! OpenTelemetry semantic conventions where one exists (`gen_ai.*`,
//! `http.*`, `session.id`, `enduser.id`), and use a `wye.` prefix otherwise.

use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Uri, header::CONTENT_TYPE};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::{sync::mpsc, task::JoinHandle};

use super::Line;

/// Records per batch before it is sent without waiting for the timer.
pub(super) const MAX_BATCH_RECORDS: usize = 512;
/// Batches waiting for the exporter before new ones are dropped.
const EXPORT_QUEUE: usize = 8;
/// One export attempt; a slower collector loses the batch.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

type ExportClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Where and how usage records are exported, resolved at startup.
pub(crate) struct OtlpTarget {
    logs_uri: Uri,
    metrics_uri: Uri,
    traces_uri: Uri,
    /// Extra headers, sensitive values marked as such.
    headers: HeaderMap,
    client: ExportClient,
    /// The `Resource` object shared by both signals.
    pub(super) resource: Box<[u8]>,
    /// The log batch envelope up to the first log record.
    logs_prefix: Box<[u8]>,
    /// The span batch envelope up to the first span.
    spans_prefix: Box<[u8]>,
}

impl OtlpTarget {
    pub(crate) fn new(
        logs_uri: Uri,
        metrics_uri: Uri,
        traces_uri: Uri,
        headers: HeaderMap,
        client: ExportClient,
        service_name: &str,
    ) -> Self {
        let mut resource = br#"{"attributes":["#.to_vec();
        write_attribute(&mut resource, "service.name", Value::Str(service_name));
        resource.push(b',');
        write_attribute(
            &mut resource,
            "service.version",
            Value::Str(env!("CARGO_PKG_VERSION")),
        );
        resource.extend_from_slice(b"]}");
        let envelope = |open: &[u8], scope: &[u8]| {
            let mut prefix = open.to_vec();
            prefix.extend_from_slice(&resource);
            prefix.extend_from_slice(scope);
            prefix.into_boxed_slice()
        };
        Self {
            logs_prefix: envelope(
                br#"{"resourceLogs":[{"resource":"#,
                br#","scopeLogs":[{"scope":{"name":"wye"},"logRecords":["#,
            ),
            spans_prefix: envelope(
                br#"{"resourceSpans":[{"resource":"#,
                br#","scopeSpans":[{"scope":{"name":"wye"},"spans":["#,
            ),
            logs_uri,
            metrics_uri,
            traces_uri,
            headers,
            client,
            resource: resource.into(),
        }
    }
}

/// The OTLP signal an export carries.
#[derive(Clone, Copy)]
enum Signal {
    Logs,
    Metrics,
    Traces,
}

/// One signal's records waiting to be sent.
#[derive(Default)]
struct Buffer {
    buf: Vec<u8>,
    records: usize,
}

/// The batches being filled by the writer task: each usage record becomes
/// a log record and a span.
pub(super) struct Batch {
    target: Arc<OtlpTarget>,
    logs: Buffer,
    spans: Buffer,
    random: SystemRandom,
    queue: mpsc::Sender<Export>,
    /// Records lost to a full queue or a failed export.
    dropped: Arc<AtomicU64>,
}

impl Batch {
    /// Starts the exporter task, owned by the returned handle.
    pub(super) fn start(target: Arc<OtlpTarget>) -> (Self, JoinHandle<()>) {
        let (queue, rx) = mpsc::channel(EXPORT_QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let exporter = tokio::spawn(export(Arc::clone(&target), rx, Arc::clone(&dropped)));
        let batch = Self {
            logs: Buffer::default(),
            spans: Buffer::default(),
            random: SystemRandom::new(),
            target,
            queue,
            dropped,
        };
        (batch, exporter)
    }

    pub(super) fn is_full(&self) -> bool {
        self.logs.records >= MAX_BATCH_RECORDS
    }

    pub(super) fn push(&mut self, line: &Line<'_>) {
        append(&mut self.logs, &self.target.logs_prefix, |buf| {
            write_log_record(buf, line);
        });
        let random = &self.random;
        append(&mut self.spans, &self.target.spans_prefix, |buf| {
            write_span(buf, line, random);
        });
    }

    /// Hands the batches to the exporter, or drops them if the queue is full.
    pub(super) fn flush(&mut self) {
        self.send(Signal::Logs);
        self.send(Signal::Traces);
    }

    fn send(&mut self, signal: Signal) {
        let buffer = match signal {
            Signal::Logs => &mut self.logs,
            Signal::Traces => &mut self.spans,
            Signal::Metrics => return,
        };
        if buffer.records == 0 {
            return;
        }
        buffer.buf.extend_from_slice(b"]}]}]}");
        let body = Bytes::copy_from_slice(&buffer.buf);
        let records = std::mem::take(&mut buffer.records);
        buffer.buf.clear();
        // A lost span batch repeats the loss of its log batch; count records once.
        let counted = if matches!(signal, Signal::Logs) {
            records
        } else {
            0
        };
        let export = Export {
            signal,
            body,
            records: counted,
        };
        if self.queue.try_send(export).is_err() {
            // Statistics counter only.
            self.dropped.fetch_add(counted as u64, Ordering::Relaxed);
        }
    }

    /// Records lost so far.
    pub(super) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Queues a metrics export; a full queue skips it, and the next one
    /// carries the cumulative totals.
    pub(super) fn send_metrics(&self, body: Vec<u8>) {
        let _ = self.queue.try_send(Export {
            signal: Signal::Metrics,
            body: body.into(),
            records: 0,
        });
    }

    pub(super) fn target(&self) -> &OtlpTarget {
        &self.target
    }
}

/// Appends one record to `buffer`, opening the envelope first.
fn append(buffer: &mut Buffer, prefix: &[u8], write: impl FnOnce(&mut Vec<u8>)) {
    if buffer.records == 0 {
        buffer.buf.extend_from_slice(prefix);
    } else {
        buffer.buf.push(b',');
    }
    write(&mut buffer.buf);
    buffer.records += 1;
}

/// One request for the exporter task.
struct Export {
    signal: Signal,
    body: Bytes,
    /// Log records in `body`, counted as lost if it is not delivered.
    records: usize,
}

/// Sends queued batches one at a time until the writer drops its sender.
async fn export(target: Arc<OtlpTarget>, mut rx: mpsc::Receiver<Export>, dropped: Arc<AtomicU64>) {
    while let Some(Export {
        signal,
        body,
        records,
    }) = rx.recv().await
    {
        let mut req = Request::new(Full::new(body));
        *req.method_mut() = Method::POST;
        *req.uri_mut() = match signal {
            Signal::Logs => target.logs_uri.clone(),
            Signal::Metrics => target.metrics_uri.clone(),
            Signal::Traces => target.traces_uri.clone(),
        };
        let headers = req.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        for (name, value) in &target.headers {
            headers.insert(name, value.clone());
        }
        let sent = tokio::time::timeout(EXPORT_TIMEOUT, async {
            let res = target.client.request(req).await.ok()?;
            let ok = res.status().is_success();
            // Read the (small) response so the connection can be reused.
            res.into_body().collect().await.ok()?;
            ok.then_some(())
        })
        .await;
        if !matches!(sent, Ok(Some(()))) {
            dropped.fetch_add(records as u64, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Value<'a> {
    Str(&'a str),
    Int(u64),
    Double(f64),
}

/// One OTLP `KeyValue`. Strings are escaped by serde_json; 64-bit integers
/// are strings in the protobuf JSON mapping.
pub(super) fn write_attribute(buf: &mut Vec<u8>, key: &str, value: Value<'_>) {
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
fn write_log_record(buf: &mut Vec<u8>, line: &Line<'_>) {
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
fn write_span(buf: &mut Vec<u8>, line: &Line<'_>, random: &SystemRandom) {
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

/// Parses `key1=value1,key2=value2` (the `OTEL_EXPORTER_OTLP_HEADERS` format)
/// into headers whose values are marked sensitive.
pub(crate) fn parse_headers(spec: &str) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    for pair in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=')?;
        let name = http::HeaderName::from_bytes(name.trim().as_bytes()).ok()?;
        let mut value = HeaderValue::from_str(value.trim()).ok()?;
        value.set_sensitive(true);
        headers.append(name, value);
    }
    Some(headers)
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
    fn headers_parse_and_are_sensitive() {
        let headers = parse_headers("api-key=secret, x-team = research").unwrap();
        assert_eq!(headers["api-key"], "secret");
        assert!(headers["api-key"].is_sensitive());
        assert_eq!(headers["x-team"], "research");
        assert!(parse_headers("no-equals-sign").is_none());
        assert!(parse_headers("").unwrap().is_empty());
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
