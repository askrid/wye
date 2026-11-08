use super::*;

fn claude(config: &Config) -> &Route {
    config.routes.claude.as_ref().unwrap()
}

/// An upstream that needs no key of its own.
const FORWARDING: &str =
    "base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n";
/// A valid 32-byte key, base64url without padding.
const PUBLIC_KEY: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";

fn parse(upstream: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[routes.claude.upstreams]]\n{upstream}"
    ))
}

/// `error` in the Claude route's only upstream, named by its provider.
fn in_upstream(error: ConfigError) -> ConfigError {
    ConfigError::RouteUpstream {
        route: "claude",
        id: "anthropic".into(),
        error: Box::new(error),
    }
}

/// A forwarding gateway with extra `[server]` lines.
fn server(lines: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n{lines}\n[[routes.claude.upstreams]]\n{FORWARDING}"
    ))
}

#[test]
fn minimal_config_gets_defaults() {
    let config = parse(
        "base_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"ANTHROPIC_API_KEY\" }",
    )
    .unwrap();
    assert_eq!(
        config.server.max_request_body_bytes,
        DEFAULT_MAX_REQUEST_BODY_BYTES
    );
    let upstream = &claude(&config).upstreams()[0];
    assert_eq!(upstream.scheme, Scheme::HTTPS);
    assert_eq!(upstream.authority.as_str(), "api.anthropic.com");
    assert_eq!(upstream.path_prefix, "");
    assert_eq!(
        upstream.provider_key,
        ProviderKey::Secret(Secret::Env("ANTHROPIC_API_KEY".into()))
    );
    assert_eq!(upstream.response_header_timeout, Duration::from_secs(600));
}

#[test]
fn server_is_optional_and_listens_on_loopback_port_8080() {
    let bare =
        Config::from_toml_str(&format!("[[routes.claude.upstreams]]\n{FORWARDING}")).unwrap();
    assert_eq!(bare.server.listen.to_string(), DEFAULT_LISTEN);
    assert_eq!(DEFAULT_LISTEN, "127.0.0.1:8080");
    let written = server("").unwrap();
    assert_eq!(
        format!("{:?}", bare.server),
        format!("{:?}", written.server)
    );
    let empty = Config::from_toml_str(&format!(
        "[server]\n[[routes.claude.upstreams]]\n{FORWARDING}"
    ));
    assert_eq!(empty.unwrap().server.listen, bare.server.listen);
}

#[test]
fn base_path_keeps_prefix_without_trailing_slash() {
    let config =
        parse("base_url = \"https://gw.example.com/anthropic/\"\nforward_caller_credential = true")
            .unwrap();
    let upstream = &claude(&config).upstreams()[0];
    assert_eq!(upstream.path_prefix, "/anthropic");
    assert_eq!(upstream.provider_key, ProviderKey::ForwardCaller);
}

#[test]
fn rejects_non_loopback_listen() {
    let err = Config::from_toml_str(&format!(
        "[server]\nlisten = \"0.0.0.0:8080\"\n[[routes.claude.upstreams]]\n{FORWARDING}"
    ))
    .unwrap_err();
    assert!(matches!(err, ConfigError::NonLoopbackListen(_)));
}

