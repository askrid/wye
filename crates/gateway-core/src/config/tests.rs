use super::*;
use crate::test_support::config;

fn claude(config: &Config) -> &Route {
    config.routes.claude.as_ref().unwrap()
}

/// An upstream that needs no key of its own.
const FORWARDING: &str =
    "base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n";
/// A valid 32-byte key, base64url without padding.
const PUBLIC_KEY: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";

fn parse(upstream: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(&format!("[[upstreams]]\n{upstream}"))
}

/// `error` in the only upstream, named by its provider.
fn in_upstream(error: ConfigError) -> ConfigError {
    ConfigError::Upstream {
        id: "anthropic".into(),
        error: Box::new(error),
    }
}

/// A forwarding gateway with extra `[server]` lines.
fn server(lines: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n{lines}\n[[upstreams]]\n{FORWARDING}"
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
    let bare = Config::from_toml_str(&format!("[[upstreams]]\n{FORWARDING}")).unwrap();
    assert_eq!(bare.server.listen.to_string(), DEFAULT_LISTEN);
    assert_eq!(DEFAULT_LISTEN, "127.0.0.1:8080");
    let written = server("").unwrap();
    assert_eq!(
        format!("{:?}", bare.server),
        format!("{:?}", written.server)
    );
    let empty = Config::from_toml_str(&format!("[server]\n[[upstreams]]\n{FORWARDING}"));
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
        "[server]\nlisten = \"0.0.0.0:8080\"\n[[upstreams]]\n{FORWARDING}"
    ))
    .unwrap_err();
    assert!(matches!(err, ConfigError::NonLoopbackListen(_)));
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
fn a_provider_key_or_forwarding_exactly() {
    let error = |lines: &str| {
        parse(&format!(
            "base_url = \"https://api.anthropic.com\"\n{lines}"
        ))
        .unwrap_err()
        .to_string()
    };
    assert_eq!(
        error(""),
        "upstreams \"anthropic\": set provider_key = { env = \"NAME\" } or \
         { file = \"path\" }, or forward_caller_credential = true"
    );
    assert_eq!(
        error("provider_key = { env = \"K\" }\nforward_caller_credential = true"),
        "upstreams \"anthropic\": set provider_key or \
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
    assert!(config("").unwrap().cache_diagnostics.is_none());
    let with = |lines: &str| config(&format!("[cache_diagnostics]\n{lines}"));
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
    let with = |t: &str| config(&format!("[telemetry]\n{t}"));
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
    assert!(config("").unwrap().telemetry.is_none());
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
        "upstreams \"anthropic\": provider_key must name one source: \
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

mod auth;
mod examples;
mod models;
mod routes;
