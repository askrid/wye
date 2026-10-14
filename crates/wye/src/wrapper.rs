//! `wye claude`: runs Claude Code against a Wye gateway.
//!
//! Only environment variables change, so nothing needs cleaning up: the
//! wrapper checks that the gateway answers, sets `ANTHROPIC_BASE_URL` (and
//! `ANTHROPIC_AUTH_TOKEN` when a gateway credential is given), and then
//! replaces itself with `claude`. Signals, the terminal, and the exit code
//! are therefore Claude Code's own.

use std::{ffi::OsString, process::Command};

/// Gateway URL when neither `--gateway` nor `WYE_GATEWAY_URL` is set.
const DEFAULT_GATEWAY: &str = "http://127.0.0.1:8080";

/// Where the wrapper sends Claude Code, and with which credential.
#[derive(Debug, PartialEq, Eq)]
pub struct ClaudeLaunch {
    pub gateway: String,
    /// A gateway credential, sent as `Authorization: Bearer`; `None` keeps
    /// Claude Code's own login (subscription passthrough).
    pub credential: Option<String>,
    /// Arguments for `claude` itself.
    pub args: Vec<String>,
}

impl ClaudeLaunch {
    /// Parses `[--gateway URL] [--credential-file PATH] [--] [claude args...]`,
    /// falling back to `WYE_GATEWAY_URL` and `WYE_CREDENTIAL`.
    pub fn from_args(args: &[&str], env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut gateway = env("WYE_GATEWAY_URL");
        let mut credential = env("WYE_CREDENTIAL");
        let mut rest = args;
        loop {
            match rest {
                ["--gateway", url, tail @ ..] => {
                    gateway = Some((*url).to_owned());
                    rest = tail;
                }
                ["--credential-file", path, tail @ ..] => {
                    let token = std::fs::read_to_string(path)
                        .map_err(|e| format!("cannot read {path}: {e}"))?;
                    credential = Some(token.trim().to_owned());
                    rest = tail;
                }
                ["--", tail @ ..] => {
                    rest = tail;
                    break;
                }
                _ => break,
            }
        }
        let gateway = gateway
            .unwrap_or_else(|| DEFAULT_GATEWAY.to_owned())
            .trim_end_matches('/')
            .to_owned();
        if credential.as_deref() == Some("") {
            return Err("the gateway credential is empty".into());
        }
        Ok(Self {
            gateway,
            credential,
            args: rest.iter().map(|a| (*a).to_owned()).collect(),
        })
    }

    /// The `claude` command with the gateway environment.
    pub fn command(&self, program: impl Into<OsString>) -> Command {
        let mut command = Command::new(program.into());
        command
            .args(&self.args)
            .env("ANTHROPIC_BASE_URL", &self.gateway);
        if let Some(credential) = &self.credential {
            command.env("ANTHROPIC_AUTH_TOKEN", credential);
            // An API key would be sent too and take precedence in Claude Code.
            command.env_remove("ANTHROPIC_API_KEY");
        }
        command
    }
}

/// Runs `claude` through the gateway. On Unix the process becomes `claude`
/// and this returns only if it cannot start; elsewhere it returns
/// Claude Code's exit code.
pub fn claude(args: &[&str]) -> Result<i32, String> {
    let launch = ClaudeLaunch::from_args(args, |name| std::env::var(name).ok())?;
    let health = format!("{}/healthz", launch.gateway);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(crate::probe(&health))
        .map_err(|e| format!("gateway not reachable ({e}); start it with `wye serve`"))?;
    let program = std::env::var_os("WYE_CLAUDE_BIN").unwrap_or_else(|| "claude".into());
    let mut command = launch.command(program);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only returns on failure.
        let error = command.exec();
        Err(format!("cannot run claude: {error}"))
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .map_err(|e| format!("cannot run claude: {e}"))?;
        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn flags_override_environment_and_the_rest_goes_to_claude() {
        let env = |name: &str| match name {
            "WYE_GATEWAY_URL" => Some("http://gw:9000/".to_owned()),
            "WYE_CREDENTIAL" => Some("from-env".to_owned()),
            _ => None,
        };
        let launch = ClaudeLaunch::from_args(&["-p", "fix tests"], env).unwrap();
        assert_eq!(launch.gateway, "http://gw:9000");
        assert_eq!(launch.credential.as_deref(), Some("from-env"));
        assert_eq!(launch.args, ["-p", "fix tests"]);

        let launch =
            ClaudeLaunch::from_args(&["--gateway", "http://other", "--", "--gateway"], env)
                .unwrap();
        assert_eq!(launch.gateway, "http://other");
        assert_eq!(launch.args, ["--gateway"]);

        let defaults = ClaudeLaunch::from_args(&[], |_| None).unwrap();
        assert_eq!(defaults.gateway, DEFAULT_GATEWAY);
        assert_eq!(defaults.credential, None);
    }

    #[test]
    fn command_sets_the_gateway_environment() {
        let launch = ClaudeLaunch {
            gateway: "http://gw".into(),
            credential: Some("tok".into()),
            args: vec!["-p".into()],
        };
        let command = launch.command("claude");
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&("ANTHROPIC_BASE_URL".as_ref(), Some("http://gw".as_ref()))));
        assert!(envs.contains(&("ANTHROPIC_AUTH_TOKEN".as_ref(), Some("tok".as_ref()))));
        assert!(envs.contains(&("ANTHROPIC_API_KEY".as_ref(), None)));
    }
}