#[test]
fn auth_allows_network_listen_and_validates_keys() {
    let with_auth = |listen: &str, upstream: &str, auth: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"{listen}\"\n[[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\n\
             {upstream}\n[auth]\naudience = \"wye-prod\"\n{auth}"
        ))
    };
    let keys = format!("signing_keys = [\"{PUBLIC_KEY}\"]");
    let config = with_auth("0.0.0.0:8080", "provider_key = { env = \"K\" }", &keys).unwrap();
    let auth = config.auth.unwrap();
    let own = auth.own.unwrap();
    assert_eq!(own.keys.len(), 1);
    // Named by its RFC 7638 thumbprint (RFC 8037, appendix A.3).
    assert_eq!(
        own.keys[0].thumbprint,
        "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k"
    );
    assert_eq!(own.keys[0].encoded(), PUBLIC_KEY);
    assert_eq!(
        auth.max_gateway_token_ttl,
        Duration::from_secs(7 * 24 * 3600)
    );

    let error = |upstream: &str, auth: &str| {
        with_auth("127.0.0.1:8080", upstream, auth)
            .unwrap_err()
            .to_string()
    };
    let key = "provider_key = { env = \"K\" }";
    assert!(
        error("forward_caller_credential = true", &keys)
            .starts_with("auth: cannot be combined with forward_caller_credential")
    );
    assert_eq!(
        error(key, "signing_keys = []"),
        "auth: audience needs at least one key in signing_keys"
    );
    assert_eq!(
        error(key, "signing_keys = [\"not-a-key\"]"),
        "auth: signing_keys: \"not-a-key\" is not an Ed25519 public key, 32 bytes in \
         unpadded base64url"
    );
    assert_eq!(
        error(
            key,
            &format!("signing_keys = [\"{PUBLIC_KEY}\", \"{PUBLIC_KEY}\"]")
        ),
        format!("auth: signing_keys: \"{PUBLIC_KEY}\" is listed twice")
    );
    // Wye's own tokens always name the issuer "wye", and keys need no id.
    assert!(error(key, &format!("{keys}\nissuer = \"wye\"")).contains("unknown field"));
    let table =
        format!("[[auth.signing_keys]]\nid = \"k1\"\ned25519_public_key = \"{PUBLIC_KEY}\"");
    assert!(error(key, &table).starts_with("invalid configuration"));
}

/// Gateway tokens cannot be revoked one by one, so how long one may live
/// has a hard cap.
#[test]
fn gateway_token_lifetimes_are_capped_at_30_days() {
    let with_ttl = |ttl: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[[routes.claude.upstreams]]\n\
             base_url = \"https://api.anthropic.com\"\nprovider_key = {{ env = \"K\" }}\n\
             [auth]\naudience = \"wye-prod\"\nmax_gateway_token_ttl = \"{ttl}\"\n\
             signing_keys = [\"{PUBLIC_KEY}\"]"
        ))
    };
    let ttl = |text| with_ttl(text).unwrap().auth.unwrap().max_gateway_token_ttl;
    assert_eq!(ttl("30d"), Duration::from_secs(2_592_000));
    assert_eq!(ttl("1h"), Duration::from_secs(3600));
    assert_eq!(
        with_ttl("2592001s").unwrap_err(),
        ConfigError::TooLong {
            field: "auth.max_gateway_token_ttl",
            max: MAX_GATEWAY_TOKEN_TTL
        }
    );
    assert_eq!(
        with_ttl("31d").unwrap_err().to_string(),
        "auth.max_gateway_token_ttl must be at most 30d"
    );
    assert_eq!(
        with_ttl("0s").unwrap_err(),
        ConfigError::Zero("auth.max_gateway_token_ttl")
    );
}

#[test]
fn auth_models_are_validated_patterns() {
    let with_models = |models: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\n\
             provider_key = {{ env = \"K\" }}\n[auth]\naudience = \"wye-prod\"\n{models}\n\
             signing_keys = [\"{PUBLIC_KEY}\"]"
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
}

#[test]
fn rejects_unknown_fields() {
    let err = parse(
        "base_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"K\" }\napi_key = \"sk-secret\"",
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)));
}

