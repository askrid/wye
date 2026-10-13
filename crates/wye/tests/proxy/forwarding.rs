//! Request forwarding, credentials replacement, errors, and local endpoints.

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
