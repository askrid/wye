//! The `wye` binary under test, as a child process.

use std::{path::Path, process::Stdio, time::Duration};

use tokio::process::Child;

use crate::client::Target;

pub struct WyeProcess {
    /// Held so the process lives as long as this value; `kill_on_drop` stops it.
    _child: Child,
    pid: u32,
    target: Target,
}

impl WyeProcess {
    /// Starts Wye in front of `upstream_base_url`, trusting `ca_pem` when given.
    pub async fn start(
        binary: &str,
        upstream_base_url: &str,
        ca_pem: Option<&str>,
        dir: &Path,
        name: &str,
    ) -> Self {
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let mut config = format!(
            "[server]\nlisten = \"127.0.0.1:{port}\"\nshutdown_grace_secs = 0\n\n\
             [upstream]\nbase_url = \"{upstream_base_url}\"\nforward_caller_credential = true\n"
        );
        if let Some(pem) = ca_pem {
            let ca_file = format!("bench-{name}-ca.pem");
            std::fs::write(dir.join(&ca_file), pem).unwrap();
            config.push_str(&format!("ca_file = \"{ca_file}\"\n"));
        }
        let config_path = dir.join(format!("bench-{name}.toml"));
        std::fs::write(&config_path, config).unwrap();

        let child = tokio::process::Command::new(binary)
            .args(["serve", "--config"])
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {binary}: {e}"));
        let pid = child.id().unwrap();
        let target = Target::plain(format!("127.0.0.1:{port}").parse().unwrap());
        for _ in 0..200 {
            if target
                .client()
                .request(target.get("/healthz"))
                .await
                .is_ok()
            {
                return Self {
                    _child: child,
                    pid,
                    target,
                };
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("wye did not become healthy");
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
