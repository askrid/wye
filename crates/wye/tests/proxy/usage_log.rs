//! Per-request usage records.

use super::harness::*;

/// A Messages stream whose usage the tests check.
pub(super) const USAGE_STREAM: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":90,\"output_tokens\":1}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// A small request, sent raw, so a test can leave before its answer.
const POST: &[u8] = b"POST /v1/messages HTTP/1.1\r\nhost: localhost\r\ncontent-length: 2\r\n\r\n{}";

#[tokio::test]
async fn an_empty_response_is_completed() {
    // hyper sends an empty body with the headers and never polls it.
    let h = Harness::provider_key(answering(|| full(StatusCode::OK, ""))).await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(read_body(res).await.is_empty());
    assert_eq!(h.usage_record().await["outcome"], "completed");
}

#[tokio::test]
async fn completed_stream_logs_usage_without_changing_bytes() {
    let h = Harness::provider_key(answering(|| sse(full(StatusCode::OK, USAGE_STREAM)))).await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(read_body(res).await, USAGE_STREAM);

    let record = h.usage_record().await;
    assert_eq!(record["event"], "request");
    assert_eq!(record["v"], 2, "the record format's version");
    assert_eq!(record["endpoint"], "/v1/messages");
    assert!(record.get("path").is_none(), "v2 renamed it");
    assert_eq!(record["status"], 200);
    assert_eq!(record["outcome"], "completed");
    // A route's only upstream, named by its provider.
    assert_eq!(record["upstream"], "anthropic");
    assert_eq!(record["session_id"], "s-1");
    assert_eq!(record["model"], "claude-test");
    assert_eq!(record["usage_status"], "complete");
    assert_eq!(record["input_tokens"], 10);
    assert_eq!(record["cache_read_input_tokens"], 90);
    assert_eq!(record["output_tokens"], 7);
    assert!(record.get("cache_creation_input_tokens").is_none());

    // A session id that is not a bounded token never reaches telemetry.
    let res = h
        .send(
            Request::post("/v1/messages").header("x-claude-code-session-id", "a b\"c"),
            "{}",
        )
        .await;
    read_body(res).await;
    assert!(h.usage_record().await.get("session_id").is_none());
}

#[tokio::test]
async fn usage_records_carry_claude_code_agent_headers() {
    // A cache write with no read: a miss, explained by the compaction.
    const MISS: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":5,\"cache_creation_input_tokens\":900,\"cache_read_input_tokens\":0,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{}}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let h = Harness::provider_key(answering(|| sse(full(StatusCode::OK, MISS)))).await;
    let req = messages()
        .header("x-claude-code-request-class", "subagent")
        .header("x-claude-code-agent-type", "Explore")
        .header("x-claude-code-agent-id", "a-1")
        .header("x-claude-code-parent-agent-id", "has space")
        .header(
            "x-claude-code-prompt-id",
            "5f0c1a2e-0b7d-4a51-9a39-2b0f5c3d1e7a",
        )
        .header("x-claude-code-context-compacted", "manual")
        .header(
            "x-claude-code-prev-tool-durations",
            "Bash=742;Web%20Fetch=3;Read=9",
        );
    read_body(h.send(req, "{}").await).await;

    let record = h.usage_record().await;
    assert_eq!(record["request_class"], "subagent");
    assert_eq!(record["agent_type"], "Explore");
    assert_eq!(record["agent_id"], "a-1");
    assert!(
        record.get("parent_agent_id").is_none(),
        "values outside the token alphabet are dropped"
    );
    assert_eq!(record["prompt_id"], "5f0c1a2e-0b7d-4a51-9a39-2b0f5c3d1e7a");
    assert_eq!(record["after_compaction"], "manual");
    assert!(record.get("compaction").is_none());
    assert_eq!(record["cache_miss"], "after_compaction");
    assert_eq!(record["tool_calls"], serde_json::json!({"Bash": 1}));
    assert_eq!(
        record["tool_durations"],
        serde_json::json!([{"name": "Bash", "ms": 742}, {"name": "Read", "ms": 9}]),
        "a decoded name outside the token alphabet is dropped"
    );

    // Without the headers, none of the fields appear.
    read_body(h.send(messages(), "{}").await).await;
    let record = h.usage_record().await;
    for field in ["request_class", "agent_id", "prompt_id", "after_compaction"] {
        assert!(record.get(field).is_none(), "{field}");
    }
    assert_eq!(record["cache_miss"], "unexplained");
}

