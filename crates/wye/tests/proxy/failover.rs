//! Failover within a route: refusals that prove a request was not processed
//! move it to the next upstream, with the same bytes; nothing else does.

use super::harness::*;

/// An upstream that records the bodies it receives and answers `status`.
async fn upstream(
    status: StatusCode,
    hits: &Arc<AtomicUsize>,
) -> (SocketAddr, mpsc::Receiver<Bytes>) {
    let (tx, rx) = mpsc::channel(8);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(seen.body).await.unwrap();
            full(status, r#"{"from":"upstream"}"#)
        })
    });
    (
        spawn_upstream(handler, Arc::clone(hits), None, None).await,
        rx,
    )
}

async fn gateway(route: &str, a: &str, b: &str) -> Harness {
    gateway_with(route, (a, "anthropic"), (b, "anthropic")).await
}

/// Two upstreams, each a base URL and a provider, tried in order.
async fn gateway_with(
    route: &str,
    (a, a_provider): (&str, &str),
    (b, b_provider): (&str, &str),
) -> Harness {
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n\
         [routes.claude]\nselect = \"ordered\"\n{route}\n\
         [[routes.claude.upstreams]]\nid = \"a\"\nprovider = \"{a_provider}\"\n\
         base_url = \"{a}\"\napi_key_env = \"UNUSED\"\n\
         [[routes.claude.upstreams]]\nid = \"b\"\nprovider = \"{b_provider}\"\n\
         base_url = \"{b}\"\napi_key_env = \"UNUSED\"\n"
    );
    let resources = Resources {
        upstreams: vec![
            UpstreamResources::new(UpstreamAuth::api_key("key-a").unwrap()),
            UpstreamResources::new(UpstreamAuth::api_key("key-b").unwrap()),
        ],
        verifier: None,
        prompt_key: None,
        otlp_headers: HeaderMap::new(),
    };
    Harness::launch_config(&config, resources, Arc::new(AtomicUsize::new(0))).await
}

const BODY: &str =
    r#"{"model":"claude-opus-5-5","max_tokens":1,"messages":[{"role":"user","content":"hello"}]}"#;

#[tokio::test]
async fn a_rate_limit_moves_the_request_to_the_next_upstream() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (a, mut seen_a) = upstream(StatusCode::TOO_MANY_REQUESTS, &hits).await;
    let (b, mut seen_b) = upstream(StatusCode::OK, &hits).await;
    let h = gateway(
        "failover = [\"429\", \"529\", \"connect\"]",
        &format!("http://{a}"),
        &format!("http://{b}"),
    )
    .await;
    let res = h.send(messages(), BODY).await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    assert_eq!(seen_a.recv().await.unwrap(), BODY);
    assert_eq!(seen_b.recv().await.unwrap(), BODY, "the same bytes, resent");
    let record = h.usage_record().await;
    assert_eq!(record["upstream"], "b");
    assert_eq!(
        record["failovers"],
        serde_json::json!([{"upstream": "a", "status": 429}])
    );
}

#[tokio::test]
async fn a_refused_connection_moves_the_request_on() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (b, mut seen_b) = upstream(StatusCode::OK, &hits).await;
    // Nothing listens on port 1.
    let h = gateway(
        "failover = [\"connect\"]",
        "http://127.0.0.1:1",
        &format!("http://{b}"),
    )
    .await;
    let res = h.send(messages(), BODY).await;
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
    let (a, _seen_a) = upstream(StatusCode::INTERNAL_SERVER_ERROR, &hits).await;
    let (b, _seen_b) = upstream(StatusCode::OK, &hits).await;
    let h = gateway(
        "failover = [\"429\", \"529\", \"connect\"]",
        &format!("http://{a}"),
        &format!("http://{b}"),
    )
    .await;
    let res = h.send(messages(), BODY).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    read_body(res).await;
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert!(h.usage_record().await.get("failovers").is_none());

    // A body over failover_max_body_bytes is not kept: the 429 stands.
    let hits = Arc::new(AtomicUsize::new(0));
    let (a, _seen_a) = upstream(StatusCode::TOO_MANY_REQUESTS, &hits).await;
    let (b, _seen_b) = upstream(StatusCode::OK, &hits).await;
    let h = gateway(
        "failover = [\"429\"]\nfailover_max_body_bytes = 16",
        &format!("http://{a}"),
        &format!("http://{b}"),
    )
    .await;
    let res = h.send(messages(), BODY).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    read_body(res).await;
    assert_eq!(hits.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn failover_respells_the_model_for_each_provider() {
    let bedrock_body = BODY.replace("claude-opus-5-5", "anthropic.claude-opus-5-5");
    for (providers, expect) in [
        // Anthropic refuses: Bedrock gets its own model id.
        (("anthropic", "bedrock"), (BODY, bedrock_body.as_str())),
        // Bedrock refuses: Anthropic gets the id the client sent.
        (("bedrock", "anthropic"), (bedrock_body.as_str(), BODY)),
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let (a, mut seen_a) = upstream(StatusCode::TOO_MANY_REQUESTS, &hits).await;
        let (b, mut seen_b) = upstream(StatusCode::OK, &hits).await;
        let h = gateway_with(
            "failover = [\"429\"]",
            (&format!("http://{a}"), providers.0),
            (&format!("http://{b}"), providers.1),
        )
        .await;
        let res = h.send(messages(), BODY).await;
        assert_eq!(res.status(), StatusCode::OK, "{providers:?}");
        read_body(res).await;
        assert_eq!(seen_a.recv().await.unwrap(), expect.0, "{providers:?}");
        assert_eq!(seen_b.recv().await.unwrap(), expect.1, "{providers:?}");
        assert_eq!(h.usage_record().await["upstream"], "b");
    }
}
