use super::*;

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
    assert_eq!(config.upstream.scheme, Scheme::HTTPS);
    assert_eq!(config.upstream.authority.as_str(), "api.anthropic.com");
    assert_eq!(config.upstream.path_prefix, "");
    assert_eq!(
        config.upstream.credential,
        UpstreamCredential::ApiKeyEnv("ANTHROPIC_API_KEY".into())
    );
    assert_eq!(
        config.upstream.response_header_timeout,
        Duration::from_secs(600)
    );
}

#[test]
fn base_path_keeps_prefix_without_trailing_slash() {
    let config =
        parse("base_url = \"https://gw.example.com/anthropic/\"\nforward_caller_credential = true")
            .unwrap();
    assert_eq!(config.upstream.path_prefix, "/anthropic");
    assert_eq!(
        config.upstream.credential,
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
    assert_eq!(auth.keys.len(), 1);
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
    assert_eq!(config.upstream.ca_file, Some(PathBuf::from("certs/ca.pem")));
    assert_eq!(parse(ok).unwrap().upstream.ca_file, None);
    assert_eq!(
        parse(&format!("{ok}ca_file = \"\"")).unwrap_err(),
        ConfigError::EmptyPath("upstream.ca_file")
    );
}

#[test]
fn lifecycle_defaults_and_zero_concurrency_rejected() {
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
        config.upstream.credential,
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
