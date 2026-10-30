//! `wye claude` and `wye codex`: run an agent against a Wye gateway.
//!
//! Nothing on disk changes, so nothing needs cleaning up: the wrapper checks
//! that the gateway answers, points the agent at it through its environment
//! and `--settings` (Claude Code) or command-line config overrides (Codex),
//! and then replaces itself with the agent. Signals, the terminal, and the
//! exit code are therefore the agent's own.

use std::{ffi::OsString, process::Command};

use gateway_core::config::is_loopback_host;

/// Gateway URL when neither `--url` nor `WYE_URL` is set.
const DEFAULT_GATEWAY: &str = "http://127.0.0.1:8080";
/// The environment variable Codex reads its bearer token from (the Codex
/// provider's `env_key`).
const CODEX_TOKEN_ENV: &str = "WYE_CODEX_TOKEN";
/// Sent by Codex when the gateway takes no credential: Codex needs a token,
/// and a gateway without `[auth]` replaces it with the provider key.
const CODEX_NO_CREDENTIAL: &str = "wye-loopback";
/// Makes Claude Code send its request class, prompt id, and tool times,
/// which it sends a custom base URL only when asked.
const HINT_HEADERS_ENV: &str = "CLAUDE_CODE_GATEWAY_HINT_HEADERS";

/// The agents the wrapper can launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn program(self) -> OsString {
        let default = match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        };
        std::env::var_os(self.program_variable())
            .filter(|program| !program.is_empty())
            .unwrap_or_else(|| default.into())
    }

    /// The variable that names another path to the program.
    fn program_variable(self) -> &'static str {
        match self {
            Self::Claude => "WYE_CLAUDE_BIN",
            Self::Codex => "WYE_CODEX_BIN",
        }
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
    /// Claude Code's `CLAUDE_CODE_GATEWAY_HINT_HEADERS`: the caller's, or
    /// `1`, since the gateway is the caller's choice and the usage log
    /// needs the hints.
    pub hint_headers: String,
    /// Arguments for the agent itself.
    pub args: Vec<String>,
}

