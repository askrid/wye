//! How each gateway under test is started: its program, arguments,
//! environment, health path, and the target clients load.

use std::path::{Path, PathBuf};

use super::Gateway;
use crate::{
    client::Target,
    gateway_config::{
        BENCH_HASH_KEY_ENV, BENCH_PROVIDER_KEY_ENV, BIFROST_API_PREFIX, BIFROST_USER_AGENT,
        BenchCredentials, LITELLM_API_PREFIX, LITELLM_MASTER_KEY, WyeOptions, bifrost_config,
        krakend_config, litellm_config, wye_config,
    },
};

/// [`Gateway::WyeRefused`]'s first account refuses one request in this many.
const REFUSE_EVERY: u64 = 4;

/// How to launch one gateway in front of an upstream.
pub(super) struct Launch {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) env: Vec<(&'static str, String)>,
    pub(super) health: &'static str,
    pub(super) target: Target,
}

/// Where a gateway listens and what it forwards to, and where its files go.
pub(super) struct Setup<'a> {
    pub(super) port: u16,
    pub(super) addr: std::net::SocketAddr,
    pub(super) upstream: &'a str,
    pub(super) ca_pem: Option<&'a str>,
    /// `ca_pem` written to a file, absolute: Wye resolves relative paths
    /// against its config file, the others against their working directory.
    pub(super) ca_file: Option<PathBuf>,
    pub(super) dir: &'a Path,
    /// Distinguishes one run's files from another's (`plain`, `tls`).
    pub(super) name: &'a str,
}

impl Setup<'_> {
    /// Writes `contents` as this run's config file for `gateway`; returns its path.
    fn config(&self, gateway: Gateway<'_>, extension: &str, contents: String) -> String {
        let path = self.dir.join(format!(
            "bench-{}-{}.{extension}",
            self.name,
            gateway.name()
        ));
        std::fs::write(&path, contents).unwrap();
        path.display().to_string()
    }
}

impl Launch {
    pub(super) async fn new(gateway: Gateway<'_>, setup: &Setup<'_>) -> Self {
        match gateway {
            Gateway::Wye(program)
            | Gateway::WyeAuth(program)
            | Gateway::WyeModels(program)
            | Gateway::WyeDiag(program)
            | Gateway::WyeOtlp(program)
            | Gateway::WyeOpenai(program)
            | Gateway::WyeBedrock(program)
            | Gateway::WyeFailover(program)
            | Gateway::WyeRefused(program) => Self::wye(program, gateway, setup).await,
            Gateway::KrakenD(program) => Self::krakend(program, gateway, setup),
            Gateway::LiteLlm(program) => Self::litellm(program, gateway, setup),
            Gateway::Bifrost(program) => Self::bifrost(program, setup),
        }
    }

    async fn wye(program: &str, gateway: Gateway<'_>, setup: &Setup<'_>) -> Self {
        // The refusing first account, spawned for this process only.
        let refusing = match gateway {
            Gateway::WyeRefused(_) => Some(format!(
                "http://{}",
                crate::mock::spawn_refusing(REFUSE_EVERY).await
            )),
            _ => None,
        };
        let authenticated = matches!(gateway, Gateway::WyeAuth(_) | Gateway::WyeModels(_));
        let models = matches!(gateway, Gateway::WyeModels(_));
        // Requests name model "anthropic/bench" (Bifrost relays bytes unchanged
        // only for a provider-qualified model): allowed by both lists.
        let credentials = authenticated
            .then(|| BenchCredentials::generate(models.then_some(&["anthropic/bench"][..])));
        let options = WyeOptions {
            auth_public_key: credentials.as_ref().map(|c| c.public_key.as_str()),
            auth_models: models.then_some(r#"["anthropic/bench*", "claude-*"]"#),
            cache_diagnostics: matches!(gateway, Gateway::WyeDiag(_)),
            // The mock answers the export POSTs like any request.
            otlp_endpoint: matches!(gateway, Gateway::WyeOtlp(_)).then_some(setup.upstream),
            openai: matches!(gateway, Gateway::WyeOpenai(_)),
            bedrock: matches!(gateway, Gateway::WyeBedrock(_)),
            failover: matches!(gateway, Gateway::WyeFailover(_) | Gateway::WyeRefused(_)),
            refusing_upstream: refusing.as_deref(),
        };
        let mut env = Vec::new();
        if authenticated || options.openai || options.bedrock || options.failover {
            // `[auth]` excludes caller-credential passthrough; the mock ignores the key.
            env.push((BENCH_PROVIDER_KEY_ENV, "sk-bench-provider".to_owned()));
        }
        if options.cache_diagnostics {
            env.push((BENCH_HASH_KEY_ENV, "b".repeat(64)));
        }
        let config = wye_config(
            setup.port,
            setup.upstream,
            setup.ca_file.as_deref(),
            &options,
        );
        Self {
            program: program.into(),
            args: vec![
                "serve".into(),
                "--config".into(),
                setup.config(gateway, "toml", config),
            ],
            env,
            health: "/healthz",
            target: match (&credentials, options.openai) {
                (Some(c), _) => Target::gateway(
                    setup.addr,
                    "",
                    vec![("authorization", format!("Bearer {}", c.token))],
                ),
                (None, true) => Target::plain(setup.addr).with_responses(),
                (None, false) => Target::plain(setup.addr),
            },
        }
    }

    fn krakend(program: &str, gateway: Gateway<'_>, setup: &Setup<'_>) -> Self {
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

    fn litellm(program: &str, gateway: Gateway<'_>, setup: &Setup<'_>) -> Self {
        Self {
            program: program.into(),
            args: vec![
                "--config".into(),
                setup.config(gateway, "yaml", litellm_config(setup.upstream)),
                "--port".into(),
                setup.port.to_string(),
                // LiteLLM's production recommendation: one worker per core.
                "--num_workers".into(),
                std::thread::available_parallelism()
                    .map_or(1, |n| n.get())
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

    fn bifrost(program: &str, setup: &Setup<'_>) -> Self {
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
}
