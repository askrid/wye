use super::*;

fn claude(config: &Config) -> &Route {
    config.routes.claude.as_ref().unwrap()
}

fn parse(upstream: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[upstream]\n{upstream}"
    ))
}

#[test]
fn minimal_config_gets_defaults() {
    let config =
        parse("base_url = \"https://api.anthropic.com\"\napi_key_env = \"ANTHROPIC_API_KEY\"")
            .unwrap();
    assert_eq!(
        config.server.max_request_body_bytes,
        DEFAULT_MAX_REQUEST_BODY_BYTES
    );
    assert_eq!(claude(&config).upstreams()[0].scheme, Scheme::HTTPS);
    assert_eq!(
        claude(&config).upstreams()[0].authority.as_str(),
        "api.anthropic.com"
    );
    assert_eq!(claude(&config).upstreams()[0].path_prefix, "");
    assert_eq!(
        claude(&config).upstreams()[0].credential,
        UpstreamCredential::ApiKeyEnv("ANTHROPIC_API_KEY".into())
    );
    assert_eq!(
        claude(&config).upstreams()[0].response_header_timeout,
        Duration::from_secs(600)
    );
}

#[test]
fn base_path_keeps_prefix_without_trailing_slash() {
    let config =
        parse("base_url = \"https://gw.example.com/anthropic/\"\nforward_caller_credential = true")
            .unwrap();
    assert_eq!(claude(&config).upstreams()[0].path_prefix, "/anthropic");
    assert_eq!(
        claude(&config).upstreams()[0].credential,
        UpstreamCredential::ForwardCaller
    );
}

#[test]
fn rejects_non_loopback_listen() {
    let err = Config::from_toml_str(
        "[server]\nlisten = \"0.0.0.0:8080\"\n[upstream]\nbase_url = \"https://api.anthropic.com\"\nforward_caller_credential = true",
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::NonLoopbackListen(_)));
}

#[test]
fn auth_allows_network_listen_and_validates_keys() {
    // A valid 32-byte key, base64url without padding.
    let key = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
    let with_auth = |listen: &str, upstream: &str, auth: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"{listen}\"\n[upstream]\nbase_url = \"https://api.anthropic.com\"\n\
             {upstream}\n[auth]\nissuer = \"wye\"\naudience = \"wye-prod\"\n{auth}"
        ))
    };
    let keys = format!("[[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{key}\"");
    let config = with_auth("0.0.0.0:8080", "api_key_env = \"K\"", &keys).unwrap();
    let auth = config.auth.unwrap();
    assert_eq!(auth.own.unwrap().keys.len(), 1);
    assert_eq!(auth.max_credential_ttl, Duration::from_secs(7 * 24 * 3600));

    assert!(matches!(
        with_auth("0.0.0.0:8080", "forward_caller_credential = true", &keys),
        Err(ConfigError::Auth(_))
    ));
    assert!(matches!(
        with_auth("127.0.0.1:8080", "api_key_env = \"K\"", "keys = []"),
        Err(ConfigError::Auth(_))
    ));
    let bad = "[[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"not-a-key\"";
    assert!(matches!(
        with_auth("127.0.0.1:8080", "api_key_env = \"K\"", bad),
        Err(ConfigError::Auth(_))
    ));
    let twice = format!("{keys}\n{keys}");
    assert!(matches!(
        with_auth("127.0.0.1:8080", "api_key_env = \"K\"", &twice),
        Err(ConfigError::Auth(_))
    ));
}

#[test]
fn auth_models_are_validated_patterns() {
    let key = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
    let with_models = |models: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[upstream]\nbase_url = \"https://api.anthropic.com\"\n\
             api_key_env = \"K\"\n[auth]\nissuer = \"wye\"\naudience = \"wye-prod\"\n{models}\n\
             [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{key}\""
        ))
    };
    assert!(with_models("").unwrap().auth.unwrap().models.is_none());
    let models = with_models("models = [\"claude-haiku-*\", \"claude-sonnet-4-5\"]")
        .unwrap()
        .auth
        .unwrap()
        .models
        .unwrap();
    assert!(models[0].matches("claude-haiku-4-5"));
    for bad in ["models = []", "models = [\"a*b\"]", "models = [\"\"]"] {
        assert!(
            matches!(with_models(bad), Err(ConfigError::Auth(_))),
            "{bad}"
        );
    }
    assert_eq!(
        with_models("max_output_tokens = 8000")
            .unwrap()
            .auth
            .unwrap()
            .max_output_tokens,
        Some(8000)
    );
    assert_eq!(
        with_models("max_output_tokens = 0").unwrap_err(),
        ConfigError::Zero("auth.max_output_tokens")
    );
    let held = parse("base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true")
        .unwrap()
        .server
        .max_held_request_bytes;
    assert_eq!(held, DEFAULT_MAX_HELD_REQUEST_BYTES);
}

