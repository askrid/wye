#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use proptest::prelude::*;

use super::*;

fn scan(body: &[u8]) -> Result<String, ModelError> {
    let mut scanner = ModelScanner::new();
    scanner.feed(body);
    scanner.finish().map(str::to_owned)
}

fn scan_chunked(body: &[u8], sizes: &[usize]) -> (Result<String, ModelError>, Option<usize>) {
    let mut scanner = ModelScanner::new();
    let mut fed = 0;
    let mut known_after = None;
    let mut rest = body;
    for &size in sizes.iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let (chunk, tail) = rest.split_at(size.clamp(1, rest.len()));
        scanner.feed(chunk);
        fed += chunk.len();
        if known_after.is_none() && scanner.model().is_some() {
            known_after = Some(fed);
        }
        rest = tail;
    }
    (scanner.finish().map(str::to_owned), known_after)
}

fn patterns(list: &[&str]) -> Vec<ModelPattern> {
    list.iter()
        .map(|p| ModelPattern::parse(p).unwrap())
        .collect()
}

#[test]
fn patterns_match_exact_names_or_prefixes() {
    let haiku = ModelPattern::parse("claude-haiku-*").unwrap();
    assert!(haiku.matches("claude-haiku-4-5-20251001"));
    assert!(!haiku.matches("claude-sonnet-4-5"));
    let exact = ModelPattern::parse("claude-sonnet-4-5").unwrap();
    assert!(exact.matches("claude-sonnet-4-5"));
    assert!(!exact.matches("claude-sonnet-4-5-20250929"));
    assert!(ModelPattern::parse("*").unwrap().matches("anything"));
    for bad in ["", "claude-*-4", "**", &"x".repeat(300)] {
        assert_eq!(ModelPattern::parse(bad), None, "{bad}");
    }
    assert_eq!(haiku.to_string(), "claude-haiku-*");
}

#[test]
fn the_stricter_list_applies() {
    let config = patterns(&["claude-sonnet-*", "claude-haiku-*"]);
    let credential = patterns(&["claude-haiku-*"]);
    assert!(permitted(None, None, "any"));
    assert!(permitted(Some(&config), None, "claude-sonnet-4-5"));
    assert!(!permitted(Some(&config), None, "claude-opus-4-1"));
    assert!(permitted(
        Some(&config),
        Some(&credential),
        "claude-haiku-4-5"
    ));
    assert!(!permitted(
        Some(&config),
        Some(&credential),
        "claude-sonnet-4-5"
    ));
    // A credential cannot widen the configured ceiling.
    let wide = patterns(&["*"]);
    assert!(!permitted(Some(&config), Some(&wide), "claude-opus-4-1"));
    assert!(!permitted(None, Some(&credential), "claude-sonnet-4-5"));
}

#[test]
fn reads_the_top_level_model_only() {
    assert_eq!(
        scan(br#"{"model":"claude-sonnet-4-5","max_tokens":1}"#).unwrap(),
        "claude-sonnet-4-5"
    );
    // Nested `model` keys and strings mentioning it are content.
    let body = br#" { "metadata": {"model": "x"}, "messages": [{"role":"user","content":"\"model\":\"y\""}], "model" : "claude-haiku-4-5" } "#;
    assert_eq!(scan(body).unwrap(), "claude-haiku-4-5");
}

#[test]
fn rejects_what_it_cannot_check() {
    for (body, error) in [
        (&br#"{"max_tokens":1}"#[..], ModelError::Missing),
        (br#"{"model":1}"#, ModelError::Invalid),
        (br#"{"model":["a"]}"#, ModelError::Invalid),
        (br#"{"model":"a\u0062"}"#, ModelError::Invalid),
        (br#"{"model":"a","x":1,"model":"b"}"#, ModelError::Repeated),
        (br#"{"model":"a","mod\u0065l":"b"}"#, ModelError::EscapedKey),
        (br#"["model"]"#, ModelError::Malformed),
        (br#"{"model":"a"]"#, ModelError::Malformed),
    ] {
        assert_eq!(scan(body), Err(error), "{}", String::from_utf8_lossy(body));
    }
    let long = format!(r#"{{"model":"{}"}}"#, "m".repeat(MAX_MODEL_BYTES + 1));
    assert_eq!(scan(long.as_bytes()), Err(ModelError::Invalid));
}

#[test]
fn model_is_known_right_after_its_value() {
    let body = br#"{"model":"claude-x","messages":[]}"#;
    let end = body.iter().position(|&b| b == b',').unwrap();
    let mut scanner = ModelScanner::new();
    scanner.feed(&body[..end]);
    assert_eq!(scanner.model(), Some("claude-x"));
}

/// A top-level member for the property test.
#[derive(Debug, Clone)]
enum Member {
    Model(String),
    Other(String, String),
}

fn value_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "a",
            "\"",
            "\\",
            "model",
            "\"model\":",
            "{",
            "]",
            " ",
            "é",
        ]),
        0..8,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn same_answer_under_any_chunking(
        members in prop::collection::vec(
            prop_oneof![
                "[a-z0-9.-]{1,40}".prop_map(Member::Model),
                (prop::sample::select(vec!["max_tokens", "messages", "metadata", "models", "mode"]), value_text())
                    .prop_map(|(k, v)| Member::Other(k.to_owned(), v)),
            ],
            0..6,
        ),
        sizes in prop::collection::vec(1usize..24, 1..6),
    ) {
        let rendered: Vec<String> = members
            .iter()
            .map(|m| match m {
                Member::Model(name) => format!(r#""model":{}"#, serde_json::to_string(name).unwrap()),
                Member::Other(key, text) => format!(
                    r#"{}:{{"model":{},"list":[{}]}}"#,
                    serde_json::to_string(key).unwrap(),
                    serde_json::to_string(text).unwrap(),
                    serde_json::to_string(text).unwrap(),
                ),
            })
            .collect();
        let body = format!("{{{}}}", rendered.join(","));
        let models: Vec<&String> = members
            .iter()
            .filter_map(|m| match m { Member::Model(n) => Some(n), Member::Other(..) => None })
            .collect();
        let want = match models.as_slice() {
            [] => Err(ModelError::Missing),
            [one] => Ok((*one).clone()),
            _ => Err(ModelError::Repeated),
        };
        let (got, known_after) = scan_chunked(body.as_bytes(), &sizes);
        prop_assert_eq!(&got, &want, "{}", body);
        prop_assert_eq!(scan(body.as_bytes()), want.clone());
        if let Ok(model) = &want {
            // Known no later than the chunk holding the end of its value.
            let value = format!(r#""model":{}"#, serde_json::to_string(model).unwrap());
            let end = body.find(&value).unwrap() + value.len();
            prop_assert!(known_after.unwrap() <= end + 24, "{} {:?}", end, known_after);
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic(
        bytes in prop::collection::vec(prop::sample::select(b"{}[],:\"\\ amodel".to_vec()), 0..200),
        sizes in prop::collection::vec(1usize..16, 1..4),
    ) {
        let _ = scan_chunked(&bytes, &sizes);
    }
}
