//! End-to-end forwarding tests against a local mock upstream. No network access.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use gateway_core::config::Config;
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
use http_body_util::{BodyExt, Either, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader, DuplexStream, Lines},
    net::TcpListener,
    sync::{Mutex, mpsc},
};
use wye::{Gateway, Resources, UpstreamAuth};
use wye_testkit::TestCa;

const API_KEY: &str = "sk-test-provider-key";
const WAIT: Duration = Duration::from_secs(5);

/// Upstream response body that yields chunks as the test sends them.
struct ChannelBody(mpsc::Receiver<Bytes>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|b| Ok(Frame::data(b))))
    }
}

type UpBody = Either<Full<Bytes>, ChannelBody>;

struct Seen {
    version: Version,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

type Handler =
    Arc<dyn Fn(Seen) -> Pin<Box<dyn Future<Output = Response<UpBody>> + Send>> + Send + Sync>;

struct Harness {
    gateway: SocketAddr,
    upstream_hits: Arc<AtomicUsize>,
    client: Client<HttpConnector, Full<Bytes>>,
    usage: Mutex<Lines<BufReader<DuplexStream>>>,
}

impl Harness {
    /// Gateway in front of a plain-HTTP mock upstream.
    async fn start(handler: Handler, upstream_extra: &str, auth: UpstreamAuth) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
        let resources = Resources {
            auth,
            extra_roots: Vec::new(),
        };
        Self::launch(
            &format!("http://{upstream}"),
            upstream_extra,
            resources,
            hits,
        )
        .await
    }

    /// Gateway in front of a TLS mock upstream; `trusted` adds the mock's CA to Wye's roots.
    async fn start_tls(handler: Handler, ca: &TestCa, trusted: bool) -> Self {
        Self::start_tls_with(handler, ca, trusted, None, "").await
    }

    /// Like `start_tls`, with the mock's HTTP/2 stream limit and extra `[upstream]` lines.
    async fn start_tls_with(
        handler: Handler,
        ca: &TestCa,
        trusted: bool,
        mock_max_streams: Option<u32>,
        upstream_extra: &str,
    ) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let upstream = spawn_upstream(handler, Arc::clone(&hits), Some(ca), mock_max_streams).await;
        let resources = Resources {
            auth: UpstreamAuth::api_key(API_KEY).unwrap(),
            extra_roots: if trusted {
                vec![ca.ca_cert()]
            } else {
                Vec::new()
            },
        };
        let base_url = format!("https://{upstream}");
        let extra = format!("api_key_env = \"UNUSED\"\n{upstream_extra}");
        Self::launch(&base_url, &extra, resources, hits).await
    }

    async fn launch(
        base_url: &str,
        upstream_extra: &str,
        resources: Resources,
        upstream_hits: Arc<AtomicUsize>,
    ) -> Self {
        let config = Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n\n\
             [upstream]\nbase_url = \"{base_url}\"\n{upstream_extra}"
        ))
        .unwrap();
        let gateway = Arc::new(Gateway::new(&config, resources).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (usage_out, usage_in) = tokio::io::duplex(64 * 1024);
        tokio::spawn(wye::serve(
            listener,
            gateway,
            std::future::pending(),
            Duration::ZERO,
            usage_out,
        ));
        Self {
            gateway: addr,
            upstream_hits,
            client: Client::builder(TokioExecutor::new()).build_http(),
            usage: Mutex::new(BufReader::new(usage_in).lines()),
        }
    }

    async fn api_key(handler: Handler) -> Self {
        Self::start(
            handler,
            "api_key_env = \"UNUSED\"",
            UpstreamAuth::api_key(API_KEY).unwrap(),
        )
        .await
    }

    async fn send(&self, req: http::request::Builder, body: &'static str) -> Response<Incoming> {
        let path = req.uri_ref().unwrap().to_string();
        let req = req
            .uri(format!("http://{}{path}", self.gateway))
            .body(Full::new(Bytes::from_static(body.as_bytes())))
            .unwrap();
        tokio::time::timeout(WAIT, self.client.request(req))
            .await
            .unwrap()
            .unwrap()
    }

    /// Next usage record written by the gateway.
    async fn usage_record(&self) -> serde_json::Value {
        let line = tokio::time::timeout(WAIT, self.usage.lock().await.next_line())
            .await
            .expect("no usage record")
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

async fn spawn_upstream(
    handler: Handler,
    hits: Arc<AtomicUsize>,
    tls: Option<&TestCa>,
    max_streams: Option<u32>,
) -> SocketAddr {
    let serve = move |req: Request<Incoming>| {
        let handler = Arc::clone(&handler);
        hits.fetch_add(1, Ordering::Relaxed);
        async move {
            let (parts, body) = req.into_parts();
            let body = body.collect().await.unwrap().to_bytes();
            handler(Seen {
                version: parts.version,
                method: parts.method,
                uri: parts.uri,
                headers: parts.headers,
                body,
            })
            .await
        }
    };
    wye_testkit::spawn_upstream(serve, tls, max_streams).await
}

fn full(status: StatusCode, body: &'static str) -> Response<UpBody> {
    let mut res = Response::new(Either::Left(Full::new(Bytes::from_static(body.as_bytes()))));
    *res.status_mut() = status;
    res
}

fn messages() -> http::request::Builder {
    Request::post("/v1/messages?beta=true")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "future-capability-2099,another")
        .header("x-claude-code-session-id", "s-1")
}

