//! Token usage extraction from an Anthropic Messages SSE stream.
//!
//! [`SseUsageScanner`] reads the forwarded bytes without changing, delaying, or
//! copying them. It keeps at most one partial line, parses JSON only for the two
//! events that carry usage (`message_start` and `message_delta`), and skips
//! every other event after reading its line prefix. HTTP chunk boundaries are
//! not SSE boundaries: lines may be split anywhere, including inside UTF-8.

use serde::Deserialize;

/// Longest usage-bearing data line kept. Larger events mark usage unknown
/// instead of growing memory.
pub const MAX_USAGE_LINE_BYTES: usize = 64 * 1024;
/// Bytes kept from lines of other events: enough to recognize `event:` lines.
const CLASSIFY_PREFIX_BYTES: usize = 32;

/// Token counts reported by the provider. `None` when the stream did not report the field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

/// How much of the usage a scan saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageStatus {
    /// `message_delta` usage arrived: the counts are the provider's final report.
    Complete,
    /// Only `message_start` arrived (stream ended early): output tokens are a lower bound.
    Partial,
    /// No usable usage event, or a usage event was malformed or oversized.
    Unknown,
}

impl UsageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Other,
    MessageStart,
    MessageDelta,
}

/// Incremental usage scanner for one response stream.
#[derive(Debug)]
pub struct SseUsageScanner {
    /// Partial line carried over from the previous chunk.
    line: Vec<u8>,
    /// The carried line was longer than the bytes kept.
    line_truncated: bool,
    event: Event,
    usage: Usage,
    model: Option<Box<str>>,
    saw_start: bool,
    saw_delta: bool,
    /// A usage event could not be read; the counts are not trustworthy.
    corrupt: bool,
}

impl Default for SseUsageScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl SseUsageScanner {
    pub fn new() -> Self {
        Self {
            line: Vec::new(),
            line_truncated: false,
            event: Event::Other,
            usage: Usage::default(),
            model: None,
            saw_start: false,
            saw_delta: false,
            corrupt: false,
        }
    }

    /// Feeds the next chunk of response bytes.
    pub fn feed(&mut self, mut chunk: &[u8]) {
        // SIMD search: every streamed byte passes through here.
        while let Some(newline) = memchr::memchr(b'\n', chunk) {
            let (segment, rest) = chunk.split_at(newline);
            chunk = rest.get(1..).unwrap_or_default();
            if self.line.is_empty() && !self.line_truncated {
                // Common case: the whole line is in this chunk; no copy.
                self.process_line(segment, false);
            } else {
                self.carry(segment);
                let line = std::mem::take(&mut self.line);
                let truncated = std::mem::replace(&mut self.line_truncated, false);
                self.process_line(&line, truncated);
                // Keep the allocation for the next carried line.
                self.line = line;
                self.line.clear();
            }
        }
        if !chunk.is_empty() {
            self.carry(chunk);
        }
    }

    /// Appends part of a line, keeping only as much as the current event needs.
    fn carry(&mut self, bytes: &[u8]) {
        let limit = match self.event {
            Event::Other => CLASSIFY_PREFIX_BYTES,
            Event::MessageStart | Event::MessageDelta => MAX_USAGE_LINE_BYTES,
        };
        let room = limit.saturating_sub(self.line.len());
        if bytes.len() > room {
            self.line_truncated = true;
        }
        self.line
            .extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
    }

    fn process_line(&mut self, line: &[u8], truncated: bool) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            self.event = Event::Other;
        } else if let Some(name) = line.strip_prefix(b"event:") {
            self.event = match name.trim_ascii() {
                b"message_start" => Event::MessageStart,
                b"message_delta" => Event::MessageDelta,
                _ => Event::Other,
            };
        } else if let Some(data) = line.strip_prefix(b"data:") {
            if self.event == Event::Other {
                return;
            }
            if truncated {
                self.corrupt = true;
                return;
            }
            self.read_data(data.trim_ascii());
        }
    }

    fn read_data(&mut self, data: &[u8]) {
        match self.event {
            Event::MessageStart => match serde_json::from_slice::<StartData>(data) {
                Ok(start) => {
                    self.saw_start = true;
                    self.model = start.message.model.map(String::into_boxed_str);
                    if let Some(raw) = start.message.usage {
                        raw.merge_into(&mut self.usage);
                    }
                }
                Err(_) => self.corrupt = true,
            },
            Event::MessageDelta => match serde_json::from_slice::<DeltaData>(data) {
                Ok(delta) => {
                    if let Some(raw) = delta.usage {
                        self.saw_delta = true;
                        raw.merge_into(&mut self.usage);
                    }
                }
                Err(_) => self.corrupt = true,
            },
            Event::Other => {}
        }
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Model the provider reported in `message_start`.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn status(&self) -> UsageStatus {
        match (self.corrupt, self.saw_start, self.saw_delta) {
            (true, _, _) | (false, false, false) => UsageStatus::Unknown,
            (false, _, true) => UsageStatus::Complete,
            (false, true, false) => UsageStatus::Partial,
        }
    }
}

#[derive(Deserialize)]
struct StartData {
    message: StartMessage,
}

#[derive(Deserialize)]
struct StartMessage {
    model: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct DeltaData {
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

impl RawUsage {
    /// Later events replace earlier counts; `message_delta` output tokens are cumulative.
    fn merge_into(self, usage: &mut Usage) {
        let take = |field: &mut Option<u64>, value: Option<u64>| {
            if value.is_some() {
                *field = value;
            }
        };
        take(&mut usage.input_tokens, self.input_tokens);
        take(&mut usage.output_tokens, self.output_tokens);
        take(
            &mut usage.cache_creation_input_tokens,
            self.cache_creation_input_tokens,
        );
        take(
            &mut usage.cache_read_input_tokens,
            self.cache_read_input_tokens,
        );
    }
}

#[cfg(test)]
mod tests {
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
}