#[test]
fn rejects_unknown_fields() {
    let err = parse(
        "base_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\napi_key = \"sk-secret\"",
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)));
}

#[test]
fn credential_must_be_exactly_one() {
    assert!(matches!(
        parse("base_url = \"https://api.anthropic.com\""),
        Err(ConfigError::Credential(_))
    ));
    assert!(matches!(
        parse(
            "base_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\nforward_caller_credential = true"
        ),
        Err(ConfigError::Credential(_))
    ));
    assert!(matches!(
        parse("base_url = \"https://api.anthropic.com\"\napi_key_env = \"1BAD\""),
        Err(ConfigError::InvalidEnvName { .. })
    ));
}

#[test]
fn base_url_rules() {
    for (url, ok) in [
        ("http://127.0.0.1:9000", true),
        ("http://localhost:9000", true),
        ("http://[::1]:9000", true),
        ("http://api.anthropic.com", false),
        ("ftp://api.anthropic.com", false),
        ("https://api.anthropic.com/?x=1", false),
        ("https://user:pw@api.anthropic.com", false),
        ("/relative", false),
    ] {
        let result = parse(&format!(
            "base_url = \"{url}\"\nforward_caller_credential = true"
        ));
        assert_eq!(result.is_ok(), ok, "{url}: {result:?}");
    }
}

#[test]
fn ca_file_is_kept_as_written() {
    let ok = "base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n";
    let config = parse(&format!("{ok}ca_file = \"certs/ca.pem\"")).unwrap();
    assert_eq!(
        claude(&config).upstreams()[0].ca_file,
        Some(PathBuf::from("certs/ca.pem"))
    );
    assert_eq!(claude(&parse(ok).unwrap()).upstreams()[0].ca_file, None);
    assert_eq!(
        parse(&format!("{ok}ca_file = \"\"")).unwrap_err(),
        ConfigError::EmptyPath("upstream.ca_file")
    );
}

#[test]
fn lifecycle_defaults_and_zero_limits_rejected() {
    let config =
        parse("base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true")
            .unwrap();
    assert_eq!(config.server.max_concurrent_requests, 10_000);
    assert_eq!(config.server.drain_delay, Duration::ZERO);
    let err = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\nmax_concurrent_requests = 0\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\nforward_caller_credential = true",
    )
    .unwrap_err();
    assert_eq!(err, ConfigError::Zero("server.max_concurrent_requests"));
    assert_eq!(config.server.client_write_timeout, Duration::from_secs(60));
    let err = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\nclient_write_timeout_secs = 0\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\nforward_caller_credential = true",
    )
    .unwrap_err();
    assert_eq!(err, ConfigError::Zero("server.client_write_timeout_secs"));
}

#[test]
fn cache_diagnostics_require_a_key_variable() {
    let base = "[server]\nlisten = \"127.0.0.1:8080\"\n[upstream]\n\
                base_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n";
    assert!(
        Config::from_toml_str(base)
            .unwrap()
            .cache_diagnostics
            .is_none()
    );
    let config = Config::from_toml_str(&format!(
        "{base}[cache_diagnostics]\nhash_key_env = \"WYE_CACHE_HASH_KEY\""
    ))
    .unwrap();
    let diagnostics = config.cache_diagnostics.unwrap();
    assert_eq!(diagnostics.hash_key_env, "WYE_CACHE_HASH_KEY");
    assert_eq!(diagnostics.sample_percent, 100);
    let with_percent = |p: &str| {
        Config::from_toml_str(&format!(
            "{base}[cache_diagnostics]\nhash_key_env = \"K\"\nsample_percent = {p}"
        ))
    };
    assert_eq!(
        with_percent("10")
            .unwrap()
            .cache_diagnostics
            .unwrap()
            .sample_percent,
        10
    );
    for bad in ["0", "101"] {
        assert_eq!(
            with_percent(bad).unwrap_err(),
            ConfigError::Percent("cache_diagnostics.sample_percent")
        );
    }
    assert!(matches!(
        Config::from_toml_str(&format!("{base}[cache_diagnostics]\n")),
        Err(ConfigError::Parse(_))
    ));
    assert_eq!(
        Config::from_toml_str(&format!(
            "{base}[cache_diagnostics]\nhash_key_env = \"a-b\""
        ))
        .unwrap_err(),
        ConfigError::InvalidEnvName {
            field: "cache_diagnostics.hash_key_env",
            name: "a-b".into()
        }
    );
}

#[test]
fn rejects_zero_timeouts() {
    assert_eq!(
        parse("base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\nstream_idle_timeout_secs = 0")
            .unwrap_err(),
        ConfigError::Zero("upstream.stream_idle_timeout_secs")
    );
}

