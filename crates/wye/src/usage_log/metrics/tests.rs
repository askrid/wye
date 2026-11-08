use super::*;

fn line(
    endpoint: &'static str,
    status: u16,
    ms: f64,
    model: Option<&'static str>,
) -> Line<'static> {
    Line {
        endpoint,
        status,
        duration_ms: ms,
        request_class: Some("main"),
        model,
        usage_status: "complete",
        input_tokens: Some(10),
        output_tokens: Some(5),
        cache_read_input_tokens: Some(0),
        ..Line::blank()
    }
}

#[test]
fn totals_are_cumulative_and_encode_as_otlp_json() {
    let mut metrics = Metrics::new(1);
    metrics.record(&line("/v1/messages", 200, 3.0, Some("m")));
    let priced = Line {
        cost: Some(1_500_000_000_000),
        ..line("/v1/messages", 200, 700.0, Some("m"))
    };
    metrics.record(&priced);
    metrics.record(&line("/v1/messages", 529, 1.0, None));
    let json: serde_json::Value =
        serde_json::from_slice(&metrics.encode(br#"{"attributes":[]}"#, 9, 4, 2)).unwrap();
    let list = json["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .clone();
    let metric = |name: &str| list.iter().find(|m| m["name"] == name).unwrap().clone();

    let requests = metric("wye.requests")["sum"]["dataPoints"].clone();
    assert_eq!(requests.as_array().unwrap().len(), 2);
    assert_eq!(requests[0]["asInt"], "2");
    assert_eq!(requests[0]["startTimeUnixNano"], "1");
    assert_eq!(requests[1]["attributes"][1]["value"]["intValue"], "529");

    let duration = metric("wye.request.duration")["histogram"]["dataPoints"][0].clone();
    assert_eq!(duration["count"], "3");
    assert_eq!(duration["sum"], 704.0);
    assert_eq!(
        duration["bucketCounts"][0], "2",
        "1 and 3 ms fall in the first bucket"
    );
    assert_eq!(
        duration["bucketCounts"][7], "1",
        "700 ms falls in the 1 s bucket"
    );

    let tokens = metric("wye.tokens")["sum"]["dataPoints"].clone();
    // Zero counts are skipped: input and output only.
    assert_eq!(tokens.as_array().unwrap().len(), 2);
    assert_eq!(tokens[0]["asInt"], "20");
    assert_eq!(tokens[0]["attributes"][2]["value"]["stringValue"], "main");

    let cost = metric("wye.cost")["sum"]["dataPoints"].clone();
    assert_eq!(cost.as_array().unwrap().len(), 1);
    assert_eq!(cost[0]["asDouble"], 1.5);
    assert_eq!(metric("wye.cost")["unit"], "USD");

    assert_eq!(
        metric("wye.records.dropped")["sum"]["dataPoints"][0]["asInt"],
        "2"
    );
    assert_eq!(
        metric("wye.requests.active")["gauge"]["dataPoints"][0]["asInt"],
        "4"
    );
}

#[test]
fn series_beyond_the_limit_are_counted_not_kept() {
    let mut metrics = Metrics::new(0);
    let routes: Vec<&'static str> = (0..MAX_SERIES + 5)
        .map(|i| &*Box::leak(format!("/r{i}").into_boxed_str()))
        .collect();
    for route in &routes {
        metrics.record(&line(route, 200, 1.0, None));
    }
    assert_eq!(metrics.requests.len(), MAX_SERIES);
    assert_eq!(metrics.overflow, 10, "5 request and 5 duration series");
}

#[test]
fn cache_misses_are_counted_by_reason() {
    let mut metrics = Metrics::new(0);
    for miss in [
        Some(CacheMiss::Unexplained),
        Some(CacheMiss::AfterCompaction),
        Some(CacheMiss::Unexplained),
        None,
    ] {
        metrics.record(&Line {
            cache_miss: miss,
            ..line("/v1/messages", 200, 1.0, None)
        });
    }
    let json: serde_json::Value =
        serde_json::from_slice(&metrics.encode(br#"{"attributes":[]}"#, 1, 0, 0)).unwrap();
    let misses = json["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "wye.cache.misses")
        .unwrap()["sum"]["dataPoints"]
        .clone();
    let counts: Vec<_> = misses
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["attributes"][0]["value"]["stringValue"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                p["asInt"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        counts,
        [
            ("compaction".into(), "0".into()),
            ("after_compaction".into(), "1".into()),
            ("unexplained".into(), "2".into())
        ]
    );
}

#[test]
fn tool_calls_are_summed_by_name() {
    let mut scanner = gateway_core::usage::MessagesUsageScanner::new();
    for name in ["Bash", "Read", "Bash"] {
        scanner.feed(
            format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"content_block\":{{\"type\":\"tool_use\",\"name\":\"{name}\",\"input\":{{}}}}}}\n\n"
            )
            .as_bytes(),
        );
    }
    let calls = scanner.take_tool_calls().unwrap();
    let mut metrics = Metrics::new(0);
    for _ in 0..2 {
        metrics.record(&Line {
            tool_calls: Some(crate::usage_log::line::ToolCallsField(&calls)),
            ..line("/v1/messages", 200, 1.0, None)
        });
    }
    assert_eq!(&*metrics.tool_calls[0].key, "Bash");
    assert_eq!(metrics.tool_calls[0].count, 4);
    assert_eq!(metrics.tool_calls[1].count, 2);
}

#[test]
fn route_upstreams_split_requests_tokens_and_costs() {
    let mut metrics = Metrics::new(1);
    for upstream in [Some("team-a"), Some("team-b"), Some("team-a")] {
        metrics.record(&Line {
            upstream,
            cost: Some(1_000_000),
            ..line("/v1/messages", 200, 3.0, Some("m"))
        });
    }
    let json: serde_json::Value =
        serde_json::from_slice(&metrics.encode(br#"{"attributes":[]}"#, 9, 0, 0)).unwrap();
    let list = json["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .clone();
    for name in ["wye.requests", "wye.tokens", "wye.cost"] {
        let points = list.iter().find(|m| m["name"] == name).unwrap()["sum"]["dataPoints"]
            .as_array()
            .unwrap()
            .clone();
        let upstreams: Vec<String> = points
            .iter()
            .map(|p| {
                let attributes = p["attributes"].as_array().unwrap();
                let a = attributes
                    .iter()
                    .find(|a| a["key"] == "wye.upstream")
                    .unwrap();
                a["value"]["stringValue"].as_str().unwrap().to_owned()
            })
            .collect();
        assert!(
            upstreams.contains(&"team-a".to_owned()),
            "{name}: {upstreams:?}"
        );
        assert!(
            upstreams.contains(&"team-b".to_owned()),
            "{name}: {upstreams:?}"
        );
    }
}

#[test]
fn upstream_errors_of_every_kind_are_one_outcome_series() {
    use crate::usage_log::{Outcome, UpstreamError};
    let mut metrics = Metrics::new(0);
    for error in [UpstreamError::Connect, UpstreamError::Reset] {
        metrics.record(&Line {
            outcome: Outcome::UpstreamError(error),
            ..line("/v1/messages", 502, 1.0, None)
        });
    }
    assert_eq!(metrics.requests.len(), 1);
    assert_eq!(metrics.requests[0].key.outcome, "upstream_error");
    assert_eq!(metrics.requests[0].count, 2);
    assert_eq!(metrics.durations.len(), 1);
}

#[test]
fn failovers_name_the_rejecting_upstream_apart_from_the_answering_one() {
    let rejected = [crate::usage_log::RejectedAttempt {
        upstream: "a".into(),
        reason: FailoverReason::RateLimited,
    }];
    let mut metrics = Metrics::new(0);
    metrics.record(&Line {
        upstream: Some("b"),
        failovers: &rejected,
        ..line("/v1/messages", 200, 1.0, None)
    });
    let json: serde_json::Value =
        serde_json::from_slice(&metrics.encode(br#"{"attributes":[]}"#, 1, 0, 0)).unwrap();
    let failovers = json["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "wye.failovers")
        .unwrap()
        .clone();
    assert_eq!(failovers["unit"], "{failover}");
    let attributes = &failovers["sum"]["dataPoints"][0]["attributes"];
    assert_eq!(attributes[0]["key"], "wye.failover.upstream");
    assert_eq!(attributes[0]["value"]["stringValue"], "a");
    assert_eq!(attributes[1]["key"], "wye.failover.reason");
    assert_eq!(attributes[1]["value"]["stringValue"], "429");
}
