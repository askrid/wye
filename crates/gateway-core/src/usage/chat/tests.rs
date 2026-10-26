use proptest::prelude::*;

use super::*;

/// A streamed chat completion with a tool call and, when asked for, usage.
fn stream(with_usage: bool) -> String {
    let mut s = String::from(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5.5\",\"service_tier\":\"default\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}],\"usage\":null}\n\n\
         data: {\"id\":\"c1\",\"model\":\"gpt-5.5\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"the \\\"usage\\\":{} word\"}}],\"usage\":null}\n\n\
         data: {\"id\":\"c1\",\"model\":\"gpt-5.5\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}],\"usage\":null}\n\n\
         data: {\"id\":\"c1\",\"model\":\"gpt-5.5\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"name\\\":\\\"x\\\"}\"}}]}}],\"usage\":null}\n\n",
    );
    if with_usage {
        s.push_str("data: {\"id\":\"c1\",\"model\":\"gpt-5.5-2026-10-01\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"total_tokens\":120,\"prompt_tokens_details\":{\"cached_tokens\":60},\"completion_tokens_details\":{\"reasoning_tokens\":5}}}\n\n");
    }
    s.push_str("data: [DONE]\n\n");
    s
}

fn scan(bytes: &[u8]) -> ChatUsageScanner {
    let mut s = ChatUsageScanner::new();
    s.feed(bytes);
    s
}

#[test]
fn reads_the_usage_chunk_and_tool_calls() {
    let mut s = scan(stream(true).as_bytes());
    assert_eq!(s.status(), UsageStatus::Complete);
    assert_eq!(s.model(), Some("gpt-5.5-2026-10-01"));
    let usage = s.usage();
    assert_eq!(usage.input_tokens, Some(40));
    assert_eq!(usage.cache_read_input_tokens, Some(60));
    assert_eq!(usage.output_tokens, Some(20));
    assert!(!usage.unpriced);
    // The argument delta's escaped "name" is not a second call.
    let tools = s.take_tool_calls().unwrap();
    assert_eq!(tools.iter().collect::<Vec<_>>(), [("get_weather", 1)]);
}

#[test]
fn without_include_usage_the_usage_is_unknown() {
    let s = scan(stream(false).as_bytes());
    assert_eq!(s.status(), UsageStatus::Unknown);
    assert_eq!(s.model(), Some("gpt-5.5"), "from the first chunk");
}

#[test]
fn a_cut_or_odd_usage_chunk_is_unknown() {
    let odd = stream(true).replace("\"prompt_tokens\":100", "\"prompt_tokens\":\"many\"");
    assert_eq!(scan(odd.as_bytes()).status(), UsageStatus::Unknown);
    let flex = stream(true).replace(
        "\"model\":\"gpt-5.5-2026-10-01\",",
        "\"model\":\"gpt-5.5\",\"service_tier\":\"flex\",",
    );
    assert!(scan(flex.as_bytes()).usage().unpriced);
}

proptest! {
    #[test]
    fn any_chunking_reads_the_same(cuts in proptest::collection::vec(0usize..1500, 0..12)) {
        let full = stream(true);
        let bytes = full.as_bytes();
        let whole = scan(bytes);
        let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c % (bytes.len() + 1)).collect();
        cuts.sort_unstable();
        let mut s = ChatUsageScanner::new();
        let mut at = 0;
        for cut in cuts {
            s.feed(&bytes[at..cut]);
            at = cut;
        }
        s.feed(&bytes[at..]);
        prop_assert_eq!(s.usage(), whole.usage());
        prop_assert_eq!(s.model(), whole.model());
        prop_assert_eq!(s.status(), UsageStatus::Complete);
        prop_assert_eq!(s.tools.as_deref(), whole.tools.as_deref());
    }
}
