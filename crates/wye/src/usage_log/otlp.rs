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
use gateway_core::config::Telemetry;
use http::{HeaderMap, HeaderValue, Method, Request, Uri, header::CONTENT_TYPE};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use ring::rand::SystemRandom;
use rustls::pki_types::CertificateDer;
use tokio::{sync::mpsc, task::JoinHandle};

mod encode;

use super::Line;
use crate::StartupError;
pub(super) use encode::{Value, write_attributes};
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
    /// The export client trusts `extra_roots`, as the upstream connections do.
    pub(crate) fn new(
        telemetry: &Telemetry,
        extra_roots: &[CertificateDer<'static>],
        headers: HeaderMap,
    ) -> Result<Self, StartupError> {
        let mut resource = br#"{"attributes":["#.to_vec();
        write_attributes(
            &mut resource,
            &[
                ("service.name", Some(Value::Str(&telemetry.service_name))),
                (
                    "service.version",
                    Some(Value::Str(env!("CARGO_PKG_VERSION"))),
                ),
            ],
        );
        resource.extend_from_slice(b"]}");
        let envelope = |open: &[u8], scope: &[u8]| {
            let mut prefix = open.to_vec();
            prefix.extend_from_slice(&resource);
            prefix.extend_from_slice(scope);
            prefix.into_boxed_slice()
        };
        Ok(Self {
            logs_prefix: envelope(
                br#"{"resourceLogs":[{"resource":"#,
                br#","scopeLogs":[{"scope":{"name":"wye"},"logRecords":["#,
            ),
            spans_prefix: envelope(
                br#"{"resourceSpans":[{"resource":"#,
                br#","scopeSpans":[{"scope":{"name":"wye"},"spans":["#,
            ),
            logs_uri: telemetry.otlp_logs_uri.clone(),
            metrics_uri: telemetry.otlp_metrics_uri.clone(),
            traces_uri: telemetry.otlp_traces_uri.clone(),
            headers,
            client: Client::builder(TokioExecutor::new())
                .build(crate::tls::https_connector(extra_roots)?),
            resource: resource.into(),
        })
    }
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
    pub(super) target: Arc<OtlpTarget>,
    logs: Buffer,
    spans: Buffer,
    random: SystemRandom,
    queue: mpsc::Sender<Export>,
    /// Records lost to a full queue or a failed export; the exporter task
    /// keeps adding to it after the batch is gone.
    pub(super) dropped: Arc<AtomicU64>,
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
        for (buffer, uri) in [
            (&mut self.logs, &self.target.logs_uri),
            (&mut self.spans, &self.target.traces_uri),
        ] {
            if buffer.records == 0 {
                continue;
            }
            buffer.buf.extend_from_slice(b"]}]}]}");
            let body = Bytes::copy_from_slice(&buffer.buf);
            let records = std::mem::take(&mut buffer.records);
            buffer.buf.clear();
            // Log records and spans are counted alike: a collector can take one
            // signal and refuse the other (one without a traces pipeline).
            let export = Export {
                uri: uri.clone(),
                body,
                records,
            };
            if self.queue.try_send(export).is_err() {
                // Statistics counter only.
                self.dropped.fetch_add(records as u64, Ordering::Relaxed);
            }
        }
    }

    /// Queues a metrics export; a full queue skips it, and the next one
    /// carries the cumulative totals.
    pub(super) fn send_metrics(&self, body: Vec<u8>) {
        let _ = self.queue.try_send(Export {
            uri: self.target.metrics_uri.clone(),
            body: body.into(),
            records: 0,
        });
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
    uri: Uri,
    body: Bytes,
    /// Log records or spans in `body`, counted as lost if it is not delivered.
    records: usize,
}

/// Sends queued batches one at a time until the writer drops its sender.
async fn export(target: Arc<OtlpTarget>, mut rx: mpsc::Receiver<Export>, dropped: Arc<AtomicU64>) {
    while let Some(Export { uri, body, records }) = rx.recv().await {
        let mut req = Request::new(Full::new(body));
        *req.method_mut() = Method::POST;
        *req.uri_mut() = uri;
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

/// OTLP attribute names both encoders write (logs and spans, metrics), so
/// a query on one matches the other.
pub(super) mod attr {
    pub(crate) const ROUTE: &str = "http.route";
    pub(crate) const STATUS: &str = "http.response.status_code";
    pub(crate) const MODEL: &str = "gen_ai.response.model";
    pub(crate) const TOOL: &str = "gen_ai.tool.name";
    pub(crate) const SUBJECT: &str = "enduser.id";
    pub(crate) const UPSTREAM: &str = "wye.upstream";
    pub(crate) const OUTCOME: &str = "wye.outcome";
    pub(crate) const REQUEST_CLASS: &str = "wye.request_class";
    pub(crate) const CACHE_MISS: &str = "wye.cache_miss";
}
