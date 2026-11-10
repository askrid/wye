//! `wye claude` and `wye codex`: run an agent against a Wye gateway.
//!
//! Nothing on disk changes, so nothing needs cleaning up: the wrapper checks
//! that the gateway answers, points the agent at it through its environment
//! and `--settings` (Claude Code) or command-line config overrides (Codex),
//! and then replaces itself with the agent. Signals, the terminal, and the
//! exit code are therefore the agent's own.

use std::{ffi::OsString, process::Command};

use gateway_core::config::is_cleartext_remote;

use super::{
    Env, Error, Exit,
    args::Parsed,
    files,
    probe::{self, gateway_url, health_url, no_gateway, on_this_machine},
};

/// The environment variable Codex reads its bearer token from (the Codex
/// provider's `env_key`).
const CODEX_TOKEN_ENV: &str = "WYE_CODEX_TOKEN";
/// Sent by Codex when the gateway takes no gateway token and the caller has
/// no OpenAI key: Codex needs a token, and a gateway without `[auth]`
/// replaces it with the provider key.
const CODEX_NO_TOKEN: &str = "wye-loopback";
/// The caller's own OpenAI API key, which Codex sends to a gateway on this
/// machine that takes no gateway token: a passthrough gateway forwards it,
/// and one with a provider key replaces it.
const OPENAI_KEY_ENV: &str = "OPENAI_API_KEY";
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

    /// The agent's name, for what to install.
    fn product(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// The variable that names another path to the program.
    fn program_variable(self) -> &'static str {
        match self {
            Self::Claude => "WYE_CLAUDE_BIN",
            Self::Codex => "WYE_CODEX_BIN",
        }
    }
}

/// Where the wrapper sends the agent, and with which gateway token.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    pub gateway: String,
    /// A gateway token, sent as `Authorization: Bearer`; `None` keeps
    /// Claude Code's own login (subscription passthrough), or lets Codex
    /// reach a gateway without `[auth]`.
    pub token: Option<String>,
    /// What Codex sends as its bearer token without a gateway token: the
    /// caller's `OPENAI_API_KEY` ([`codex_key`]); `None` sends a placeholder.
    pub codex_key: Option<String>,
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
        env: impl Env,
    ) -> Result<Self, String> {
        let token = files::caller_token(token_file, &env)?;
        let gateway = gateway_url(url, &env);
        // Plain http beyond this machine would carry the gateway token, or
        // the agent's own login, in the clear.
        if let Ok(uri) = gateway.parse::<http::Uri>()
            && is_cleartext_remote(&uri)
        {
            return Err(format!(
                "{gateway} is a remote gateway over plain http; use https"
            ));
        }
        Ok(Self {
            gateway,
            token,
            codex_key: None,
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
                // `--settings`. The token stays out of it: other local
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
                if let Some(token) = &self.token {
                    command.env("ANTHROPIC_AUTH_TOKEN", token);
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
                let bearer = self.token.as_ref().or(self.codex_key.as_ref());
                command.env(
                    CODEX_TOKEN_ENV,
                    bearer.map_or(CODEX_NO_TOKEN, String::as_str),
                );
            }
        }
        command.args(&self.args);
        command
    }
}

/// The OpenAI key Codex sends without a gateway token: the caller's own
/// `OPENAI_API_KEY`, to a gateway on this machine only, so the key never
/// travels to another. A passthrough gateway forwards it; one with a
/// provider key replaces it.
pub fn codex_key(launch: &Launch, env: &impl Env) -> Option<String> {
    if launch.token.is_some() || !on_this_machine(&launch.gateway) {
        return None;
    }
    env.var(OPENAI_KEY_ENV)
}