#[test]
fn telemetry_endpoint_gets_the_logs_path() {
    let base = "[server]\nlisten = \"127.0.0.1:8080\"\n[upstream]\n\
                base_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n";
    let with = |t: &str| Config::from_toml_str(&format!("{base}[telemetry]\n{t}"));
    let telemetry = with("otlp_endpoint = \"http://collector:4318\"")
        .unwrap()
        .telemetry
        .unwrap();
    assert_eq!(telemetry.otlp_logs_uri, "http://collector:4318/v1/logs");
    assert_eq!(
        telemetry.otlp_metrics_uri,
        "http://collector:4318/v1/metrics"
    );
    assert_eq!(telemetry.otlp_traces_uri, "http://collector:4318/v1/traces");
    assert_eq!(telemetry.service_name, "wye");
    assert_eq!(telemetry.otlp_headers_env, None);
    let telemetry = with(
        "otlp_endpoint = \"https://otlp.example.com/otel/\"\notlp_headers_env = \"OTLP_HEADERS\"\nservice_name = \"gw\"",
    )
    .unwrap()
    .telemetry
    .unwrap();
    assert_eq!(
        telemetry.otlp_logs_uri,
        "https://otlp.example.com/otel/v1/logs"
    );
    assert_eq!(telemetry.otlp_headers_env.as_deref(), Some("OTLP_HEADERS"));
    for bad in [
        "otlp_endpoint = \"ftp://c\"",
        "otlp_endpoint = \"http://c/?x=1\"",
        "otlp_endpoint = \"/relative\"",
    ] {
        assert!(
            matches!(with(bad), Err(ConfigError::InvalidUrl { .. })),
            "{bad}"
        );
    }
    assert!(Config::from_toml_str(base).unwrap().telemetry.is_none());
}

#[test]
fn api_key_file_is_a_third_exclusive_credential_mode() {
    let config =
        parse("base_url = \"https://api.anthropic.com\"\napi_key_file = \"secrets/key\"").unwrap();
    assert_eq!(
        claude(&config).upstreams()[0].credential,
        UpstreamCredential::ApiKeyFile(PathBuf::from("secrets/key"))
    );
    for both in [
        "api_key_file = \"k\"\napi_key_env = \"K\"",
        "api_key_file = \"k\"\nforward_caller_credential = true",
    ] {
        assert!(matches!(
            parse(&format!("base_url = \"https://api.anthropic.com\"\n{both}")),
            Err(ConfigError::Credential(_))
        ));
    }
    assert_eq!(
        parse("base_url = \"https://api.anthropic.com\"\napi_key_file = \"\"").unwrap_err(),
        ConfigError::EmptyPath("upstream.api_key_file")
    );
}

#[test]
fn http_version_is_auto_unless_pinned_to_1_1() {
    let ok = "base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n";
    assert_eq!(
        claude(&parse(ok).unwrap()).upstreams()[0].http_version,
        UpstreamHttpVersion::Auto
    );
    assert_eq!(
        claude(&parse(&format!("{ok}http_version = \"1.1\"")).unwrap()).upstreams()[0].http_version,
        UpstreamHttpVersion::Http1
    );
    assert!(matches!(
        parse(&format!("{ok}http_version = \"2\"")),
        Err(ConfigError::Invalid(_))
    ));
}

