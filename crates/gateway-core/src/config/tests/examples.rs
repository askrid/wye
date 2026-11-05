//! The shipped example configs. `examples/wye.toml` is the configuration
//! reference, so it must parse as it stands and with every section enabled.

use super::*;

const REFERENCE: &str = include_str!("../../../../../examples/wye.toml");
const DEVELOPER: &str = include_str!("../../../../../examples/developer/wye.toml");
const TEAM: &str = include_str!("../../../../../examples/team/wye.toml");
const APP: &str = include_str!("../../../../../examples/app/wye.toml");

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
            let alternative = body.starts_with("forward_caller_credential")
                || body.starts_with("provider_key_file");
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
    for example in [REFERENCE, DEVELOPER, TEAM, APP] {
        Config::from_toml_str(example).unwrap();
    }
}

/// The reference's live options are set to their defaults, so that every key
/// shows: it must run exactly as its required lines alone.
#[test]
fn the_reference_sets_only_defaults() {
    const REQUIRED: &str = "[server]\nlisten = \"127.0.0.1:8080\"\n\n[upstream]\n\
        base_url = \"https://api.anthropic.com\"\nprovider_key_env = \"ANTHROPIC_API_KEY\"\n";
    let reference = Config::from_toml_str(REFERENCE).unwrap();
    let required = Config::from_toml_str(REQUIRED).unwrap();
    assert_eq!(
        format!("{:?}", reference.server),
        format!("{:?}", required.server)
    );
    assert_eq!(
        format!("{:?}", reference.routes),
        format!("{:?}", required.routes)
    );
}

#[test]
fn the_reference_parses_with_every_section_enabled() {
    let enabled = every_section_enabled(REFERENCE);
    let config = Config::from_toml_str(&enabled).unwrap_or_else(|e| panic!("{e}\n{enabled}"));
    // Every section was enabled.
    assert!(config.auth.is_some() && config.telemetry.is_some());
    assert!(config.cache_diagnostics.is_some() && config.discovery.is_some());
    assert!(config.aliases.is_some() && config.pricing.is_some() && config.upgrade_check.is_some());
    assert_eq!(claude(&config).upstreams().len(), 3);
    assert!(config.routes.openai.is_some());
}
