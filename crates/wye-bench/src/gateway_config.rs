//! Configuration each gateway under test is started with.

use std::path::Path;

use gateway_core::credential::{self, NewCredential};
use ring::signature::KeyPair;

/// LiteLLM refuses to start without a master key and then authenticates every route.
pub(crate) const LITELLM_MASTER_KEY: &str = "sk-bench-master";
/// LiteLLM's own `/v1/messages` is its Anthropic API (parses requests, needs a
/// model mapping); its byte-level pass-through is mounted here instead.
pub(crate) const LITELLM_API_PREFIX: &str = "/pt";

/// Bifrost's Anthropic-native routes live under this prefix.
pub(crate) const BIFROST_API_PREFIX: &str = "/anthropic";
/// Bifrost relays Anthropic request and response bytes unchanged only for
/// Claude Code (detected by this user agent) asking for a Claude model;
/// other requests are normalized and re-encoded. Its best case is Claude Code.
pub(crate) const BIFROST_USER_AGENT: &str = "claude-cli/2.1.0 (external, cli)";

/// Provider key variable for the authenticated Wye variant.
pub(crate) const BENCH_PROVIDER_KEY_ENV: &str = "WYE_BENCH_PROVIDER_KEY";
/// Hash key variable for the cache-diagnostics Wye variant.
pub(crate) const BENCH_HASH_KEY_ENV: &str = "WYE_BENCH_HASH_KEY";

/// A throwaway signing key and one credential it signed.
pub(crate) struct BenchCredentials {
    pub(crate) public_key: String,
    pub(crate) token: String,
}

impl BenchCredentials {
    /// With `models`, the credential carries that `models` claim.
    pub(crate) fn generate(models: Option<&[&str]>) -> Self {
        let key = wye_testkit::ed25519_keypair();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = credential::issue(
            &key,
            "bench",
            &NewCredential {
                iss: "wye-bench",
                sub: "bench",
                aud: "wye-bench",
                iat: now,
                exp: now + 3_600,
                models,
                max_output_tokens: None,
            },
        );
        Self {
            public_key: credential::encode_public_key(key.public_key().as_ref()),
            token,
        }
    }
}

/// Optional parts of Wye's config.
#[derive(Default)]
pub(crate) struct WyeOptions<'a> {
    /// An `[auth]` section trusting this public key.
    pub(crate) auth_public_key: Option<&'a str>,
    /// `[auth] models`, a TOML list.
    pub(crate) auth_models: Option<&'a str>,
    /// `[cache_diagnostics]`, keyed from [`BENCH_HASH_KEY_ENV`].
    pub(crate) cache_diagnostics: bool,
    /// `[telemetry]` exporting to this OTLP endpoint.
    pub(crate) otlp_endpoint: Option<&'a str>,
    /// `[routes.openai]` instead of `[upstream]`: requests go to
    /// `/v1/responses`, with the key from [`BENCH_PROVIDER_KEY_ENV`].
    pub(crate) openai: bool,
    /// `[routes.claude]` with one Bedrock upstream instead of `[upstream]`:
    /// model ids are re-spelled, with the key from [`BENCH_PROVIDER_KEY_ENV`].
    pub(crate) bedrock: bool,
}

/// Wye's config in front of `upstream_base_url`.
pub(crate) fn wye_config(
    port: u16,
    upstream_base_url: &str,
    ca_file: Option<&Path>,
    options: &WyeOptions<'_>,
) -> String {
    let credential = if options.auth_public_key.is_some() || options.openai || options.bedrock {
        format!("api_key_env = \"{BENCH_PROVIDER_KEY_ENV}\"")
    } else {
        "forward_caller_credential = true".to_owned()
    };
    let upstream = if options.openai {
        "[routes.openai]\n[[routes.openai.upstreams]]\nid = \"openai\""
    } else if options.bedrock {
        "[routes.claude]\n[[routes.claude.upstreams]]\nid = \"bedrock\"\nprovider = \"bedrock\""
    } else {
        "[upstream]"
    };
    let mut config = format!(
        "[server]\nlisten = \"127.0.0.1:{port}\"\nshutdown_grace_secs = 0\n\n\
         {upstream}\nbase_url = \"{upstream_base_url}\"\n{credential}\n"
    );
    if let Some(path) = ca_file {
        config.push_str(&format!("ca_file = {:?}\n", path.display().to_string()));
    }
    // Tuning experiments: extra `[upstream]` lines without code changes.
    if let Ok(extra) = std::env::var("WYE_BENCH_UPSTREAM") {
        config.push_str(&extra);
        config.push('\n');
    }
    if let Some(key) = options.auth_public_key {
        config.push_str(&format!(
            "\n[auth]\nissuer = \"wye-bench\"\naudience = \"wye-bench\"\n{}\n\
             [[auth.keys]]\nid = \"bench\"\ned25519_public_key = \"{key}\"\n",
            options
                .auth_models
                .map_or(String::new(), |list| format!("models = {list}"))
        ));
    }
    if options.cache_diagnostics {
        config.push_str(&format!(
            "\n[cache_diagnostics]\nhash_key_env = \"{BENCH_HASH_KEY_ENV}\"\n"
        ));
    }
    if let Some(endpoint) = options.otlp_endpoint {
        config.push_str(&format!("\n[telemetry]\notlp_endpoint = \"{endpoint}\"\n"));
    }
    config
}