async fn read_body(res: Response<Incoming>) -> Bytes {
    tokio::time::timeout(WAIT, res.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes()
}

#[tokio::test]
async fn forwards_request_unchanged_and_replaces_credential() {
    let (tx, mut rx) = mpsc::channel(1);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(seen).await.unwrap();
            let mut res = full(StatusCode::OK, r#"{"id":"msg_1"}"#);
            res.headers_mut()
                .insert("request-id", "req_1".parse().unwrap());
            res.headers_mut().insert(
                "anthropic-ratelimit-unified-status",
                "allowed".parse().unwrap(),
            );
            res
        })
    });
    let h = Harness::api_key(handler).await;

    let res = h
        .send(
            messages()
                .header("authorization", "Bearer caller-token")
                .header("x-api-key", "caller-key")
                .header("connection", "x-hop")
                .header("x-hop", "1"),
            r#"{"model":"claude-x","unknown_field":{"kept":true}}"#,
        )
        .await;

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["request-id"], "req_1");
    assert_eq!(
        res.headers()["anthropic-ratelimit-unified-status"],
        "allowed"
    );
    assert_eq!(read_body(res).await, r#"{"id":"msg_1"}"#);

    let seen = rx.recv().await.unwrap();
    assert_eq!(seen.method, Method::POST);
    assert_eq!(seen.uri.path_and_query().unwrap(), "/v1/messages?beta=true");
    assert_eq!(
        seen.body,
        r#"{"model":"claude-x","unknown_field":{"kept":true}}"#
    );
    assert_eq!(seen.headers["x-api-key"], API_KEY);
    assert!(seen.headers.get("authorization").is_none());
    assert_eq!(
        seen.headers["anthropic-beta"],
        "future-capability-2099,another"
    );
    assert_eq!(seen.headers["anthropic-version"], "2023-06-01");
    assert_eq!(seen.headers["x-claude-code-session-id"], "s-1");
    assert!(seen.headers.get("x-hop").is_none());
}

