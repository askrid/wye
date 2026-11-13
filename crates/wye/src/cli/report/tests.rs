use super::*;

/// The report of one usage log, all of it.
fn report(input: impl BufRead, paint: Paint) -> std::io::Result<String> {
    let mut summary = Summary::default();
    summary.read(input)?;
    Ok(render(&summary, paint))
}

/// `out` with runs of whitespace as one space: lines wrap at 80 columns.
fn flat(out: &str) -> String {
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

const LOG: &str = r#"{"event":"request","unix_ms":1000,"path":"/v1/messages","status":200,"outcome":"completed","duration_ms":1500.0,"subject":"alice","session_id":"session-aaaa-1","request_class":"main","prompt_id":"prompt-1111","usage_status":"complete","input_tokens":10,"output_tokens":100,"cache_creation_input_tokens":5000,"cache_read_input_tokens":0,"cache_miss":"unexplained"}
{"event":"request","unix_ms":3000,"path":"/v1/messages","status":200,"outcome":"completed","duration_ms":2000.0,"subject":"alice","session_id":"session-aaaa-1","request_class":"subagent","agent_type":"Explore","agent_id":"agent-1","prompt_id":"prompt-1111","usage_status":"complete","input_tokens":10,"output_tokens":50,"cache_creation_input_tokens":800,"cache_read_input_tokens":0,"cache_miss":"unexplained","tool_durations":[{"name":"Bash","ms":742},{"name":"Read","ms":9}],"tool_calls":{"Bash":2,"Grep":1}}
{"event":"request","unix_ms":5000,"path":"/v1/messages","status":200,"outcome":"completed","duration_ms":900.0,"session_id":"session-aaaa-1","request_class":"compaction","compaction":"auto","usage_status":"complete","input_tokens":20,"output_tokens":300,"cache_creation_input_tokens":0,"cache_read_input_tokens":6000}
{"event":"request","unix_ms":2000,"path":"/v1/messages","status":529,"outcome":"refused","duration_ms":0.1,"session_id":"session-bbbb-2","usage_status":"none"}
not json
wye: listening on http://127.0.0.1:8080
{"event":"usage_log_dropped","records":3}
"#;

#[test]
fn summarizes_sessions_classes_and_cache_misses() {
    let out = report(LOG.as_bytes(), Paint::OFF).unwrap();
    assert!(
        flat(&out).starts_with(
            "4 requests in 2 sessions (1 gateway notice and 2 other lines skipped); \
             outcomes: completed 3, refused 1 error statuses: 529 1 "
        ),
        "{out}"
    );
    // The first cache writes of session aaaa and of its subagent are
    // expected misses: each is a conversation of its own.
    assert!(
        flat(&out).contains(
            "0 unexplained; expected: 1 session start, 1 subagent start, 0 compactions, \
             0 after compaction"
        ),
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
    // Nothing is priced, so there is no cost column.
    assert!(!out.contains(" cost "), "{out}");
    // all: 4 requests, input 40, output 450, cache read 6000, write 5800.
    assert_eq!(row("all")[1..6], ["4", "40", "450", "6.0K", "5.8K"]);
    assert_eq!(row("all")[6], "50.7%");
    assert_eq!(row("compaction")[1], "1");
    assert_eq!(row("(unclassified)")[1], "1", "the request without hints");
    // "cache" and "time" over the columns they qualify.
    let header: Vec<&str> = out
        .lines()
        .skip_while(|l| !l.starts_with("By request class"))
        .skip(1)
        .take(2)
        .collect();
    assert_eq!(
        header.iter().map(|l| flat(l)).collect::<Vec<_>>(),
        [
            "cache cache time time",
            "class reqs input output read write hit model tools"
        ],
        "{out}"
    );
    // Each ends where the column it qualifies ends.
    let end = |line: &str, word: &str| line.find(word).map(|i| i + word.len());
    assert_eq!(end(header[0], "cache"), end(header[1], " read"));
    assert_eq!(
        header[0].rfind("time").map(|i| i + 4),
        end(header[1], " tools")
    );
    assert_eq!(row("Bash")[1..], ["2", "1", "0.7s"]);
    assert_eq!(row("Grep")[1..], ["1", "0", "-"]);
    // The subject on a line of its own, under the session's row.
    assert!(flat(&out).contains(" session- 3 "), "{out}");
    // Session bbbb has no tokens, so it is not ranked.
    assert_eq!(
        out.lines()
            .filter(|l| l.trim_start().starts_with("session-"))
            .count(),
        1,
        "{out}"
    );
    assert!(out.contains("\n    alice, 1 compaction\n"), "{out}");
    assert_eq!(row("prompt-1")[1], "2");
}

#[test]
fn a_session_starts_at_its_first_cache_write() {
    // Claude Code counts tokens, and some requests are refused, before the
    // first request that writes the prompt cache.
    let log = r#"{"event":"request","unix_ms":1000,"path":"/v1/messages/count_tokens","status":200,"outcome":"completed","session_id":"s","request_class":"auxiliary","usage_status":"none"}
{"event":"request","unix_ms":1100,"path":"/v1/messages","status":429,"outcome":"completed","session_id":"s","request_class":"auxiliary","usage_status":"none"}
{"event":"request","unix_ms":1200,"path":"/v1/messages","status":200,"outcome":"completed","session_id":"s","request_class":"auxiliary","usage_status":"complete","input_tokens":1172,"output_tokens":9,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}
{"event":"request","unix_ms":1300,"path":"/v1/messages","status":200,"outcome":"completed","session_id":"s","request_class":"main","usage_status":"complete","input_tokens":2,"output_tokens":4,"cache_creation_input_tokens":36820,"cache_read_input_tokens":0,"cache_miss":"unexplained"}
"#;
    let out = report(log.as_bytes(), Paint::OFF).unwrap();
    assert!(
        flat(&out).contains("0 unexplained; expected: 1 session start, 0 subagent starts"),
        "{out}"
    );
}

#[test]
fn a_subagent_misses_once_at_its_start() {
    let line = |at: u64, agent: &str| {
        format!(
            r#"{{"event":"request","unix_ms":{at},"path":"/v1/messages","status":200,"outcome":"completed","session_id":"s",{agent}"usage_status":"complete","input_tokens":2,"output_tokens":4,"cache_creation_input_tokens":98000,"cache_read_input_tokens":0,"cache_miss":"unexplained"}}"#
        )
    };
    let log = [
        line(1000, ""),
        line(2000, r#""agent_id":"a1","#),
        line(3000, r#""agent_id":"a2","#),
        // Its own prefix again: a miss nothing explains.
        line(4000, r#""agent_id":"a1","#),
    ]
    .join("\n");
    let out = report(log.as_bytes(), Paint::OFF).unwrap();
    assert!(
        flat(&out).contains("1 unexplained; expected: 1 session start, 2 subagent starts"),
        "{out}"
    );
}

#[test]
fn a_log_without_requests_says_where_they_come_from() {
    let out = report(&b"not json\n"[..], Paint::OFF).unwrap();
    assert!(
        out.starts_with("No request records (1 other line skipped)"),
        "{out}"
    );
    let empty = report(&b""[..], Paint::OFF).unwrap();
    assert!(
        empty.starts_with("No request records. The gateway writes"),
        "{empty}"
    );
    assert!(
        flat(&out).contains(
            "to stdout, or to the file named by `wye serve --usage-log`; \
             `wye service install` keeps them"
        ),
        "{out}"
    );
}

#[test]
fn a_request_without_tokens_has_no_hit_rate() {
    let failed = LOG.lines().nth(3).unwrap();
    let out = report(failed.as_bytes(), Paint::OFF).unwrap();
    assert!(out.starts_with("1 request in 1 session"), "{out}");
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
{"event":"request","outcome":"refused","request_class":"subagent","cost_status":"unknown"}
"#;
    let out = report(log.as_bytes(), Paint::OFF).unwrap();
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
    let unpriced = report(LOG.as_bytes(), Paint::OFF).unwrap();
    assert!(!unpriced.contains('$'), "{unpriced}");

    // Records written before `[pricing]` was added are missing from the sum.
    let mixed = format!(
        "{}{}",
        r#"{"event":"request","request_class":"main","cost_usd":"0.1","cost_status":"complete"}"#,
        "\n{\"event\":\"request\",\"request_class\":\"main\",\"output_tokens\":5}\n"
    );
    let out = report(mixed.as_bytes(), Paint::OFF).unwrap();
    assert!(out.contains("$0.1000+"), "{out}");
}

#[test]
fn lists_upgrade_issues_by_target_rule_and_client() {
    let log = r#"{"event":"request","subject":"svc-a","upgrade_issues":{"claude-opus-5-5":["budget_tokens","sampling"]}}
{"event":"request","subject":"svc-b","upgrade_issues":{"claude-opus-5-5":["budget_tokens"]}}
{"event":"request","subject":"svc-a","upgrade_issues":{}}
"#;
    let out = report(log.as_bytes(), Paint::OFF).unwrap();
    let line = |rule: &str| {
        out.lines()
            .find(|l| l.contains(rule))
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(line("budget_tokens"), "claude-opus-5-5 budget_tokens 2");
    assert_eq!(line("sampling"), "claude-opus-5-5 sampling 1");
    // The clients on the line below each issue.
    assert!(
        flat(&out).contains("budget_tokens 2 from svc-a, svc-b "),
        "{out}"
    );
}

#[test]
fn splits_cost_by_upstream_when_records_name_one() {
    let log = r#"{"event":"request","outcome":"completed","upstream":"team-a","cost_usd":"0.1","cost_status":"complete"}
{"event":"request","outcome":"completed","upstream":"team-b","cost_usd":"0.2","cost_status":"complete"}
{"event":"request","outcome":"completed","upstream":"team-a","cost_usd":"0.3","cost_status":"complete"}
"#;
    let out = report(log.as_bytes(), Paint::OFF).unwrap();
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
    // Without upstream names, or with one, no section: it would repeat
    // the "all" row.
    assert!(
        !report(LOG.as_bytes(), Paint::OFF)
            .unwrap()
            .contains("By upstream")
    );
    let one = log.replace("team-b", "team-a");
    let out = report(one.as_bytes(), Paint::OFF).unwrap();
    assert!(!out.contains("By upstream"), "{out}");
}

#[test]
fn counts_failovers_by_upstream_and_reason() {
    let log = r#"{"event":"request","outcome":"completed","upstream":"b","failovers":[{"upstream":"a","reason":"429"}]}
{"event":"request","outcome":"completed","upstream":"b","failovers":[{"upstream":"a","reason":"429"}]}
{"event":"request","outcome":"completed","upstream":"a","failovers":[{"upstream":"c","reason":"connect"}]}
"#;
    let out = report(log.as_bytes(), Paint::OFF).unwrap();
    let section = out.split("\nFailovers:").nth(1).unwrap();
    let rows: Vec<String> = section
        .lines()
        .skip(2)
        .take_while(|l| !l.trim().is_empty())
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(rows, ["a 429 2", "c connect 1"]);
    assert!(
        !report(LOG.as_bytes(), Paint::OFF)
            .unwrap()
            .contains("Failovers")
    );
}

#[test]
fn only_what_happened_is_reported() {
    // Every line a record, none a cache miss: no count of skipped lines,
    // and no cache-miss section.
    let log = r#"{"event":"request","v":2,"endpoint":"/v1/messages","outcome":"completed","session_id":"s","input_tokens":5,"output_tokens":1,"cache_read_input_tokens":90}
"#;
    let out = report(log.as_bytes(), Paint::OFF).unwrap();
    assert!(
        flat(&out).starts_with("1 request in 1 session; outcomes: completed 1 By request class "),
        "{out}"
    );
    assert!(!out.contains("skipped"), "{out}");
    assert!(!out.contains("Cache misses"), "{out}");
    // A notice alone is named alone.
    let noticed = format!("{log}wye: stopped\n");
    let out = report(noticed.as_bytes(), Paint::OFF).unwrap();
    assert!(
        out.starts_with("1 request in 1 session (1 gateway notice skipped);"),
        "{out}"
    );
}

#[test]
fn the_summary_line_counts_in_the_singular_too() {
    let out = report(
        r#"{"event":"request","outcome":"completed","session_id":"s-1"}
not a record
"#
        .as_bytes(),
        Paint::OFF,
    )
    .unwrap();
    assert!(
        out.starts_with("1 request in 1 session (1 other line skipped)"),
        "{out}"
    );
}

#[test]
fn a_count_stays_on_the_line_of_its_name() {
    // Every padding of the first name moves the break to another place.
    for pad in 0..40 {
        let first = format!("{}cancelled", "a".repeat(pad));
        let log: String = [first.as_str(), "completed", "upstream_error", "refused"]
            .iter()
            .map(|o| {
                format!("{{\"event\":\"request\",\"outcome\":\"{o}\",\"session_id\":\"s\"}}\n")
            })
            .collect();
        let out = report(log.as_bytes(), Paint::OFF).unwrap();
        let summary: Vec<&str> = out.lines().take_while(|l| !l.is_empty()).collect();
        for line in summary.iter().skip(1) {
            assert!(
                !line.starts_with(|c: char| c.is_ascii_digit()),
                "a count wrapped away from its name: {out}"
            );
        }
    }
}

#[test]
fn upstream_errors_are_counted_by_why() {
    let log = r#"{"event":"request","status":502,"outcome":"upstream_error","upstream_error":"connect","session_id":"s"}
{"event":"request","status":504,"outcome":"upstream_error","upstream_error":"timeout","session_id":"s"}
{"event":"request","status":502,"outcome":"upstream_error","upstream_error":"connect","session_id":"s"}
{"event":"request","status":200,"outcome":"truncated","session_id":"s"}
{"event":"request","v":3,"status":502,"outcome":"upstream_error","upstream_error":"connect_dns","session_id":"s"}
{"event":"request","v":3,"status":200,"outcome":"truncated","upstream_error":"idle_timeout","session_id":"s"}
"#;
    let out = flat(&report(log.as_bytes(), Paint::OFF).unwrap());
    assert!(
        out.contains(
            "outcomes: truncated 2, upstream_error 4 error statuses: 502 3, 504 1 \
             upstream errors: connect 2, connect_dns 1, idle_timeout 1, timeout 1 "
        ),
        "{out}"
    );
}

#[test]
fn outcomes_of_older_logs_are_shown_as_written() {
    let log = r#"{"event":"request","status":502,"outcome":"gateway_error","session_id":"s"}
{"event":"request","status":200,"outcome":"failed","session_id":"s"}
"#;
    let out = flat(&report(log.as_bytes(), Paint::OFF).unwrap());
    assert!(
        out.contains("outcomes: failed 1, gateway_error 1 "),
        "{out}"
    );
    assert!(!out.contains("upstream errors"), "{out}");
}

/// 2026-11-07 09:15:00 UTC.
const NOW: u64 = 1_794_042_900;

#[test]
fn a_window_reads_durations_back_from_now_and_utc_dates() {
    let window = |since| Window::parse(since, NOW);
    assert_eq!(window(None), Ok(Window::default()));
    assert_eq!(
        window(Some("7d")),
        Ok(Window {
            since: Some((NOW - 7 * 86_400) * 1000),
        })
    );
    assert_eq!(
        window(Some("2026-11-01")),
        Ok(Window {
            since: Some(1_793_491_200_000),
        })
    );
    assert_eq!(
        window(Some("1w")),
        Err(
            "--since 1w: write it as a time ago (30s, 10m, 8h, 7d) or a UTC date (2026-11-01)"
                .into()
        )
    );
    assert_eq!(
        window(Some("2026-02-30")).unwrap_err(),
        "--since 2026-02-30: write it as a time ago (30s, 10m, 8h, 7d) or a UTC date (2026-11-01)"
    );
}

#[test]
fn records_before_the_window_are_left_out_and_counted() {
    // `unix_ms` 1000 to 5000 in LOG; the window keeps 2000 on.
    let mut summary = Summary {
        window: Window { since: Some(2000) },
        ..Summary::default()
    };
    summary.read(LOG.as_bytes()).unwrap();
    let out = flat(&render(&summary, Paint::OFF));
    assert!(
        out.starts_with(
            "3 requests in 2 sessions since 1970-01-01 00:00:02 UTC (1 record outside it, \
             1 gateway notice and 2 other lines skipped); outcomes: completed 2, refused 1 "
        ),
        "{out}"
    );
    let mut empty = Summary {
        window: Window { since: Some(9000) },
        ..Summary::default()
    };
    empty.read(LOG.as_bytes()).unwrap();
    let out = flat(&render(&empty, Paint::OFF));
    assert!(
        out.starts_with(
            "No request records since 1970-01-01 00:00:09 UTC (4 records outside it, 1 \
             gateway notice and 2 other lines skipped)."
        ),
        "{out}"
    );
}

/// Logs without `v`, and of versions 1 (`path`) and 2 (`endpoint`), add up
/// alike: a log the gateway wrote before an upgrade still counts.
#[test]
fn several_logs_add_up_whatever_their_format_version() {
    let v1 = LOG.replace(r#"{"event":"request","#, r#"{"event":"request","v":1,"#);
    assert!(v1.contains(r#""v":1,"unix_ms":1000,"path":"#));
    let v2 = LOG
        .replace(r#"{"event":"request","#, r#"{"event":"request","v":2,"#)
        .replace(r#""path":"#, r#""endpoint":"#);
    assert!(v2.contains(r#""v":2,"unix_ms":1000,"endpoint":"#));
    let mut summary = Summary::default();
    for log in [LOG, &v1, &v2] {
        summary.read(log.as_bytes()).unwrap();
    }
    let out = flat(&render(&summary, Paint::OFF));
    assert!(
        out.starts_with(
            "12 requests in 2 sessions (3 gateway notices and 6 other lines skipped); \
             outcomes: completed 9, refused 3 "
        ),
        "{out}"
    );
}

#[test]
fn the_home_log_is_read_after_its_rotated_files() {
    let home = wye_testkit::TempDir::new("report-home");
    let env = {
        let home = home.display().to_string();
        move |name: &str| (name == "WYE_HOME").then(|| home.clone())
    };
    let missing = home_logs(&env).unwrap_err();
    assert!(
        missing.contains("`wye serve --usage-log ") && missing.contains("wye service install"),
        "{missing}"
    );
    let newer = home.join("usage-20261102T000000Z.jsonl");
    let older = home.join("usage-20261101T235959Z.jsonl");
    for file in [
        &newer,
        &older,
        &home.join("wye.log"),
        &home.join("usage.jsonl.bak"),
    ] {
        std::fs::write(file, "").unwrap();
    }
    assert_eq!(home_logs(&env).unwrap(), [older.clone(), newer.clone()]);
    let log = home.join("usage.jsonl");
    std::fs::write(&log, "").unwrap();
    assert_eq!(home_logs(&env).unwrap(), [older, newer, log]);
}

/// A record's strings are borrowed from the line, but one with an escape
/// is unescaped into a string of its own: its record is read, not skipped.
#[test]
fn a_record_with_escaped_strings_is_read_unescaped() {
    let log = r#"{"event":"request","outcome":"completed","subject":"al\"ice","session_id":"s-1","request_class":"main","input_tokens":1,"tool_calls":{"Bash":1}}
{"event":"request","outcome":"completed","subject":"bob","session_id":"s-1","request_class":"main","input_tokens":1,"tool_calls":{"Bash":2}}
"#;
    let out = report(log.as_bytes(), Paint::OFF).unwrap();
    assert!(
        flat(&out).starts_with("2 requests in 1 session; outcomes: completed 2 "),
        "{out}"
    );
    // The first subject seen is the session's.
    assert!(out.contains("\n    al\"ice\n"), "{out}");
    let row = |name: &str| {
        out.lines()
            .find(|l| l.trim_start().starts_with(name))
            .map(flat)
            .unwrap_or_else(|| panic!("no {name} row in\n{out}"))
    };
    assert!(row("main ").starts_with("main 2 "), "{out}");
    assert!(row("Bash ").starts_with("Bash 3 "), "{out}");
}
