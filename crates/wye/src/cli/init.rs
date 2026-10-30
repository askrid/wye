//! `wye init`: writes a minimal working config, so a first run needs no
//! example files and no reading.
//!
//! The config is validated before it is written, an existing file is never
//! overwritten, and the next commands are printed.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use gateway_core::config::Config;

use super::{args, token};

/// Who the gateway is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// One developer with an Anthropic API key, read from a file next to the
    /// config, so a background service finds it too.
    ApiKey,
    /// One developer forwarding Claude Code's own claude.ai login.
    Subscription,
    /// A shared gateway: gateway tokens (`[auth]`), all interfaces, and the
    /// provider key from the deployment's environment.
    Team,
}

/// What `wye init` writes, and where.
#[derive(Debug, PartialEq, Eq)]
pub struct InitOptions {
    pub mode: Mode,
    /// Also serve OpenAI Responses and Chat Completions (for Codex).
    pub openai: bool,
    pub out: PathBuf,
    /// Wye's home directory, where `--auth` keeps the signing key.
    pub home: Option<PathBuf>,
}

impl InitOptions {
    /// The config text, validated; `public_key` is the signing key's public
    /// half, for `Mode::Team`.
    pub fn config(&self, public_key: &str) -> Result<String, String> {
        let (listen, credential, openai_key) = match self.mode {
            Mode::ApiKey => (
                "127.0.0.1:8080",
                "# Read at startup from this file, next to this config:\n\
                 api_key_file = \"anthropic.key\"",
                "api_key_file = \"openai.key\"",
            ),
            Mode::Subscription => (
                "127.0.0.1:8080",
                "# Claude Code's own login is forwarded: loopback only, no [auth].\n\
                 forward_caller_credential = true",
                "api_key_file = \"openai.key\"",
            ),
            Mode::Team => (
                "0.0.0.0:8080",
                "api_key_env = \"ANTHROPIC_API_KEY\"",
                "api_key_env = \"OPENAI_API_KEY\"",
            ),
        };
        let mut text = format!(
            "# Written by `wye init`. Every field, with its default and limits:\n\
             # https://github.com/askrid/wye/blob/main/examples/wye.toml\n\n\
             [server]\nlisten = \"{listen}\"\n\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\n{credential}\n"
        );
        if self.openai {
            text.push_str(&format!(
                "\n# OpenAI Responses and Chat Completions (wye codex, OpenAI SDKs at /v1).\n\
                 [routes.openai]\n[[routes.openai.upstreams]]\nid = \"openai\"\n\
                 base_url = \"https://api.openai.com\"\n{openai_key}\n"
            ));
        }
        if self.mode == Mode::Team {
            text.push_str(&format!(
                "\n# Gateway tokens: `wye token issue <name>` signs one with the key whose\n\
                 # public half is below. Give each gateway its own audience.\n\
                 [auth]\nissuer = \"wye\"\naudience = \"wye\"\n\n\
                 [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{public_key}\"\n"
            ));
        }
        Config::from_toml_str(&text).map_err(|e| e.to_string())?;
        Ok(text)
    }

    /// Where `--auth` keeps the signing key.
    fn key_path(&self) -> Result<PathBuf, String> {
        self.home
            .as_ref()
            .map(|home| home.join("issuer.key"))
            .ok_or_else(|| "no home directory for the signing key: set WYE_HOME".to_owned())
    }

    /// ` -c <out>` when the default lookup would not find the config.
    fn config_flag(&self) -> String {
        let found = self
            .home
            .as_ref()
            .is_some_and(|h| h.join("wye.toml") == self.out);
        if found {
            String::new()
        } else {
            format!(" -c {}", self.out.display())
        }
    }

    /// What to run next: each step's words, and its command, if any.
    pub fn next_steps(&self, show: impl Fn(&Path) -> String) -> String {
        let c = self.config_flag();
        let dir = self.out.parent().unwrap_or(Path::new("."));
        let mut steps: Vec<(String, String)> = vec![];
        let key = |name: &str, file: &str| format!("{name} in {}", show(&dir.join(file)));
        match (self.mode, self.openai) {
            (Mode::ApiKey, openai) => {
                let mut keys = format!("Put your {}", key("Anthropic API key", "anthropic.key"));
                if openai {
                    keys.push_str(&format!(" and your {}", key("OpenAI key", "openai.key")));
                }
                keys.push_str(", readable only by you (chmod 600).");
                steps.push((keys, String::new()));
            }
            (Mode::Subscription, true) => steps.push((
                format!(
                    "Put your {}, readable only by you (chmod 600).",
                    key("OpenAI key", "openai.key")
                ),
                String::new(),
            )),
            (Mode::Subscription, false) => {}
            (Mode::Team, openai) => {
                let keys = if openai {
                    "ANTHROPIC_API_KEY and OPENAI_API_KEY"
                } else {
                    "ANTHROPIC_API_KEY"
                };
                steps.push((format!("Set {keys} where the gateway runs."), String::new()));
            }
        }
        steps.push((
            "Check the keys, files, and endpoints:".to_owned(),
            format!("wye doctor{c}"),
        ));
        if self.mode == Mode::Team {
            steps.push((
                "Issue a token for each person or service:".to_owned(),
                format!("wye token issue alice{c} > alice.token"),
            ));
            steps.push((
                "Deploy it: https://github.com/askrid/wye/blob/main/docs/operations.md#deployment-modes"
                    .to_owned(),
                String::new(),
            ));
        } else {
            steps.push((
                "Run the gateway in the background, now and at every login (it shows \
                 what it sets up and asks first; or run `wye serve` in a terminal):"
                    .to_owned(),
                format!("wye service install{c}"),
            ));
            let agents = if self.openai {
                "Start Claude Code, or Codex, through it:"
            } else {
                "Start Claude Code through it:"
            };
            steps.push((agents.to_owned(), "wye claude".to_owned()));
            steps.push(("See what it recorded:".to_owned(), "wye report".to_owned()));
        }
        let mut out = "Next:\n".to_owned();
        for (i, (words, command)) in steps.iter().enumerate() {
            // Words wrap under themselves; a command stays on one line, so
            // it can be copied.
            let wrapped = args::wrap(words, 5, args::WIDTH);
            out.push_str(&format!("  {}. {}", i + 1, wrapped.trim_start()));
            if !command.is_empty() {
                out.push_str(&format!("     {command}\n"));
            }
        }
        out
    }
}

