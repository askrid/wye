//! The shipped example configs. `examples/wye.toml` is the configuration
//! reference, so it must parse as it stands and with every section enabled.

use super::*;

const REFERENCE: &str = include_str!("../../../../../examples/wye.toml");
const SUBSCRIPTION: &str = include_str!("../../../../../examples/subscription.toml");

/// The reference with every commented-out option enabled: its active
/// `[upstream]` gives way to `[routes.claude]`, and the credential
/// alternatives that exclude each other stay commented out.
fn every_section_enabled(reference: &str) -> String {
    let mut out = String::new();
    let mut in_upstream = false;
    for line in reference.lines() {
        if line == "[upstream]" {
            in_upstream = true;
            continue;
        }
        if in_upstream {
            // The single upstream ends where the route section starts.
            if !line.starts_with("# Several accounts") {
                continue;
            }
            in_upstream = false;
        }
        let option = line.strip_prefix("# ").filter(|body| {
            // A table header is the whole line, with at most a comment after it.
            let table = body.split(" #").next().unwrap_or_default().trim_end();
            let header = table.starts_with('[') && table.ends_with(']') && !table.contains(' ');
            let field = body.split_once(" = ").is_some_and(|(key, _)| {
                !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
            let list_item = body.trim_start().starts_with("{ id") || *body == "]";
            let alternative =
                body.starts_with("forward_caller_credential") || body.starts_with("api_key_file");
            (header || field || list_item) && !alternative
        });
        out.push_str(option.unwrap_or(line));
        out.push('\n');
    }
    // Any 32 bytes stand in for the key `wye token keygen` would print.
    out.replace(
        "<printed by wye token keygen>",
        "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc",
    )
}

#[test]
fn the_examples_parse_as_they_stand() {
    Config::from_toml_str(REFERENCE).unwrap();
    Config::from_toml_str(SUBSCRIPTION).unwrap();
}

#[test]
fn the_reference_parses_with_every_section_enabled() {
    let enabled = every_section_enabled(REFERENCE);
    let config = Config::from_toml_str(&enabled).unwrap_or_else(|e| panic!("{e}\n{enabled}"));
    // Every section really was enabled.
    assert!(config.auth.is_some() && config.telemetry.is_some());
    assert!(config.cache_diagnostics.is_some() && config.discovery.is_some());
    assert!(config.aliases.is_some() && config.pricing.is_some() && config.upgrade_check.is_some());
    assert_eq!(claude(&config).upstreams().len(), 3);
    assert!(config.routes.openai.is_some());
}
