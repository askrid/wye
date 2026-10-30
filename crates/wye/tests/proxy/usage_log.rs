//! Per-request usage records.

use super::harness::*;

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
async fn an_empty_response_is_completed() {
    // hyper sends an empty body with the headers and never polls it.
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "") }))).await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(read_body(res).await.is_empty());
    assert_eq!(h.usage_record().await["outcome"], "completed");
}

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
    let h = Harness::api_key(Arc::new(|_| {
        Box::pin(async {
            let mut res = full(StatusCode::OK, MISS);
            res.headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            res
        })
    }))
    .await;
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
    assert_eq!(record["context_compacted"], "manual");
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
    for field in [
        "request_class",
        "agent_id",
        "prompt_id",
        "context_compacted",
    ] {
        assert!(record.get(field).is_none(), "{field}");
    }
    assert_eq!(record["cache_miss"], "unexplained");
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
async fn a_client_that_leaves_before_the_headers_is_recorded() {
    use tokio::io::AsyncWriteExt;
    // The upstream never answers; the client stops waiting.
    let h = Harness::api_key(Arc::new(|_| Box::pin(std::future::pending()))).await;
    let mut client = tokio::net::TcpStream::connect(h.gateway).await.unwrap();
    client
        .write_all(b"POST /v1/messages HTTP/1.1\r\nhost: wye\r\ncontent-length: 2\r\n\r\n{}")
        .await
        .unwrap();
    tokio::time::timeout(WAIT, async {
        while h.upstream_hits.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    drop(client);

    let record = h.usage_record().await;
    assert_eq!(record["outcome"], "cancelled");
    assert_eq!(record["status"], 499);
    assert_eq!(record["path"], "/v1/messages");
}

/// A client body that breaks mid-upload (bad chunk framing here) is the
/// client's error, `400`, never an upstream failure (`502`).
#[tokio::test]
async fn a_broken_client_body_is_the_clients_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") }))).await;
    let mut client = tokio::net::TcpStream::connect(h.gateway).await.unwrap();
    client
        .write_all(
            b"POST /v1/messages HTTP/1.1\r\nhost: wye\r\ntransfer-encoding: chunked\r\n\r\n\
              2\r\n{}\r\nzz\r\n",
        )
        .await
        .unwrap();
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
    assert_eq!(record["outcome"], "gateway_error");
}

#[tokio::test]
async fn gateway_errors_and_count_tokens_are_logged_without_usage() {
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
            let mut res = Response::new(Either::Right(ChannelBody(rx)));
            res.headers_mut()
                .insert("content-type", "application/json".parse().unwrap());
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
    assert_eq!(record["path"], "/v1/messages/count_tokens");
    assert_eq!(record["outcome"], "completed");
    assert_eq!(open.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cache_diagnostics_log_keyed_prompt_hashes() {
    const BODY: &str = r#"{"model":"claude-test","system":"be brief","tools":[{"name":"Read"}],"messages":[{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#;
    let (tx, mut rx) = mpsc::channel::<Bytes>(2);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(seen.body).await.unwrap();
            full(StatusCode::OK, "{}")
        })
    });
    let key = [7u8; 32];
    let h = Harness::with_cache_diagnostics(handler, &key, "").await;

    let res = h.send(messages(), BODY).await;
    assert_eq!(read_body(res).await, "{}");
    assert_eq!(rx.recv().await.unwrap(), BODY, "body forwarded unchanged");
    let prompt = h.usage_record().await["prompt"].clone();
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
        h.usage_record().await["prompt"],
        serde_json::json!({"status": "unknown"})
    );
}

#[tokio::test]
async fn usage_records_omit_prompt_without_diagnostics() {
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") }))).await;
    let res = h.send(messages(), r#"{"messages":[]}"#).await;
    read_body(res).await;
    assert!(h.usage_record().await.get("prompt").is_none());
}

#[tokio::test]
async fn cache_diagnostics_sample_whole_sessions() {
    let h = Harness::with_cache_diagnostics(
        Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") })),
        &[7; 32],
        "sample_percent = 50",
    )
    .await;
    let sampled = |id: &String| gateway_core::prompt::session_sampled(Some(id.as_bytes()), 50);
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
        assert_eq!(record.get("prompt").is_some(), diagnosed, "{id:?}");
    }
}

