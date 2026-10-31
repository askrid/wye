use proptest::prelude::*;

use super::*;
use crate::usage::tests::{arbitrary_chunks, feed_cut};

const MESSAGE: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"a \"usage\":{\"input_tokens\":99} lookalike"},{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls","name":"not-a-tool","usage":{"input_tokens":7}}}],"stop_reason":"tool_use","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":5,"output_tokens":20,"service_tier":"standard"}}"#;

const RESPONSE: &str = r#"{"id":"resp_1","object":"response","status":"completed","model":"gpt-5.5-2026-10-01","output":[{"type":"reasoning","summary":[]},{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"usage\":1}"},{"type":"web_search_call","status":"completed"},{"type":"message","content":[{"type":"output_text","text":"hi"}]}],"service_tier":"default","usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":600},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":30}}}"#;

fn scan(api: Api, body: &str) -> JsonUsageScanner {
    let mut s = JsonUsageScanner::new(api);
    s.feed(body.as_bytes());
    s
}

#[test]
fn reads_a_messages_body() {
    let mut s = scan(Api::Messages, MESSAGE);
    assert_eq!(s.status(), UsageStatus::Complete);
    assert_eq!(s.model(), Some("claude-opus-5-5"));
    let usage = s.usage();
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.cache_read_input_tokens, Some(5));
    assert_eq!(usage.output_tokens, Some(20));
    assert!(!usage.unpriced);
    let tools = s.take_tool_calls().unwrap();
    assert_eq!(tools.iter().collect::<Vec<_>>(), [("Bash", 1)]);
}

#[test]
fn reads_a_responses_body() {
    let mut s = scan(Api::Responses, RESPONSE);
    assert_eq!(s.status(), UsageStatus::Complete);
    assert_eq!(s.model(), Some("gpt-5.5-2026-10-01"));
    let usage = s.usage();
    assert_eq!(usage.input_tokens, Some(400));
    assert_eq!(usage.cache_read_input_tokens, Some(600));
    assert_eq!(usage.output_tokens, Some(40));
    assert!(usage.unpriced, "a hosted web search");
    let tools = s.take_tool_calls().unwrap();
    assert_eq!(
        tools.iter().collect::<Vec<_>>(),
        [("shell", 1), ("web_search", 1)]
    );
}

#[test]
fn server_tools_and_tiers_are_unpriced() {
    let search = MESSAGE.replace(
        r#""service_tier":"standard""#,
        r#""server_tool_use":{"web_search_requests":1}"#,
    );
    assert!(scan(Api::Messages, &search).usage().unpriced);
    let flex = RESPONSE
        .replace(r#""service_tier":"default""#, r#""service_tier":"flex""#)
        .replace(r#"{"type":"web_search_call","status":"completed"},"#, "");
    assert!(scan(Api::Responses, &flex).usage().unpriced);
}

#[test]
fn a_cut_or_odd_body_is_unknown() {
    let cut = &MESSAGE[..MESSAGE.len() - 30];
    assert_eq!(scan(Api::Messages, cut).status(), UsageStatus::Unknown);
    assert_eq!(scan(Api::Messages, "[1,2]").status(), UsageStatus::Unknown);
    assert_eq!(
        scan(Api::Messages, r#"{"model":"m","usage":null}"#).status(),
        UsageStatus::Unknown
    );
    // A long completion does not matter.
    let long = MESSAGE.replace("lookalike", &"x".repeat(2 << 20));
    assert_eq!(scan(Api::Messages, &long).usage().output_tokens, Some(20));
}

proptest! {
    #[test]
    fn any_chunking_reads_the_same(cuts in proptest::collection::vec(0usize..700, 0..10), responses: bool) {
        let (api, body) = if responses { (Api::Responses, RESPONSE) } else { (Api::Messages, MESSAGE) };
        let whole = scan(api, body);
        let mut s = JsonUsageScanner::new(api);
        feed_cut(body.as_bytes(), cuts, |piece| s.feed(piece));
        prop_assert_eq!(s.usage(), whole.usage());
        prop_assert_eq!(s.model(), whole.model());
        prop_assert_eq!(s.status(), UsageStatus::Complete);
        prop_assert_eq!(s.fields.tools.as_deref(), whole.fields.tools.as_deref());
    }

    #[test]
    fn arbitrary_bytes_never_panic(
        chunks in arbitrary_chunks(),
        api in prop::sample::select(vec![Api::Messages, Api::Responses, Api::ChatCompletions]),
    ) {
        let mut s = JsonUsageScanner::new(api);
        for chunk in &chunks {
            s.feed(chunk);
        }
        let _ = (s.status(), s.usage(), s.model());
    }
}

#[test]
fn reads_a_chat_completion_body() {
    let body = r#"{"id":"c1","object":"chat.completion","model":"gpt-5.5","choices":[{"index":0,"message":{"role":"assistant","content":"x","tool_calls":[{"id":"t","type":"function","function":{"name":"f","arguments":"{}"}}]}}],"service_tier":"default","usage":{"prompt_tokens":50,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":10}}}"#;
    let s = scan(Api::ChatCompletions, body);
    assert_eq!(s.status(), UsageStatus::Complete);
    assert_eq!(s.model(), Some("gpt-5.5"));
    let usage = s.usage();
    assert_eq!(usage.input_tokens, Some(40));
    assert_eq!(usage.cache_read_input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(7));
}