#[test]
fn discovery_lists_unique_model_names() {
    let with = |models: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
             [discovery]\nmodels = [{models}]"
        ))
    };
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
        r#"{ id = "a" }, { id = "a" }"#,
        r#"{ id = "a", extra = 1 }"#,
    ] {
        assert!(with(bad).is_err(), "{bad}");
    }
    let long = "x".repeat(1025);
    assert!(with(&format!(r#"{{ id = "a", description = "{long}" }}"#)).is_err());
}

#[test]
fn identity_provider_issuers_are_validated() {
    let parse_auth = |auth: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"0.0.0.0:8080\"\n[upstream]\nbase_url = \"https://api.anthropic.com\"\n\
             api_key_env = \"K\"\n[auth]\n{auth}"
        ))
    };
    let idp = "[[auth.issuers]]\nissuer = \"https://token.actions.githubusercontent.com\"\n\
               audience = \"wye-prod\"\nalgorithm = \"RS256\"\njwks_file = \"github.jwks\"\n";
    // Issuers alone are enough: Wye's own keys are optional.
    let auth = parse_auth(idp).unwrap().auth.unwrap();
    assert!(auth.own.is_none());
    let issuer = &auth.issuers[0];
    assert_eq!(issuer.algorithm, Algorithm::Rs256);
    assert_eq!(issuer.subject_claim, "sub");
    assert!(issuer.grants.is_empty());

    let grants = format!(
        "{idp}subject_claim = \"repository\"\n[[auth.issuers.grants]]\nclaim = \"repository_owner\"\n\
         value = \"acme\"\nmodels = [\"claude-haiku-*\"]\n"
    );
    let auth = parse_auth(&grants).unwrap().auth.unwrap();
    assert_eq!(auth.issuers[0].subject_claim, "repository");
    assert_eq!(
        auth.issuers[0].grants[0].models[0].to_string(),
        "claude-haiku-*"
    );

    let key = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
    let own = format!(
        "issuer = \"wye\"\naudience = \"wye-prod\"\n[[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{key}\"\n"
    );
    let auth = parse_auth(&format!("{own}{idp}")).unwrap().auth.unwrap();
    assert!(auth.own.is_some() && auth.issuers.len() == 1);

    for bad in [
        String::new(),                   // neither own keys nor issuers
        "issuer = \"wye\"\n".to_owned(), // own issuer without keys or audience
        idp.replace("RS256", "HS256"),   // HMAC is never accepted
        idp.replace("RS256", "none"),
        idp.replace("github.jwks", ""),
        format!("{idp}{idp}"), // duplicate issuer
        format!(
            "{own}{}",
            idp.replace("https://token.actions.githubusercontent.com", "wye")
        ),
        format!("{idp}subject_claim = \"\"\n"),
        format!("{idp}[[auth.issuers.grants]]\nclaim = \"groups\"\nvalue = \"x\"\nmodels = []\n"),
        format!("{idp}[[auth.issuers.grants]]\nclaim = \"\"\nvalue = \"x\"\nmodels = [\"a\"]\n"),
        format!("{idp}[[auth.issuers.grants]]\nclaim = \"g\"\nvalue = \"x\"\nmodels = [\"a*b\"]\n"),
        format!("{idp}jwks_url = \"https://example.com\"\n"), // no runtime fetching
    ] {
        assert!(parse_auth(&bad).is_err(), "accepted:\n{bad}");
    }
}

#[test]
fn aliases_parse_and_reject_duplicates() {
    let base = "base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n";
    assert!(parse(base).unwrap().aliases.is_none());
    let config = parse(&format!(
        "{base}[[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n\
         when = [{{ subjects = [\"canary-*\"], model = \"claude-sonnet-5-5\" }}]\n"
    ))
    .unwrap();
    let aliases = config.aliases.unwrap();
    let alias = aliases.get("coding").unwrap();
    assert_eq!(&**alias.model_for(Some("canary-1")), "claude-sonnet-5-5");
    assert_eq!(&**alias.model_for(Some("main")), "claude-opus-5-5");

    let twice = "[[aliases]]\nname = \"a\"\nmodel = \"m\"\n";
    assert_eq!(
        parse(&format!("{base}{twice}{twice}")).unwrap_err(),
        ConfigError::Invalid("aliases: duplicate name")
    );
    assert!(
        parse(&format!(
            "{base}[[aliases]]\nname = \"a\"\nmodel = \"m*\"\n"
        ))
        .is_err()
    );
}

/// The pricing example in docs/policy.md parses, so the documented format
/// and the parser cannot drift apart.
#[test]
fn documented_pricing_example_parses() {
    let doc = include_str!("../../../../docs/policy.md");
    let start = doc.find("```toml\n[pricing]").unwrap() + "```toml\n".len();
    let example = &doc[start..start + doc[start..].find("```").unwrap()];
    let base = "base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n";
    let pricing = parse(&format!("{base}{example}")).unwrap().pricing.unwrap();
    assert_eq!(&*pricing.version, "anthropic-2026-10-06");
    assert_eq!(pricing.entries.len(), 3);
    assert_eq!(pricing.entries[0].prices.input, 4_000_000);
    assert_eq!(pricing.entries[2].prices.cache_read, None);
    assert_eq!(pricing.entries[2].tiers[0].above_prompt_tokens, 100_000);

    let one = |extra: &str| {
        parse(&format!(
            "{base}[pricing]\nversion = \"v\"\n[[pricing.models]]\nmodels = [\"m\"]\ninput = \"1\"\noutput = \"2\"\n{extra}"
        ))
    };
    assert!(one("").is_ok());
    assert!(one("cache_read = \"0.1234567\"").is_err());
    assert!(one("cache_read = \"-1\"").is_err());
    assert!(one("upstreams = [\"x\"]").is_err());
    assert!(
        one("[[pricing.models.tiers]]\nabove_prompt_tokens = 5\n[[pricing.models.tiers]]\nabove_prompt_tokens = 5\n")
            .is_err()
    );
}

#[test]
fn upgrade_check_targets_are_unique_model_names() {
    let base = "base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n";
    let check = |body: &str| parse(&format!("{base}[upgrade_check]\n{body}"));
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
        "targets = [\"a\", \"a\"]",
        "targets = [\"a\"]\nsample_percent = 0",
    ] {
        assert!(check(bad).is_err(), "{bad}");
    }
}

mod routes;

mod examples;
