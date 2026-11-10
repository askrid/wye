//! How a gateway under test is launched: the program, arguments,
//! environment, health path, and load target that [`super::wye`] and
//! [`super::competitors`] fill in for each gateway, and the CPUs it may run
//! on.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use super::Gateway;
use crate::client::Target;

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
    pub(super) fn config(&self, gateway: Gateway<'_>, extension: &str, contents: String) -> String {
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
