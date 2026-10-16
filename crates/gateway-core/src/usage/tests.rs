use super::*;
use proptest::prelude::*;

/// Shape of a real Messages stream, including a `ping` and a multi-byte character.
const STREAM: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1200,\"cache_creation_input_tokens\":300,\"cache_read_input_tokens\":4000,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: ping\n",
    "data: {\"type\": \"ping\"}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"héllo — wörld\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":57}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

const EXPECTED: Usage = Usage {
    input_tokens: Some(1200),
    output_tokens: Some(57),
    cache_creation_input_tokens: Some(300),
    cache_read_input_tokens: Some(4000),
};

fn scan(chunks: &[&[u8]]) -> SseUsageScanner {
    let mut scanner = SseUsageScanner::new();
    for chunk in chunks {
        scanner.feed(chunk);
    }
    scanner
}

#[test]
fn reads_final_usage_and_model() {
    let s = scan(&[STREAM.as_bytes()]);
    assert_eq!(s.usage(), EXPECTED);
    assert_eq!(s.model(), Some("claude-sonnet-5-5"));
    assert_eq!(s.status(), UsageStatus::Complete);
}

#[test]
fn every_two_way_split_gives_the_same_result() {
    let bytes = STREAM.as_bytes();
    for at in 0..=bytes.len() {
        let (a, b) = bytes.split_at(at);
        let s = scan(&[a, b]);
        assert_eq!(s.usage(), EXPECTED, "split at {at}");
        assert_eq!(s.status(), UsageStatus::Complete, "split at {at}");
    }
}

proptest! {
    #[test]
    fn arbitrary_chunking_gives_the_same_result(sizes in proptest::collection::vec(1usize..64, 1..400)) {
        let bytes = STREAM.as_bytes();
        let mut scanner = SseUsageScanner::new();
        let mut pos = 0;
        for size in sizes.iter().cycle() {
            if pos >= bytes.len() {
                break;
            }
            let end = (pos + size).min(bytes.len());
            scanner.feed(&bytes[pos..end]);
            pos = end;
        }
        prop_assert_eq!(scanner.usage(), EXPECTED);
        prop_assert_eq!(scanner.status(), UsageStatus::Complete);
    }

    #[test]
    fn arbitrary_bytes_never_panic(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..256), 0..32)) {
        let mut scanner = SseUsageScanner::new();
        for chunk in &chunks {
            scanner.feed(chunk);
        }
        let _ = scanner.status();
    }
}

#[test]
fn crlf_line_endings() {
    let s = scan(&[STREAM.replace('\n', "\r\n").as_bytes()]);
    assert_eq!(s.usage(), EXPECTED);
}

#[test]
fn stream_cut_before_message_delta_is_partial() {
    let cut = STREAM.find("event: message_delta").unwrap();
    let s = scan(&[&STREAM.as_bytes()[..cut]]);
    assert_eq!(s.status(), UsageStatus::Partial);
    assert_eq!(s.usage().input_tokens, Some(1200));
    assert_eq!(s.usage().output_tokens, Some(1));
}

#[test]
fn no_usage_events_is_unknown() {
    let s = scan(&[b"event: ping\ndata: {}\n\n"]);
    assert_eq!(s.status(), UsageStatus::Unknown);
    assert_eq!(s.usage(), Usage::default());
}

#[test]
fn malformed_usage_event_is_unknown() {
    let s = scan(&[b"event: message_delta\ndata: {not json\n\n"]);
    assert_eq!(s.status(), UsageStatus::Unknown);
}

#[test]
fn oversized_usage_event_is_unknown_and_bounded() {
    let mut s = SseUsageScanner::new();
    s.feed(b"event: message_start\ndata: {\"message\":{\"pad\":\"");
    let filler = vec![b'x'; 8 * 1024];
    for _ in 0..32 {
        s.feed(&filler);
    }
    assert!(s.line.len() <= MAX_USAGE_LINE_BYTES);
    s.feed(b"\"}}\n\n");
    assert_eq!(s.status(), UsageStatus::Unknown);
}

#[test]
fn long_lines_of_other_events_are_not_kept() {
    let mut s = SseUsageScanner::new();
    s.feed(b"event: content_block_delta\ndata: ");
    s.feed(&vec![b'x'; 100_000]);
    assert!(s.line.len() <= CLASSIFY_PREFIX_BYTES);
    s.feed(b"\n\n");
    s.feed(STREAM.as_bytes());
    assert_eq!(s.usage(), EXPECTED);
    assert_eq!(s.status(), UsageStatus::Complete);
}

fn tool_block(index: usize, name: &str) -> String {
    format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_{index}\",\"name\":\"{name}\",\"input\":{{}}}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"command\\\": \\\"ls\\\"}}\"}}}}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n"
    )
}

#[test]
fn counts_tool_use_blocks_by_name_across_any_split() {
    let stream = [
        STREAM
            .split("event: message_delta")
            .next()
            .unwrap()
            .to_owned(),
        tool_block(1, "Bash"),
        tool_block(2, "Read"),
        tool_block(3, "Bash"),
        format!(
            "event: message_delta{}",
            STREAM.split("event: message_delta").nth(1).unwrap()
        ),
    ]
    .concat();
    let bytes = stream.as_bytes();
    for at in 0..=bytes.len() {
        let (a, b) = bytes.split_at(at);
        let s = scan(&[a, b]);
        let tools: Vec<_> = s.tool_calls().unwrap().iter().collect();
        assert_eq!(tools, [("Bash", 2), ("Read", 1)], "split at {at}");
        assert_eq!(s.status(), UsageStatus::Complete, "split at {at}");
        assert_eq!(s.usage(), EXPECTED, "split at {at}");
    }
    assert!(
        scan(&[STREAM.as_bytes()]).tool_calls().is_none(),
        "text only"
    );
}

#[test]
fn tool_names_are_bounded() {
    let long = "x".repeat(MAX_TOOL_NAME_BYTES + 1);
    let mut stream: String = (0..MAX_TOOL_NAMES + 2)
        .map(|i| tool_block(i, &format!("t{i}")))
        .collect();
    stream.push_str(&tool_block(20, &long));
    stream.push_str(&tool_block(21, "has space"));
    let s = scan(&[stream.as_bytes()]);
    let tools = s.tool_calls().unwrap();
    assert_eq!(tools.iter().count(), MAX_TOOL_NAMES);
    assert_eq!(
        tools.other(),
        4,
        "two beyond the table, one too long, one not a token"
    );
    // An oversized block start is skipped without marking usage unknown.
    let big = format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"content_block\":{{\"type\":\"text\",\"text\":\"{}\"}}}}\n\n",
        "y".repeat(10_000)
    );
    let s = scan(&[STREAM.as_bytes(), big.as_bytes()]);
    assert_eq!(s.status(), UsageStatus::Complete);
}