#[tokio::test]
async fn forward_caller_credential_keeps_caller_headers() {
    let (tx, mut rx) = mpsc::channel(1);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(seen.headers).await.unwrap();
            full(StatusCode::OK, "{}")
        })
    });
    let h = Harness::start(
        handler,
        "forward_caller_credential = true",
        UpstreamAuth::ForwardCaller,
    )
    .await;
    let res = h
        .send(
            messages().header("authorization", "Bearer oauth-login"),
            "{}",
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let headers = rx.recv().await.unwrap();
    assert_eq!(headers["authorization"], "Bearer oauth-login");
    assert!(headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn preserves_upstream_errors() {
    let handler: Handler = Arc::new(|_| {
        Box::pin(async {
            let mut res = full(
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
            );
            res.headers_mut()
                .insert("retry-after", "7".parse().unwrap());
            res.headers_mut()
                .insert("x-should-retry", "true".parse().unwrap());
            res
        })
    });
    let h = Harness::api_key(handler).await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.headers()["retry-after"], "7");
    assert_eq!(res.headers()["x-should-retry"], "true");
    assert_eq!(
        read_body(res).await,
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
    );
}

#[tokio::test]
async fn streams_events_before_upstream_completes() {
    let (chunks_tx, chunks_rx) = mpsc::channel::<Bytes>(4);
    let body = std::sync::Mutex::new(Some(chunks_rx));
    let handler: Handler = Arc::new(move |_| {
        let rx = body.lock().unwrap().take().unwrap();
        Box::pin(async move {
            let mut res = Response::new(Either::Right(ChannelBody(rx)));
            res.headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            res
        })
    });
    let h = Harness::api_key(handler).await;

    chunks_tx
        .send(Bytes::from_static(b"event: ping\ndata: {}\n\n"))
        .await
        .unwrap();
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let mut body = res.into_body();

    let first = tokio::time::timeout(WAIT, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap(), "event: ping\ndata: {}\n\n");

    chunks_tx
        .send(Bytes::from_static(b"event: message_stop\ndata: {}\n\n"))
        .await
        .unwrap();
    drop(chunks_tx);
    let rest = tokio::time::timeout(WAIT, body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(rest, "event: message_stop\ndata: {}\n\n");
}

#[tokio::test]
async fn client_disconnect_releases_upstream_stream() {
    let (chunks_tx, chunks_rx) = mpsc::channel::<Bytes>(1);
    let body = std::sync::Mutex::new(Some(chunks_rx));
    let handler: Handler = Arc::new(move |_| {
        let rx = body.lock().unwrap().take().unwrap();
        Box::pin(async move { Response::new(Either::Right(ChannelBody(rx))) })
    });
    let h = Harness::api_key(handler).await;

    chunks_tx
        .send(Bytes::from_static(b"data: 1\n\n"))
        .await
        .unwrap();
    let res = h.send(messages(), "{}").await;
    let mut body = res.into_body();
    tokio::time::timeout(WAIT, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(body);
    drop(h.client);

    // The upstream body is dropped once Wye drops its upstream connection.
    tokio::time::timeout(WAIT, chunks_tx.closed())
        .await
        .expect("upstream stream was not released");
}

#[tokio::test]
async fn idle_upstream_stream_is_terminated() {
    let handler: Handler = Arc::new(|_| {
        Box::pin(async {
            let (tx, rx) = mpsc::channel::<Bytes>(1);
            tx.send(Bytes::from_static(b"data: 1\n\n")).await.unwrap();
            // Keep the sender alive without sending, so the stream goes idle.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(tx);
            });
            Response::new(Either::Right(ChannelBody(rx)))
        })
    });
    let h = Harness::start(
        handler,
        "api_key_env = \"UNUSED\"\nstream_idle_timeout_secs = 1",
        UpstreamAuth::api_key(API_KEY).unwrap(),
    )
    .await;
    let res = h.send(messages(), "{}").await;
    let mut body = res.into_body();
    tokio::time::timeout(WAIT, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let next = tokio::time::timeout(WAIT, body.frame()).await.unwrap();
    assert!(
        matches!(next, Some(Err(_))),
        "expected a terminated stream, got {next:?}"
    );
}

#[tokio::test]
async fn slow_upstream_headers_time_out() {
    let handler: Handler = Arc::new(|_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            full(StatusCode::OK, "{}")
        })
    });
    let h = Harness::start(
        handler,
        "api_key_env = \"UNUSED\"\nresponse_header_timeout_secs = 1",
        UpstreamAuth::api_key(API_KEY).unwrap(),
    )
    .await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(res.headers()["content-type"], "application/json");
    let body = read_body(res).await;
    assert!(body.starts_with(br#"{"type":"error","error":{"type":"api_error""#));
}

#[tokio::test]
async fn oversized_body_is_rejected_before_upstream() {
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") }))).await;
    let big: &'static str = Box::leak("x".repeat(2048).into_boxed_str());
    let res = h.send(messages(), big).await;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        read_body(res)
            .await
            .starts_with(br#"{"type":"error","error":{"type":"request_too_large""#)
    );
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn local_endpoints_and_unknown_paths() {
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") }))).await;
    assert_eq!(
        h.send(Request::head("/api/hello"), "").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        h.send(Request::get("/healthz"), "").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        h.send(Request::get("/readyz"), "").await.status(),
        StatusCode::OK
    );
    let res = h.send(Request::post("/v1/chat/completions"), "{}").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert!(
        read_body(res)
            .await
            .starts_with(br#"{"type":"error","error":{"type":"not_found_error""#)
    );
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn count_tokens_is_forwarded() {
    let h = Harness::api_key(Arc::new(|seen: Seen| {
        Box::pin(async move {
            assert_eq!(seen.uri.path(), "/v1/messages/count_tokens");
            full(StatusCode::OK, r#"{"input_tokens":3}"#)
        })
    }))
    .await;
    let res = h
        .send(Request::post("/v1/messages/count_tokens"), "{}")
        .await;
    assert_eq!(read_body(res).await, r#"{"input_tokens":3}"#);
}

const USAGE_STREAM: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":90,\"output_tokens\":1}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test]
async fn completed_stream_logs_usage_without_changing_bytes() {
    let h = Harness::api_key(Arc::new(|_| {
        Box::pin(async {
            let mut res = full(StatusCode::OK, USAGE_STREAM);
            res.headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            res
        })
    }))
    .await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(read_body(res).await, USAGE_STREAM);

    let record = h.usage_record().await;
    assert_eq!(record["event"], "request");
    assert_eq!(record["path"], "/v1/messages");
    assert_eq!(record["status"], 200);
    assert_eq!(record["outcome"], "completed");
    assert_eq!(record["session_id"], "s-1");
    assert_eq!(record["model"], "claude-test");
    assert_eq!(record["usage_status"], "complete");
    assert_eq!(record["input_tokens"], 10);
    assert_eq!(record["cache_read_input_tokens"], 90);
    assert_eq!(record["output_tokens"], 7);
    assert!(record.get("cache_creation_input_tokens").is_none());
}

#[tokio::test]
async fn cancelled_stream_logs_partial_usage() {
    let (chunks_tx, chunks_rx) = mpsc::channel::<Bytes>(1);
    let body = std::sync::Mutex::new(Some(chunks_rx));
    let handler: Handler = Arc::new(move |_| {
        let rx = body.lock().unwrap().take().unwrap();
        Box::pin(async move {
            let mut res = Response::new(Either::Right(ChannelBody(rx)));
            res.headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            res
        })
    });
    let h = Harness::api_key(handler).await;
    let start_event = &USAGE_STREAM[..USAGE_STREAM.find("event: content_block_delta").unwrap()];
    chunks_tx
        .send(Bytes::from_static(start_event.as_bytes()))
        .await
        .unwrap();
    let res = h.send(messages(), "{}").await;
    let mut body = res.into_body();
    tokio::time::timeout(WAIT, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(body);

    let record = h.usage_record().await;
    assert_eq!(record["outcome"], "cancelled");
    assert_eq!(record["usage_status"], "partial");
    assert_eq!(record["input_tokens"], 10);
    assert_eq!(record["output_tokens"], 1);
}

#[tokio::test]
async fn gateway_errors_and_non_stream_responses_are_logged() {
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") }))).await;
    let big: &'static str = Box::leak("x".repeat(2048).into_boxed_str());
    assert_eq!(
        h.send(messages(), big).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let record = h.usage_record().await;
    assert_eq!(record["status"], 413);
    assert_eq!(record["outcome"], "gateway_error");
    assert_eq!(record["usage_status"], "not_observed");

    read_body(
        h.send(Request::post("/v1/messages/count_tokens"), "{}")
            .await,
    )
    .await;
    let record = h.usage_record().await;
    assert_eq!(record["path"], "/v1/messages/count_tokens");
    assert_eq!(record["outcome"], "completed");
    assert_eq!(record["usage_status"], "not_observed");
}

#[tokio::test]
async fn forwards_over_tls_with_http2_to_a_trusted_upstream() {
    let ca = TestCa::generate();
    let (tx, mut rx) = mpsc::channel(1);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send((seen.version, seen.headers, seen.body))
                .await
                .unwrap();
            full(StatusCode::OK, r#"{"id":"msg_tls"}"#)
        })
    });
    let h = Harness::start_tls(handler, &ca, true).await;
    let res = h.send(messages(), r#"{"model":"claude-x"}"#).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(read_body(res).await, r#"{"id":"msg_tls"}"#);
    let (version, headers, body) = rx.recv().await.unwrap();
    assert_eq!(version, Version::HTTP_2);
    assert_eq!(headers["x-api-key"], API_KEY);
    assert_eq!(body, r#"{"model":"claude-x"}"#);
}

#[tokio::test]
async fn untrusted_upstream_certificate_is_rejected() {
    let ca = TestCa::generate();
    let h = Harness::start_tls(
        Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") })),
        &ca,
        false,
    )
    .await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 0);
}

#[test]
fn ca_file_resolves_relative_to_the_config_directory() {
    let ca = TestCa::generate();
    let dir = std::env::temp_dir().join(format!("wye-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("certs")).unwrap();
    std::fs::write(dir.join("certs/ca.pem"), ca.ca_pem()).unwrap();
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:0\"\n[upstream]\nbase_url = \"https://api.anthropic.com\"\n\
         forward_caller_credential = true\nca_file = \"certs/ca.pem\"",
    )
    .unwrap();

    let resources = Resources::resolve(&config, &dir).unwrap();
    assert_eq!(resources.extra_roots, vec![ca.ca_cert()]);

    std::fs::write(dir.join("certs/ca.pem"), "not a certificate").unwrap();
    let err = Resources::resolve(&config, &dir).unwrap_err().to_string();
    assert!(err.contains("no PEM certificates found"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn concurrent_streams_beyond_one_connection_limit_are_not_queued() {
    const PEER_LIMIT: u32 = 4;
    const STREAMS: usize = 10;
    let ca = TestCa::generate();
    // Streams stay open until the test ends, holding their HTTP/2 stream slots.
    let open: Arc<std::sync::Mutex<Vec<mpsc::Sender<Bytes>>>> = Arc::default();
    let held = Arc::clone(&open);
    let handler: Handler = Arc::new(move |_| {
        let (tx, rx) = mpsc::channel(1);
        held.lock().unwrap().push(tx);
        Box::pin(async move { Response::new(Either::Right(ChannelBody(rx))) })
    });
    let h = Harness::start_tls_with(
        handler,
        &ca,
        true,
        Some(PEER_LIMIT),
        &format!("max_streams_per_connection = {PEER_LIMIT}"),
    )
    .await;

    // Each response keeps its stream open, so stream n needs a free slot while n-1 are held.
    let mut responses = Vec::new();
    for _ in 0..STREAMS {
        let res = h.send(messages(), "{}").await;
        assert_eq!(res.status(), StatusCode::OK);
        responses.push(res);
    }
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), STREAMS);
    drop(open);
}