#[tokio::test]
async fn cancelled_stream_logs_partial_usage() {
    let (chunks_tx, chunks_rx) = mpsc::channel::<Bytes>(1);
    let h = Harness::provider_key(once(chunks_rx, |rx| async { sse(channel(rx)) })).await;
    let start_event = &USAGE_STREAM[..USAGE_STREAM.find("event: content_block_delta").unwrap()];
    chunks_tx
        .send(Bytes::from_static(start_event.as_bytes()))
        .await
        .unwrap();
    let res = h.send(messages(), "{}").await;
    let mut body = res.into_body();
    next_frame(&mut body).await;
    drop(body);

    let record = h.usage_record().await;
    assert_eq!(record["outcome"], "cancelled");
    assert_eq!(record["usage_status"], "partial");
    assert_eq!(record["input_tokens"], 10);
    assert_eq!(record["output_tokens"], 1);
}

#[tokio::test]
async fn a_client_that_leaves_before_the_headers_is_recorded() {
    // The upstream never answers; the client stops waiting.
    let h = Harness::provider_key(Arc::new(|_| Box::pin(std::future::pending()))).await;
    let client = h.raw(POST).await;
    h.reached(1).await;
    drop(client);

    let record = h.usage_record().await;
    assert_eq!(record["outcome"], "cancelled");
    assert_eq!(record["status"], 499);
    assert_eq!(record["endpoint"], "/v1/messages");
}

/// A client that leaves before the response headers also cancels the
/// upstream request: on HTTP/2 the stream is reset, and the provider stops.
#[tokio::test]
async fn a_client_that_leaves_before_the_headers_cancels_the_upstream() {
    let ca = TestCa::generate();
    let (guard, released) = oneshot::channel::<()>();
    // The upstream holds `guard` until its request is dropped.
    let handler = once(guard, |guard| async move {
        let _guard = guard;
        std::future::pending::<Response<UpBody>>().await
    });
    let h = Harness::start_tls(handler, &ca, true).await;
    let client = h.raw(POST).await;
    h.reached(1).await;
    drop(client);
    assert!(
        tokio::time::timeout(WAIT, released).await.is_ok(),
        "the upstream request outlived its client"
    );
}

