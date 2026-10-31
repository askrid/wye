//! Failover within a route: refusals that prove a request was not processed
//! move it to the next upstream, with the same bytes; nothing else does.

use super::harness::*;

/// An upstream that records the bodies it receives and answers `status`,
/// counted in `hits`.
async fn counted(
    status: StatusCode,
    hits: &Arc<AtomicUsize>,
) -> (SocketAddr, mpsc::Receiver<Bytes>) {
    let (handler, rx) = recording_with(status, |seen| seen.body);
    (
        spawn_upstream(handler, Arc::clone(hits), None, None).await,
        rx,
    )
}

/// [`counted`], for a test that does not count.
async fn upstream(status: StatusCode) -> (SocketAddr, mpsc::Receiver<Bytes>) {
    counted(status, &Arc::default()).await
}

/// An upstream that refuses every request at once, without reading its body.
async fn refusing(tls: Option<&TestCa>) -> SocketAddr {
    let refuse = |_req: Request<Incoming>| async {
        let mut res = Response::new(Full::new(Bytes::from_static(
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        )));
        *res.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        res
    };
    wye_testkit::spawn_upstream(refuse, tls, None).await
}

fn base_url(addr: SocketAddr) -> String {
    format!("base_url = \"http://{addr}\"")
}

/// The config of two upstreams tried in order, each given as its own lines.
fn config(max_body: usize, route: &str, a: &str, b: &str) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = {max_body}\n\
         [routes.claude]\nselect = \"ordered\"\n{route}\n\
         [[routes.claude.upstreams]]\nid = \"a\"\n{a}\napi_key_env = \"UNUSED\"\n\
         [[routes.claude.upstreams]]\nid = \"b\"\n{b}\napi_key_env = \"UNUSED\"\n"
    )
}

fn key(name: &str) -> UpstreamResources {
    UpstreamResources::new(UpstreamAuth::api_key(name).unwrap())
}

async fn launch(config: &str, a: UpstreamResources) -> Harness {
    let resources = with_upstreams(vec![a, key("key-b")]);
    Harness::launch_config(config, resources, Arc::default()).await
}

/// Two plain upstreams.
async fn gateway(route: &str, a: SocketAddr, b: SocketAddr) -> Harness {
    let config = config(1024, route, &base_url(a), &base_url(b));
    launch(&config, key("key-a")).await
}

/// Sends [`BODY`] and expects `200`, read to its end.
async fn send_ok(h: &Harness) {
    let res = h.send(messages(), BODY).await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
}

const BODY: &str =
    r#"{"model":"claude-opus-5-5","max_tokens":1,"messages":[{"role":"user","content":"hello"}]}"#;

#[tokio::test]
async fn a_rate_limit_or_an_overload_moves_the_request_to_the_next_upstream() {
    for (status, route) in [
        (429, "failover = [\"429\", \"529\", \"connect\"]"),
        (529, "failover = [\"529\"]"),
    ] {
        let (a, mut seen_a) = upstream(StatusCode::from_u16(status).unwrap()).await;
        let (b, mut seen_b) = upstream(StatusCode::OK).await;
        let h = gateway(route, a, b).await;
        send_ok(&h).await;
        assert_eq!(seen_a.recv().await.unwrap(), BODY);
        assert_eq!(seen_b.recv().await.unwrap(), BODY, "the same bytes, resent");
        let record = h.usage_record().await;
        assert_eq!(record["upstream"], "b");
        assert_eq!(
            record["failovers"],
            serde_json::json!([{"upstream": "a", "status": status}])
        );
    }
}

#[tokio::test]
async fn a_refused_connection_moves_the_request_on() {
    let (b, mut seen_b) = upstream(StatusCode::OK).await;
    // Nothing listens on port 1.
    let h = gateway(
        "failover = [\"connect\"]",
        "127.0.0.1:1".parse().unwrap(),
        b,
    )
    .await;
    send_ok(&h).await;
    assert_eq!(seen_b.recv().await.unwrap(), BODY);
    let record = h.usage_record().await;
    assert_eq!(
        record["failovers"],
        serde_json::json!([{"upstream": "a", "error": "connect"}])
    );
}

/// An upstream that accepts connections and never answers the TLS
/// handshake: the connect timeout covers the handshake, so the request moves
/// on as a refused connection instead of waiting out the response header
/// timeout.
#[tokio::test]
async fn a_stalled_tls_handshake_moves_the_request_on() {
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = silent.local_addr().unwrap();
    let _accepting = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            open.push(stream);
        }
    });
    let (b, mut seen_b) = upstream(StatusCode::OK).await;
    let config = config(
        1024,
        "failover = [\"connect\"]",
        &format!("base_url = \"https://{a}\"\nconnect_timeout_secs = 1"),
        &base_url(b),
    );
    let h = launch(&config, key("key-a")).await;
    let res = tokio::time::timeout(WAIT, h.send(messages(), BODY))
        .await
        .expect("answered within the connect timeout, not the header timeout");
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    assert_eq!(seen_b.recv().await.unwrap(), BODY);
    let record = h.usage_record().await;
    assert_eq!(
        record["failovers"],
        serde_json::json!([{"upstream": "a", "error": "connect"}])
    );
}