impl Launch {
    /// The launch for `url` and `token_file` (`--url`, `--token-file`),
    /// falling back to `WYE_URL` and `WYE_TOKEN`; `args` go to the agent.
    pub fn resolve(
        url: Option<&str>,
        token_file: Option<&str>,
        args: &[&str],
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let credential = match token_file {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read {path}: {e}"))?
                    .trim()
                    .to_owned(),
            ),
            None => env("WYE_TOKEN"),
        };
        let gateway = gateway_url(url, &env);
        // Plain http beyond this machine would carry the gateway token, or
        // the agent's own login, in the clear.
        if let Ok(uri) = gateway.parse::<http::Uri>()
            && uri.scheme_str() == Some("http")
            && uri.host().is_some_and(|host| !is_loopback_host(host))
        {
            return Err(format!(
                "{gateway} is a remote gateway over plain http; use https"
            ));
        }
        if credential.as_deref() == Some("") {
            return Err("the gateway token is empty".into());
        }
        Ok(Self {
            gateway,
            credential,
            hint_headers: env(HINT_HEADERS_ENV).unwrap_or_else(|| "1".to_owned()),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        })
    }

    /// The agent's command, pointed at the gateway.
    pub fn command(&self, agent: Agent, program: impl Into<OsString>) -> Command {
        let mut command = Command::new(program.into());
        // The agent gets the token its own way below; every tool it runs
        // would inherit this one.
        command.env_remove("WYE_TOKEN");
        match agent {
            Agent::Claude => {
                // Background sessions (`--bg`) are started by Claude Code's
                // daemon, which does not inherit this environment but reads
                // `--settings`. The credential stays out of it: other local
                // users can read a command line.
                let env = serde_json::json!({
                    "ANTHROPIC_BASE_URL": self.gateway,
                    HINT_HEADERS_ENV: self.hint_headers,
                });
                command
                    .arg("--settings")
                    .arg(serde_json::json!({ "env": env }).to_string());
                command.env("ANTHROPIC_BASE_URL", &self.gateway);
                command.env(HINT_HEADERS_ENV, &self.hint_headers);
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

/// The gateway to use: `url` (`--url`), else `$WYE_URL`, else this machine's.
pub fn gateway_url(url: Option<&str>, env: &impl Fn(&str) -> Option<String>) -> String {
    url.map(str::to_owned)
        .or_else(|| env("WYE_URL").filter(|v| !v.is_empty()))
        .unwrap_or_else(|| DEFAULT_GATEWAY.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

/// Runs the agent through the gateway, as the module docs describe. On
/// Unix the process becomes the agent and this returns only if it cannot
/// start; elsewhere it returns the agent's exit code.
pub fn run(agent: Agent, launch: &Launch) -> Result<i32, String> {
    let health = super::health_url(&launch.gateway);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(super::probe(&health))
        .map_err(|e| {
            format!(
                "no gateway at {} ({e}); start one with `wye service install` or \
                 `wye serve`, or name another with --url",
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
        Err(cannot_run(agent, &name, &command.exec()))
    }
    #[cfg(not(unix))]
    {
        let status = command.status().map_err(|e| cannot_run(agent, &name, &e))?;
        Ok(status.code().unwrap_or(1))
    }
}

fn cannot_run(agent: Agent, name: &str, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "cannot run {name}: not found; install it, or set {} to its path",
            agent.program_variable()
        )
    } else {
        format!("cannot run {name}: {error}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn flags_override_environment_and_the_https_rule_holds() {
        let env = |name: &str| match name {
            "WYE_URL" => Some("http://localhost:9000/".to_owned()),
            "WYE_TOKEN" => Some("from-env".to_owned()),
            _ => None,
        };
        let launch = Launch::resolve(None, None, &["-p", "fix tests"], env).unwrap();
        assert_eq!(launch.gateway, "http://localhost:9000");
        assert_eq!(launch.credential.as_deref(), Some("from-env"));
        assert_eq!(launch.args, ["-p", "fix tests"]);

        let launch = Launch::resolve(Some("https://other"), None, &[], env).unwrap();
        assert_eq!(launch.gateway, "https://other");
        let remote = Launch::resolve(Some("http://gw.example.com:8080"), None, &[], env);
        assert!(remote.unwrap_err().contains("plain http"));
        assert!(Launch::resolve(Some("http://[::1]:8080"), None, &[], env).is_ok());
        let empty = |name: &str| (name == "WYE_TOKEN").then(String::new);
        assert!(
            Launch::resolve(None, None, &[], empty)
                .unwrap_err()
                .contains("empty")
        );

        let defaults = Launch::resolve(None, None, &[], |_| None).unwrap();
        assert_eq!(defaults.gateway, DEFAULT_GATEWAY);
        assert_eq!(defaults.credential, None);
        assert_eq!(defaults.hint_headers, "1");
        let caller = |name: &str| (name == HINT_HEADERS_ENV).then(|| "0".to_owned());
        assert_eq!(
            Launch::resolve(None, None, &[], caller)
                .unwrap()
                .hint_headers,
            "0"
        );
    }

    #[test]
    fn claude_gets_the_gateway_environment() {
        let launch = Launch {
            gateway: "http://gw".into(),
            credential: Some("tok".into()),
            hint_headers: "1".into(),
            args: vec!["-p".into()],
        };
        let command = launch.command(Agent::Claude, "claude");
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&("ANTHROPIC_BASE_URL".as_ref(), Some("http://gw".as_ref()))));
        assert!(envs.contains(&("ANTHROPIC_AUTH_TOKEN".as_ref(), Some("tok".as_ref()))));
        assert!(envs.contains(&("ANTHROPIC_API_KEY".as_ref(), None)));
        assert!(envs.contains(&("WYE_TOKEN".as_ref(), None)));
        assert!(envs.contains(&(HINT_HEADERS_ENV.as_ref(), Some("1".as_ref()))));
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
            hint_headers: "1".into(),
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
