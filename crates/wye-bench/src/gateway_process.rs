//! The gateway under test (Wye, KrakenD, or LiteLLM), as a child process.

use std::{collections::HashSet, path::Path, process::Stdio, time::Duration};

use tokio::process::Child;

use crate::{
    client::Target,
    gateway_config::{
        BENCH_HASH_KEY_ENV, BENCH_PROVIDER_KEY_ENV, BenchCredentials, LITELLM_API_PREFIX,
        LITELLM_MASTER_KEY, WyeOptions, krakend_config, litellm_config, wye_config,
    },
};

/// Which gateway to start, with the path to its executable.
#[derive(Clone, Copy)]
pub enum Gateway<'a> {
    Wye(&'a str),
    /// Wye with `[auth]`: every request carries a gateway credential to verify.
    WyeAuth(&'a str),
    /// Wye with `[cache_diagnostics]`: every request body is scanned and hashed.
    WyeDiag(&'a str),
    /// Wye with `[telemetry]`: every usage record is also exported as OTLP.
    WyeOtlp(&'a str),
    /// Wye with `[auth]` and model permissions in config and credential:
    /// every request is held until its `model` is read, and its body checked.
    WyeModels(&'a str),
    KrakenD(&'a str),
    LiteLlm(&'a str),
}

impl Gateway<'_> {
    pub fn name(self) -> &'static str {
        match self {
            Self::Wye(_) => "wye",
            Self::WyeAuth(_) => "wye-auth",
            Self::WyeDiag(_) => "wye-diag",
            Self::WyeOtlp(_) => "wye-otlp",
            Self::WyeModels(_) => "wye-models",
            Self::KrakenD(_) => "krakend",
            Self::LiteLlm(_) => "litellm",
        }
    }

    /// Metric name prefix for this gateway.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Wye(_) => "",
            Self::WyeAuth(_) => "auth_",
            Self::WyeDiag(_) => "diag_",
            Self::WyeOtlp(_) => "otlp_",
            Self::WyeModels(_) => "models_",
            Self::KrakenD(_) => "krakend_",
            Self::LiteLlm(_) => "litellm_",
        }
    }

    /// Whether the gateway relays SSE events as they arrive. KrakenD CE buffers
    /// the whole response in `no-op` mode (streaming is an Enterprise feature;
    /// CE answers `"sse"` encoding with 500), so scenarios that wait for the
    /// first event of a long silent stream cannot run through it.
    pub fn streams_sse(self) -> bool {
        !matches!(self, Self::KrakenD(_))
    }
}

/// How to launch one gateway in front of an upstream.
struct Launch {
    program: String,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
    health: &'static str,
    target: Target,
}

pub struct GatewayProcess {
    /// Held so the process lives as long as this value; `kill_on_drop` stops it.
    _child: Child,
    pid: u32,
    target: Target,
}

impl GatewayProcess {
    /// Starts `gateway` as a pass-through proxy for `POST /v1/messages` in front of
    /// `upstream_base_url`, trusting `ca_pem` for the upstream when given.
    pub async fn start(
        gateway: Gateway<'_>,
        upstream_base_url: &str,
        ca_pem: Option<&str>,
        dir: &Path,
        name: &str,
    ) -> Self {
        Self::start_with_env(gateway, upstream_base_url, ca_pem, dir, name, &[]).await
    }

    /// [`Self::start`] with extra environment variables for the gateway
    /// process only (for example `TOKIO_WORKER_THREADS`).
    pub async fn start_with_env(
        gateway: Gateway<'_>,
        upstream_base_url: &str,
        ca_pem: Option<&str>,
        dir: &Path,
        name: &str,
        extra_env: &[(&'static str, &str)],
    ) -> Self {
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        // Absolute, because Wye resolves relative paths against its config file
        // and the others against their working directory.
        let ca_file = ca_pem.map(|pem| {
            let path = dir.join(format!("bench-{name}-ca.pem"));
            std::fs::write(&path, pem).unwrap();
            std::fs::canonicalize(path).unwrap()
        });
        let config = |extension: &str, contents: String| {
            let path = dir.join(format!("bench-{name}-{}.{extension}", gateway.name()));
            std::fs::write(&path, contents).unwrap();
            path.display().to_string()
        };
        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let launch = match gateway {
            Gateway::Wye(program)
            | Gateway::WyeAuth(program)
            | Gateway::WyeModels(program)
            | Gateway::WyeDiag(program)
            | Gateway::WyeOtlp(program) => {
                let authenticated = matches!(gateway, Gateway::WyeAuth(_) | Gateway::WyeModels(_));
                let models = matches!(gateway, Gateway::WyeModels(_));
                // The mock's requests name model "bench": allowed by both lists.
                let credentials = authenticated
                    .then(|| BenchCredentials::generate(models.then_some(&["bench"][..])));
                let options = WyeOptions {
                    auth_public_key: credentials.as_ref().map(|c| c.public_key.as_str()),
                    auth_models: models.then_some(r#"["bench*", "claude-*"]"#),
                    cache_diagnostics: matches!(gateway, Gateway::WyeDiag(_)),
                    // The mock answers the export POSTs like any request.
                    otlp_endpoint: matches!(gateway, Gateway::WyeOtlp(_))
                        .then_some(upstream_base_url),
                };
                let mut env = Vec::new();
                if authenticated {
                    // `[auth]` excludes caller-credential passthrough; the mock ignores the key.
                    env.push((BENCH_PROVIDER_KEY_ENV, "sk-bench-provider".to_owned()));
                }
                if options.cache_diagnostics {
                    env.push((BENCH_HASH_KEY_ENV, "b".repeat(64)));
                }
                Launch {
                    program: program.into(),
                    args: vec![
                        "serve".into(),
                        "--config".into(),
                        config(
                            "toml",
                            wye_config(port, upstream_base_url, ca_file.as_deref(), &options),
                        ),
                    ],
                    env,
                    health: "/healthz",
                    target: match &credentials {
                        Some(c) => Target::gateway(
                            addr,
                            "",
                            vec![("authorization", format!("Bearer {}", c.token))],
                        ),
                        None => Target::plain(addr),
                    },
                }
            }
            Gateway::KrakenD(program) => Launch {
                program: program.into(),
                args: vec![
                    "run".into(),
                    "-c".into(),
                    config(
                        "json",
                        krakend_config(port, upstream_base_url, ca_file.as_deref()),
                    ),
                ],
                env: Vec::new(),
                health: "/__health",
                target: Target::plain(addr),
            },
            Gateway::LiteLlm(program) => Launch {
                program: program.into(),
                args: vec![
                    "--config".into(),
                    config("yaml", litellm_config(upstream_base_url)),
                    "--port".into(),
                    port.to_string(),
                    // LiteLLM's production recommendation: one worker per core.
                    "--num_workers".into(),
                    std::thread::available_parallelism()
                        .map_or(1, |n| n.get())
                        .to_string(),
                ],
                env: ca_file
                    .iter()
                    // Pass-through requests trust the CA path in SSL_VERIFY. Not
                    // SSL_CERT_FILE: it replaces the system roots for every client,
                    // breaking LiteLLM's other outbound HTTPS calls.
                    .map(|path| ("SSL_VERIFY", path.display().to_string()))
                    .chain([("LITELLM_LOG", "ERROR".into())])
                    .collect(),
                health: "/health/liveliness",
                target: Target::gateway(
                    addr,
                    LITELLM_API_PREFIX,
                    vec![("authorization", format!("Bearer {LITELLM_MASTER_KEY}"))],
                ),
            },
        };

        // `WYE_BENCH_GATEWAY_LOG=1` keeps each gateway's output next to its config.
        let (stdout, stderr) = match std::env::var_os("WYE_BENCH_GATEWAY_LOG") {
            Some(_) => {
                let log = dir.join(format!("bench-{name}-{}.log", gateway.name()));
                let file = std::fs::File::create(log).unwrap();
                (Stdio::from(file.try_clone().unwrap()), Stdio::from(file))
            }
            None => (Stdio::null(), Stdio::null()),
        };
        let child = tokio::process::Command::new(&launch.program)
            // Own process group, so `Drop` can stop workers the gateway forks.
            .process_group(0)
            .args(&launch.args)
            .envs(launch.env)
            .envs(extra_env.iter().copied())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", launch.program));
        let pid = child.id().unwrap();
        let target = launch.target;
        // LiteLLM needs several seconds to import and start its workers.
        for _ in 0..1_200 {
            if let Ok(res) = target.client().request(target.get(launch.health)).await
                && res.status().is_success()
            {
                return Self {
                    _child: child,
                    pid,
                    target,
                };
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{} did not become healthy", launch.program);
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn target(&self) -> &Target {
        &self.target
    }
}

impl Drop for GatewayProcess {
    /// `kill_on_drop` stops only the direct child; LiteLLM's workers would
    /// outlive it and disturb later runs. Kill the whole process group.
    fn drop(&mut self) {
        let killed = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.pid)])
            .status();
        if !killed.as_ref().is_ok_and(|status| status.success()) {
            eprintln!(
                "warning: could not stop process group {}: {killed:?}",
                self.pid
            );
        }
    }
}

impl GatewayProcess {
    /// Resident memory of the gateway and every process it started, in KB.
    pub fn rss_kb(&self) -> f64 {
        self.tree_sum("rss", |raw| raw.parse().unwrap())
    }

    /// CPU time (user + system) of the gateway and every process it started, in seconds.
    pub fn cpu_secs(&self) -> f64 {
        self.tree_sum("time", parse_cpu_time)
    }

    fn tree_sum(&self, field: &str, parse: impl Fn(&str) -> f64) -> f64 {
        let pids: Vec<String> = process_tree(self.pid).iter().map(u32::to_string).collect();
        let out = std::process::Command::new("ps")
            .args(["-o", &format!("{field}="), "-p", &pids.join(",")])
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(parse)
            .sum()
    }
}

/// `root` and all of its descendants. Wye and KrakenD are one process; LiteLLM forks workers.
fn process_tree(root: u32) -> Vec<u32> {
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
        .unwrap();
    let pairs: Vec<(u32, u32)> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect();
    let mut tree = vec![root];
    let mut seen = HashSet::from([root]);
    let mut i = 0;
    while let Some(&parent) = tree.get(i) {
        for &(pid, ppid) in &pairs {
            if ppid == parent && seen.insert(pid) {
                tree.push(pid);
            }
        }
        i += 1;
    }
    tree
}

/// `ps -o time` format: `[[dd-]hh:]mm:ss.cc`.
fn parse_cpu_time(raw: &str) -> f64 {
    let (days, rest) = raw
        .split_once('-')
        .map_or((0.0, raw), |(d, r)| (d.parse().unwrap(), r));
    rest.split(':')
        .fold(0.0, |acc, part| acc * 60.0 + part.parse::<f64>().unwrap())
        + days * 86_400.0
}