#[tokio::test]
async fn other_failures_and_large_bodies_are_answered_as_they_are() {
    // A 500 may already have been processed: never resent.
    let hits = Arc::new(AtomicUsize::new(0));
    let (a, _seen_a) = counted(StatusCode::INTERNAL_SERVER_ERROR, &hits).await;
    let (b, _seen_b) = counted(StatusCode::OK, &hits).await;
    let h = gateway("failover = [\"429\", \"529\", \"connect\"]", a, b).await;
    let res = h.send(messages(), BODY).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    read_body(res).await;
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert!(h.usage_record().await.get("failovers").is_none());

    // A body over failover_max_body_bytes is not kept: the 429 stands.
    let hits = Arc::new(AtomicUsize::new(0));
    let (a, _seen_a) = counted(StatusCode::TOO_MANY_REQUESTS, &hits).await;
    let (b, _seen_b) = counted(StatusCode::OK, &hits).await;
    let h = gateway("failover = [\"429\"]\nfailover_max_body_bytes = 16", a, b).await;
    let res = h.send(messages(), BODY).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    read_body(res).await;
    assert_eq!(hits.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn failover_sends_each_provider_its_own_model_id() {
    let bedrock_body = BODY.replace("claude-opus-5-5", "anthropic.claude-opus-5-5");
    for (providers, expect) in [
        // Anthropic refuses: Bedrock gets its own model id.
        (("anthropic", "bedrock"), (BODY, bedrock_body.as_str())),
        // Bedrock refuses: Anthropic gets the id the client sent.
        (("bedrock", "anthropic"), (bedrock_body.as_str(), BODY)),
    ] {
        let (a, mut seen_a) = upstream(StatusCode::TOO_MANY_REQUESTS).await;
        let (b, mut seen_b) = upstream(StatusCode::OK).await;
        let lines = |provider: &str, addr| format!("provider = \"{provider}\"\n{}", base_url(addr));
        let config = config(
            1024,
            "failover = [\"429\"]",
            &lines(providers.0, a),
            &lines(providers.1, b),
        );
        let h = launch(&config, key("key-a")).await;
        let res = h.send(messages(), BODY).await;
        assert_eq!(res.status(), StatusCode::OK, "{providers:?}");
        read_body(res).await;
        assert_eq!(seen_a.recv().await.unwrap(), expect.0, "{providers:?}");
        assert_eq!(seen_b.recv().await.unwrap(), expect.1, "{providers:?}");
        assert_eq!(h.usage_record().await["upstream"], "b");
    }
}

/// Past its bound a body stops being kept, which only shows once it has
/// started: a body without a declared length.
#[tokio::test]
async fn a_chunked_body_too_large_to_keep_answers_the_refusal() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let hits = Arc::new(AtomicUsize::new(0));
    let (a, _seen_a) = counted(StatusCode::TOO_MANY_REQUESTS, &hits).await;
    let (b, _seen_b) = counted(StatusCode::OK, &hits).await;
    let h = gateway("failover = [\"429\"]\nfailover_max_body_bytes = 16", a, b).await;
    let mut client = tokio::net::TcpStream::connect(h.gateway).await.unwrap();
    client
        .write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: wye\r\ntransfer-encoding: chunked\r\n\
                 connection: close\r\n\r\n{:x}\r\n{BODY}\r\n0\r\n\r\n",
                BODY.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut answer = Vec::new();
    client.read_to_end(&mut answer).await.unwrap();
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 429"), "{answer}");
    assert_eq!(hits.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_refusal_during_the_upload_resends_the_whole_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let a = refusing(None).await;
    let (b, mut seen_b) = upstream(StatusCode::OK).await;
    let h = gateway("failover = [\"429\"]", a, b).await;

    // The refusal arrives while the client is still uploading: hyper drops
    // the attempt's body, which hands back the bytes sent and the unread
    // rest, so the next upstream gets the whole body as the client finishes.
    let body = format!(
        r#"{{"model":"claude-opus-5-5","messages":[],"pad":"{}"}}"#,
        "x".repeat(900)
    );
    let (start, rest) = body.split_at(40);
    let mut client = tokio::net::TcpStream::connect(h.gateway).await.unwrap();
    client
        .write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: wye\r\ncontent-type: application/json\r\n\
                 anthropic-version: 2023-06-01\r\ncontent-length: {}\r\n\r\n{start}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    client.write_all(rest.as_bytes()).await.unwrap();
    let mut head = [0u8; 12];
    tokio::time::timeout(WAIT, client.read_exact(&mut head))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&head, b"HTTP/1.1 200");
    assert_eq!(seen_b.recv().await.unwrap(), body.as_str());
    assert_eq!(
        h.usage_record().await["failovers"],
        serde_json::json!([{"upstream": "a", "status": 429}])
    );
}

#[tokio::test]
async fn an_upload_cut_off_by_the_upstream_moves_on() {
    use tokio::io::AsyncReadExt;

    // Reads the request head and a little of the body, then resets the
    // connection, like a provider that refuses early and closes: the body
    // was not delivered whole, so the request cannot have been processed.
    let cutting = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cut_addr = cutting.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = cutting.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 16 * 1024];
            let _ = socket.read(&mut buf).await;
            // Closing with unread data resets the connection.
            drop(socket);
        }
    });
    let (b, mut seen_b) = upstream(StatusCode::OK).await;
    let config = config(
        8 << 20,
        "failover = [\"connect\"]",
        &base_url(cut_addr),
        &base_url(b),
    );
    let h = launch(&config, key("key-a")).await;
    let body = format!(
        r#"{{"model":"claude-opus-5-5","pad":"{}"}}"#,
        "x".repeat(4 << 20)
    );
    let res = h.send(messages(), body.clone()).await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    assert_eq!(seen_b.recv().await.unwrap(), body, "the whole body, resent");
    assert_eq!(
        h.usage_record().await["failovers"],
        serde_json::json!([{"upstream": "a", "error": "connect"}])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn early_refusals_over_http2_move_concurrent_uploads_on() {
    // The production shape: TLS and HTTP/2 to the provider, which refuses
    // each upload before reading it.
    let ca = TestCa::generate();
    let a = refusing(Some(&ca)).await;
    let (b, mut seen_b) = upstream(StatusCode::OK).await;
    let config = config(
        4 << 20,
        "failover = [\"429\", \"connect\"]",
        &format!("base_url = \"https://{a}\""),
        &base_url(b),
    );
    let trusting = UpstreamResources {
        auth: UpstreamAuth::api_key("key-a").unwrap(),
        extra_roots: vec![ca.ca_cert()],
    };
    let h = Arc::new(launch(&config, trusting).await);
    let body = format!(
        r#"{{"model":"claude-opus-5-5","pad":"{}"}}"#,
        "x".repeat(1 << 20)
    );
    // The recording upstream holds 8 bodies: read them as they arrive.
    let received = tokio::spawn(async move {
        let mut bodies = Vec::new();
        while bodies.len() < 32 {
            bodies.push(seen_b.recv().await.unwrap());
        }
        bodies
    });
    let mut uploads = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let h = Arc::clone(&h);
        let body = body.clone();
        uploads.spawn(async move {
            let res = h.send(messages(), body.clone()).await;
            let status = res.status();
            read_body(res).await;
            status
        });
    }
    while let Some(status) = uploads.join_next().await {
        assert_eq!(status.unwrap(), StatusCode::OK);
    }
    let bodies = tokio::time::timeout(WAIT, received).await.unwrap().unwrap();
    assert!(bodies.iter().all(|b| *b == body), "every body resent whole");
}

