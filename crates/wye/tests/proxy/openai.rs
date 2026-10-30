//! The OpenAI route: `/v1/responses` and `/v1/chat/completions` passthrough
//! for Codex and the OpenAI SDKs, with the provider key as a bearer token.

use super::harness::*;

const OPENAI_KEY: &str = "sk-openai-test";
const EVENTS: &str = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n";

/// A gateway with only `[routes.openai]` (plus `extra` config), whose
/// upstream reports what it received and answers with an SSE stream.
async fn harness(extra: &str) -> (Harness, mpsc::Receiver<Seen>) {
    let (handler, rx) = recording_reply(|seen| seen, || sse(full(StatusCode::OK, EVENTS)));
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n{extra}\n\
         [routes.openai]\n[[routes.openai.upstreams]]\nid = \"openai\"\n\
         base_url = \"http://{upstream}\"\napi_key_env = \"UNUSED\"\n"
    );
    let resources = with_upstreams(vec![UpstreamResources::new(
        UpstreamAuth::bearer(OPENAI_KEY).unwrap(),
    )]);
    (Harness::launch_config(&config, resources, hits).await, rx)
}

fn responses() -> http::request::Builder {
    Request::post("/v1/responses")
        .header("content-type", "application/json")
        .header("session-id", "codex-session-1")
        .header("originator", "codex_cli_rs")
}

const BODY: &str = r#"{"model":"gpt-5.5-codex","stream":true,"input":[]}"#;

#[tokio::test]
async fn responses_reach_openai_with_the_provider_key() {
    let (h, mut seen) = harness("").await;
    // The operator's key decides what it bills: the caller's choice of
    // organization or project does not reach OpenAI.
    let req = responses()
        .header("x-api-key", "client-junk")
        .header("openai-organization", "org-other")
        .header("openai-project", "proj_other");
    let res = h.send(req, BODY).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(read_body(res).await, EVENTS);
    let got = seen.recv().await.unwrap();
    assert_eq!(got.uri.path(), "/v1/responses");
    assert_eq!(got.headers["authorization"], format!("Bearer {OPENAI_KEY}"));
    assert!(got.headers.get("x-api-key").is_none());
    assert!(got.headers.get("openai-organization").is_none());
    assert!(got.headers.get("openai-project").is_none());
    assert_eq!(got.headers["session-id"], "codex-session-1");
    assert_eq!(got.body, BODY);
    let record = h.usage_record().await;
    assert_eq!(record["path"], "/v1/responses");
    assert_eq!(record["session_id"], "codex-session-1");
    assert_eq!(record["upstream"], "openai");
    // The main thread of a Codex session.
    assert_eq!(record["request_class"], "main");

    // A spawned thread: a subagent with its parent.
    let spawned = responses()
        .header("thread-id", "thread-2")
        .header("x-codex-parent-thread-id", "thread-1")
        .header("x-openai-subagent", "collab_spawn");
    read_body(h.send(spawned, BODY).await).await;
    seen.recv().await.unwrap();
    let record = h.usage_record().await;
    assert_eq!(record["request_class"], "subagent");
    assert_eq!(record["agent_type"], "collab_spawn");
    assert_eq!(record["agent_id"], "thread-2");
    assert_eq!(record["parent_agent_id"], "thread-1");

    // No Claude route configured: Messages is not proxied, and the 404 says why.
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = String::from_utf8(read_body(res).await.to_vec()).unwrap();
    assert!(body.contains("add [upstream] or [routes.claude]"), "{body}");
}

fn token(key: &Ed25519KeyPair, models: Option<&[&str]>, max_output_tokens: Option<u64>) -> String {
    issue(
        key,
        &NewCredential {
            models,
            max_output_tokens,
            ..claims("dev")
        },
    )
}