#[test]
fn credential_must_be_exactly_one() {
    let error = |lines: &str| {
        parse(&format!(
            "base_url = \"https://api.anthropic.com\"\n{lines}"
        ))
        .unwrap_err()
        .to_string()
    };
    assert_eq!(
        error(""),
        "routes.claude.upstreams \"anthropic\": set provider_key = { env = \"NAME\" } or \
         { file = \"path\" }, or forward_caller_credential = true"
    );
    assert_eq!(
        error("provider_key = { env = \"K\" }\nforward_caller_credential = true"),
        "routes.claude.upstreams \"anthropic\": set provider_key or \
         forward_caller_credential, not both"
    );
    assert_eq!(
        parse("base_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"1BAD\" }")
            .unwrap_err(),
        in_upstream(ConfigError::InvalidEnvName {
            field: "provider_key",
            name: "1BAD".into()
        })
    );
    // The old shapes are gone: one shape for every secret.
    for old in [
        "provider_key_env = \"K\"",
        "provider_key_file = \"k\"",
        "provider_key = \"K\"",
    ] {
        assert!(error(old).contains("invalid configuration"), "{old}");
    }
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
    let config = parse(&format!("{FORWARDING}ca_file = \"certs/ca.pem\"")).unwrap();
    assert_eq!(
        claude(&config).upstreams()[0].ca_file,
        Some(PathBuf::from("certs/ca.pem"))
    );
    assert_eq!(
        claude(&parse(FORWARDING).unwrap()).upstreams()[0].ca_file,
        None
    );
    assert_eq!(
        parse(&format!("{FORWARDING}ca_file = \"\"")).unwrap_err(),
        in_upstream(ConfigError::Empty("ca_file"))
    );
}

#[test]
fn lifecycle_defaults_and_zero_limits_rejected() {
    let config = parse(FORWARDING).unwrap();
    assert_eq!(config.server.max_concurrent_requests, 10_000);
    assert_eq!(config.server.drain_delay, Duration::ZERO);
    assert_eq!(config.server.shutdown_grace, Duration::from_secs(30));
    assert_eq!(
        server("max_concurrent_requests = 0").unwrap_err(),
        ConfigError::Zero("server.max_concurrent_requests")
    );
    let timers = server("drain_delay = \"10s\"\nshutdown_grace = \"2m\"").unwrap();
    assert_eq!(timers.server.drain_delay, Duration::from_secs(10));
    assert_eq!(timers.server.shutdown_grace, Duration::from_secs(120));
    assert_eq!(
        server("drain_delay = \"10\"").unwrap_err().to_string(),
        "server.drain_delay must be a duration such as 30s, 10m, 8h, 7d"
    );
    assert!(
        server("drain_delay = 10")
            .unwrap_err()
            .to_string()
            .starts_with("invalid configuration")
    );
}

/// Internal bounds are fixed or follow from the configured ones, never
/// configured themselves.
#[test]
fn internal_bounds_are_derived_not_configured() {
    let config = parse(FORWARDING).unwrap();
    assert_eq!(config.server.max_total_held_bytes(), 64 * 1024 * 1024);
    assert_eq!(config.server.max_connections(), 20_000);
    let small = server("max_request_body_bytes = 1024\nmax_concurrent_requests = 100").unwrap();
    // One body may be held whole, and a request in flight holds a connection.
    assert_eq!(small.server.max_total_held_bytes(), 2048);
    assert_eq!(small.server.max_connections(), 200);
    assert_eq!(
        server("max_concurrent_requests = 1000000")
            .unwrap()
            .server
            .max_connections(),
        MAX_CONNECTIONS
    );
    for gone in [
        "max_total_held_bytes = 1048576",
        "max_total_kept_bytes = 1024",
        "max_connections = 100",
        "client_write_timeout = \"1m\"",
        "client_write_timeout_secs = 60",
        "drain_delay_secs = 0",
    ] {
        assert!(
            server(gone)
                .unwrap_err()
                .to_string()
                .contains("unknown field"),
            "{gone}"
        );
    }
}