/// Writes the starter config to `options.out`, never over an existing file
/// (or through a symlink); with `--auth`, also the signing key, unless one is
/// there to reuse.
pub fn init(
    options: &InitOptions,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let show = |path: &Path| args::tilde(path, env);
    let path = &options.out;
    // Checked first so a refused run creates no key; `create_new` below
    // still refuses a file that appears in between.
    if path.symlink_metadata().is_ok() {
        return Err(format!(
            "{} exists; choose another path with --out",
            show(path)
        ));
    }
    let mut wrote_key = None;
    let public_key = if options.mode == Mode::Team {
        let path = options.key_path()?;
        if let Some(dir) = path.parent() {
            create_dir(dir)?;
        }
        let (key, created) = token::signing_key(&path)?;
        if created {
            wrote_key = Some(path);
        }
        token::public_key(&key)
    } else {
        String::new()
    };
    let text = options.config(&public_key)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_dir(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                format!("{} exists; choose another path with --out", show(path))
            }
            _ => format!("cannot write {}: {e}", show(path)),
        })?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", show(path)))?;
    let mut done = format!("wrote {}", show(path));
    if let Some(key) = wrote_key {
        done.push_str(&format!(
            ", and the signing key {} (keep it out of Git)",
            show(&key)
        ));
    }
    Ok(format!("{done}\n{}", options.next_steps(show)))
}

/// Creates `dir` if missing, readable only by its owner: it holds keys.
fn create_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn options(mode: Mode, openai: bool, out: &Path) -> InitOptions {
        InitOptions {
            mode,
            openai,
            out: out.to_owned(),
            home: Some(out.parent().unwrap().to_owned()),
        }
    }

    #[test]
    fn writes_a_valid_config_for_each_mode_and_never_overwrites() {
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        for mode in [Mode::ApiKey, Mode::Subscription, Mode::Team] {
            for openai in [false, true] {
                let text = options(mode, openai, Path::new("/h/wye.toml"))
                    .config(key)
                    .unwrap();
                let config = Config::from_toml_str(&text).unwrap();
                assert_eq!(config.routes.openai.is_some(), openai);
                assert_eq!(config.auth.is_some(), mode == Mode::Team);
            }
        }

        let dir = std::env::temp_dir().join(format!("wye-init-{}", std::process::id()));
        let out = dir.join("home/wye.toml");
        let env = |_: &str| None;
        let team = options(Mode::Team, false, &out);
        let printed = init(&team, &env).unwrap();
        assert!(printed.contains("signing key"), "{printed}");
        assert!(
            printed.contains("\n     wye token issue alice > alice.token\n"),
            "{printed}"
        );
        assert!(init(&team, &env).unwrap_err().contains("exists"));
        // A second team config reuses the key.
        let again = options(Mode::Team, false, &dir.join("home/other.toml"));
        let printed = init(&again, &env).unwrap();
        assert!(!printed.contains("signing key"), "{printed}");
        assert!(printed.contains("\n     wye doctor -c "), "{printed}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_next_steps_name_a_config_the_lookup_would_not_find() {
        let steps = |out: &str| {
            let options = InitOptions {
                home: Some("/h".into()),
                ..options(Mode::Team, false, Path::new(out))
            };
            options.next_steps(|p| p.display().to_string())
        };
        assert!(steps("/h/wye.toml").contains("\n     wye doctor\n"));
        let repo = steps("wye.toml");
        assert!(repo.contains("\n     wye doctor -c wye.toml\n"), "{repo}");
        assert!(
            repo.contains("\n     wye token issue alice -c wye.toml > alice.token\n"),
            "{repo}"
        );
    }

    #[test]
    fn personal_configs_read_keys_from_files_next_to_them() {
        let steps = options(Mode::ApiKey, true, Path::new("/h/wye.toml"))
            .next_steps(|p| p.display().to_string());
        assert!(steps.contains("/h/anthropic.key") && steps.contains("/h/openai.key"));
        assert!(
            steps.contains("  2. Check the keys, files, and endpoints:\n     wye doctor\n"),
            "{steps}"
        );
        assert!(!steps.contains(" -c "), "{steps}");
    }
}