/// KrakenD in its documented streaming pass-through mode: `no-op` encoding,
/// every header and query string forwarded, one backend, long timeout.
/// Config format version 4 (KrakenD 3).
pub(crate) fn krakend_config(port: u16, upstream_base_url: &str, ca_file: Option<&Path>) -> String {
    let mut config = serde_json::json!({
        "version": 4,
        "port": port,
        "timeout": "600s",
        // KrakenD's best case: no access log and quiet logging, while Wye keeps
        // writing its per-request usage log.
        "extra_config": {
            "router": { "disable_access_log": true },
            "telemetry/logging": { "level": "ERROR", "stdout": true },
        },
        "endpoints": [{
            "endpoint": "/v1/messages",
            "method": "POST",
            "output_encoding": "no-op",
            "input_headers": ["*"],
            "input_query_strings": ["*"],
            "backend": [{
                "url_pattern": "/v1/messages",
                "encoding": "no-op",
                "host": [upstream_base_url],
            }],
        }],
    });
    if let Some(path) = ca_file {
        config["client_tls"] = serde_json::json!({ "ca_certs": [path.display().to_string()] });
    }
    config.to_string()
}

/// LiteLLM's byte-level pass-through endpoint (streams SSE as it arrives),
/// without a database, so no spend logging: its leanest proxy configuration.
/// The upstream CA comes from `SSL_VERIFY`.
pub(crate) fn litellm_config(upstream_base_url: &str) -> String {
    format!(
        "general_settings:\n  master_key: {LITELLM_MASTER_KEY}\n  pass_through_endpoints:\n    \
         - path: \"{LITELLM_API_PREFIX}/v1/messages\"\n      \
         target: \"{upstream_base_url}/v1/messages\"\n      \
         forward_headers: true\n      merge_query_params: true\n      timeout: 600\n"
    )
}

/// Bifrost with one Anthropic key and no stores (no config or log database,
/// no request logging), retries off, the pool and queue sizes of Bifrost's own
/// published benchmark (t3.xlarge), and provider workers above this
/// benchmark's peak (2,000 streams plus uploads), so neither queueing nor
/// bookkeeping is measured. `ca_pem` is the upstream's CA, trusted for
/// provider connections. `WYE_BENCH_BIFROST_POOLS=default` keeps Bifrost's
/// default pool and queue sizes instead: the published sizes are allocated up
/// front and inflate its memory.
pub(crate) fn bifrost_config(upstream_base_url: &str, ca_pem: Option<&str>) -> String {
    let published = std::env::var("WYE_BENCH_BIFROST_POOLS").as_deref() != Ok("default");
    let mut network = serde_json::json!({
        "base_url": upstream_base_url,
        "max_retries": 0,
        "default_request_timeout_in_seconds": 600,
        "stream_idle_timeout_in_seconds": 300,
    });
    if let Some(pem) = ca_pem {
        network["ca_cert_pem"] = pem.into();
    }
    let mut client = serde_json::json!({
        "enable_logging": false,
        "disable_content_logging": true,
        "drop_excess_requests": false,
    });
    if published {
        client["initial_pool_size"] = 15_000.into();
    }
    serde_json::json!({
        "client": client,
        "config_store": {"enabled": false},
        "logs_store": {"enabled": false},
        "providers": {
            "anthropic": {
                "keys": [{"name": "bench", "value": "sk-bench-upstream", "models": ["bench"], "weight": 1.0}],
                "network_config": network,
                "concurrency_and_buffer_size": {
                    "concurrency": 4_000,
                    "buffer_size": if published { 20_000 } else { 8_000 },
                },
            }
        }
    })
    .to_string()
}