#[tokio::test]
async fn gateway_credentials_and_policy_apply_with_openai_errors() {
    let key = keypair();
    let auth = auth_section(&key, "");
    let (h, mut seen) = harness(&auth).await;

    // Codex sends its env_key value as a bearer token: here, a gateway credential.
    let t = token(&key, Some(&["gpt-5.5-*"]), None);
    let res = h
        .send(
            responses().header("authorization", format!("Bearer {t}")),
            BODY,
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    let got = seen.recv().await.unwrap();
    assert_eq!(got.headers["authorization"], format!("Bearer {OPENAI_KEY}"));
    assert_eq!(h.usage_record().await["subject"], "dev");

    let refused = |res: Response<Incoming>, status: StatusCode| async move {
        assert_eq!(res.status(), status);
        let body: serde_json::Value = serde_json::from_slice(&read_body(res).await).unwrap();
        assert!(body["error"]["message"].is_string(), "{body}");
        assert!(
            body.get("type").is_none(),
            "OpenAI shape, not Anthropic's: {body}"
        );
        body["error"]["code"].as_str().unwrap().to_owned()
    };
    let res = h.send(responses(), BODY).await;
    assert_eq!(
        refused(res, StatusCode::UNAUTHORIZED).await,
        "authentication_error"
    );

    let other = token(&key, Some(&["claude-*"]), None);
    let res = h
        .send(
            responses().header("authorization", format!("Bearer {other}")),
            BODY,
        )
        .await;
    assert_eq!(
        refused(res, StatusCode::FORBIDDEN).await,
        "permission_error"
    );

    // Under an output-token ceiling, Responses must say max_output_tokens.
    let capped = token(&key, None, Some(1000));
    let res = h
        .send(
            responses().header("authorization", format!("Bearer {capped}")),
            BODY,
        )
        .await;
    assert_eq!(
        refused(res, StatusCode::FORBIDDEN).await,
        "permission_error"
    );
    let res = h
        .send(
            responses().header("authorization", format!("Bearer {capped}")),
            r#"{"model":"gpt-5.5-codex","max_output_tokens":800,"input":[]}"#,
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 2);
}

/// A Responses stream as OpenAI sends it, ending with usage.
const COMPLETED: &str = "event: response.created\n\
data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.5-codex\",\"usage\":null}}\n\n\
event: response.output_item.added\n\
data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"\"}}\n\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.5-codex\",\"output\":[],\"service_tier\":\"default\",\
\"usage\":{\"input_tokens\":1000,\"input_tokens_details\":{\"cached_tokens\":900},\"output_tokens\":50}}}\n\n";

#[tokio::test]
async fn responses_usage_and_cost_are_recorded() {
    let handler: Handler =
        Arc::new(|_seen: Seen| Box::pin(async { sse(full(StatusCode::OK, COMPLETED)) }));
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n\
         [routes.openai]\n[[routes.openai.upstreams]]\nid = \"openai\"\n\
         base_url = \"http://{upstream}\"\napi_key_env = \"UNUSED\"\n\
         [pricing]\nversion = \"openai-test\"\n[[pricing.models]]\nmodels = [\"gpt-5.5-*\"]\n\
         input = \"1.25\"\noutput = \"10.00\"\ncache_read = \"0.125\"\n"
    );
    let resources = with_upstreams(vec![UpstreamResources::new(
        UpstreamAuth::bearer(OPENAI_KEY).unwrap(),
    )]);
    let h = Harness::launch_config(&config, resources, hits).await;
    let res = h.send(responses(), BODY).await;
    assert_eq!(read_body(res).await, COMPLETED);
    let record = h.usage_record().await;
    assert_eq!(record["model"], "gpt-5.5-codex");
    assert_eq!(record["usage_status"], "complete");
    // Cached tokens are split out of OpenAI's input count.
    assert_eq!(record["input_tokens"], 100);
    assert_eq!(record["cache_read_input_tokens"], 900);
    assert_eq!(record["output_tokens"], 50);
    assert_eq!(record["tool_calls"]["shell"], 1);
    // 100 x 1.25 + 900 x 0.125 + 50 x 10.00 per million tokens.
    assert_eq!(record["cost_usd"], "0.0007375");
    assert_eq!(record["cost_status"], "complete");
}

