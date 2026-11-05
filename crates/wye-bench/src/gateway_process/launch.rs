//! How each gateway under test is started: its program, arguments,
//! environment, health path, the target clients load, and the CPUs it may
//! run on.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use super::{Gateway, Variant};
use crate::{
    client::Target,
    gateway_config::{
        BENCH_HASH_KEY_ENV, BENCH_PROVIDER_KEY_ENV, BIFROST_API_PREFIX, BIFROST_USER_AGENT,
        BenchCredentials, LITELLM_API_PREFIX, LITELLM_MASTER_KEY, WyeOptions, agentgateway_config,
        bifrost_config, krakend_config, litellm_config, wye_config,
    },
};

/// [`Variant::Refused`]'s first upstream rejects one request in this many.
const REFUSE_EVERY: u64 = 4;

/// A Linux cpu list (`0-3,8-11`): the gateway runs on those CPUs only, under
/// `taskset`, so it shares no core with the load generator and the mock
/// (pin `wye-bench` itself to the others). Unset or empty, nothing is pinned.
const GATEWAY_CPUS_ENV: &str = "WYE_BENCH_GATEWAY_CPUS";

/// The CPUs [`GATEWAY_CPUS_ENV`] names.
struct GatewayCpus {
    list: String,
    count: usize,
}

impl GatewayCpus {
    fn from_env() -> Option<Self> {
        let list = std::env::var(GATEWAY_CPUS_ENV)
            .ok()
            .filter(|l| !l.is_empty())?;
        let count = cpu_count(&list).unwrap_or_else(|| {
            panic!("{GATEWAY_CPUS_ENV}={list}: not a cpu list such as 0-3,8-11")
        });
        Some(Self { list, count })
    }
}

/// How many CPUs a cpu list (`0-3,8-11`, as `taskset -c` takes it) names;
/// `None` when it is not one.
fn cpu_count(list: &str) -> Option<usize> {
    let mut cpus = BTreeSet::new();
    for part in list.split(',') {
        let (first, last) = match part.split_once('-') {
            Some((first, last)) => (first.parse::<u32>().ok()?, last.parse().ok()?),
            None => {
                let cpu = part.parse().ok()?;
                (cpu, cpu)
            }
        };
        if first > last {
            return None;
        }
        cpus.extend(first..=last);
    }
    Some(cpus.len())
}

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
        let cpus = GatewayCpus::from_env();
        let launch = match gateway {
            Gateway::Wye(program, variant) => Self::wye(program, variant, setup).await,
            Gateway::KrakenD(program) => Self::krakend(program, gateway, setup),
            Gateway::LiteLlm(program) => {
                let cpus = cpus.as_ref().map(|c| c.count);
                Self::litellm(program, gateway, setup, cpus)
            }
            Gateway::Bifrost(program) => Self::bifrost(program, setup),
            Gateway::Agentgateway(program) => Self::agentgateway(program, gateway, setup),
        };
        launch.pinned(cpus.as_ref().map(|c| c.list.as_str()))
    }

    /// Runs the gateway under `taskset -c <cpus>`, which sets the CPU
    /// affinity and then executes the gateway: the pid stays the gateway's,
    /// and processes it starts inherit the affinity.
    fn pinned(mut self, cpus: Option<&str>) -> Self {
        if let Some(cpus) = cpus {
            let program = std::mem::replace(&mut self.program, "taskset".into());
            self.args
                .splice(0..0, ["-c".to_owned(), cpus.to_owned(), program]);
        }
        self
    }

    async fn wye(program: &str, variant: Variant, setup: &Setup<'_>) -> Self {
        // The refusing first account, spawned for this process only.
        let refusing = match variant {
            Variant::Refused => Some(format!(
                "http://{}",
                crate::mock::spawn_refusing(REFUSE_EVERY).await
            )),
            _ => None,
        };
        let authenticated = matches!(variant, Variant::Auth | Variant::Models);
        let models = variant == Variant::Models;
        // Requests name model "anthropic/bench" (Bifrost relays bytes unchanged
        // only for a provider-qualified model): allowed by both lists.
        let credentials = authenticated
            .then(|| BenchCredentials::generate(models.then_some(&["anthropic/bench"][..])));
        let options = WyeOptions {
            auth_public_key: credentials.as_ref().map(|c| c.public_key.as_str()),
            auth_models: models.then_some(r#"["anthropic/bench*", "claude-*"]"#),
            cache_diagnostics: variant == Variant::Diag,
            // The mock answers the export POSTs like any request.
            otlp_endpoint: (variant == Variant::Otlp).then_some(setup.upstream),
            openai: variant == Variant::Openai,
            bedrock: variant == Variant::Bedrock,
            failover: matches!(variant, Variant::Failover | Variant::Refused),
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
                setup.config(Gateway::Wye(program, variant), "toml", config),
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

    /// `cpus`: how many CPUs the gateway is pinned to, if it is.
    fn litellm(
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

    fn agentgateway(program: &str, gateway: Gateway<'_>, setup: &Setup<'_>) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cpu_list_counts_each_cpu_once() {
        assert_eq!(cpu_count("0-3,8-11"), Some(8));
        assert_eq!(cpu_count("6,7,14,15"), Some(4));
        assert_eq!(cpu_count("2"), Some(1));
        assert_eq!(cpu_count("0-3,2-5"), Some(6));
        for bad in ["", "3-1", "a", "0-", "0,,1", "0-10:2"] {
            assert_eq!(cpu_count(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_pinned_gateway_runs_under_taskset() {
        let launch = || Launch {
            program: "/bin/wye".into(),
            args: vec!["serve".into(), "--config".into(), "wye.toml".into()],
            env: Vec::new(),
            health: "/healthz",
            target: Target::plain("127.0.0.1:1".parse().unwrap()),
        };
        let pinned = launch().pinned(Some("6,7,14,15"));
        assert_eq!(pinned.program, "taskset");
        assert_eq!(
            pinned.args,
            [
                "-c",
                "6,7,14,15",
                "/bin/wye",
                "serve",
                "--config",
                "wye.toml"
            ]
        );
        let unpinned = launch().pinned(None);
        assert_eq!(unpinned.program, "/bin/wye");
        assert_eq!(unpinned.args.len(), 3);
    }
}
