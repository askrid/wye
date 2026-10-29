//! `wye init`: writes a minimal working config, so a first run needs no
//! example files and no reading.
//!
//! The config is validated before it is written, an existing file is never
//! overwritten, and the next commands are printed.

use std::path::Path;

use gateway_core::config::Config;

/// What the starter config connects to.
#[derive(Debug, PartialEq, Eq)]
pub struct InitOptions {
    /// Forward Claude Code's own login (a claude.ai subscription) instead of
    /// a provider key.
    pub subscription: bool,
    /// The environment variable holding the Anthropic API key.
    pub anthropic_key_env: String,
    /// Also serve OpenAI Responses and Chat Completions, with the key in
    /// this environment variable.
    pub openai_key_env: Option<String>,
    pub out: String,
}

impl InitOptions {
    /// Parses `[--subscription] [--anthropic-key-env NAME]
    /// [--openai-key-env NAME] [--out PATH]`.
    pub fn from_args(args: &[&str]) -> Result<Self, String> {
        let mut options = Self {
            subscription: false,
            anthropic_key_env: "ANTHROPIC_API_KEY".into(),
            openai_key_env: None,
            out: "wye.toml".into(),
        };
        let mut rest = args;
        loop {
            match rest {
                [] => break,
                ["--subscription", tail @ ..] => {
                    options.subscription = true;
                    rest = tail;
                }
                ["--anthropic-key-env", name, tail @ ..] => {
                    options.anthropic_key_env = (*name).to_owned();
                    rest = tail;
                }
                ["--openai-key-env", name, tail @ ..] => {
                    options.openai_key_env = Some((*name).to_owned());
                    rest = tail;
                }
                ["--out", path, tail @ ..] => {
                    options.out = (*path).to_owned();
                    rest = tail;
                }
                [other, ..] => return Err(format!("unknown option {other}")),
            }
        }
        Ok(options)
    }

    /// The config text, validated.
    pub fn config(&self) -> Result<String, String> {
        let credential = if self.subscription {
            "# Claude Code's own login is forwarded: loopback only, no [auth].\n\
             forward_caller_credential = true"
                .to_owned()
        } else {
            format!("api_key_env = \"{}\"", self.anthropic_key_env)
        };
        let mut text = format!(
            "# Written by `wye init`. Every field, with its default and limits:\n\
             # https://github.com/askrid/wye/blob/main/examples/wye.toml\n\n\
             [server]\nlisten = \"127.0.0.1:8080\"\n\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\n{credential}\n"
        );
        if let Some(name) = &self.openai_key_env {
            text.push_str(&format!(
                "\n# OpenAI Responses and Chat Completions (wye codex, OpenAI SDKs at /v1).\n\
                 [routes.openai]\n[[routes.openai.upstreams]]\nid = \"openai\"\n\
                 base_url = \"https://api.openai.com\"\napi_key_env = \"{name}\"\n"
            ));
        }
        Config::from_toml_str(&text).map_err(|e| e.to_string())?;
        Ok(text)
    }

    /// What to run next.
    pub fn next_steps(&self) -> String {
        let out = &self.out;
        let mut steps = String::from("Next:\n");
        let mut keys = Vec::new();
        if !self.subscription {
            keys.push(self.anthropic_key_env.as_str());
        }
        if let Some(name) = &self.openai_key_env {
            keys.push(name);
        }
        for key in keys {
            steps.push_str(&format!("  export {key}=...\n"));
        }
        steps.push_str(&format!(
            "  wye doctor --config {out}\n  wye serve --config {out} 2>>usage.jsonl &\n  wye claude\n"
        ));
        if self.openai_key_env.is_some() {
            steps.push_str("  wye codex\n");
        }
        steps.push_str("  wye report usage.jsonl\n");
        steps.push_str(
            "To keep it running across logins: \
             https://github.com/askrid/wye/blob/main/docs/operations.md#running-as-a-service\n",
        );
        steps
    }
}

/// Writes the starter config to `options.out`, never over an existing file.
pub fn init(options: &InitOptions) -> Result<String, String> {
    let text = options.config()?;
    let path = Path::new(&options.out);
    if path.exists() {
        return Err(format!(
            "{} exists; choose another path with --out",
            path.display()
        ));
    }
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(format!(
        "wrote {}\n{}",
        path.display(),
        options.next_steps()
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn writes_a_valid_config_for_each_mode_and_never_overwrites() {
        for args in [
            &[][..],
            &["--subscription"][..],
            &[
                "--anthropic-key-env",
                "TEAM_KEY",
                "--openai-key-env",
                "OPENAI_API_KEY",
            ][..],
        ] {
            let options = InitOptions::from_args(args).unwrap();
            let config = Config::from_toml_str(&options.config().unwrap()).unwrap();
            assert_eq!(
                config.routes.openai.is_some(),
                options.openai_key_env.is_some()
            );
        }
        assert!(InitOptions::from_args(&["--bogus"]).is_err());

        let dir = std::env::temp_dir().join(format!("wye-init-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("wye.toml").display().to_string();
        let options = InitOptions::from_args(&["--out", &out]).unwrap();
        let printed = init(&options).unwrap();
        assert!(
            printed.contains("export ANTHROPIC_API_KEY=..."),
            "{printed}"
        );
        assert!(init(&options).unwrap_err().contains("exists"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
