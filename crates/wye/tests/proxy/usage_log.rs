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
    let h = Harness::with_cache_diagnostics(handler, &key).await;

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
