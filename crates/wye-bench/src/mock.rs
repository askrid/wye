//! Mock Anthropic upstream driven by request headers.
//!
//! - `x-bench-unary: N`: a Messages-shaped JSON response of N bytes.
//! - otherwise SSE: `x-bench-events`, `x-bench-size` (payload bytes),
//!   `x-bench-interval-us`. Each event carries the send time (`"t"`, ns since
//!   process start), so the client can measure per-event delay.
//! - `x-bench-report-drop`: report when the stream is dropped (cancel scenario).
//!
//! A request to `/v1/responses` gets an OpenAI Responses JSON body or stream
//! (`response.created`, `response.output_text.delta` events, then
//! `response.completed` with usage).
//!
//! [`spawn_refusing`] answers every n-th request with `429` before reading
//! its body, like a rate-limited provider, for failover under load.
//!
//! With [`use_anthropic_frames`], streams are valid Anthropic Messages streams
//! (`message_start` first, deltas as `text_delta`, `message_stop` last), for
//! gateways that parse events and drop what is not Anthropic-shaped.

use std::{
    convert::Infallible,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
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

/// Set once before any stream starts; see [`use_anthropic_frames`].
static ANTHROPIC_FRAMES: AtomicBool = AtomicBool::new(false);

/// Frames every SSE stream as a complete Anthropic Messages stream, and gives
/// every unary Messages reply the fields a real one has (`model`,
/// `stop_reason`): a gateway that parses responses rejects one without.
pub fn use_anthropic_frames() {
    ANTHROPIC_FRAMES.store(true, Ordering::Relaxed);
}

const STREAM_START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_bench\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"bench\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
const STREAM_END: &[u8] = b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

const RESPONSES_START: &[u8] = b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_bench\",\"model\":\"openai/bench\",\"output\":[],\"usage\":null}}\n\n";
const RESPONSES_END: &[u8] = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_bench\",\"model\":\"openai/bench\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

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

/// A plain-HTTP mock that refuses every `every`-th request (the first
/// included) with an Anthropic `429`, without reading its body.
pub async fn spawn_refusing(every: u64) -> SocketAddr {
    let count = Arc::new(AtomicU64::new(0));
    let handler = move |req: Request<Incoming>| {
        let refuse = count.fetch_add(1, Ordering::Relaxed).is_multiple_of(every);
        async move {
            if refuse {
                let mut res = json_response(Bytes::from_static(
                    br#"{"type":"error","error":{"type":"rate_limit_error","message":"bench refusal"}}"#,
                ));
                *res.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
                return res;
            }
            handle(req).await
        }
    };
    wye_testkit::spawn_upstream(handler, None, Some(MAX_STREAMS)).await
}

async fn handle(req: Request<Incoming>) -> Response<Either<Full<Bytes>, SseBody>> {
    let headers = req.headers().clone();
    let responses = req.uri().path() == "/v1/responses";
    // Drain the request body like a real upstream would.
    let _ = req.into_body().collect().await;
    if let Some(size) = header_u64(&headers, "x-bench-unary") {
        let (head, tail) = if responses {
            (
                r#"{"id":"resp_bench","object":"response","model":"openai/bench","output":[{"type":"message","content":[{"type":"output_text","text":""#,
                r#""}]}],"usage":{"input_tokens":1,"output_tokens":1}}"#,
            )
        } else if ANTHROPIC_FRAMES.load(Ordering::Relaxed) {
            (
                r#"{"id":"msg_bench","type":"message","role":"assistant","model":"bench","stop_reason":"end_turn","stop_sequence":null,"content":[{"type":"text","text":""#,
                r#""}],"usage":{"input_tokens":1,"output_tokens":1}}"#,
            )
        } else {
            (
                r#"{"id":"msg_bench","type":"message","role":"assistant","content":[{"type":"text","text":""#,
                r#""}],"usage":{"input_tokens":1,"output_tokens":1}}"#,
            )
        };
        return json_response(padded_json(head, tail, size as usize));
    }
    let size = header_u64(&headers, "x-bench-size").unwrap_or(64) as usize;
    let mut res = Response::new(Either::Right(SseBody {
        remaining: header_u64(&headers, "x-bench-events").unwrap_or(1) as u32,
        payload: Bytes::from(vec![b'x'; size]),
        interval: Duration::from_micros(header_u64(&headers, "x-bench-interval-us").unwrap_or(0)),
        sleep: None,
        report_drop: headers.contains_key("x-bench-report-drop"),
        started: false,
        responses,
    }));
    res.headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    res
}

fn json_response(body: Bytes) -> Response<Either<Full<Bytes>, SseBody>> {
    let mut res = Response::new(Either::Left(Full::new(body)));
    res.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    res
}

/// A Messages request of exactly `size` bytes whose one user message is
/// filler: an agent-sized context as one long string.
pub fn message_json(size: usize) -> Bytes {
    padded_json(
        r#"{"model":"anthropic/bench","max_tokens":1,"messages":[{"role":"user","content":""#,
        r#""}]}"#,
        size,
    )
}

/// A Messages request shaped like Claude Code's, of at least `size` bytes:
/// tool definitions with JSON schemas, a cached system prompt, then turns of
/// tool calls and code-carrying tool results (quotes, backslashes, newlines),
/// the last one marked with `cache_control`. Exercises the cache diagnostics
/// scanner on realistic structure, where one long string would be its best case.
pub fn agent_json(size: usize) -> Bytes {
    // Claude Code streams; but upload scenarios get a unary reply, which a
    // gateway that parses requests would reject for `"stream":true`.
    let stream = !ANTHROPIC_FRAMES.load(Ordering::Relaxed);
    const CODE: &str = "fn main() {\n    let path = \"src/main.rs\";\n    println!(\"{}\\n\", path);\n    if args.len() > 1 { return Err(\"usage: wye <cmd>\".into()); }\n}\n";
    let code = serde_json::to_string(&CODE.repeat(12)).unwrap();
    let words =
        serde_json::to_string(&"Reads a file from the local filesystem. ".repeat(30)).unwrap();
    let tools: Vec<String> = (0..24)
        .map(|i| {
            format!(
                r#"{{"name":"Tool{i}","description":{words},"input_schema":{{"type":"object","properties":{{"file_path":{{"type":"string","description":"Absolute path"}},"offset":{{"type":"number"}},"limit":{{"type":"number"}}}},"required":["file_path"],"additionalProperties":false}}}}"#
            )
        })
        .collect();
    let mut body = format!(
        r#"{{"model":"anthropic/bench","max_tokens":32000,"stream":{stream},"system":[{{"type":"text","text":{words}}},{{"type":"text","text":{words},"cache_control":{{"type":"ephemeral"}}}}],"tools":[{}],"messages":["#,
        tools.join(",")
    );
    let mut turn = 0;
    while body.len() < size {
        if turn > 0 {
            body.push(',');
        }
        body.push_str(&format!(
            r#"{{"role":"assistant","content":[{{"type":"text","text":"Let me look at that file."}},{{"type":"tool_use","id":"toolu_{turn:04}","name":"Tool1","input":{{"file_path":"/repo/src/main.rs","offset":{turn},"limit":200}}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_{turn:04}","content":{code}}}]}}"#
        ));
        turn += 1;
    }
    body.push_str(r#",{"role":"user","content":[{"type":"text","text":"Now fix it.","cache_control":{"type":"ephemeral"}}]}]}"#);
    Bytes::from(body)
}

/// Valid JSON of exactly `size` bytes (at least the fixed parts): `head`, then
/// filler text, then `tail`. Gateways that parse bodies must not fail on the
/// benchmark's payloads where real traffic would pass.
fn padded_json(head: &str, tail: &str, size: usize) -> Bytes {
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
    started: bool,
    /// Framed as an OpenAI Responses stream.
    responses: bool,
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
        let frames = ANTHROPIC_FRAMES.load(Ordering::Relaxed);
        let mut event = Vec::with_capacity(self.payload.len() + 512);
        let first = !std::mem::replace(&mut self.started, true);
        let sent = now_nanos();
        if self.responses {
            if first {
                event.extend_from_slice(RESPONSES_START);
            }
            event.extend_from_slice(
                format!("event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"t\":{sent},\"delta\":\"").as_bytes(),
            );
            event.extend_from_slice(&self.payload);
            event.extend_from_slice(b"\"}\n\n");
            if self.remaining == 0 {
                event.extend_from_slice(RESPONSES_END);
            }
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(event)))));
        }
        if frames && first {
            event.extend_from_slice(STREAM_START);
        }
        if frames {
            // A valid `text_delta`; the send time rides in an extra field.
            event.extend_from_slice(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"");
            event.extend_from_slice(&self.payload);
            event.extend_from_slice(format!("\"}},\"t\":{sent}}}\n\n").as_bytes());
        } else {
            event.extend_from_slice(
                format!("event: content_block_delta\ndata: {{\"t\":{sent},\"p\":\"").as_bytes(),
            );
            event.extend_from_slice(&self.payload);
            event.extend_from_slice(b"\"}\n\n");
        }
        if frames && self.remaining == 0 {
            event.extend_from_slice(STREAM_END);
        }
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