/// A client body that breaks mid-upload (bad chunk framing here) is the
/// client's error, `400`, never an upstream failure (`502`).
#[tokio::test]
async fn a_broken_client_body_is_the_clients_error() {
    use tokio::io::AsyncReadExt;
    let h = Harness::provider_key(ok()).await;
    let mut client = h
        .raw(
            b"POST /v1/messages HTTP/1.1\r\nhost: localhost\r\ntransfer-encoding: chunked\r\n\r\n\
              2\r\n{}\r\nzz\r\n",
        )
        .await;
    let mut answer = Vec::new();
    tokio::time::timeout(WAIT, async {
        let mut buf = [0; 1024];
        while !String::from_utf8_lossy(&answer).contains("could not be read") {
            match client.read(&mut buf).await.unwrap() {
                0 => break,
                n => answer.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await
    .unwrap();
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
    assert!(
        answer.contains("request body could not be read"),
        "{answer}"
    );
    let record = h.usage_record().await;
    assert_eq!(record["status"], 400);
    assert_eq!(record["outcome"], "refused");
    assert!(record.get("upstream_error").is_none());
}

#[tokio::test]
async fn refusals_and_count_tokens_are_logged_without_usage() {
    let h = Harness::provider_key(ok()).await;
    let big = "x".repeat(2048);
    assert_eq!(
        h.send(messages(), big).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let record = h.usage_record().await;
    assert_eq!(record["status"], 413);
    assert_eq!(record["outcome"], "refused");
    assert_eq!(record["usage_status"], "none");

    read_body(
        h.send(Request::post("/v1/messages/count_tokens"), "{}")
            .await,
    )
    .await;
    let record = h.usage_record().await;
    assert_eq!(record["endpoint"], "/v1/messages/count_tokens");
    assert_eq!(record["outcome"], "completed");
    assert_eq!(record["usage_status"], "none");
}

/// Anthropic's API is HTTP/2, and its `count_tokens` reply has a
/// `content-length`; the client's HTTP/1.1 connection stops polling once that
/// many bytes are written, so the end-of-stream frame may never be read.
#[tokio::test]
async fn a_body_delivered_to_its_length_is_completed_before_its_stream_ends() {
    const COUNT: &str = r#"{"input_tokens":12}"#;
    let ca = TestCa::generate();
    // The senders stay alive: the upstream stream never ends.
    let open = Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = Arc::clone(&open);
    let handler: Handler = Arc::new(move |_| {
        let kept = Arc::clone(&kept);
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(1);
            tx.send(Bytes::from_static(COUNT.as_bytes())).await.unwrap();
            kept.lock().unwrap().push(tx);
            let mut res = json(channel(rx));
            res.headers_mut()
                .insert("content-length", COUNT.len().into());
            res
        })
    });
    let h = Harness::start_tls(handler, &ca, true).await;
    let res = h
        .send(Request::post("/v1/messages/count_tokens"), "{}")
        .await;
    assert_eq!(read_body(res).await, COUNT);
    let record = h.usage_record().await;
    assert_eq!(record["endpoint"], "/v1/messages/count_tokens");
    assert_eq!(record["outcome"], "completed");
    assert_eq!(open.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cache_diagnostics_log_keyed_prompt_hashes() {
    const BODY: &str = r#"{"model":"claude-test","system":"be brief","tools":[{"name":"Read"}],"messages":[{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#;
    let (handler, mut rx) = recording(|seen| seen.body);
    let key = [7u8; 32];
    let h = Harness::with_cache_diagnostics(handler, &key, "").await;

    let res = h.send(messages(), BODY).await;
    assert_eq!(read_body(res).await, "{}");
    assert_eq!(rx.recv().await.unwrap(), BODY, "body forwarded unchanged");
    let prompt = h.usage_record().await["prompt_hashes"].clone();
    assert_eq!(prompt["status"], "complete");
    assert_eq!(prompt["tools"]["count"], 1);
    assert_eq!(prompt["messages"], 1);
    assert_eq!(prompt["breakpoint_count"], 1);
    assert_eq!(prompt["breakpoints"][0]["section"], "messages");
    // Keyed: the tag of the hash, which only the key holder can recompute.
    let mut scanner = gateway_core::prompt::PromptScanner::new();
    scanner.feed(BODY.as_bytes());
    let system = scanner.finish().unwrap().system.unwrap().hash;
    let tag = HashKey::new(&key).unwrap().tag(system);
    assert_eq!(prompt["system"]["hash"], format!("{tag:016x}"));
    assert_eq!(prompt["system"]["hash"].as_str().unwrap().len(), 16);

    let res = h.send(messages(), "not json").await;
    assert_eq!(read_body(res).await, "{}");
    assert_eq!(rx.recv().await.unwrap(), "not json");
    assert_eq!(
        h.usage_record().await["prompt_hashes"],
        serde_json::json!({"status": "unknown"})
    );
}

#[tokio::test]
async fn usage_records_omit_prompt_without_diagnostics() {
    let h = Harness::provider_key(ok()).await;
    let res = h.send(messages(), r#"{"messages":[]}"#).await;
    read_body(res).await;
    assert!(h.usage_record().await.get("prompt_hashes").is_none());
}

#[tokio::test]
async fn cache_diagnostics_sample_whole_sessions() {
    let h = Harness::with_cache_diagnostics(ok(), &[7; 32], "sample_percent = 50").await;
    let sampled = |id: &String| gateway_core::route::sampled(Some(id.as_bytes()), 50);
    let ids: Vec<String> = (0..100).map(|i| format!("s-{i}")).collect();
    let (inside, outside) = (
        ids.iter().find(|id| sampled(id)).unwrap(),
        ids.iter().find(|id| !sampled(id)).unwrap(),
    );
    for (id, diagnosed) in [(Some(inside), true), (Some(outside), false), (None, false)] {
        let mut req = messages();
        req.headers_mut()
            .unwrap()
            .remove("x-claude-code-session-id");
        if let Some(id) = id {
            req = req.header("x-claude-code-session-id", id.as_str());
        }
        read_body(h.send(req, r#"{"messages":[]}"#).await).await;
        let record = h.usage_record().await;
        assert_eq!(record.get("prompt_hashes").is_some(), diagnosed, "{id:?}");
    }
}

#[tokio::test]
async fn priced_records_carry_cost_status_and_version() {
    let handler = answering(|| sse(full(StatusCode::OK, USAGE_STREAM)));
    let pricing = "\n[pricing]\nversion = \"test-1\"\n\
                   [[pricing.prices]]\nmodels = [\"claude-test\"]\ninput = \"1.00\"\noutput = \"10.00\"\n\
                   cache_read = \"0.10\"\n";
    let h = Harness::keyed(handler, pricing).await;
    read_body(h.send(messages(), "{}").await).await;
    let record = h.usage_record().await;
    // 10 input x $1 + 90 cache reads x $0.10 + 7 output x $10, per million.
    assert_eq!(record["cost_usd"], "0.000089");
    assert_eq!(record["cost_status"], "complete");
    assert_eq!(record["pricing_version"], "test-1");

    // No usage observed (a refused request): unknown, never $0.
    read_body(
        h.send(messages().header("content-length", "99999"), "{}")
            .await,
    )
    .await;
    let refused = h.usage_record().await;
    assert_eq!(refused["status"], 413);
    assert!(refused.get("cost_usd").is_none());
    assert_eq!(refused["cost_status"], "unknown");
}

#[tokio::test]
async fn checked_requests_record_what_each_target_would_reject() {
    let h = Harness::keyed(
        ok(),
        "\n[upgrade_check]\ntargets = [\"claude-opus-5-5\", \"claude-opus-4-7\"]\n",
    )
    .await;
    let body = r#"{"model":"claude-opus-4-6","thinking":{"type":"enabled","budget_tokens":2048},"tool_choice":{"type":"any"},"messages":[{"role":"user","content":"x"}]}"#;
    read_body(h.send(messages(), body).await).await;
    let record = h.usage_record().await;
    assert_eq!(
        record["upgrade_issues"],
        serde_json::json!({
            "claude-opus-5-5": ["budget_tokens", "forced_tool_choice"],
            "claude-opus-4-7": ["budget_tokens"],
        })
    );

    read_body(h.send(messages(), r#"{"model":"m","messages":[]}"#).await).await;
    assert_eq!(
        h.usage_record().await["upgrade_issues"],
        serde_json::json!({})
    );
}

#[tokio::test]
async fn non_streaming_json_responses_report_usage() {
    const BODY: &str = r#"{"id":"msg_1","type":"message","model":"claude-sonnet-5-5","content":[{"type":"tool_use","id":"t","name":"Read","input":{}}],"usage":{"input_tokens":12,"output_tokens":34}}"#;
    let h = Harness::provider_key(answering(|| json(full(StatusCode::OK, BODY)))).await;
    assert_eq!(read_body(h.send(messages(), "{}").await).await, BODY);
    let record = h.usage_record().await;
    assert_eq!(record["usage_status"], "complete");
    assert_eq!(record["model"], "claude-sonnet-5-5");
    assert_eq!(record["input_tokens"], 12);
    assert_eq!(record["output_tokens"], 34);
    assert_eq!(record["tool_calls"]["Read"], 1);
}

#[tokio::test]
async fn tool_calls_past_eight_names_go_under_other() {
    let names = ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"];
    let blocks: Vec<String> = names
        .iter()
        .map(|n| format!(r#"{{"type":"tool_use","id":"t{n}","name":"{n}","input":{{}}}}"#))
        .collect();
    let body = format!(
        r#"{{"type":"message","model":"m","content":[{}],"usage":{{"input_tokens":1,"output_tokens":1}}}}"#,
        blocks.join(",")
    );
    let body = Bytes::from(body);
    let h =
        Harness::provider_key(answering(move || json(full(StatusCode::OK, body.clone())))).await;
    read_body(h.send(messages(), "{}").await).await;
    let calls = &h.usage_record().await["tool_calls"];
    assert_eq!(calls.as_object().unwrap().len(), 9, "{calls}");
    assert_eq!(calls["(other)"], 2, "{calls}");
}
