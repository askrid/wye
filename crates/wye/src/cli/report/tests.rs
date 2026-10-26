#![allow(clippy::unwrap_used)]

use super::*;

const LOG: &str = r#"{"event":"request","unix_ms":1000,"path":"/v1/messages","status":200,"outcome":"completed","duration_ms":1500.0,"subject":"alice","session_id":"session-aaaa-1","request_class":"main","prompt_id":"prompt-1111","usage_status":"complete","input_tokens":10,"output_tokens":100,"cache_creation_input_tokens":5000,"cache_read_input_tokens":0,"cache_miss":"unexplained"}
{"event":"request","unix_ms":3000,"path":"/v1/messages","status":200,"outcome":"completed","duration_ms":2000.0,"subject":"alice","session_id":"session-aaaa-1","request_class":"subagent","agent_type":"Explore","prompt_id":"prompt-1111","usage_status":"complete","input_tokens":10,"output_tokens":50,"cache_creation_input_tokens":800,"cache_read_input_tokens":0,"cache_miss":"unexplained","tool_durations":[{"name":"Bash","ms":742},{"name":"Read","ms":9}],"tool_calls":{"Bash":2,"Grep":1}}
{"event":"request","unix_ms":5000,"path":"/v1/messages","status":200,"outcome":"completed","duration_ms":900.0,"session_id":"session-aaaa-1","request_class":"compaction","compaction":"auto","usage_status":"complete","input_tokens":20,"output_tokens":300,"cache_creation_input_tokens":0,"cache_read_input_tokens":6000}
{"event":"request","unix_ms":2000,"path":"/v1/messages","status":529,"outcome":"gateway_error","duration_ms":0.1,"session_id":"session-bbbb-2","usage_status":"not_observed"}
not json
{"event":"usage_log_dropped","count":3}
"#;

#[test]
fn summarizes_sessions_classes_and_cache_misses() {
    let out = report(LOG.as_bytes()).unwrap();
    assert!(
        out.starts_with("4 requests in 2 sessions (2 other lines skipped); outcomes: completed 3, gateway_error 1"),
        "{out}"
    );
    // The first request of session aaaa is an expected miss; the subagent's is not.
    assert!(
        out.contains("1 unexplained, 1 session starts, 0 compactions, 0 after compaction"),
        "{out}"
    );
    let row = |name: &str| {
        out.lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap_or_else(|| panic!("no {name} row in\n{out}"))
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    // all: 4 requests, input 40, output 450, cache read 6000, write 5800.
    assert_eq!(row("all")[1..6], ["4", "40", "450", "6000", "5800"]);
    assert_eq!(row("all")[6], "50.7%");
    assert_eq!(row("compaction")[1], "1");
    assert_eq!(row("(no")[2], "1", "the request without hints");
    assert_eq!(row("Bash")[1..], ["2", "1", "0.7s"]);
    assert_eq!(row("Grep")[1..], ["1", "0", "0.0s"]);
    let session = row("session-");
    assert_eq!(session[1], "(alice)");
    assert!(out.contains("1 compactions"), "{out}");
    assert_eq!(row("prompt-1")[1], "2");
}

#[test]
fn an_empty_log_reports_zeros() {
    let out = report(&b""[..]).unwrap();
    assert!(out.starts_with("0 requests in 0 sessions"), "{out}");
    assert!(
        out.contains(" - "),
        "no hit rate without prompt tokens: {out}"
    );
}

#[test]
fn sums_costs_exactly_and_marks_lower_bounds() {
    let log = r#"{"event":"request","outcome":"completed","request_class":"main","cost_usd":"0.1","cost_status":"complete"}
{"event":"request","outcome":"completed","request_class":"main","cost_usd":"0.2","cost_status":"complete"}
{"event":"request","outcome":"completed","request_class":"subagent","cost_usd":"0.05","cost_status":"partial"}
{"event":"request","outcome":"gateway_error","request_class":"subagent","cost_status":"unknown"}
"#;
    let out = report(log.as_bytes()).unwrap();
    let cost = |name: &str| {
        out.lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap()
            .split_whitespace()
            .nth(7)
            .unwrap()
            .to_owned()
    };
    // 0.1 + 0.2 is exact: no floating-point 0.30000000000000004.
    assert_eq!(cost("main"), "$0.3000");
    assert_eq!(cost("subagent"), "$0.0500+");
    assert_eq!(cost("all"), "$0.3500+");
    let unpriced = report(LOG.as_bytes()).unwrap();
    assert!(!unpriced.contains('$'), "{unpriced}");
}

#[test]
fn lists_upgrade_issues_by_target_rule_and_client() {
    let log = r#"{"event":"request","subject":"svc-a","upgrade_issues":{"claude-opus-5-5":["budget_tokens","sampling"]}}
{"event":"request","subject":"svc-b","upgrade_issues":{"claude-opus-5-5":["budget_tokens"]}}
{"event":"request","subject":"svc-a","upgrade_issues":{}}
"#;
    let out = report(log.as_bytes()).unwrap();
    let line = |rule: &str| {
        out.lines()
            .find(|l| l.contains(rule))
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(
        line("budget_tokens"),
        "claude-opus-5-5 budget_tokens 2 svc-a, svc-b"
    );
    assert_eq!(line("sampling"), "claude-opus-5-5 sampling 1 svc-a");
}

#[test]
fn splits_cost_by_upstream_when_records_name_one() {
    let log = r#"{"event":"request","outcome":"completed","upstream":"team-a","cost_usd":"0.1","cost_status":"complete"}
{"event":"request","outcome":"completed","upstream":"team-b","cost_usd":"0.2","cost_status":"complete"}
{"event":"request","outcome":"completed","upstream":"team-a","cost_usd":"0.3","cost_status":"complete"}
"#;
    let out = report(log.as_bytes()).unwrap();
    let section = out.split("\nBy upstream\n").nth(1).unwrap();
    let cost = |name: &str| {
        section
            .lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap()
            .split_whitespace()
            .nth(7)
            .unwrap()
            .to_owned()
    };
    assert_eq!(cost("team-a"), "$0.4000");
    assert_eq!(cost("team-b"), "$0.2000");
    // Without upstream names, no section.
    assert!(!report(LOG.as_bytes()).unwrap().contains("By upstream"));
}
