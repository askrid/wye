use proptest::prelude::*;

use super::*;

/// A Codex-like stream: created, a reasoning item, a function call, a hosted
/// web search, text deltas, and the terminal event with the full output.
fn stream(output_text: &str, terminal: &str, usage: &str, tier: &str) -> String {
    format!(
        "event: response.created\n\
         data: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-5.5-codex\",\"service_tier\":\"auto\",\"output\":[],\"usage\":null}}}}\n\n\
         event: response.output_item.added\n\
         data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[]}}}}\n\n\
         event: response.output_item.added\n\
         data: {{\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{{\"id\":\"fc_1\",\"type\":\"function_call\",\"status\":\"in_progress\",\"arguments\":\"\",\"call_id\":\"call_1\",\"name\":\"shell\"}}}}\n\n\
         event: response.output_item.added\n\
         data: {{\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{{\"id\":\"ws_1\",\"type\":\"web_search_call\",\"status\":\"in_progress\"}}}}\n\n\
         event: response.output_text.delta\n\
         data: {{\"type\":\"response.output_text.delta\",\"delta\":\"he\\\"llo\"}}\n\n\
         event: {terminal}\n\
         data: {{\"type\":\"{terminal}\",\"response\":{{\"id\":\"resp_1\",\"metadata\":{{\"usage\":{{\"input_tokens\":1}}}},\"model\":\"gpt-5.5-codex-2026-10-01\",\"output\":[{{\"type\":\"message\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{output_text}\"}}]}}],\"service_tier\":\"{tier}\",\"usage\":{usage}}}}}\n\n"
    )
}

const USAGE: &str = r#"{"input_tokens":1000,"input_tokens_details":{"cached_tokens":800},"output_tokens":200,"output_tokens_details":{"reasoning_tokens":150},"total_tokens":1200}"#;

fn scan(bytes: &[u8]) -> ResponsesUsageScanner {
    let mut scanner = ResponsesUsageScanner::new();
    scanner.feed(bytes);
    scanner
}

#[test]
fn reads_usage_model_and_tool_calls() {
    let s = scan(stream("done", "response.completed", USAGE, "default").as_bytes());
    assert_eq!(s.status(), UsageStatus::Complete);
    assert_eq!(s.model(), Some("gpt-5.5-codex-2026-10-01"));
    let usage = s.usage();
    // OpenAI's input includes cached tokens; ours does not.
    assert_eq!(usage.input_tokens, Some(200));
    assert_eq!(usage.cache_read_input_tokens, Some(800));
    assert_eq!(usage.output_tokens, Some(200));
    assert_eq!(usage.cache_creation_input_tokens, None);
    // The hosted web search is billed per call.
    assert!(usage.unpriced);
    let tools: Vec<_> = s.tools.as_deref().unwrap().iter().collect();
    assert_eq!(tools, [("shell", 1), ("web_search", 1)]);
}

#[test]
fn a_service_tier_other_than_default_is_unpriced() {
    let no_search = |tier: &str| {
        stream("x", "response.completed", USAGE, tier)
            .replace("\"type\":\"web_search_call\"", "\"type\":\"message\"")
    };
    assert!(!scan(no_search("default").as_bytes()).usage().unpriced);
    assert!(scan(no_search("priority").as_bytes()).usage().unpriced);
    assert!(scan(no_search("flex").as_bytes()).usage().unpriced);
}

#[test]
fn a_huge_terminal_event_is_read_in_constant_memory() {
    // 3 MiB of output, with text that looks like a usage member.
    let decoy = r#"\"usage\":{\"input_tokens\":999999}"#;
    let text = format!("{decoy}{}", "x".repeat(3 << 20));
    let s = scan(stream(&text, "response.completed", USAGE, "default").as_bytes());
    assert_eq!(s.status(), UsageStatus::Complete);
    assert_eq!(s.usage().input_tokens, Some(200));
    // Only the usage object was kept, never the line.
    assert!(s.item.capacity() <= MAX_ITEM_LINE_BYTES);
}

#[test]
fn incomplete_and_failed_responses() {
    let s = scan(stream("x", "response.incomplete", USAGE, "default").as_bytes());
    assert_eq!(s.status(), UsageStatus::Complete);
    assert_eq!(s.usage().output_tokens, Some(200));
    let failed = scan(stream("x", "response.failed", "null", "default").as_bytes());
    assert_eq!(failed.status(), UsageStatus::Unknown);
    assert_eq!(failed.model(), Some("gpt-5.5-codex-2026-10-01"));
}

#[test]
fn a_stream_cut_before_the_terminal_event_is_unknown() {
    let full = stream("x", "response.completed", USAGE, "default");
    let cut = full.split("event: response.completed").next().unwrap();
    let s = scan(cut.as_bytes());
    assert_eq!(s.status(), UsageStatus::Unknown);
    assert_eq!(s.model(), Some("gpt-5.5-codex"), "from response.created");
}

#[test]
fn malformed_or_oversized_usage_is_unknown() {
    let s = scan(
        stream(
            "x",
            "response.completed",
            r#"{"input_tokens":"many"}"#,
            "default",
        )
        .as_bytes(),
    );
    assert_eq!(s.status(), UsageStatus::Unknown);
    let big = format!(r#"{{"input_tokens":1,"pad":"{}"}}"#, "p".repeat(4096));
    let s = scan(stream("x", "response.completed", &big, "default").as_bytes());
    assert_eq!(s.status(), UsageStatus::Unknown);
}

#[test]
fn crlf_line_ends_are_accepted() {
    let crlf = stream("x", "response.completed", USAGE, "default").replace('\n', "\r\n");
    let s = scan(crlf.as_bytes());
    assert_eq!(s.status(), UsageStatus::Complete);
    assert_eq!(s.usage().cache_read_input_tokens, Some(800));
}

proptest! {
    /// Chunk boundaries anywhere, even inside UTF-8 or the field name,
    /// change nothing.
    #[test]
    fn any_chunking_reads_the_same(cuts in proptest::collection::vec(0usize..2000, 0..12)) {
        let full = stream("héllo wörld", "response.completed", USAGE, "default");
        let bytes = full.as_bytes();
        let whole = scan(bytes);
        let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c % (bytes.len() + 1)).collect();
        cuts.sort_unstable();
        let mut scanner = ResponsesUsageScanner::new();
        let mut at = 0;
        for cut in cuts {
            scanner.feed(&bytes[at..cut]);
            at = cut;
        }
        scanner.feed(&bytes[at..]);
        prop_assert_eq!(scanner.usage(), whole.usage());
        prop_assert_eq!(scanner.model(), whole.model());
        prop_assert_eq!(scanner.status(), UsageStatus::Complete);
        prop_assert_eq!(scanner.tools, whole.tools);
    }
}
