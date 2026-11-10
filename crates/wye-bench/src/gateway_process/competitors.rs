//! How the competitors under test are started (KrakenD, LiteLLM, Bifrost,
//! agentgateway): each at its best case for this traffic.

use std::path::Path;

use super::{
    Gateway,
    launch::{Launch, Setup},
};
use crate::client::Target;

/// LiteLLM refuses to start without a master key and then authenticates every route.
const LITELLM_MASTER_KEY: &str = "sk-bench-master";
/// LiteLLM's own `/v1/messages` is its Anthropic API (parses requests, needs a
/// model mapping); its byte-level pass-through is mounted here instead.
const LITELLM_API_PREFIX: &str = "/pt";

/// Bifrost's Anthropic-native routes live under this prefix.
const BIFROST_API_PREFIX: &str = "/anthropic";
/// Bifrost relays Anthropic request and response bytes unchanged only for
/// Claude Code (detected by this user agent) asking for a Claude model;
/// other requests are normalized and re-encoded. Its best case is Claude Code.
const BIFROST_USER_AGENT: &str = "claude-cli/2.1.0 (external, cli)";

impl Launch {
    pub(super) fn krakend(program: &str, gateway: Gateway<'_>, setup: &Setup<'_>) -> Self {
        let config = krakend_config(setup.port, setup.upstream, setup.ca_file.as_deref());
        Self {
            program: program.into(),
            args: vec![
                "run".into(),
                "-c".into(),
                setup.config(gateway, "json", config),
            ],
            env: Vec::new(),
            health: "/__health",
            target: Target::plain(setup.addr),
        }
    }

    /// `cpus`: how many CPUs the gateway is pinned to, if it is.
    pub(super) fn litellm(
        program: &str,
        gateway: Gateway<'_>,
        setup: &Setup<'_>,
        cpus: Option<usize>,
    ) -> Self {
        Self {
            program: program.into(),
            args: vec![
                "--config".into(),
                setup.config(gateway, "yaml", litellm_config(setup.upstream)),
                "--port".into(),
                setup.port.to_string(),
                // LiteLLM's production recommendation: one worker per core,
                // here per CPU it may run on.
                "--num_workers".into(),
                cpus.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
                    .to_string(),
            ],
            env: setup
                .ca_file
                .iter()
                // Pass-through requests trust the CA path in SSL_VERIFY. Not
                // SSL_CERT_FILE: it replaces the system roots for every client,
                // breaking LiteLLM's other outbound HTTPS calls.
                .map(|path| ("SSL_VERIFY", path.display().to_string()))
                .chain([("LITELLM_LOG", "ERROR".into())])
                .collect(),
            health: "/health/liveliness",
            target: Target::gateway(
                setup.addr,
                LITELLM_API_PREFIX,
                vec![("authorization", format!("Bearer {LITELLM_MASTER_KEY}"))],
            ),
        }
    }

    pub(super) fn bifrost(program: &str, setup: &Setup<'_>) -> Self {
        // Bifrost reads `config.json` from its app directory.
        let app_dir = setup.dir.join(format!("bench-{}-bifrost", setup.name));
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.json"),
            bifrost_config(setup.upstream, setup.ca_pem),
        )
        .unwrap();
        Self {
            program: program.into(),
            args: vec![
                "-app-dir".into(),
                app_dir.display().to_string(),
                "-host".into(),
                "127.0.0.1".into(),
                "-port".into(),
                setup.port.to_string(),
                "-log-level".into(),
                "error".into(),
            ],
            env: Vec::new(),
            health: "/health",
            target: Target::gateway(
                setup.addr,
                BIFROST_API_PREFIX,
                vec![("user-agent", BIFROST_USER_AGENT.into())],
            )
            // Bifrost forwards only allowlisted headers, plus any sent
            // as `x-bf-eh-<name>` (as `<name>`); its Anthropic route drops
            // those when the request carries `anthropic-version`, which
            // Bifrost adds upstream itself.
            .with_control_prefix("x-bf-eh-"),
        }
    }

    pub(super) fn agentgateway(program: &str, gateway: Gateway<'_>, setup: &Setup<'_>) -> Self {
        let config = agentgateway_config(setup.port, setup.upstream, setup.ca_file.as_deref());
        Self {
            program: program.into(),
            args: vec!["-f".into(), setup.config(gateway, "yaml", config)],
            env: Vec::new(),
            // The LLM listener's model list; readiness has a port of its own.
            health: "/v1/models",
            target: Target::plain(setup.addr),
        }
    }
}

/// KrakenD in its documented streaming pass-through mode: `no-op` encoding,
/// every header and query string forwarded, one backend, long timeout.
/// Config format version 4 (KrakenD 3).
fn krakend_config(port: u16, upstream_base_url: &str, ca_file: Option<&Path>) -> String {
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
fn litellm_config(upstream_base_url: &str) -> String {
    format!(
        "general_settings:\n  master_key: {LITELLM_MASTER_KEY}\n  pass_through_endpoints:\n    \
         - path: \"{LITELLM_API_PREFIX}/v1/messages\"\n      \
         target: \"{upstream_base_url}/v1/messages\"\n      \
         forward_headers: true\n      merge_query_params: true\n      timeout: 600\n"
    )
}

/// agentgateway's LLM listener at its best case for this traffic: one model,
/// the one requests name, in `detect` passthrough (the request is relayed
/// unchanged and its usage read, as Wye does), access logging off, and a
/// request buffer as large as Wye's body limit. Its admin, stats, and
/// readiness listeners take free ports, so runs do not collide.
fn agentgateway_config(port: u16, upstream_base_url: &str, ca_file: Option<&Path>) -> String {
    let free = || {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap()
    };
    let tls = ca_file
        .map(|ca| format!("    tls:\n      root: {:?}\n", ca.display().to_string()))
        .unwrap_or_default();
    format!(
        "config:\n  adminAddr: {}\n  statsAddr: {}\n  readinessAddr: {}\n\
         \x20 logging:\n    level: error\n\
         frontendPolicies:\n  accessLog:\n    filter: \"false\"\n\
         \x20 http:\n    maxBufferSize: 33554432\n\
         llm:\n  port: {port}\n  models:\n  - name: anthropic/bench\n    provider: anthropic\n\
         \x20   passthrough: detect\n    params:\n      apiKey: sk-bench-upstream\n\
         \x20     baseUrl: {upstream_base_url}\n{tls}",
        free(),
        free(),
        free(),
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
fn bifrost_config(upstream_base_url: &str, ca_pem: Option<&str>) -> String {
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
