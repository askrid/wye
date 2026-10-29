//! `wye claude` and `wye codex`: run an agent against a Wye gateway.
//!
//! Nothing on disk changes, so nothing needs cleaning up: the wrapper checks
//! that the gateway answers, points the agent at it through its environment
//! and `--settings` (Claude Code) or command-line config overrides (Codex),
//! and then replaces itself with the agent. Signals, the terminal, and the
//! exit code are therefore the agent's own.

use std::{ffi::OsString, process::Command};

/// Gateway URL when neither `--gateway` nor `WYE_GATEWAY_URL` is set.
const DEFAULT_GATEWAY: &str = "http://127.0.0.1:8080";
/// The environment variable Codex reads its bearer token from (the Codex
/// provider's `env_key`).
const CODEX_TOKEN_ENV: &str = "WYE_CODEX_TOKEN";
/// Sent by Codex when the gateway takes no credential: Codex needs a token,
/// and a gateway without `[auth]` replaces it with the provider key.
const CODEX_NO_CREDENTIAL: &str = "wye-loopback";
/// Makes Claude Code send its request class, prompt id, and tool times.
const HINT_HEADERS_ENV: &str = "CLAUDE_CODE_GATEWAY_HINT_HEADERS";

/// The agents the wrapper can launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn program(self) -> OsString {
        let (variable, default) = match self {
            Self::Claude => ("WYE_CLAUDE_BIN", "claude"),
            Self::Codex => ("WYE_CODEX_BIN", "codex"),
        };
        std::env::var_os(variable).unwrap_or_else(|| default.into())
    }
}

/// Where the wrapper sends the agent, and with which credential.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    pub gateway: String,
    /// A gateway credential, sent as `Authorization: Bearer`; `None` keeps
    /// Claude Code's own login (subscription passthrough), or lets Codex
    /// reach a gateway without `[auth]`.
    pub credential: Option<String>,
    /// The caller's `CLAUDE_CODE_GATEWAY_HINT_HEADERS`, passed on to
    /// background sessions with the gateway URL.
    pub hint_headers: Option<String>,
    /// Arguments for the agent itself.
    pub args: Vec<String>,
}

impl Launch {
    /// Parses `[--gateway URL] [--credential-file PATH] [--] [agent args...]`,
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
            hint_headers: env(HINT_HEADERS_ENV),
            args: rest.iter().map(|a| (*a).to_owned()).collect(),
        })
    }

    /// The agent's command, pointed at the gateway.
    pub fn command(&self, agent: Agent, program: impl Into<OsString>) -> Command {
        let mut command = Command::new(program.into());
        match agent {
            Agent::Claude => {
                // Background sessions (`--bg`) are started by Claude Code's
                // daemon, which does not inherit this environment but reads
                // `--settings`. The credential stays out of it: other local
                // users can read a command line.
                let mut env = serde_json::Map::new();
                env.insert("ANTHROPIC_BASE_URL".into(), self.gateway.clone().into());
                if let Some(value) = &self.hint_headers {
                    env.insert(HINT_HEADERS_ENV.into(), value.clone().into());
                }
                command
                    .arg("--settings")
                    .arg(serde_json::json!({ "env": env }).to_string());
                command.env("ANTHROPIC_BASE_URL", &self.gateway);
                if let Some(credential) = &self.credential {
                    command.env("ANTHROPIC_AUTH_TOKEN", credential);
                    // An API key would be sent too and take precedence in Claude Code.
                    command.env_remove("ANTHROPIC_API_KEY");
                }
            }
            Agent::Codex => {
                // A custom provider: Codex speaks Responses over HTTP and SSE
                // to it (no WebSocket transport, no ChatGPT-only headers).
                for (key, value) in [
                    ("model_provider", "wye"),
                    ("model_providers.wye.name", "Wye"),
                    (
                        "model_providers.wye.base_url",
                        &format!("{}/v1", self.gateway),
                    ),
                    ("model_providers.wye.env_key", CODEX_TOKEN_ENV),
                    ("model_providers.wye.wire_api", "responses"),
                ] {
                    command
                        .arg("-c")
                        .arg(format!("{key}={}", toml_string(value)));
                }
                command.env(
                    CODEX_TOKEN_ENV,
                    self.credential.as_deref().unwrap_or(CODEX_NO_CREDENTIAL),
                );
            }
        }
        command.args(&self.args);
        command
    }
}