#[tokio::test]
async fn usage_records_are_exported_as_otlp_logs() {
    let (collected_tx, mut collected) = mpsc::channel::<Seen>(8);
    let collector: Handler = Arc::new(move |seen: Seen| {
        let tx = collected_tx.clone();
        Box::pin(async move {
            tx.send(seen).await.unwrap();
            full(StatusCode::OK, "{}")
        })
    });
    let collector = spawn_upstream(collector, Arc::new(AtomicUsize::new(0)), None, None).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(
        Arc::new(|_| {
            Box::pin(async {
                let mut res = full(StatusCode::OK, USAGE_STREAM);
                res.headers_mut()
                    .insert("content-type", "text/event-stream".parse().unwrap());
                res
            })
        }),
        Arc::clone(&hits),
        None,
        None,
    )
    .await;
    let mut otlp_headers = HeaderMap::new();
    otlp_headers.insert("api-key", "telemetry-secret".parse().unwrap());
    let resources = Resources {
        upstreams: vec![UpstreamResources::new(
            UpstreamAuth::api_key(API_KEY).unwrap(),
        )],
        verifier: None,
        prompt_key: None,
        otlp_headers,
    };
    let mut h = Harness::launch(
        &format!("http://{upstream}"),
        "",
        &format!(
            "api_key_env = \"UNUSED\"\n[telemetry]\notlp_endpoint = \"http://{collector}\"\nservice_name = \"wye-test\""
        ),
        resources,
        hits,
    )
    .await;
    const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let res = h.send(messages().header("traceparent", PARENT), "{}").await;
    read_body(res).await;
    h.usage_record().await;
    // A partly filled batch goes out within the one-second flush interval;
    // shutdown exports the cumulative metrics once more.
    let mut exports = std::collections::HashMap::new();
    while exports.len() < 2 {
        let seen = tokio::time::timeout(WAIT, collected.recv())
            .await
            .unwrap()
            .unwrap();
        exports.insert(seen.uri.path().to_owned(), seen);
    }
    h.begin_shutdown();
    tokio::time::timeout(WAIT, &mut h.served)
        .await
        .unwrap()
        .unwrap();
    while let Ok(seen) = collected.try_recv() {
        exports.insert(seen.uri.path().to_owned(), seen);
    }
    let export = exports.remove("/v1/logs").expect("logs exported");
    let traces = exports.remove("/v1/traces").expect("traces exported");
    let metrics = exports.remove("/v1/metrics").expect("metrics exported");
    let traces: serde_json::Value = serde_json::from_slice(&traces.body).unwrap();
    let span = &traces["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    assert_eq!(span["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(span["parentSpanId"], "00f067aa0ba902b7");
    assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
    assert_eq!(span["name"], "POST /v1/messages");
    assert_eq!(span["kind"], 2);
    assert_eq!(span["status"]["code"], 0);
    assert_eq!(span["events"][0]["name"], "response headers");
    let start: u64 = span["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
    let end: u64 = span["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
    assert!(start <= end);
    assert_eq!(metrics.uri.path(), "/v1/metrics");
    let metrics: serde_json::Value = serde_json::from_slice(&metrics.body).unwrap();
    let list = metrics["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .clone();
    let metric = |name: &str| list.iter().find(|m| m["name"] == name).unwrap().clone();
    assert_eq!(metric("wye.requests")["sum"]["dataPoints"][0]["asInt"], "1");
    let tokens = metric("wye.tokens")["sum"]["dataPoints"].clone();
    assert!(
        tokens
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["asInt"] == "90"
                && p["attributes"][0]["value"]["stringValue"] == "cache_read")
    );
    assert_eq!(
        metric("wye.requests.active")["gauge"]["dataPoints"][0]["asInt"],
        "0"
    );
    assert_eq!(export.method, Method::POST);
    assert_eq!(export.uri.path(), "/v1/logs");
    assert_eq!(export.headers["content-type"], "application/json");
    assert_eq!(export.headers["api-key"], "telemetry-secret");
    let body: serde_json::Value = serde_json::from_slice(&export.body).unwrap();
    let resource = &body["resourceLogs"][0];
    assert_eq!(
        resource["resource"]["attributes"][0],
        serde_json::json!({"key": "service.name", "value": {"stringValue": "wye-test"}})
    );
    let record = &resource["scopeLogs"][0]["logRecords"][0];
    let attribute = |key: &str| {
        record["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == key)
            .map(|a| a["value"].clone())
    };
    assert_eq!(
        attribute("gen_ai.usage.input_tokens").unwrap()["intValue"],
        "10"
    );
    assert_eq!(
        attribute("gen_ai.usage.output_tokens").unwrap()["intValue"],
        "7"
    );
    assert_eq!(
        attribute("gen_ai.usage.cache_read.input_tokens").unwrap()["intValue"],
        "90"
    );
    assert_eq!(
        attribute("gen_ai.response.model").unwrap()["stringValue"],
        "claude-test"
    );
    assert_eq!(attribute("session.id").unwrap()["stringValue"], "s-1");
    assert_eq!(
        attribute("http.response.status_code").unwrap()["intValue"],
        "200"
    );
    assert_eq!(
        attribute("wye.outcome").unwrap()["stringValue"],
        "completed"
    );
    assert!(record["timeUnixNano"].as_str().unwrap().len() >= 19);
}

#[tokio::test]
async fn priced_records_carry_cost_status_and_version() {
    let handler: Handler = Arc::new(|_| {
        Box::pin(async {
            let mut res = full(StatusCode::OK, USAGE_STREAM);
            res.headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            res
        })
    });
    let pricing = "api_key_env = \"UNUSED\"\n\n[pricing]\nversion = \"test-1\"\n\
                   [[pricing.models]]\nmodels = [\"claude-test\"]\ninput = \"1.00\"\noutput = \"10.00\"\n\
                   cache_read = \"0.10\"\n";
    let h = Harness::start(handler, pricing, UpstreamAuth::api_key(API_KEY).unwrap()).await;
    read_body(h.send(messages(), "{}").await).await;
    let record = h.usage_record().await;
    // 10 input x $1 + 90 cache reads x $0.10 + 7 output x $10, per million.
    assert_eq!(record["cost_usd"], "0.000089");
    assert_eq!(record["cost_status"], "complete");
    assert_eq!(record["pricing_version"], "test-1");

    // No usage observed (a refused request): unknown, never $0.
    read_body(h.send(Request::get("/v1/nothing"), "").await).await;
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
    let h = Harness::start(
        Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") })),
        "api_key_env = \"UNUSED\"\n\n[upgrade_check]\ntargets = [\"claude-opus-5-5\", \"claude-opus-4-7\"]\n",
        UpstreamAuth::api_key(API_KEY).unwrap(),
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
    let h = Harness::api_key(Arc::new(|_| {
        Box::pin(async {
            let mut res = full(StatusCode::OK, BODY);
            res.headers_mut()
                .insert("content-type", "application/json".parse().unwrap());
            res
        })
    }))
    .await;
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
    let body: &'static str = Box::leak(
        format!(
            r#"{{"type":"message","model":"m","content":[{}],"usage":{{"input_tokens":1,"output_tokens":1}}}}"#,
            blocks.join(",")
        )
        .into_boxed_str(),
    );
    let h = Harness::api_key(Arc::new(move |_| {
        Box::pin(async move {
            let mut res = full(StatusCode::OK, body);
            res.headers_mut()
                .insert("content-type", "application/json".parse().unwrap());
            res
        })
    }))
    .await;
    read_body(h.send(messages(), "{}").await).await;
    let calls = &h.usage_record().await["tool_calls"];
    assert_eq!(calls.as_object().unwrap().len(), 9, "{calls}");
    assert_eq!(calls["(other)"], 2, "{calls}");
}

/// The process reading stderr exits: records still reach OTLP, and the
/// lines the log could not take are counted as lost.
#[tokio::test]
async fn a_closed_log_leaves_otlp_export_running() {
    let (collected_tx, mut collected) = mpsc::channel::<Seen>(16);
    let collector: Handler = Arc::new(move |seen: Seen| {
        let tx = collected_tx.clone();
        Box::pin(async move {
            let _ = tx.send(seen).await;
            full(StatusCode::OK, "{}")
        })
    });
    let collector = spawn_upstream(collector, Arc::new(AtomicUsize::new(0)), None, None).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(
        Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") })),
        Arc::clone(&hits),
        None,
        None,
    )
    .await;
    let resources = with_upstreams(vec![UpstreamResources::new(
        UpstreamAuth::api_key(API_KEY).unwrap(),
    )]);
    let h = Harness::launch(
        &format!("http://{upstream}"),
        "",
        &format!("api_key_env = \"UNUSED\"\n[telemetry]\notlp_endpoint = \"http://{collector}\""),
        resources,
        hits,
    )
    .await;
    h.close_usage_log().await;
    // Each record is exported once the log has failed its write.
    let mut exported = 0;
    for _ in 0..2 {
        read_body(h.send(messages(), "{}").await).await;
        let mut logs = 0;
        while logs == 0 {
            let seen = tokio::time::timeout(WAIT, collected.recv())
                .await
                .expect("a log export after the log closed")
                .unwrap();
            if seen.uri.path() == "/v1/logs" {
                let body: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
                logs = body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .map_or(0, Vec::len);
            }
        }
        exported += logs;
    }
    assert_eq!(exported, 2);
}

#[tokio::test]
async fn spans_a_collector_refuses_are_counted_as_lost() {
    // A collector without a traces pipeline: logs in, traces refused.
    let collector: Handler = Arc::new(|seen: Seen| {
        Box::pin(async move {
            match seen.uri.path() {
                "/v1/traces" => full(StatusCode::NOT_FOUND, "{}"),
                _ => full(StatusCode::OK, "{}"),
            }
        })
    });
    let collector = spawn_upstream(collector, Arc::new(AtomicUsize::new(0)), None, None).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(
        Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") })),
        Arc::clone(&hits),
        None,
        None,
    )
    .await;
    let resources = with_upstreams(vec![UpstreamResources::new(
        UpstreamAuth::api_key(API_KEY).unwrap(),
    )]);
    let mut h = Harness::launch(
        &format!("http://{upstream}"),
        "",
        &format!("api_key_env = \"UNUSED\"\n[telemetry]\notlp_endpoint = \"http://{collector}\""),
        resources,
        hits,
    )
    .await;
    read_body(h.send(messages(), "{}").await).await;
    h.usage_record().await;
    h.begin_shutdown();
    tokio::time::timeout(WAIT, &mut h.served)
        .await
        .unwrap()
        .unwrap();
    // The shutdown summary follows the request's record.
    let summary = h.usage_record().await;
    assert_eq!(summary["event"], "otlp_dropped", "{summary}");
    assert_eq!(summary["records"], 1, "{summary}");
}
