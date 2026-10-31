use proptest::prelude::*;
use xxhash_rust::xxh3::xxh3_64;

use super::*;

fn scan(body: &[u8]) -> Option<PromptHashes> {
    let mut scanner = PromptScanner::new();
    scanner.feed(body);
    scanner.finish()
}

fn scan_chunked(body: &[u8], sizes: &[usize]) -> Option<PromptHashes> {
    let mut scanner = PromptScanner::new();
    let mut rest = body;
    for &size in sizes.iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let (chunk, tail) = rest.split_at(size.clamp(1, rest.len()));
        scanner.feed(chunk);
        rest = tail;
    }
    scanner.finish()
}

fn h(canonical: &str) -> u64 {
    xxh3_64(canonical.as_bytes())
}

#[test]
fn hashes_sections_without_whitespace_or_markers() {
    let body = r#" {
        "model": "claude-sonnet-4-5",
        "system": [ {"type": "text", "text": "You are terse."},
                    {"type": "text", "text": "Repo: wye", "cache_control": {"type": "ephemeral"}} ],
        "tools": [ {"name": "Read", "input_schema": {"type": "object"}},
                   {"cache_control": {"type": "ephemeral"}, "name": "Edit"} ],
        "messages": [
            {"role": "user", "content": "hi \"there\" \\"},
            {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            {"role": "user", "content": [{"type": "text", "text": "a b", "cache_control": {"type": "ephemeral", "ttl": "1h"}}, {"type":"text","text":"x"}]}
        ],
        "max_tokens": 1024
    } "#;
    let got = scan(body.as_bytes()).unwrap();

    let tool0 = r#"{"name":"Read","input_schema":{"type":"object"}}"#;
    let tool1 = r#"{"name":"Edit"}"#;
    assert_eq!(
        got.tools,
        Some(SectionHash {
            count: 2,
            hash: h(&format!("[{tool0},{tool1}]"))
        })
    );
    let sys0 = r#"{"type":"text","text":"You are terse."}"#;
    let sys1 = r#"{"type":"text","text":"Repo: wye"}"#;
    assert_eq!(
        got.system,
        Some(SectionHash {
            count: 2,
            hash: h(&format!("[{sys0},{sys1}]"))
        })
    );
    assert_eq!(got.messages, Some(3));
    assert_eq!(got.breakpoint_count, 3);

    let m0 = r#"{"role":"user","content":"hi \"there\" \\"}"#;
    let m1 = r#"{"role":"assistant","content":[{"type":"text","text":"ok"}]}"#;
    let m2_through_block0 = r#"{"role":"user","content":[{"type":"text","text":"a b"}"#;
    let m2 = format!(r#"{m2_through_block0},{{"type":"text","text":"x"}}]}}"#);
    assert_eq!(
        got.breakpoints(),
        [
            Breakpoint {
                section: Section::System,
                index: 1,
                block: None,
                hash: h(&format!("[{sys0},{sys1}"))
            },
            Breakpoint {
                section: Section::Tools,
                index: 1,
                block: None,
                hash: h(&format!("[{tool0},{tool1}"))
            },
            Breakpoint {
                section: Section::Messages,
                index: 2,
                block: Some(0),
                hash: h(&format!("[{m0},{m1},{m2_through_block0}"))
            },
        ]
    );
    assert_eq!(
        got.message_prefixes(),
        [
            MessagePrefix {
                index: 0,
                hash: h(&format!("[{m0}"))
            },
            MessagePrefix {
                index: 1,
                hash: h(&format!("[{m0},{m1}"))
            },
            MessagePrefix {
                index: 2,
                hash: h(&format!("[{m0},{m1},{m2}"))
            },
        ]
    );
}

#[test]
fn moving_the_marker_keeps_earlier_prefixes() {
    let turn1 = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}}]}]}"#;
    let turn2 = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"a"}]},{"role":"assistant","content":"b"},{"role":"user","content":[{"type":"text","text":"c","cache_control":{"type":"ephemeral"}}]}]}"#;
    let (a, b) = (
        scan(turn1.as_bytes()).unwrap(),
        scan(turn2.as_bytes()).unwrap(),
    );
    assert_eq!(a.message_prefixes()[0], b.message_prefixes()[0]);
    assert_ne!(a.breakpoints()[0].hash, b.breakpoints()[0].hash);
}