/// `value` as a TOML basic string, as Codex parses `-c` values.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Runs `claude` through the gateway, as the module docs describe.
pub fn claude(args: &[&str]) -> Result<i32, String> {
    run(Agent::Claude, args)
}

/// Runs `codex` through the gateway, as the module docs describe.
pub fn codex(args: &[&str]) -> Result<i32, String> {
    run(Agent::Codex, args)
}

/// Runs the agent through the gateway. On Unix the process becomes the
/// agent and this returns only if it cannot start; elsewhere it returns the
/// agent's exit code.
fn run(agent: Agent, args: &[&str]) -> Result<i32, String> {
    let launch = Launch::from_args(args, |name| std::env::var(name).ok())?;
    let health = format!("{}/healthz", launch.gateway);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(super::probe(&health))
        .map_err(|e| {
            format!(
                "no gateway at {} ({e}); start one with `wye serve --config <path>`, \
                 or name another with --gateway",
                launch.gateway
            )
        })?;
    let program = agent.program();
    let name = program.to_string_lossy().into_owned();
    let mut command = launch.command(agent, program);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only returns on failure.
        let error = command.exec();
        Err(format!("cannot run {name}: {error}"))
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .map_err(|e| format!("cannot run {name}: {e}"))?;
        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn flags_override_environment_and_the_rest_goes_to_the_agent() {
        let env = |name: &str| match name {
            "WYE_GATEWAY_URL" => Some("http://gw:9000/".to_owned()),
            "WYE_CREDENTIAL" => Some("from-env".to_owned()),
            _ => None,
        };
        let launch = Launch::from_args(&["-p", "fix tests"], env).unwrap();
        assert_eq!(launch.gateway, "http://gw:9000");
        assert_eq!(launch.credential.as_deref(), Some("from-env"));
        assert_eq!(launch.args, ["-p", "fix tests"]);

        let launch =
            Launch::from_args(&["--gateway", "http://other", "--", "--gateway"], env).unwrap();
        assert_eq!(launch.gateway, "http://other");
        assert_eq!(launch.args, ["--gateway"]);

        let defaults = Launch::from_args(&[], |_| None).unwrap();
        assert_eq!(defaults.gateway, DEFAULT_GATEWAY);
        assert_eq!(defaults.credential, None);
    }

    #[test]
    fn claude_gets_the_gateway_environment() {
        let launch = Launch {
            gateway: "http://gw".into(),
            credential: Some("tok".into()),
            hint_headers: Some("1".into()),
            args: vec!["-p".into()],
        };
        let command = launch.command(Agent::Claude, "claude");
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&("ANTHROPIC_BASE_URL".as_ref(), Some("http://gw".as_ref()))));
        assert!(envs.contains(&("ANTHROPIC_AUTH_TOKEN".as_ref(), Some("tok".as_ref()))));
        assert!(envs.contains(&("ANTHROPIC_API_KEY".as_ref(), None)));
        // Background sessions get the URL and hint headers, never the credential.
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "--settings",
                r#"{"env":{"ANTHROPIC_BASE_URL":"http://gw","CLAUDE_CODE_GATEWAY_HINT_HEADERS":"1"}}"#,
                "-p"
            ]
        );
    }

    #[test]
    fn codex_gets_a_wye_provider_and_its_token() {
        let launch = Launch {
            gateway: "http://gw:9000".into(),
            credential: None,
            hint_headers: None,
            args: vec!["exec".into(), "fix tests".into()],
        };
        let command = launch.command(Agent::Codex, "codex");
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-c",
                r#"model_provider="wye""#,
                "-c",
                r#"model_providers.wye.name="Wye""#,
                "-c",
                r#"model_providers.wye.base_url="http://gw:9000/v1""#,
                "-c",
                r#"model_providers.wye.env_key="WYE_CODEX_TOKEN""#,
                "-c",
                r#"model_providers.wye.wire_api="responses""#,
                "exec",
                "fix tests",
            ]
        );
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&("WYE_CODEX_TOKEN".as_ref(), Some("wye-loopback".as_ref()))));
        assert_eq!(toml_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
