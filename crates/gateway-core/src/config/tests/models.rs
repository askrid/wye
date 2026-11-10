//! The model tables: `[discovery]`, `[[aliases]]`, `[pricing]`, and
//! `[upgrade_check]`.

use super::*;

#[test]
fn discovery_lists_unique_model_names() {
    let with = |models: &str| config(&format!("[discovery]\nmodels = [{models}]"));
    let config = with(
        r#"{ id = "claude-sonnet-4-6", display_name = "Claude Sonnet 4.6", description = "Daily driver" }, { id = "claude-haiku-4-5" }"#,
    )
    .unwrap();
    let models = config.discovery.unwrap().models;
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].display_name.as_deref(), Some("Claude Sonnet 4.6"));
    assert_eq!(models[1].description, None);

    for bad in [
        "",
        r#"{ id = "" }"#,
        r#"{ id = "claude-*" }"#,
        // Listed, but a request naming it would be refused.
        r#"{ id = "claude\"x" }"#,
        r#"{ id = "a" }, { id = "a" }"#,
        r#"{ id = "a", extra = 1 }"#,
    ] {
        assert!(with(bad).is_err(), "{bad}");
    }
    let long = "x".repeat(1025);
    assert!(with(&format!(r#"{{ id = "a", description = "{long}" }}"#)).is_err());
}

#[test]
fn aliases_parse_and_reject_duplicates() {
    assert!(parse(FORWARDING).unwrap().aliases.is_none());
    let config = parse(&format!(
        "{FORWARDING}[[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n\
         when = [{{ subjects = [\"canary-*\"], model = \"claude-sonnet-5-5\" }}]\n"
    ))
    .unwrap();
    let aliases = config.aliases.unwrap();
    let alias = aliases.get("coding").unwrap();
    assert_eq!(&**alias.model_for(Some("canary-1")), "claude-sonnet-5-5");
    assert_eq!(&**alias.model_for(Some("main")), "claude-opus-5-5");

    let twice = "[[aliases]]\nname = \"a\"\nmodel = \"m\"\n";
    assert_eq!(
        parse(&format!("{FORWARDING}{twice}{twice}")).unwrap_err(),
        ConfigError::Invalid("aliases: duplicate name")
    );
    assert!(
        parse(&format!(
            "{FORWARDING}[[aliases]]\nname = \"a\"\nmodel = \"m*\"\n"
        ))
        .is_err()
    );
}

/// The pricing example in docs/policy.md parses, so the documented format
/// and the parser cannot drift apart.
#[test]
fn documented_pricing_example_parses() {
    let doc = include_str!("../../../../../docs/policy.md");
    let start = doc.find("```toml\n[pricing]").unwrap() + "```toml\n".len();
    let example = &doc[start..start + doc[start..].find("```").unwrap()];
    let pricing = parse(&format!("{FORWARDING}{example}"))
        .unwrap()
        .pricing
        .unwrap();
    assert_eq!(&*pricing.version, "anthropic-2026-11-01");
    assert_eq!(pricing.entries.len(), 4);
    assert_eq!(pricing.entries[0].prices.input, 4_000_000);
    // Opus 5.5 reads its cache at 0.05 times input, not the usual 0.1.
    assert_eq!(pricing.entries[0].prices.cache_read, Some(200_000));
    assert_eq!(pricing.entries[3].prices.cache_read, None);
    assert_eq!(pricing.entries[3].tiers[0].above_prompt_tokens, 100_000);

    let one = |extra: &str| {
        parse(&format!(
            "{FORWARDING}[pricing]\nversion = \"v\"\n[[pricing.prices]]\nmodels = [\"m\"]\ninput = \"1\"\noutput = \"2\"\n{extra}"
        ))
    };
    assert!(one("").is_ok());
    assert!(one("cache_read = \"0.1234567\"").is_err());
    assert!(one("cache_read = \"-1\"").is_err());
    assert!(one("upstreams = [\"x\"]").is_err());
    assert!(
        one("[[pricing.prices.tiers]]\nabove_prompt_tokens = 5\n[[pricing.prices.tiers]]\nabove_prompt_tokens = 5\n")
            .is_err()
    );
}

/// Without `version`, a price table is named by its content: the same on
/// every replica and run, whatever its comments and layout, and another
/// whenever a price changes.
#[test]
fn a_price_table_without_a_version_is_named_by_its_prices() {
    let version = |table: &str| {
        parse(&format!("{FORWARDING}[pricing]\n{table}"))
            .unwrap()
            .pricing
            .unwrap()
            .version
    };
    let table = "[[pricing.prices]]\nmodels = [\"claude-*\"]\ninput = \"1\"\noutput = \"2\"\n";
    let hashed = version(table);
    assert_eq!(hashed.len(), 16, "{hashed}");
    assert!(hashed.bytes().all(|b| b.is_ascii_hexdigit()), "{hashed}");
    let reformatted = "# Copied from the pricing page.\n[[pricing.prices]]\n\
                       output = \"2.00\"   # per million\ninput = \"1.0\"\nmodels = [ \"claude-*\" ]\n";
    assert_eq!(version(reformatted), hashed);
    for changed in [
        table.replace("\"2\"", "\"3\""),
        table.replace("claude-*", "claude-opus-*"),
        format!("{table}cache_read = \"0.1\"\n"),
        format!("{table}[[pricing.prices.tiers]]\nabove_prompt_tokens = 9\n"),
        format!("{table}[[pricing.prices]]\nmodels = [\"m\"]\ninput = \"1\"\noutput = \"2\"\n"),
    ] {
        assert_ne!(version(&changed), hashed, "{changed}");
    }
    // A cache read of zero is a price; an absent one is not.
    assert_ne!(version(&format!("{table}cache_read = \"0\"\n")), hashed);
    // A written version is kept as it is.
    assert_eq!(&*version(&format!("version = \"v1\"\n{table}")), "v1");
}

#[test]
fn upgrade_check_targets_are_unique_model_names() {
    let check = |body: &str| parse(&format!("{FORWARDING}[upgrade_check]\n{body}"));
    let ok = check("targets = [\"claude-opus-5-5\"]\nsample_percent = 10").unwrap();
    let ok = ok.upgrade_check.unwrap();
    assert_eq!(&*ok.targets[0], "claude-opus-5-5");
    assert_eq!(ok.sample_percent, 10);
    assert_eq!(
        check("targets = [\"claude-opus-5-5\"]")
            .unwrap()
            .upgrade_check
            .unwrap()
            .sample_percent,
        100
    );
    for bad in [
        "targets = []",
        "targets = [\"claude-*\"]",
        "targets = [\"claude\\\\x\"]",
        "targets = [\"a\", \"a\"]",
        "targets = [\"a\"]\nsample_percent = 0",
    ] {
        assert!(check(bad).is_err(), "{bad}");
    }
}