#[tokio::test]
async fn a_connection_lost_after_the_whole_body_is_not_failed_over() {
    use tokio::io::AsyncReadExt;

    // Reads the whole request, then closes without answering: the provider
    // may be processing it, so a resend could bill it twice.
    let lost = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lost_addr = lost.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = lost.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = socket.read(&mut buf).await {
            request.extend_from_slice(&buf[..n]);
            if request.ends_with(BODY.as_bytes()) {
                break;
            }
        }
        drop(socket);
    });
    let (b, mut seen_b) = upstream(StatusCode::OK).await;
    let h = gateway("failover = [\"429\", \"529\", \"connect\"]", lost_addr, b).await;
    let res = h.send(messages(), BODY).await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    read_body(res).await;
    assert!(seen_b.try_recv().is_err(), "the request was sent twice");
    assert!(h.usage_record().await.get("failovers").is_none());
}

#[tokio::test]
async fn when_every_upstream_refuses_the_last_refusal_is_the_answer() {
    let (a, _seen_a) = upstream(StatusCode::TOO_MANY_REQUESTS).await;
    let last: Handler = Arc::new(|_| {
        Box::pin(async {
            let mut res = full(StatusCode::TOO_MANY_REQUESTS, r#"{"error":"from b"}"#);
            res.headers_mut()
                .insert("retry-after", "7".parse().unwrap());
            res
        })
    });
    let (b, _) = spawn_counted(last).await;
    let h = gateway("failover = [\"429\"]", a, b).await;
    let res = h.send(messages(), BODY).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.headers()["retry-after"], "7");
    assert_eq!(read_body(res).await, r#"{"error":"from b"}"#);
    let record = h.usage_record().await;
    assert_eq!(record["upstream"], "b");
    assert_eq!(
        record["failovers"],
        serde_json::json!([{"upstream": "a", "status": 429}])
    );
}
