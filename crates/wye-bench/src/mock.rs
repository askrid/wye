//! Mock Anthropic upstream driven by request headers.
//!
//! - `x-bench-unary: N`: a Messages-shaped JSON response of N bytes.
//! - otherwise SSE: `x-bench-events`, `x-bench-size` (payload bytes),
//!   `x-bench-interval-us`. Each event carries the send time (`"t"`, ns since
//!   process start), so the client can measure per-event delay.
//! - `x-bench-report-drop`: report when the stream is dropped (cancel scenario).

use std::{
    convert::Infallible,
    net::SocketAddr,
    pin::Pin,
    sync::{Mutex, OnceLock},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body_util::{BodyExt, Either, Full};
use hyper::body::{Body, Frame, Incoming};
use tokio::{sync::mpsc, time::Sleep};
use wye_testkit::TestCa;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Receives drop times of streams that asked for it; installed by the cancel scenario.
static BODY_DROPPED: Mutex<Option<mpsc::UnboundedSender<u64>>> = Mutex::new(None);

/// Starts the shared clock before any timestamps are taken.
pub fn init_clock() {
    EPOCH.get_or_init(Instant::now);
}

/// Nanoseconds since process start; shared by mock and client, so differences are delays.
pub fn now_nanos() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Routes stream drop reports to `tx` until the next call.
pub fn report_drops_to(tx: mpsc::UnboundedSender<u64>) {
    *BODY_DROPPED.lock().unwrap() = Some(tx);
}

/// HTTP/2 stream limit per mock connection: above the paced scenario's
/// concurrency per client pool, so the direct baseline never queues.
const MAX_STREAMS: u32 = 1_000;

pub async fn spawn(tls: Option<&TestCa>) -> SocketAddr {
    wye_testkit::spawn_upstream(handle, tls, Some(MAX_STREAMS)).await
}

async fn handle(req: Request<Incoming>) -> Response<Either<Full<Bytes>, SseBody>> {
    let headers = req.headers().clone();
    // Drain the request body like a real upstream would.
    let _ = req.into_body().collect().await;
    if let Some(size) = header_u64(&headers, "x-bench-unary") {
        let body = padded_json(
            r#"{"id":"msg_bench","type":"message","role":"assistant","content":[{"type":"text","text":""#,
            r#""}],"usage":{"input_tokens":1,"output_tokens":1}}"#,
            size as usize,
        );
        return Response::new(Either::Left(Full::new(body)));
    }
    let size = header_u64(&headers, "x-bench-size").unwrap_or(64) as usize;
    let mut res = Response::new(Either::Right(SseBody {
        remaining: header_u64(&headers, "x-bench-events").unwrap_or(1) as u32,
        payload: Bytes::from(vec![b'x'; size]),
        interval: Duration::from_micros(header_u64(&headers, "x-bench-interval-us").unwrap_or(0)),
        sleep: None,
        report_drop: headers.contains_key("x-bench-report-drop"),
    }));
    res.headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    res
}

/// Valid JSON of exactly `size` bytes (at least the fixed parts): `head`, then
/// filler text, then `tail`. Gateways that parse bodies must not fail on the
/// benchmark's payloads where real traffic would pass.
pub fn padded_json(head: &str, tail: &str, size: usize) -> Bytes {
    let filler = size.saturating_sub(head.len() + tail.len());
    let mut body = Vec::with_capacity(head.len() + filler + tail.len());
    body.extend_from_slice(head.as_bytes());
    body.resize(head.len() + filler, b'x');
    body.extend_from_slice(tail.as_bytes());
    Bytes::from(body)
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

/// Emits `remaining` events of `payload` every `interval`.
struct SseBody {
    remaining: u32,
    payload: Bytes,
    interval: Duration,
    sleep: Option<Pin<Box<Sleep>>>,
    report_drop: bool,
}

impl Body for SseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        if let Some(sleep) = self.sleep.as_mut()
            && sleep.as_mut().poll(cx).is_pending()
        {
            return Poll::Pending;
        }
        self.remaining -= 1;
        if !self.interval.is_zero() && self.remaining > 0 {
            self.sleep = Some(Box::pin(tokio::time::sleep(self.interval)));
        }
        let mut event = format!(
            "event: content_block_delta\ndata: {{\"t\":{},\"p\":\"",
            now_nanos()
        )
        .into_bytes();
        event.extend_from_slice(&self.payload);
        event.extend_from_slice(b"\"}\n\n");
        Poll::Ready(Some(Ok(Frame::data(Bytes::from(event)))))
    }
}

impl Drop for SseBody {
    fn drop(&mut self) {
        if self.report_drop
            && let Some(tx) = BODY_DROPPED.lock().unwrap().as_ref()
        {
            let _ = tx.send(now_nanos());
        }
    }
}
