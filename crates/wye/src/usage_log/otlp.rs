//! OTLP export of usage records: OTLP/HTTP with JSON encoding, for the logs,
//! traces, and metrics signals.
//!
//! The writer task appends each record to a log batch as an OTLP log record
//! and to a span batch as a span, and hands full (or one-second-old) batches,
//! and the periodic metrics export, to an exporter task through one bounded
//! queue. A full queue or a failed export drops the batch and counts
//! its records; nothing here can slow a request. Attribute names follow the
//! OpenTelemetry semantic conventions where one exists (`gen_ai.*`,
//! `http.*`, `session.id`, `enduser.id`), and use a `wye.` prefix otherwise.

use std::{
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
use ring::rand::SystemRandom;
use tokio::{sync::mpsc, task::JoinHandle};

mod encode;

use super::Line;
pub(super) use encode::{Value, write_attribute};
use encode::{write_log_record, write_span};

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
    /// The `Resource` object shared by every signal.
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
        // Log records and spans are counted alike: a collector can take one
        // signal and refuse the other (one without a traces pipeline).
        let export = Export {
            signal,
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

    /// The loss counter, which the exporter task keeps adding to after the
    /// batch is gone.
    pub(super) fn lost(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
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
    /// Log records or spans in `body`, counted as lost if it is not delivered.
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
            // Read the response so the connection can be reused, keeping none
            // of it: a collector's answer is not bounded.
            let mut body = res.into_body();
            while let Some(frame) = body.frame().await {
                frame.ok()?;
            }
            ok.then_some(())
        })
        .await;
        if !matches!(sent, Ok(Some(()))) {
            dropped.fetch_add(records as u64, Ordering::Relaxed);
        }
    }
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
}