#[tokio::test]
async fn chat_completions_pass_through_with_usage() {
    const CHAT: &str = "data: {\"model\":\"gpt-5.5\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":null}\n\n\
data: {\"model\":\"gpt-5.5\",\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":10}}}\n\n\
data: [DONE]\n\n";
    let (handler, mut rx) = recording_reply(|seen| seen, || sse(full(StatusCode::OK, CHAT)));
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n\
         [routes.openai]\n[[routes.openai.upstreams]]\nid = \"openai\"\n\
         base_url = \"http://{upstream}\"\napi_key_env = \"UNUSED\"\n"
    );
    let resources = with_upstreams(vec![UpstreamResources::new(
        UpstreamAuth::bearer(OPENAI_KEY).unwrap(),
    )]);
    let h = Harness::launch_config(&config, resources, hits).await;
    let body = r#"{"model":"gpt-5.5","stream":true,"stream_options":{"include_usage":true},"messages":[]}"#;
    let req = Request::post("/v1/chat/completions").header("content-type", "application/json");
    let res = h.send(req, body).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(read_body(res).await, CHAT);
    let seen = rx.recv().await.unwrap();
    assert_eq!(seen.uri.path(), "/v1/chat/completions");
    assert_eq!(
        seen.headers["authorization"],
        format!("Bearer {OPENAI_KEY}")
    );
    assert_eq!(
        seen.body, body,
        "forwarded unchanged: no include_usage rewrite"
    );
    let record = h.usage_record().await;
    assert_eq!(record["path"], "/v1/chat/completions");
    assert_eq!(record["usage_status"], "complete");
    assert_eq!(record["input_tokens"], 20);
    assert_eq!(record["cache_read_input_tokens"], 10);
    assert_eq!(record["output_tokens"], 3);
}

#[tokio::test]
async fn a_body_past_the_limit_gets_openais_error_shape() {
    let (h, mut seen) = harness("").await;
    // The harness allows 1024 bytes.
    let big = format!(r#"{{"input":"{}"}}"#, "x".repeat(2048));
    let res = h.send(responses(), big).await;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: serde_json::Value = serde_json::from_slice(&read_body(res).await).unwrap();
    assert_eq!(body["error"]["code"], "request_too_large", "{body}");
    assert!(seen.try_recv().is_err());
}

#[tokio::test]
async fn beyond_the_concurrency_limit_responses_get_a_retryable_503() {
    // An upstream that keeps its stream open, holding the one permit.
    let (_keep, rx) = mpsc::channel::<Bytes>(1);
    let rx = std::sync::Mutex::new(Some(rx));
    let handler: Handler = Arc::new(move |_| {
        let rx = rx.lock().unwrap().take();
        Box::pin(async move {
            match rx {
                Some(rx) => Response::new(Either::Right(ChannelBody(rx))),
                None => full(StatusCode::OK, "{}"),
            }
        })
    });
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_concurrent_requests = 1\n\
         [routes.openai]\n[[routes.openai.upstreams]]\nid = \"openai\"\n\
         base_url = \"http://{upstream}\"\napi_key_env = \"UNUSED\"\n"
    );
    let resources = with_upstreams(vec![UpstreamResources::new(
        UpstreamAuth::bearer(OPENAI_KEY).unwrap(),
    )]);
    let h = Harness::launch_config(&config, resources, hits).await;
    let _held = h
        .send(responses(), r#"{"model":"gpt-5","input":"a"}"#)
        .await;
    let res = h
        .send(responses(), r#"{"model":"gpt-5","input":"b"}"#)
        .await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.headers()["retry-after"], "1");
    let body: serde_json::Value = serde_json::from_slice(&read_body(res).await).unwrap();
    assert_eq!(body["error"]["code"], "overloaded_error", "{body}");
}
