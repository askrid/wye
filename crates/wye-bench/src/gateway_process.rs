//! The gateway under test (Wye or KrakenD), as a child process.

use std::{path::Path, process::Stdio, time::Duration};

use tokio::process::Child;

use crate::client::Target;

/// Which gateway to start.
#[derive(Clone, Copy)]
pub enum Gateway<'a> {
    Wye(&'a str),
    KrakenD(&'a str),
}

impl Gateway<'_> {
    pub fn name(self) -> &'static str {
        match self {
            Self::Wye(_) => "wye",
            Self::KrakenD(_) => "krakend",
        }
    }

    /// Whether the gateway relays SSE events as they arrive. KrakenD CE buffers
    /// the whole response in `no-op` mode (streaming is an Enterprise feature;
    /// CE answers `"sse"` encoding with 500), so scenarios that wait for the
    /// first event of a long silent stream cannot run through it.
    pub fn streams_sse(self) -> bool {
        matches!(self, Self::Wye(_))
    }

    /// Metric name prefix for this gateway.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Wye(_) => "",
            Self::KrakenD(_) => "krakend_",
        }
    }
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
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        // Absolute, because Wye resolves relative paths against its config file
        // and KrakenD against its working directory.
        let ca_file = ca_pem.map(|pem| {
            let path = dir.join(format!("bench-{name}-ca.pem"));
            std::fs::write(&path, pem).unwrap();
            std::fs::canonicalize(path).unwrap()
        });
        let (binary, config_path, health) = match gateway {
            Gateway::Wye(binary) => {
                let mut config = format!(
                    "[server]\nlisten = \"127.0.0.1:{port}\"\nshutdown_grace_secs = 0\n\n\
                     [upstream]\nbase_url = \"{upstream_base_url}\"\nforward_caller_credential = true\n"
                );
                if let Some(path) = &ca_file {
                    config.push_str(&format!("ca_file = {:?}\n", path.display().to_string()));
                }
                // Tuning experiments: extra `[upstream]` lines without code changes.
                if let Ok(extra) = std::env::var("WYE_BENCH_UPSTREAM") {
                    config.push_str(&extra);
                    config.push('\n');
                }
                let path = dir.join(format!("bench-{name}.toml"));
                std::fs::write(&path, config).unwrap();
                (binary, path, "/healthz")
            }
            Gateway::KrakenD(binary) => {
                let path = dir.join(format!("bench-{name}.json"));
                std::fs::write(
                    &path,
                    krakend_config(port, upstream_base_url, ca_file.as_deref()),
                )
                .unwrap();
                (binary, path, "/__health")
            }
        };
        let args: &[&str] = match gateway {
            Gateway::Wye(_) => &["serve", "--config"],
            Gateway::KrakenD(_) => &["run", "-c"],
        };
        let child = tokio::process::Command::new(binary)
            .args(args)
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {binary}: {e}"));
        let pid = child.id().unwrap();
        let target = Target::plain(format!("127.0.0.1:{port}").parse().unwrap());
        for _ in 0..400 {
            if let Ok(res) = target.client().request(target.get(health)).await
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
        panic!("{binary} did not become healthy");
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn rss_kb(&self) -> f64 {
        self.ps("rss").parse().unwrap()
    }

    /// Total CPU time (user + system) in seconds, from `ps -o time` (`[[dd-]hh:]mm:ss.cc`).
    pub fn cpu_secs(&self) -> f64 {
        let raw = self.ps("time");
        let (days, rest) = raw
            .split_once('-')
            .map_or((0.0, raw.as_str()), |(d, r)| (d.parse().unwrap(), r));
        rest.split(':')
            .fold(0.0, |acc, part| acc * 60.0 + part.parse::<f64>().unwrap())
            + days * 86_400.0
    }

    fn ps(&self, field: &str) -> String {
        let out = std::process::Command::new("ps")
            .args(["-o", &format!("{field}="), "-p", &self.pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
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