#[test]
fn string_system_and_section_order() {
    let a = scan(br#"{"system":"be brief","messages":[{"role":"user","content":"x"}]}"#).unwrap();
    let b = scan(br#"{"messages":[{"role":"user","content":"x"}],"system":"be brief"}"#).unwrap();
    assert_eq!(
        a.system,
        Some(SectionHash {
            count: 1,
            hash: h(r#""be brief""#)
        })
    );
    assert_eq!(a, b);
    assert_eq!(a.tools, None);
}

#[test]
fn keeps_the_last_message_prefixes_in_order() {
    let messages: Vec<String> = (0..11)
        .map(|i| format!(r#"{{"role":"user","content":"m{i}"}}"#))
        .collect();
    let body = format!(r#"{{"messages":[{}]}}"#, messages.join(","));
    let got = scan(body.as_bytes()).unwrap();
    let indexes: Vec<u32> = got.message_prefixes().iter().map(|p| p.index).collect();
    assert_eq!(indexes, (3..11).collect::<Vec<_>>());
    assert_eq!(
        got.message_prefixes()[7].hash,
        h(&format!("[{}", messages.join(",")))
    );
}

#[test]
fn long_keys_in_blocks_are_hashed_whole() {
    let body =
        br#"{"tools":[{"name":"a","a_key_longer_than_sixteen_bytes":1,"cache_control":{}}]}"#;
    let got = scan(body).unwrap();
    assert_eq!(
        got.tools.unwrap().hash,
        h(r#"[{"name":"a","a_key_longer_than_sixteen_bytes":1}]"#)
    );
}

#[test]
fn markers_outside_block_objects_are_content() {
    // A tool schema may name a property `cache_control`; it is not a breakpoint.
    let body = br#"{"tools":[{"input_schema":{"cache_control":{"type":"string"}}}],"cache_control":{"type":"ephemeral"}}"#;
    let got = scan(body).unwrap();
    assert_eq!(got.breakpoint_count, 0);
    assert_eq!(
        got.tools.unwrap().hash,
        h(r#"[{"input_schema":{"cache_control":{"type":"string"}}}]"#)
    );
}

#[test]
fn gives_up_on_bodies_it_cannot_follow() {
    for body in [
        &br#""#[..],
        br#"[]"#,
        br#"{"messages":[}"#,
        br#"{"messages":[]"#,
        br#"{"messages":"x}"#,
        br#"{"messages":[]} {}"#,
        br#"{"messages":[],"messages":[]}"#,
        br#"{"mess\u0061ges":[]}"#,
        br#"{"tools":[{"cache\u005fcontrol":{}}]}"#,
        br#"{"tools":[{"name":"x"]}"#,
        br#"{}}"#,
    ] {
        assert_eq!(scan(body), None, "{}", String::from_utf8_lossy(body));
    }
}

#[test]
fn escapes_split_across_chunks() {
    let body = br#"{"system":"a\\\"b\\","messages":[]}"#;
    let whole = scan(body).unwrap();
    for split in 0..body.len() {
        let mut scanner = PromptScanner::new();
        scanner.feed(&body[..split]);
        scanner.feed(&body[split..]);
        assert_eq!(scanner.finish().as_ref(), Some(&whole), "split at {split}");
    }
    assert_eq!(whole.system.unwrap().hash, h(r#""a\\\"b\\""#));
}

#[test]
fn session_sampling_is_deterministic_and_proportional() {
    let ids: Vec<String> = (0..10_000).map(|i| format!("session-{i}")).collect();
    let sampled = |percent| {
        ids.iter()
            .filter(|id| session_sampled(Some(id.as_bytes()), percent))
            .count()
    };
    assert_eq!(sampled(100), 10_000);
    let ten = sampled(10);
    assert!((800..1_200).contains(&ten), "{ten}");
    // A session sampled at a lower rate stays sampled at a higher one.
    assert!(
        ids.iter()
            .all(|id| !session_sampled(Some(id.as_bytes()), 10)
                || session_sampled(Some(id.as_bytes()), 50))
    );
    assert!(session_sampled(None, 100));
    assert!(!session_sampled(None, 99));
}

#[test]
fn hash_key_redacts_and_requires_length() {
    assert!(HashKey::new(&[7; 31]).is_none());
    let key = HashKey::new(&[7; 32]).unwrap();
    assert_eq!(format!("{key:?}"), "HashKey(<redacted>)");
    assert_eq!(key.tag(1), key.tag(1));
    assert_ne!(key.tag(1), key.tag(2));
    assert_ne!(key.tag(1), HashKey::new(&[8; 32]).unwrap().tag(1));
}

// Property test: random bodies rendered with random whitespace and markers
// must hash exactly like the canonical rendering without them, under any
// chunking.

#[derive(Debug, Clone)]
enum Value {
    Null,
    Num(i64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

/// An object that may carry a `cache_control` member at `marker`.
#[derive(Debug, Clone)]
struct Block {
    members: Vec<(String, Value)>,
    marker: Option<usize>,
}

#[derive(Debug, Clone)]
enum Text {
    Plain(String),
    Blocks(Vec<Block>),
}

#[derive(Debug, Clone)]
struct Message {
    role: String,
    content: Text,
}

#[derive(Debug, Clone)]
struct Body {
    tools: Option<Vec<Block>>,
    system: Option<Text>,
    messages: Option<Vec<Message>>,
    /// Members outside the cached sections.
    extra: Vec<(String, Value)>,
    /// Rotation of the top-level member order.
    rotate: usize,
}

fn text_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "a",
            " ",
            "\"",
            "\\",
            "\\\"",
            "é",
            "\n",
            "{",
            "}",
            "[",
            "]",
            ",",
            ":",
            "cache_control",
        ]),
        0..6,
    )
    .prop_map(|parts| parts.concat())
}

fn value_strategy() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<i64>().prop_map(Value::Num),
        text_strategy().prop_map(Value::Str),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Value::Arr),
            prop::collection::vec(
                (
                    prop::sample::select(vec!["cache_control", "type", "q\"k", "content"]),
                    inner
                ),
                0..3
            )
            .prop_map(|m| Value::Obj(m.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())),
        ]
    })
}

fn block_strategy() -> impl Strategy<Value = Block> {
    (
        prop::collection::vec(
            (
                prop::sample::select(vec![
                    "type",
                    "text",
                    "name",
                    "content",
                    "cache_contro",
                    "cache_control_x",
                    "a_key_longer_than_sixteen_bytes",
                ]),
                value_strategy(),
            ),
            0..4,
        ),
        any::<Option<prop::sample::Index>>(),
    )
        .prop_map(|(members, marker)| {
            let members: Vec<_> = members
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect();
            let marker = marker.map(|i| i.index(members.len() + 1));
            Block { members, marker }
        })
}

fn text_or_blocks() -> impl Strategy<Value = Text> {
    prop_oneof![
        text_strategy().prop_map(Text::Plain),
        prop::collection::vec(block_strategy(), 0..4).prop_map(Text::Blocks),
    ]
}

fn body_strategy() -> impl Strategy<Value = Body> {
    (
        prop::option::of(prop::collection::vec(block_strategy(), 0..4)),
        prop::option::of(text_or_blocks()),
        prop::option::of(prop::collection::vec(
            (
                prop::sample::select(vec!["user", "assistant"]),
                text_or_blocks(),
            ),
            0..11,
        )),
        prop::collection::vec(
            (
                prop::sample::select(vec!["model", "max_tokens", "metadata", "stream"]),
                value_strategy(),
            ),
            0..3,
        ),
        0..8usize,
    )
        .prop_map(|(tools, system, messages, extra, rotate)| Body {
            tools,
            system,
            messages: messages.map(|m| {
                m.into_iter()
                    .map(|(role, content)| Message {
                        role: role.to_owned(),
                        content,
                    })
                    .collect()
            }),
            extra: extra.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
            rotate,
        })
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

/// Renders JSON. `noise` adds whitespace between tokens and the
/// `cache_control` members; without it the output is what gets hashed.
struct Writer<'a> {
    out: String,
    noise: Option<(&'a [u8], usize)>,
}

const MARKER: &str = r#"{"type":"ephemeral"}"#;

impl Writer<'_> {
    fn tok(&mut self, s: &str) {
        if let Some((spaces, next)) = &mut self.noise {
            let n = spaces[*next % spaces.len()];
            *next += 1;
            self.out.push_str(&" \n\t\r"[..usize::from(n % 4)]);
        }
        self.out.push_str(s);
    }

    fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.tok("null"),
            Value::Num(n) => self.tok(&n.to_string()),
            Value::Str(s) => self.tok(&quote(s)),
            Value::Arr(items) => {
                self.tok("[");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.tok(",");
                    }
                    self.value(item);
                }
                self.tok("]");
            }
            Value::Obj(members) => self.members(members.iter().map(|(k, v)| (k.as_str(), v))),
        }
    }

    fn members<'v>(&mut self, members: impl Iterator<Item = (&'v str, &'v Value)>) {
        self.tok("{");
        for (i, (k, v)) in members.enumerate() {
            if i > 0 {
                self.tok(",");
            }
            self.tok(&quote(k));
            self.tok(":");
            self.value(v);
        }
        self.tok("}");
    }

    fn block(&mut self, b: &Block) {
        let marker = self.noise.is_some().then_some(b.marker).flatten();
        self.tok("{");
        let mut first = true;
        for i in 0..=b.members.len() {
            if marker == Some(i) {
                if !first {
                    self.tok(",");
                }
                first = false;
                self.tok(r#""cache_control""#);
                self.tok(":");
                self.tok(MARKER);
            }
            if let Some((k, v)) = b.members.get(i) {
                if !first {
                    self.tok(",");
                }
                first = false;
                self.tok(&quote(k));
                self.tok(":");
                self.value(v);
            }
        }
        self.tok("}");
    }

    /// Writes a block array; returns the output length after each block.
    fn blocks(&mut self, blocks: &[Block]) -> Vec<usize> {
        self.tok("[");
        let mut ends = Vec::new();
        for (i, b) in blocks.iter().enumerate() {
            if i > 0 {
                self.tok(",");
            }
            self.block(b);
            ends.push(self.out.len());
        }
        self.tok("]");
        ends
    }

    fn text(&mut self, t: &Text) -> Vec<usize> {
        match t {
            Text::Plain(s) => {
                self.tok(&quote(s));
                Vec::new()
            }
            Text::Blocks(blocks) => self.blocks(blocks),
        }
    }

    /// Writes the messages array; returns, per message, the output length
    /// after each content block and after the message.
    fn messages(&mut self, messages: &[Message]) -> Vec<(Vec<usize>, usize)> {
        self.tok("[");
        let mut ends = Vec::new();
        for (i, m) in messages.iter().enumerate() {
            if i > 0 {
                self.tok(",");
            }
            self.tok("{");
            self.tok(r#""role""#);
            self.tok(":");
            self.tok(&quote(&m.role));
            self.tok(",");
            self.tok(r#""content""#);
            self.tok(":");
            let blocks = self.text(&m.content);
            self.tok("}");
            ends.push((blocks, self.out.len()));
        }
        self.tok("]");
        ends
    }
}

/// Writes one top-level member.
type RenderMember<'b> = Box<dyn Fn(&mut Writer<'_>) + 'b>;

fn render_noisy(body: &Body, spaces: &[u8]) -> String {
    let mut w = Writer {
        out: String::new(),
        noise: Some((spaces, 0)),
    };
    let mut members: Vec<RenderMember<'_>> = Vec::new();
    if let Some(tools) = &body.tools {
        members.push(Box::new(move |w| {
            w.tok(r#""tools""#);
            w.tok(":");
            w.blocks(tools);
        }));
    }
    if let Some(system) = &body.system {
        members.push(Box::new(move |w| {
            w.tok(r#""system""#);
            w.tok(":");
            w.text(system);
        }));
    }
    if let Some(messages) = &body.messages {
        members.push(Box::new(move |w| {
            w.tok(r#""messages""#);
            w.tok(":");
            w.messages(messages);
        }));
    }
    for (k, v) in &body.extra {
        members.push(Box::new(move |w| {
            w.tok(&quote(k));
            w.tok(":");
            w.value(v);
        }));
    }
    let rotate = if members.is_empty() {
        0
    } else {
        body.rotate % members.len()
    };
    members.rotate_left(rotate);
    w.tok("{");
    for (i, member) in members.iter().enumerate() {
        if i > 0 {
            w.tok(",");
        }
        member(&mut w);
    }
    w.tok("}");
    w.tok("");
    w.out
}

/// Breakpoints of one section in body order, from the canonical rendering.
fn block_breakpoints(
    section: Section,
    blocks: &[Block],
    canon: &str,
    ends: &[usize],
) -> Vec<Breakpoint> {
    blocks
        .iter()
        .zip(ends)
        .enumerate()
        .filter(|(_, (b, _))| b.marker.is_some())
        .map(|(i, (_, &end))| Breakpoint {
            section,
            index: i as u32,
            block: None,
            hash: h(&canon[..end]),
        })
        .collect()
}

struct Expected {
    tools: Option<SectionHash>,
    system: Option<SectionHash>,
    messages: Option<u32>,
    breakpoints: Vec<Breakpoint>,
    prefixes: Vec<MessagePrefix>,
}

fn expected(body: &Body) -> Expected {
    let canonical = || Writer {
        out: String::new(),
        noise: None,
    };
    let mut per_section: Vec<(Section, Vec<Breakpoint>)> = Vec::new();
    let tools = body.tools.as_ref().map(|tools| {
        let mut w = canonical();
        let ends = w.blocks(tools);
        per_section.push((
            Section::Tools,
            block_breakpoints(Section::Tools, tools, &w.out, &ends),
        ));
        SectionHash {
            count: tools.len() as u32,
            hash: h(&w.out),
        }
    });
    let system = body.system.as_ref().map(|system| {
        let mut w = canonical();
        let ends = w.text(system);
        let (count, breakpoints) = match system {
            Text::Plain(_) => (1, Vec::new()),
            Text::Blocks(blocks) => (
                blocks.len() as u32,
                block_breakpoints(Section::System, blocks, &w.out, &ends),
            ),
        };
        per_section.push((Section::System, breakpoints));
        SectionHash {
            count,
            hash: h(&w.out),
        }
    });
    let mut prefixes = Vec::new();
    let messages = body.messages.as_ref().map(|messages| {
        let mut w = canonical();
        let ends = w.messages(messages);
        let mut breakpoints = Vec::new();
        for (i, (m, (block_ends, end))) in messages.iter().zip(&ends).enumerate() {
            if let Text::Blocks(blocks) = &m.content {
                for (j, (b, &block_end)) in blocks.iter().zip(block_ends).enumerate() {
                    if b.marker.is_some() {
                        breakpoints.push(Breakpoint {
                            section: Section::Messages,
                            index: i as u32,
                            block: Some(j as u32),
                            hash: h(&w.out[..block_end]),
                        });
                    }
                }
            }
            prefixes.push(MessagePrefix {
                index: i as u32,
                hash: h(&w.out[..*end]),
            });
        }
        per_section.push((Section::Messages, breakpoints));
        messages.len() as u32
    });
    let mut order: Vec<Section> = Vec::new();
    if body.tools.is_some() {
        order.push(Section::Tools);
    }
    if body.system.is_some() {
        order.push(Section::System);
    }
    if body.messages.is_some() {
        order.push(Section::Messages);
    }
    let members = order.len() + body.extra.len();
    // Sections come first before rotation; extras follow.
    let rotate = if members == 0 {
        0
    } else {
        body.rotate % members
    };
    let mut positions: Vec<Option<Section>> = order.iter().copied().map(Some).collect();
    positions.extend(std::iter::repeat_n(None, body.extra.len()));
    positions.rotate_left(rotate);
    let breakpoints = positions
        .into_iter()
        .flatten()
        .flat_map(|s| {
            per_section
                .iter()
                .find(|(section, _)| *section == s)
                .map(|(_, b)| b.clone())
                .unwrap_or_default()
        })
        .collect();
    let skip = prefixes.len().saturating_sub(MESSAGE_PREFIXES);
    Expected {
        tools,
        system,
        messages,
        breakpoints,
        prefixes: prefixes.split_off(skip),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn noisy_bodies_hash_like_canonical_ones_under_any_chunking(
        body in body_strategy(),
        spaces in prop::collection::vec(0u8..8, 1..16),
        sizes in prop::collection::vec(1usize..48, 1..8),
    ) {
        let noisy = render_noisy(&body, &spaces);
        let want = expected(&body);
        let got = scan_chunked(noisy.as_bytes(), &sizes);
        let got = got.ok_or_else(|| TestCaseError::fail(format!("gave up on {noisy}")))?;
        prop_assert_eq!(got.tools, want.tools, "{}", noisy);
        prop_assert_eq!(got.system, want.system, "{}", noisy);
        prop_assert_eq!(got.messages, want.messages, "{}", noisy);
        let count = want.breakpoints.len() as u32;
        prop_assert_eq!(got.breakpoint_count, count, "{}", noisy);
        let first: Vec<_> = want.breakpoints.into_iter().take(MAX_BREAKPOINTS).collect();
        prop_assert_eq!(got.breakpoints(), &first[..], "{}", noisy);
        prop_assert_eq!(got.message_prefixes(), &want.prefixes[..], "{}", noisy);
    }

    #[test]
    fn arbitrary_bytes_never_panic(
        bytes in prop::collection::vec(
            prop::sample::select(b"{}[],:\"\\ ax".to_vec()),
            0..200,
        ),
        sizes in prop::collection::vec(1usize..16, 1..4),
    ) {
        let _ = scan_chunked(&bytes, &sizes);
    }

    #[test]
    fn truncated_bodies_give_up(body in body_strategy(), cut in any::<prop::sample::Index>()) {
        let noisy = render_noisy(&body, &[0]);
        let trimmed = noisy.trim_end();
        let cut = cut.index(trimmed.len());
        prop_assume!(trimmed.is_char_boundary(cut));
        prop_assert_eq!(scan(&trimmed.as_bytes()[..cut]), None);
    }
}
