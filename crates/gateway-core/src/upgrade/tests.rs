#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use super::*;

fn findings(body: &str) -> Findings {
    let mut scanner = UpgradeScanner::new();
    scanner.feed(body.as_bytes());
    scanner.findings()
}

fn rules(body: &str, target: &str) -> Vec<&'static str> {
    issues(&findings(body), target).map(Rule::name).collect()
}

/// A Claude Code-shaped request that every current model accepts.
const CLEAN: &str = r#"{"model":"claude-opus-5","max_tokens":32000,"stream":true,
  "system":[{"type":"text","text":"You are Claude Code."}],
  "thinking":{"type":"adaptive"},
  "tools":[{"name":"Read","description":"Reads \"files\"","input_schema":{"type":"object","properties":{"role":{"type":"string"}}}}],
  "tool_choice":{"type":"auto"},
  "messages":[{"role":"user","content":[{"type":"text","text":"fix it"}]},
              {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"role":"assistant"}}]},
              {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"{\"role\":\"system\"}"}]}]}"#;

#[test]
fn a_clean_request_breaks_no_rule_on_any_model() {
    for target in [
        "claude-opus-5-5",
        "claude-opus-4-7",
        "claude-sonnet-5",
        "claude-fable-5-1",
    ] {
        assert!(rules(CLEAN, target).is_empty(), "{target}");
    }
}

/// One fixture per rule: the request field, and a model that rejects it and
/// one that accepts it.
#[test]
fn each_rule_is_caught_on_its_models_only() {
    for (body, rule, rejects, accepts) in [
        (
            r#"{"thinking":{"type":"disabled"}}"#,
            "thinking_disabled",
            "claude-sonnet-5-5",
            "claude-opus-5",
        ),
        (
            r#"{"thinking":{"budget_tokens":1024,"type":"enabled"}}"#,
            "budget_tokens",
            "claude-opus-4-7",
            "claude-opus-4-6",
        ),
        (
            r#"{"tool_choice":{"type":"tool","name":"Read"}}"#,
            "forced_tool_choice",
            "claude-opus-5-5",
            "claude-sonnet-5",
        ),
        (
            r#"{"tool_choice":{"type":"any"}}"#,
            "forced_tool_choice",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
        ),
        (
            r#"{"tools":[{"type":"bash_20250124"},{"type":"computer_20251124","name":"computer"}]}"#,
            "computer_tool",
            "claude-opus-5-5",
            "claude-opus-5",
        ),
        (
            r#"{"messages":[{"role":"user","content":"x"},{"role":"assistant","content":"{"}]}"#,
            "prefill",
            "claude-opus-4-6",
            "claude-opus-4-5",
        ),
        (
            r#"{"temperature":0.2,"messages":[]}"#,
            "sampling",
            "claude-opus-4-8",
            "claude-opus-4-6",
        ),
        (
            r#"{"top_k":5}"#,
            "sampling",
            "claude-haiku-5-5",
            "claude-haiku-4-5",
        ),
        (
            r#"{"messages":[{"role":"system","content":"x"},{"role":"user","content":"y"}]}"#,
            "system_role",
            "claude-opus-4-7",
            "claude-opus-5-5",
        ),
    ] {
        assert_eq!(rules(body, rejects), [rule], "{body} on {rejects}");
        assert!(rules(body, accepts).is_empty(), "{body} on {accepts}");
        // Dated ids belong to their family.
        assert_eq!(
            rules(body, &format!("{rejects}-20261001")),
            [rule],
            "{body}"
        );
    }
}

#[test]
fn model_families_do_not_match_by_prefix() {
    let body = r#"{"thinking":{"type":"disabled"}}"#;
    assert!(rules(body, "claude-opus-5").is_empty());
    assert!(rules(body, "claude-opus-5-55").is_empty());
    assert!(rules(body, "claude-opus-5-5-x").is_empty());
}

#[test]
fn only_the_last_message_counts_as_prefill() {
    let body = r#"{"messages":[{"role":"assistant","content":"a"},{"role":"user","content":"b"}]}"#;
    assert!(!findings(body).prefill);
}

#[test]
fn every_two_way_split_gives_the_same_findings() {
    let body = r#"{"temperature":1,"thinking":{"type":"enabled","budget_tokens":9},"tools":[{"type":"computer_20250124"}],"tool_choice":{"type":"any"},"messages":[{"role":"system","content":"s \"q\" \\"},{"role":"assistant","content":"p"}]}"#;
    let whole = findings(body);
    assert_eq!(
        issues(&whole, "claude-opus-5-5")
            .map(Rule::name)
            .collect::<Vec<_>>(),
        [
            "budget_tokens",
            "forced_tool_choice",
            "computer_tool",
            "prefill",
            "sampling"
        ]
    );
    assert!(whole.system_role);
    for at in 0..=body.len() {
        let (a, b) = body.as_bytes().split_at(at);
        let mut scanner = UpgradeScanner::new();
        scanner.feed(a);
        scanner.feed(b);
        assert_eq!(scanner.findings(), whole, "split at {at}");
    }
}

#[test]
fn escaped_or_malformed_input_matches_nothing() {
    assert_eq!(
        findings(r#"{"thinking":{"type":"dis\u0061bled"}}"#).thinking,
        Some(Thinking::Other)
    );
    assert_eq!(
        findings(r#"["thinking",{"type":"disabled"}]"#),
        Findings::default()
    );
    for rule in Rule::ALL {
        assert!(
            rule.source().starts_with("https://platform.claude.com/"),
            "{rule:?}"
        );
        assert!(!rule.fix().is_empty());
    }
}
