//! OTLP export of usage records, metrics, and spans, beside the usage log.

use super::{harness::*, usage_log::USAGE_STREAM};

#[tokio::test]
async fn usage_records_are_exported_as_otlp_logs() {
    let (collector, mut collected) = recording(|seen| seen);
    let mut otlp_headers = HeaderMap::new();
    otlp_headers.insert("api-key", "telemetry-secret".parse().unwrap());
    let mut h = Harness::with_telemetry(
        answering(|| sse(full(StatusCode::OK, USAGE_STREAM))),
        collector,
        "service_name = \"wye-test\"",
        otlp_headers,
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
    h.shut_down().await;
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
    let metric = |name: &str| otlp_metric(&metrics.body, name);
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
    assert!(attribute("wye.upstream_error").is_none());
    assert_eq!(
        attribute("gen_ai.provider.name").unwrap()["stringValue"],
        "anthropic"
    );
    assert!(attribute("wye.response_headers_ms").unwrap()["doubleValue"].is_number());
    assert!(record["timeUnixNano"].as_str().unwrap().len() >= 19);
}

/// The metric `name` in the OTLP metrics export `export`.
fn otlp_metric(export: &[u8], name: &str) -> serde_json::Value {
    let export: serde_json::Value = serde_json::from_slice(export).unwrap();
    let metrics = export["resourceMetrics"][0]["scopeMetrics"][0]["metrics"].as_array();
    let metric = metrics.unwrap().iter().find(|m| m["name"] == name);
    metric.unwrap_or_else(|| panic!("no {name}")).clone()
}

/// Log records in an OTLP logs export; other exports hold none.
fn exported_records(seen: &Seen) -> usize {
    if seen.uri.path() != "/v1/logs" {
        return 0;
    }
    let body: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
    body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array()
        .map_or(0, Vec::len)
}

/// The process reading stdout exits: records still reach OTLP, and the
/// lines the log could not take are counted as lost.
#[tokio::test]
async fn a_closed_log_leaves_otlp_export_running() {
    let (collector, mut collected) = recording(|seen| seen);
    let mut h = Harness::with_telemetry(ok(), collector, "", HeaderMap::new()).await;
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
            logs = exported_records(&seen);
        }
        exported += logs;
    }
    assert_eq!(exported, 2);

    // Shutdown exports the cumulative metrics once more.
    h.shut_down().await;
    let mut metrics = None;
    while let Ok(seen) = collected.try_recv() {
        if seen.uri.path() == "/v1/metrics" {
            metrics = Some(seen);
        }
    }
    let lost = otlp_metric(
        &metrics.expect("metrics exported").body,
        "wye.records.dropped",
    );
    assert_eq!(lost["sum"]["dataPoints"][0]["asInt"], "2", "{lost}");
}

/// A log nobody reads fills its pipe and its write waits: records still reach
/// OTLP. This harness's pipe holds 64 KiB, far less than these records.
#[tokio::test]
async fn a_blocked_log_leaves_otlp_export_running() {
    const REQUESTS: usize = 800;
    let (collector, mut collected) = recording(|seen| seen);
    let h = Harness::with_telemetry(ok(), collector, "", HeaderMap::new()).await;
    for _ in 0..REQUESTS {
        read_body(h.send(messages(), "{}").await).await;
    }
    let mut exported = 0;
    while exported < REQUESTS {
        let seen = tokio::time::timeout(WAIT, collected.recv())
            .await
            .expect("every record exported while the log is blocked")
            .unwrap();
        exported += exported_records(&seen);
    }
    assert_eq!(exported, REQUESTS);
}

/// OTLP keeps a blocked log no smaller than the queue: records taken off it
/// for export while the write waits are kept for the log too.
#[tokio::test]
async fn a_blocked_log_with_otlp_keeps_what_the_queue_would() {
    const REQUESTS: usize = 1000;
    let (collector, _collected) = recording(|seen| seen);
    let h = Harness::with_telemetry(ok(), collector, "", HeaderMap::new()).await;
    for _ in 0..REQUESTS {
        read_body(h.send(messages(), "{}").await).await;
    }
    let mut logged = 0;
    while logged < REQUESTS {
        let record = h.usage_record().await;
        assert_eq!(record["event"], "request", "{record}");
        logged += 1;
    }
}

/// At shutdown, a blocked log does not hold OTLP back: what was counted is
/// exported when the queue closes, not after a write that may never end.
#[tokio::test]
async fn shutdown_exports_metrics_while_the_log_is_blocked() {
    const REQUESTS: usize = 400;
    let (collector, mut collected) = recording(|seen| seen);
    let mut h = Harness::with_telemetry(ok(), collector, "", HeaderMap::new()).await;
    for _ in 0..REQUESTS {
        read_body(h.send(messages(), "{}").await).await;
    }
    h.begin_shutdown();
    // The periodic export is 10 s after start; this one must come first.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let seen = tokio::time::timeout_at(deadline, collected.recv())
            .await
            .expect("metrics exported at shutdown")
            .unwrap();
        if seen.uri.path() != "/v1/metrics" {
            continue;
        }
        let requests = otlp_metric(&seen.body, "wye.requests");
        assert_eq!(
            requests["sum"]["dataPoints"][0]["asInt"],
            REQUESTS.to_string(),
            "{requests}"
        );
        break;
    }
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
    let mut h = Harness::with_telemetry(ok(), collector, "", HeaderMap::new()).await;
    read_body(h.send(messages(), "{}").await).await;
    h.usage_record().await;
    h.shut_down().await;
    // The shutdown summary follows the request's record.
    let summary = h.usage_record().await;
    assert_eq!(summary["event"], "otlp_dropped", "{summary}");
    assert_eq!(summary["v"], 2, "{summary}");
    assert_eq!(summary["records"], 1, "{summary}");
}
