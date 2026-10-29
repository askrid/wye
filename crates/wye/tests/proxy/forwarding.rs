//! Request forwarding, credential replacement, errors, and local endpoints.

use super::harness::*;

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
                .header("x-hop", "1")
                .header("accept-encoding", "gzip, deflate, br, zstd"),
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
    // Usage is read from the response bytes, so they must not be compressed.
    assert_eq!(seen.headers["accept-encoding"], "identity");
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
async fn a_body_without_a_length_is_refused_when_it_passes_the_limit() {
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") }))).await;
    // The harness allows 1024 bytes; the second chunk passes it mid-upload.
    let rest: &'static [u8] = Box::leak(vec![b' '; 1100].into_boxed_slice());
    let res = h
        .send_chunked(messages(), &[br#"{"messages":["#, rest])
        .await;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        read_body(res)
            .await
            .starts_with(br#"{"type":"error","error":{"type":"request_too_large""#)
    );
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
    // An unknown path gets Anthropic's error shape.
    let res = h.send(Request::post("/v1/unknown"), "{}").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert!(
        read_body(res)
            .await
            .starts_with(br#"{"type":"error","error":{"type":"not_found_error""#)
    );
    // A known endpoint without a route gets its own API's shape, naming the
    // section that would serve it.
    let res = h.send(Request::post("/v1/chat/completions"), "{}").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = String::from_utf8(read_body(res).await.to_vec()).unwrap();
    assert!(body.starts_with(r#"{"error":{"#), "{body}");
    assert!(body.contains("add [routes.openai]"), "{body}");
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

#[tokio::test]
async fn large_headers_fit_the_connection_buffer_and_oversized_ones_are_rejected() {
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") }))).await;
    // An 8 KiB gateway credential plus other headers fits the connection buffer.
    let credential = "x".repeat(8 * 1024);
    let res = h
        .send(
            messages()
                .header("authorization", format!("Bearer {credential}"))
                .header("x-claude-code-extra", "y".repeat(4 * 1024)),
            "{}",
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);

    // Headers beyond the buffer bound are refused without reaching the upstream.
    let hits = h.upstream_hits.load(Ordering::Relaxed);
    let res = h
        .send(
            messages().header("x-oversized", "z".repeat(64 * 1024)),
            "{}",
        )
        .await;
    assert_eq!(res.status(), StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), hits);
}

#[tokio::test]
async fn api_key_file_is_read_relative_to_the_config_and_trimmed() {
    let dir = std::env::temp_dir().join(format!("wye-key-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("secrets")).unwrap();
    std::fs::write(dir.join("secrets/provider-key"), "sk-from-file\n").unwrap();
    let (tx, mut rx) = mpsc::channel::<HeaderMap>(1);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(seen.headers).await.unwrap();
            full(StatusCode::OK, "{}")
        })
    });
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
    let config = Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:0\"\n[upstream]\nbase_url = \"http://{upstream}\"\n\
         api_key_file = \"secrets/provider-key\""
    ))
    .unwrap();
    let resources = Resources::resolve(&config, &dir).unwrap();
    let h = Harness::launch(
        &format!("http://{upstream}"),
        "",
        "api_key_file = \"unused\"",
        resources,
        hits,
    )
    .await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(rx.recv().await.unwrap()["x-api-key"], "sk-from-file");

    std::fs::write(dir.join("secrets/provider-key"), " \n").unwrap();
    let err = Resources::resolve(&config, &dir).unwrap_err().to_string();
    assert!(err.contains("file is empty"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