#[test]
fn cache_diagnostics_require_a_key() {
    let base = "[server]\nlisten = \"127.0.0.1:8080\"\n[[routes.claude.upstreams]]\n\
                base_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"K\" }\n";
    assert!(
        Config::from_toml_str(base)
            .unwrap()
            .cache_diagnostics
            .is_none()
    );
    let with = |lines: &str| Config::from_toml_str(&format!("{base}[cache_diagnostics]\n{lines}"));
    let diagnostics = with("hash_key = { env = \"WYE_CACHE_HASH_KEY\" }")
        .unwrap()
        .cache_diagnostics
        .unwrap();
    assert_eq!(
        diagnostics.hash_key,
        Secret::Env("WYE_CACHE_HASH_KEY".into())
    );
    assert_eq!(diagnostics.sample_percent, 100);
    assert_eq!(
        with("hash_key = { file = \"cache-hash.key\" }")
            .unwrap()
            .cache_diagnostics
            .unwrap()
            .hash_key,
        Secret::File("cache-hash.key".into())
    );
    let with_percent = |p: &str| {
        with(&format!(
            "hash_key = {{ env = \"K\" }}\nsample_percent = {p}"
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
    assert!(matches!(with(""), Err(ConfigError::Parse(_))));
    assert_eq!(
        with("hash_key = { env = \"a-b\" }").unwrap_err(),
        ConfigError::InvalidEnvName {
            field: "cache_diagnostics.hash_key",
            name: "a-b".into()
        }
    );
    for none_or_both in ["hash_key = {}", "hash_key = { env = \"K\", file = \"k\" }"] {
        assert_eq!(
            with(none_or_both).unwrap_err().to_string(),
            "cache_diagnostics.hash_key must name one source: { env = \"NAME\" } or \
             { file = \"path\" }",
            "{none_or_both}"
        );
    }
    assert!(matches!(
        with("hash_key = { env = \"K\", vault = \"x\" }"),
        Err(ConfigError::Parse(_))
    ));
    assert_eq!(
        with("hash_key = { file = \"\" }").unwrap_err(),
        ConfigError::Empty("cache_diagnostics.hash_key")
    );
}

#[test]
fn rejects_zero_timeouts_and_timers_past_a_day() {
    let upstream = |lines: &str| parse(&format!("{FORWARDING}{lines}"));
    let config = upstream(
        "connect_timeout = \"3s\"\nresponse_header_timeout = \"10m\"\nstream_idle_timeout = \"1h\"",
    )
    .unwrap();
    let timers = &claude(&config).upstreams()[0];
    assert_eq!(timers.connect_timeout, Duration::from_secs(3));
    assert_eq!(timers.response_header_timeout, Duration::from_secs(600));
    assert_eq!(timers.stream_idle_timeout, Duration::from_secs(3600));
    assert_eq!(
        upstream("stream_idle_timeout = \"0s\"").unwrap_err(),
        in_upstream(ConfigError::Zero("stream_idle_timeout"))
    );
    assert_eq!(
        upstream("stream_idle_timeout = \"86401s\"").unwrap_err(),
        in_upstream(ConfigError::TooLong {
            field: "stream_idle_timeout",
            max: Duration::from_secs(86_400)
        })
    );
    // The largest number would overflow `Instant + timeout`.
    assert!(matches!(
        server(&format!("shutdown_grace = \"{}s\"", u64::MAX)),
        Err(ConfigError::TooLong { .. })
    ));
    assert_eq!(
        server("shutdown_grace = \"2d\"").unwrap_err().to_string(),
        "server.shutdown_grace must be at most 1d"
    );
    assert!(server("shutdown_grace = \"1d\"").is_ok());
    assert!(server("shutdown_grace = \"0s\"").is_ok());
    assert_eq!(
        upstream("connect_timeout = \"ten\"").unwrap_err(),
        in_upstream(ConfigError::Duration("connect_timeout"))
    );
    assert!(
        upstream("connect_timeout_secs = 10")
            .unwrap_err()
            .to_string()
            .contains("unknown field")
    );
}

#[test]
fn telemetry_endpoint_gets_the_logs_path() {
    let base = "[server]\nlisten = \"127.0.0.1:8080\"\n[[routes.claude.upstreams]]\n\
                base_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"K\" }\n";
    let with = |t: &str| Config::from_toml_str(&format!("{base}[telemetry]\n{t}"));
    let telemetry = with("otlp_endpoint = \"http://localhost:4318\"")
        .unwrap()
        .telemetry
        .unwrap();
    assert_eq!(telemetry.otlp_logs_uri, "http://localhost:4318/v1/logs");
    assert_eq!(
        telemetry.otlp_metrics_uri,
        "http://localhost:4318/v1/metrics"
    );
    assert_eq!(telemetry.otlp_traces_uri, "http://localhost:4318/v1/traces");
    assert_eq!(telemetry.service_name, "wye");
    assert_eq!(telemetry.otlp_headers, None);
    let telemetry = with(
        "otlp_endpoint = \"https://otlp.example.com/otel/\"\notlp_headers = { env = \"OTLP_HEADERS\" }\nservice_name = \"gw\"",
    )
    .unwrap()
    .telemetry
    .unwrap();
    assert_eq!(
        telemetry.otlp_logs_uri,
        "https://otlp.example.com/otel/v1/logs"
    );
    assert_eq!(
        telemetry.otlp_headers,
        Some(Secret::Env("OTLP_HEADERS".into()))
    );
    assert_eq!(
        with(
            "otlp_endpoint = \"http://localhost:4318\"\notlp_headers = { file = \"otlp.headers\" }"
        )
        .unwrap()
        .telemetry
        .unwrap()
        .otlp_headers,
        Some(Secret::File("otlp.headers".into()))
    );
    for bad in [
        "otlp_endpoint = \"ftp://c\"",
        "otlp_endpoint = \"http://c/?x=1\"",
        "otlp_endpoint = \"/relative\"",
        // Secrets come from otlp_headers, never from the URL.
        "otlp_endpoint = \"https://user:secret@collector\"",
        // Records and OTLP headers would cross the network in the clear.
        "otlp_endpoint = \"http://collector:4318\"",
        "otlp_endpoint = \"http://10.0.0.5:4318\"",
    ] {
        assert!(
            matches!(with(bad), Err(ConfigError::InvalidUrl { .. })),
            "{bad}"
        );
    }
    assert!(Config::from_toml_str(base).unwrap().telemetry.is_none());
    for local in ["http://127.0.0.1:4318", "http://[::1]:4318"] {
        let endpoint = format!("otlp_endpoint = \"{local}\"");
        assert!(with(&endpoint).is_ok(), "{local}");
    }
    assert_eq!(
        with("otlp_endpoint = \"http://collector:4318\"")
            .unwrap_err()
            .to_string(),
        "telemetry.otlp_endpoint \"http://collector:4318\": plain http is allowed only for \
         loopback hosts"
    );
}

#[test]
fn a_provider_key_comes_from_the_environment_or_a_file() {
    let config = parse(
        "base_url = \"https://api.anthropic.com\"\nprovider_key = { file = \"secrets/key\" }",
    )
    .unwrap();
    assert_eq!(
        claude(&config).upstreams()[0].provider_key,
        ProviderKey::Secret(Secret::File(PathBuf::from("secrets/key")))
    );
    assert_eq!(
        claude(&config).upstreams()[0].provider_key.to_string(),
        "secrets/key"
    );
    let error = |key: &str| {
        parse(&format!("base_url = \"https://api.anthropic.com\"\n{key}"))
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        error("provider_key = { file = \"k\", env = \"K\" }"),
        "routes.claude.upstreams \"anthropic\": provider_key must name one source: \
         { env = \"NAME\" } or { file = \"path\" }"
    );
    assert_eq!(
        parse("base_url = \"https://api.anthropic.com\"\nprovider_key = { file = \"\" }")
            .unwrap_err(),
        in_upstream(ConfigError::Empty("provider_key"))
    );
}

#[test]
fn http_version_is_auto_unless_pinned_to_1_1() {
    assert_eq!(
        claude(&parse(FORWARDING).unwrap()).upstreams()[0].http_version,
        UpstreamHttpVersion::Auto
    );
    assert_eq!(
        claude(&parse(&format!("{FORWARDING}http_version = \"1.1\"")).unwrap()).upstreams()[0]
            .http_version,
        UpstreamHttpVersion::Http1
    );
    assert_eq!(
        parse(&format!("{FORWARDING}http_version = \"2\"")).unwrap_err(),
        in_upstream(ConfigError::Invalid(
            "http_version must be \"auto\" or \"1.1\""
        ))
    );
}

#[test]
fn discovery_lists_unique_model_names() {
    let with = |models: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key = {{ env = \"K\" }}\n\
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
fn identity_provider_issuers_are_validated() {
    let parse_auth = |auth: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"0.0.0.0:8080\"\n[[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\n\
             provider_key = {{ env = \"K\" }}\n[auth]\n{auth}"
        ))
    };
    let idp = "[[auth.identity_providers]]\nissuer = \"https://token.actions.githubusercontent.com\"\n\
               audience = \"wye-prod\"\nalgorithm = \"RS256\"\njwks_file = \"github.jwks\"\n";
    // Issuers alone are enough: Wye's own keys are optional.
    let auth = parse_auth(idp).unwrap().auth.unwrap();
    assert!(auth.own.is_none());
    let issuer = &auth.identity_providers[0];
    assert_eq!(issuer.algorithm, Algorithm::Rs256);
    assert_eq!(issuer.subject_claim, "sub");
    assert!(issuer.grants.is_empty());

    let grants = format!(
        "{idp}subject_claim = \"repository\"\n[[auth.identity_providers.grants]]\nclaim = \"repository_owner\"\n\
         value = \"acme\"\nmodels = [\"claude-haiku-*\"]\n"
    );
    let auth = parse_auth(&grants).unwrap().auth.unwrap();
    assert_eq!(auth.identity_providers[0].subject_claim, "repository");
    assert_eq!(
        auth.identity_providers[0].grants[0].models[0].to_string(),
        "claude-haiku-*"
    );

    let own = format!("audience = \"wye-prod\"\nsigning_keys = [\"{PUBLIC_KEY}\"]\n");
    let auth = parse_auth(&format!("{own}{idp}")).unwrap().auth.unwrap();
    assert!(auth.own.is_some() && auth.identity_providers.len() == 1);

    for bad in [
        String::new(),                          // neither own keys nor identity providers
        "audience = \"wye-prod\"\n".to_owned(), // own audience without keys
        format!("signing_keys = [\"{PUBLIC_KEY}\"]\n"), // own keys without an audience
        idp.replace("RS256", "HS256"),          // HMAC is never accepted
        idp.replace("RS256", "none"),
        idp.replace("github.jwks", ""),
        format!("{idp}{idp}"), // duplicate issuer
        // Wye's own tokens name the issuer "wye".
        format!(
            "{own}{}",
            idp.replace("https://token.actions.githubusercontent.com", "wye")
        ),
        format!("{idp}subject_claim = \"\"\n"),
        format!(
            "{idp}[[auth.identity_providers.grants]]\nclaim = \"groups\"\nvalue = \"x\"\nmodels = []\n"
        ),
        format!(
            "{idp}[[auth.identity_providers.grants]]\nclaim = \"\"\nvalue = \"x\"\nmodels = [\"a\"]\n"
        ),
        format!(
            "{idp}[[auth.identity_providers.grants]]\nclaim = \"g\"\nvalue = \"x\"\nmodels = [\"a*b\"]\n"
        ),
        format!("{idp}jwks_url = \"https://example.com\"\n"), // no runtime fetching
    ] {
        assert!(parse_auth(&bad).is_err(), "accepted:\n{bad}");
    }
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
    let doc = include_str!("../../../../docs/policy.md");
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

mod routes;

mod examples;
