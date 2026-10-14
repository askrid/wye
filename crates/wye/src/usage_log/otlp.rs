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
    /// Extra headers, sensitive values marked as such.
    headers: HeaderMap,
    client: ExportClient,
    /// The `Resource` object shared by both signals.
    pub(super) resource: Box<[u8]>,
    /// The log batch envelope up to the first log record.
    prefix: Box<[u8]>,
}

impl OtlpTarget {
    pub(crate) fn new(
        logs_uri: Uri,
        metrics_uri: Uri,
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
        let mut prefix = br#"{"resourceLogs":[{"resource":"#.to_vec();
        prefix.extend_from_slice(&resource);
        prefix.extend_from_slice(br#","scopeLogs":[{"scope":{"name":"wye"},"logRecords":["#);
        Self {
            logs_uri,
            metrics_uri,
            headers,
            client,
            resource: resource.into(),
            prefix: prefix.into(),
        }
    }
}

/// The batch being filled by the writer task.
pub(super) struct Batch {
    target: Arc<OtlpTarget>,
    buf: Vec<u8>,
    records: usize,
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
            buf: Vec::with_capacity(64 * 1024),
            records: 0,
            target,
            queue,
            dropped,
        };
        (batch, exporter)
    }

    pub(super) fn is_full(&self) -> bool {
        self.records >= MAX_BATCH_RECORDS
    }

    pub(super) fn push(&mut self, line: &Line<'_>) {
        if self.records == 0 {
            self.buf.extend_from_slice(&self.target.prefix);
        } else {
            self.buf.push(b',');
        }
        write_log_record(&mut self.buf, line);
        self.records += 1;
    }

    /// Hands the batch to the exporter, or drops it if the queue is full.
    pub(super) fn flush(&mut self) {
        if self.records == 0 {
            return;
        }
        self.buf.extend_from_slice(b"]}]}]}");
        let body = Bytes::copy_from_slice(&self.buf);
        let records = std::mem::take(&mut self.records);
        self.buf.clear();
        let export = Export {
            metrics: false,
            body,
            records,
        };
        if self.queue.try_send(export).is_err() {
            // Statistics counter only.
            self.dropped.fetch_add(records as u64, Ordering::Relaxed);
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
            metrics: true,
            body: body.into(),
            records: 0,
        });
    }

    pub(super) fn target(&self) -> &OtlpTarget {
        &self.target
    }
}

/// One request for the exporter task.
struct Export {
    /// Metrics instead of logs.
    metrics: bool,
    body: Bytes,
    /// Log records in `body`, counted as lost if it is not delivered.
    records: usize,
}

/// Sends queued batches one at a time until the writer drops its sender.
async fn export(target: Arc<OtlpTarget>, mut rx: mpsc::Receiver<Export>, dropped: Arc<AtomicU64>) {
    while let Some(Export {
        metrics,
        body,
        records,
    }) = rx.recv().await
    {
        let mut req = Request::new(Full::new(body));
        *req.method_mut() = Method::POST;
        *req.uri_mut() = if metrics {
            target.metrics_uri.clone()
        } else {
            target.logs_uri.clone()
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
    buf.extend_from_slice(b"]}");
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
