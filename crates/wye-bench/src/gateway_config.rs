//! Configuration each gateway under test is started with.

use std::path::Path;

use gateway_core::credential::{self, NewCredential};
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};

/// LiteLLM refuses to start without a master key and then authenticates every route.
pub(crate) const LITELLM_MASTER_KEY: &str = "sk-bench-master";
/// LiteLLM's own `/v1/messages` is its Anthropic API (parses requests, needs a
/// model mapping); its byte-level pass-through is mounted here instead.
pub(crate) const LITELLM_API_PREFIX: &str = "/pt";

/// Provider key variable for the authenticated Wye variant.
pub(crate) const BENCH_PROVIDER_KEY_ENV: &str = "WYE_BENCH_PROVIDER_KEY";

/// A throwaway signing key and one credential it signed.
pub(crate) struct BenchCredentials {
    pub(crate) public_key: String,
    pub(crate) token: String,
}

impl BenchCredentials {
    pub(crate) fn generate() -> Self {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
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
            },
        );
        Self {
            public_key: credential::encode_public_key(key.public_key().as_ref()),
            token,
        }
    }
}

/// Wye's config; with `auth_public_key`, an `[auth]` section trusting it.
pub(crate) fn wye_config(
    port: u16,
    upstream_base_url: &str,
    ca_file: Option<&Path>,
    auth_public_key: Option<&str>,
) -> String {
    let credential = match auth_public_key {
        Some(_) => format!("api_key_env = \"{BENCH_PROVIDER_KEY_ENV}\""),
        None => "forward_caller_credential = true".to_owned(),
    };
    let mut config = format!(
        "[server]\nlisten = \"127.0.0.1:{port}\"\nshutdown_grace_secs = 0\n\n\
         [upstream]\nbase_url = \"{upstream_base_url}\"\n{credential}\n"
    );
    if let Some(path) = ca_file {
        config.push_str(&format!("ca_file = {:?}\n", path.display().to_string()));
    }
    // Tuning experiments: extra `[upstream]` lines without code changes.
    if let Ok(extra) = std::env::var("WYE_BENCH_UPSTREAM") {
        config.push_str(&extra);
        config.push('\n');
    }
    if let Some(key) = auth_public_key {
        config.push_str(&format!(
            "\n[auth]\nissuer = \"wye-bench\"\naudience = \"wye-bench\"\n\
             [[auth.keys]]\nid = \"bench\"\ned25519_public_key = \"{key}\"\n"
        ));
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