/// `value` as a TOML basic string, as Codex parses `-c` values.
fn toml_string(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

/// `wye claude`, `wye codex`: the agent, run through the gateway as the
/// module docs describe. On Unix the process becomes the agent, and this
/// returns only if it cannot start; elsewhere the process exits with the
/// agent's exit code.
pub fn run(agent: Agent, parsed: &Parsed<'_>, env: &impl Env) -> Result<Exit, Error> {
    let mut launch = Launch::resolve(
        parsed.value("url"),
        parsed.value("token-file"),
        &parsed.positional,
        env,
    )?;
    if agent == Agent::Codex {
        launch.codex_key = codex_key(&launch, env);
    }
    probe::blocking(&health_url(&launch.gateway))
        .map_err(|why| no_gateway(&launch.gateway, &why, "--url", env))?;
    let program = agent.program();
    let name = program.to_string_lossy().into_owned();
    let mut command = launch.command(agent, program);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only returns on failure.
        Err(cannot_run(agent, &name, &command.exec()).into())
    }
    #[cfg(not(unix))]
    {
        let status = command.status().map_err(|e| cannot_run(agent, &name, &e))?;
        std::process::exit(status.code().unwrap_or(1))
    }
}

fn cannot_run(agent: Agent, name: &str, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "cannot run {name}: not found; install {}, or set {} to its path",
            agent.product(),
            agent.program_variable()
        )
    } else {
        format!("cannot run {name}: {error}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{super::probe::DEFAULT_GATEWAY, *};

    #[test]
    fn flags_override_environment_and_the_https_rule_holds() {
        let env = |name: &str| match name {
            "WYE_URL" => Some("http://localhost:9000/".to_owned()),
            "WYE_TOKEN" => Some("from-env".to_owned()),
            _ => None,
        };
        let launch = Launch::resolve(None, None, &["-p", "fix tests"], env).unwrap();
        assert_eq!(launch.gateway, "http://localhost:9000");
        assert_eq!(launch.token.as_deref(), Some("from-env"));
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
        assert_eq!(defaults.token, None);
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
    fn a_missing_agent_says_what_to_install() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            cannot_run(Agent::Claude, "claude", &missing),
            "cannot run claude: not found; install Claude Code, or set WYE_CLAUDE_BIN to its path"
        );
        assert!(cannot_run(Agent::Codex, "codex", &missing).contains("install Codex,"));
    }

    #[test]
    fn claude_gets_the_gateway_environment() {
        let launch = Launch {
            gateway: "http://gw".into(),
            token: Some("tok".into()),
            codex_key: None,
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
        // Background sessions get the URL and hint headers, never the token.
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
            token: None,
            codex_key: None,
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
        // The caller's own key, when there is one; a gateway token wins.
        let bearer = |token: Option<&str>| {
            let launch = Launch {
                token: token.map(str::to_owned),
                codex_key: Some("sk-mine".into()),
                ..Launch::resolve(Some("http://127.0.0.1:8080"), None, &[], |_| None).unwrap()
            };
            let command = launch.command(Agent::Codex, "codex");
            let value = command
                .get_envs()
                .find(|(name, _)| *name == CODEX_TOKEN_ENV)
                .and_then(|(_, value)| value)
                .map(|v| v.to_str().unwrap().to_owned());
            value.unwrap()
        };
        assert_eq!(bearer(None), "sk-mine");
        assert_eq!(bearer(Some("tok")), "tok");
    }

    /// Without a gateway token, Codex sends the caller's OPENAI_API_KEY to a
    /// gateway on this machine, and to no other.
    #[test]
    fn codex_sends_the_callers_openai_key_only_to_this_machine() {
        let with_key = |name: &str| (name == "OPENAI_API_KEY").then(|| "sk-mine".to_owned());
        let empty = |name: &str| (name == "OPENAI_API_KEY").then(String::new);
        let launch = |url: &str, token: Option<&str>| Launch {
            token: token.map(str::to_owned),
            ..Launch::resolve(Some(url), None, &[], |_| None).unwrap()
        };
        let local = launch("http://127.0.0.1:8080", None);
        assert_eq!(codex_key(&local, &with_key).as_deref(), Some("sk-mine"));
        assert_eq!(codex_key(&local, &|_: &str| None), None);
        assert_eq!(codex_key(&local, &empty), None);
        // A gateway token goes instead; another machine never gets the key.
        assert_eq!(
            codex_key(&launch("http://127.0.0.1:8080", Some("tok")), &with_key),
            None
        );
        assert_eq!(
            codex_key(&launch("https://gw.example.com", None), &with_key),
            None
        );
    }
}
