//! `wye init`: writes a minimal working config and sample policy tests, so a
//! first run needs no example files and no reading.
//!
//! The config is validated before it is written, an existing file is never
//! overwritten, and the next commands are printed.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use gateway_core::config::Config;

use super::{
    REPO_AT_RELEASE,
    args::Parsed,
    files::{self, CONFIG_FILE, POLICY_TESTS_DIR, SIGNING_KEY_FILE},
    plural, token,
};

/// The setup `wye init` writes (the glossary's term), from two choices: who
/// can reach the gateway, and whose credential pays. A local
/// gateway passes callers' credentials through or holds a provider key; a
/// shared one always holds the provider key, since it cannot forward
/// credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    /// One developer: each caller's own credential goes through, whether
    /// Claude Code's login or an API key, so the gateway holds no secret for
    /// Claude.
    Passthrough,
    /// A local gateway that holds an Anthropic API key, read from a file
    /// next to the config, so a background service finds it too: one
    /// developer with a company key, or a sidecar beside one app.
    ProviderKey,
    /// A shared gateway: gateway tokens (`[auth]`), all interfaces, and the
    /// provider key from the deployment's environment: a team, or an app's
    /// services.
    Shared,
}

/// What `wye init` writes, and where.
#[derive(Debug, PartialEq, Eq)]
pub struct InitOptions {
    pub setup: Setup,
    /// Also serve OpenAI Responses and Chat Completions (for Codex).
    pub openai: bool,
    pub out: PathBuf,
    /// Wye's home directory, where `--shared` keeps the signing key.
    pub home: Option<PathBuf>,
}

/// A policy test `init` writes: a request body, and the decision it expects
/// from the config `init` writes beside it.
#[derive(Debug)]
pub struct Sample {
    /// The case's name: `<name>.json` and `<name>.expect.toml`.
    pub name: &'static str,
    pub request: &'static str,
    pub expect: String,
}

impl InitOptions {
    /// The config text, validated; `public_key` is the signing key's public
    /// half, for `Setup::Shared`.
    pub fn config(&self, public_key: &str) -> Result<String, String> {
        let (listen, credential, openai_key) = match self.setup {
            Setup::Passthrough => (
                "127.0.0.1:8080",
                "# Each caller's own credential goes through, whether Claude Code's\n\
                 # login or an API key: this machine only, no [auth].\n\
                 forward_caller_credential = true",
                "provider_key_file = \"openai.key\"",
            ),
            Setup::ProviderKey => (
                "127.0.0.1:8080",
                "# Read at startup from this file, next to this config:\n\
                 provider_key_file = \"anthropic.key\"",
                "provider_key_file = \"openai.key\"",
            ),
            Setup::Shared => (
                "0.0.0.0:8080",
                "provider_key_env = \"ANTHROPIC_API_KEY\"",
                "provider_key_env = \"OPENAI_API_KEY\"",
            ),
        };
        let mut text = format!(
            "# Written by `wye init`. Every field, with its default and limits:\n\
             # {REPO_AT_RELEASE}/examples/wye.toml\n\n\
             [server]\nlisten = \"{listen}\"\n\n\
             [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\n{credential}\n"
        );
        if self.openai {
            text.push_str(&format!(
                "\n# OpenAI Responses and Chat Completions (wye codex, OpenAI SDKs at /v1).\n\
                 [routes.openai]\n[[routes.openai.upstreams]]\nid = \"openai\"\n\
                 base_url = \"https://api.openai.com\"\n{openai_key}\n"
            ));
        }
        if self.setup == Setup::Shared {
            text.push_str(&format!(
                "\n# Gateway tokens: `wye token issue <name>` signs one with the key whose\n\
                 # public half is below. Give each gateway its own audience.\n\
                 [auth]\nissuer = \"wye\"\naudience = \"wye\"\n\n\
                 [[auth.signing_keys]]\nid = \"k1\"\ned25519_public_key = \"{public_key}\"\n"
            ));
        }
        Config::from_toml_str(&text).map_err(|e| e.to_string())?;
        Ok(text)
    }

    /// The policy tests written next to the config: each passes against
    /// [`config`](Self::config), and a config change that alters its
    /// decision fails `wye policy-test`.
    pub fn samples(&self) -> Vec<Sample> {
        const MESSAGES: &str = "{\"model\":\"claude-sonnet-5-5\",\"max_tokens\":1024,\
                                \"messages\":[{\"role\":\"user\",\"content\":\"Hello\"}]}\n";
        const RESPONSES: &str =
            "{\"model\":\"gpt-5.5\",\"max_output_tokens\":1024,\"input\":\"Hello\"}\n";
        let shared = self.setup == Setup::Shared;
        let token = if shared {
            "\n# A policy test states the gateway token's claims instead of carrying a\n\
             # token, which would expire; the gateway checks the real one.\n\
             [token]\nsubject = \"alice\"\n"
        } else {
            ""
        };
        let credential = match self.setup {
            Setup::Passthrough => "the caller's own credential",
            Setup::ProviderKey => "the provider key in anthropic.key",
            Setup::Shared => "the provider key from ANTHROPIC_API_KEY",
        };
        let mut samples = vec![Sample {
            name: "messages",
            request: MESSAGES,
            expect: format!(
                "# Each <name>.json here is a request body, and <name>.expect.toml the\n\
                 # decision the gateway should make: `wye policy-test` checks them all,\n\
                 # and `wye explain <name>.json` shows the steps for one.\n\
                 #\n\
                 # A Messages request (Claude Code, the Anthropic SDKs) goes through, with\n\
                 # {credential}.\n\
                 decision = \"forward\"\n{token}"
            ),
        }];
        if shared {
            samples.push(Sample {
                name: "no-token",
                request: MESSAGES,
                expect: "# Without a gateway token, nothing gets through.\n\
                         decision = \"refuse\"\nstatus = 401\nerror = \"authentication_error\"\n"
                    .to_owned(),
            });
        }
        if self.openai {
            samples.push(Sample {
                name: "responses",
                request: RESPONSES,
                expect: format!(
                    "# An OpenAI Responses request (Codex, the OpenAI SDKs) goes to the\n\
                     # openai upstream.\n\
                     decision = \"forward\"\nendpoint = \"/v1/responses\"\n\
                     upstream = \"openai\"\n{token}"
                ),
            });
        }
        samples
    }

    /// Where `--shared` keeps the signing key.
    fn key_path(&self) -> Result<PathBuf, String> {
        self.home
            .as_ref()
            .map(|home| home.join(SIGNING_KEY_FILE))
            .ok_or_else(|| "no home directory for the signing key: set WYE_HOME".to_owned())
    }

    /// ` -c <out>` when the default lookup would not find the config.
    fn config_flag(&self) -> String {
        let found = self
            .home
            .as_ref()
            .is_some_and(|h| h.join(CONFIG_FILE) == self.out);
        if found {
            String::new()
        } else {
            format!(" -c {}", self.out.display())
        }
    }

    /// Where the sample policy tests go: next to the config, where
    /// `wye policy-test` looks.
    fn tests_dir(&self) -> PathBuf {
        files::config_dir(&self.out).join(POLICY_TESTS_DIR)
    }

    /// What to run next: each step's words, and its command, if any.
    pub fn next_steps(&self, show: impl Fn(&Path) -> String) -> String {
        let c = self.config_flag();
        let dir = self.out.parent().unwrap_or(Path::new("."));
        let mut steps: Vec<(String, String)> = vec![];
        let key = |name: &str, file: &str| format!("{name} in {}", show(&dir.join(file)));
        let mut keys = Vec::new();
        if self.setup == Setup::ProviderKey {
            keys.push(key("Anthropic API key", "anthropic.key"));
        }
        if self.openai && self.setup != Setup::Shared {
            keys.push(key("OpenAI key", "openai.key"));
        }
        if !keys.is_empty() {
            steps.push((
                format!(
                    "Put your {}, readable only by you (chmod 600).",
                    keys.join(" and your ")
                ),
                String::new(),
            ));
        }
        if self.setup == Setup::Shared {
            let keys = if self.openai {
                "ANTHROPIC_API_KEY and OPENAI_API_KEY"
            } else {
                "ANTHROPIC_API_KEY"
            };
            steps.push((format!("Set {keys} where the gateway runs."), String::new()));
        }
        steps.push((
            "Check the config's secrets, files, and upstreams:".to_owned(),
            format!("wye doctor{c}"),
        ));
        if self.setup == Setup::Shared {
            steps.push((
                "Issue a gateway token for each person or service:".to_owned(),
                format!("wye token issue alice{c} --out alice.token"),
            ));
            // Whoever holds the key mints tokens, which cannot be revoked
            // one by one.
            let key = self.key_path().map_or_else(
                |_| "The signing key".to_owned(),
                |key| format!("The signing key, {},", show(&key)),
            );
            steps.push((
                format!(
                    "{key} issues gateway tokens, and `wye serve` never reads it: keep it \
                     off the hosts that serve (move it, if this is one), with whoever \
                     issues tokens."
                ),
                String::new(),
            ));
            steps.push((
                format!("Deploy it: {REPO_AT_RELEASE}/docs/operations.md#deployment"),
                String::new(),
            ));
        } else {
            steps.push((
                "Run the gateway in the background, now and at every login (it shows \
                 what it sets up and asks first; or run `wye serve` in a terminal):"
                    .to_owned(),
                format!("wye service install{c}"),
            ));
            let agents = match (self.setup, self.openai) {
                (Setup::Passthrough, false) => {
                    "Start Claude Code through it, signed in as usual (an exported \
                     ANTHROPIC_API_KEY takes over from your login, and bills the API):"
                }
                (Setup::Passthrough, true) => {
                    "Start Claude Code, or Codex, through it (an exported \
                     ANTHROPIC_API_KEY takes over from your login, and bills the API):"
                }
                (_, true) => "Start Claude Code, or Codex, through it:",
                (_, false) => "Start Claude Code through it:",
            };
            steps.push((agents.to_owned(), "wye claude".to_owned()));
            steps.push(("See what it recorded:".to_owned(), "wye report".to_owned()));
        }
        let mut out = "Next:\n".to_owned();
        for (i, (words, command)) in steps.iter().enumerate() {
            // Words wrap under themselves; a command stays on one line, so
            // it can be copied.
            let wrapped = super::text::wrap(words, 5);
            out.push_str(&format!("  {}. {}", i + 1, wrapped.trim_start()));
            if !command.is_empty() {
                out.push_str(&format!("     {command}\n"));
            }
        }
        out
    }
}

/// `wye init`: what the options ask for, written; returns what to print.
pub fn run(parsed: &Parsed<'_>, env: &impl Fn(&str) -> Option<String>) -> Result<String, String> {
    // A shared gateway always holds the provider key, so --provider-key
    // beside --shared changes nothing.
    let setup = match (parsed.flag("provider-key"), parsed.flag("shared")) {
        (_, true) => Setup::Shared,
        (true, false) => Setup::ProviderKey,
        (false, false) => Setup::Passthrough,
    };
    // A shared gateway's config belongs in the repository it is reviewed in.
    let out = match (parsed.value("out"), setup) {
        (Some(out), _) => PathBuf::from(out),
        (None, Setup::Shared) => PathBuf::from(CONFIG_FILE),
        (None, _) => files::in_home(CONFIG_FILE, "pass --out", env)?,
    };
    init(
        &InitOptions {
            setup,
            openai: parsed.flag("openai"),
            out,
            home: files::home(env),
        },
        env,
    )
}

/// Writes the starter config to `options.out`, never over an existing file
/// (or through a symlink); with `--shared`, also the signing key, unless one is
/// there to reuse; then the sample policy tests, keeping any that exist.
pub fn init(
    options: &InitOptions,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let show = |path: &Path| files::tilde(path, env);
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
    let public_key = if options.setup == Setup::Shared {
        let path = options.key_path()?;
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
        super::create_private_dir(dir)?;
    }
    write_new(path, &text, &show)?;
    let mut done = format!("wrote {}", show(path));
    if let Some(key) = wrote_key {
        done.push_str(&format!(
            ", and the signing key {} (keep it out of Git)",
            show(&key)
        ));
    }
    let dir = options.tests_dir();
    let (wrote, kept) = write_samples(&dir, &options.samples(), &show)?;
    if !wrote.is_empty() {
        done.push_str(&format!(
            "\nwrote {} in {}, for `wye policy-test{}`",
            plural(wrote.len() as u64, "policy test", "policy tests"),
            show(&dir),
            options.config_flag()
        ));
    }
    if !kept.is_empty() {
        done.push_str(&format!(
            "\nkept the policy tests in {} that exist: {}",
            show(&dir),
            kept.join(", ")
        ));
    }
    Ok(format!("{done}\n{}", options.next_steps(show)))
}

/// Writes the samples to `dir`, keeping every case of which a file exists;
/// the names written, and those kept.
fn write_samples(
    dir: &Path,
    samples: &[Sample],
    show: &impl Fn(&Path) -> String,
) -> Result<(Vec<&'static str>, Vec<&'static str>), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", show(dir)))?;
    let (mut wrote, mut kept) = (Vec::new(), Vec::new());
    for sample in samples {
        let request = dir.join(format!("{}.json", sample.name));
        let expect = dir.join(format!("{}.expect.toml", sample.name));
        // A request beside another's expectation would test something else.
        if request.symlink_metadata().is_ok() || expect.symlink_metadata().is_ok() {
            kept.push(sample.name);
            continue;
        }
        write_new(&request, sample.request, show)?;
        write_new(&expect, &sample.expect, show)?;
        wrote.push(sample.name);
    }
    Ok((wrote, kept))
}

/// Writes `text` to the new file `path`, never over an existing one.
fn write_new(path: &Path, text: &str, show: &impl Fn(&Path) -> String) -> Result<(), String> {
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
        .map_err(|e| format!("cannot write {}: {e}", show(path)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const SETUPS: [Setup; 3] = [Setup::Passthrough, Setup::ProviderKey, Setup::Shared];

    fn options(setup: Setup, openai: bool, out: &Path) -> InitOptions {
        InitOptions {
            setup,
            openai,
            out: out.to_owned(),
            home: Some(out.parent().unwrap().to_owned()),
        }
    }

    #[test]
    fn writes_a_valid_config_for_each_setup_and_never_overwrites() {
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        for setup in SETUPS {
            for openai in [false, true] {
                let text = options(setup, openai, Path::new("/h/wye.toml"))
                    .config(key)
                    .unwrap();
                let config = Config::from_toml_str(&text).unwrap();
                assert_eq!(config.routes.openai.is_some(), openai);
                assert_eq!(config.auth.is_some(), setup == Setup::Shared);
            }
        }

        let dir = wye_testkit::TempDir::new("init");
        let out = dir.join("home/wye.toml");
        let env = |_: &str| None;
        let shared = options(Setup::Shared, false, &out);
        let printed = init(&shared, &env).unwrap();
        assert!(printed.contains("signing key"), "{printed}");
        assert!(
            printed.contains("\n     wye token issue alice --out alice.token\n"),
            "{printed}"
        );
        assert!(init(&shared, &env).unwrap_err().contains("exists"));
        // A second shared config reuses the key, and keeps the policy tests
        // the first one wrote.
        let again = options(Setup::Shared, false, &dir.join("home/other.toml"));
        let printed = init(&again, &env).unwrap();
        assert!(!printed.contains(", and the signing key"), "{printed}");
        assert!(printed.contains("\n     wye doctor -c "), "{printed}");
        assert!(
            printed.contains("kept the policy tests in ")
                && printed.contains(" that exist: messages, no-token\n"),
            "{printed}"
        );
    }

    #[test]
    fn the_config_and_the_steps_link_this_release() {
        for setup in SETUPS {
            for openai in [false, true] {
                let options = options(setup, openai, Path::new("/h/wye.toml"));
                let text = options.config("A".repeat(43).as_str()).unwrap();
                assert!(
                    text.contains(&format!("\n# {REPO_AT_RELEASE}/examples/wye.toml\n")),
                    "{text}"
                );
                let steps = options.next_steps(|p| p.display().to_string());
                for written in [&text, &steps] {
                    assert!(!written.contains("/blob/main/"), "{written}");
                }
            }
        }
        assert_eq!(
            REPO_AT_RELEASE,
            format!(
                "https://github.com/askrid/wye/blob/v{}",
                env!("CARGO_PKG_VERSION")
            )
        );
        let shared = options(Setup::Shared, false, Path::new("/h/wye.toml"));
        let steps = shared.next_steps(|p| p.display().to_string());
        assert!(
            steps.contains(&format!("{REPO_AT_RELEASE}/docs/operations.md#deployment")),
            "{steps}"
        );
    }

    #[test]
    fn the_sample_policy_tests_pass_against_the_config() {
        for setup in SETUPS {
            for openai in [false, true] {
                let dir = wye_testkit::TempDir::new("init-tests");
                let options = options(setup, openai, &dir.join("wye.toml"));
                let printed = init(&options, &|_: &str| None).unwrap();
                let config = std::fs::read_to_string(dir.join("wye.toml")).unwrap();
                let config = Config::from_toml_str(&config).unwrap();
                let (report, passed) =
                    super::super::policy_test(&config, &dir.join("policy-tests"), 0).unwrap();
                assert!(passed, "{setup:?} openai {openai}: {report}");
                let cases = options.samples().len();
                assert_eq!(
                    cases,
                    1 + usize::from(setup == Setup::Shared) + usize::from(openai)
                );
                assert!(
                    report.ends_with(&format!("{cases} passed, 0 failed\n")),
                    "{report}"
                );
                // The config is where the lookup finds it: no -c.
                assert!(
                    printed.contains(&format!(
                        "\nwrote {} in {}, for `wye policy-test`\n",
                        plural(cases as u64, "policy test", "policy tests"),
                        dir.join("policy-tests").display(),
                    )),
                    "{printed}"
                );
            }
        }
    }

    #[test]
    fn a_policy_test_that_exists_is_kept() {
        let dir = wye_testkit::TempDir::new("init-kept");
        let cases = dir.join("policy-tests");
        std::fs::create_dir_all(&cases).unwrap();
        // Half a case: its other half is not written beside it either.
        std::fs::write(
            cases.join("messages.expect.toml"),
            "decision = \"refuse\"\n",
        )
        .unwrap();
        let options = options(Setup::Passthrough, true, &dir.join("wye.toml"));
        let printed = init(&options, &|_: &str| None).unwrap();
        assert!(printed.contains(" that exist: messages\n"), "{printed}");
        assert!(printed.contains("\nwrote 1 policy test in "), "{printed}");
        assert!(!cases.join("messages.json").exists());
        assert_eq!(
            std::fs::read_to_string(cases.join("messages.expect.toml")).unwrap(),
            "decision = \"refuse\"\n"
        );
        assert!(cases.join("responses.json").exists());
    }

    #[test]
    fn the_next_steps_name_a_config_the_lookup_would_not_find() {
        let steps = |out: &str| {
            let options = InitOptions {
                home: Some("/h".into()),
                ..options(Setup::Shared, false, Path::new(out))
            };
            options.next_steps(|p| p.display().to_string())
        };
        assert!(steps("/h/wye.toml").contains("\n     wye doctor\n"));
        let repo = steps("wye.toml");
        assert!(repo.contains("\n     wye doctor -c wye.toml\n"), "{repo}");
        assert!(
            repo.contains("Issue a gateway token for each person or service:\n"),
            "{repo}"
        );
        assert!(
            repo.contains("\n     wye token issue alice -c wye.toml --out alice.token\n"),
            "{repo}"
        );
        let flat = repo.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains(
                "The signing key, /h/signing.key, issues gateway tokens, and `wye serve` \
                 never reads it: keep it off the hosts that serve"
            ),
            "{repo}"
        );
    }

    #[test]
    fn the_default_needs_no_key_and_says_what_an_exported_one_does() {
        let options = options(Setup::Passthrough, false, Path::new("/h/wye.toml"));
        let config = Config::from_toml_str(&options.config("").unwrap()).unwrap();
        assert!(config.auth.is_none());
        assert_eq!(config.server.listen.ip().to_string(), "127.0.0.1");
        let steps = options.next_steps(|p| p.display().to_string());
        assert!(!steps.contains(".key"), "{steps}");
        let flat = steps.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("an exported ANTHROPIC_API_KEY takes over"),
            "{steps}"
        );
        assert!(steps.contains("  1. Check the config's secrets"), "{steps}");
    }

    #[test]
    fn personal_configs_read_keys_from_files_next_to_them() {
        let steps = options(Setup::ProviderKey, true, Path::new("/h/wye.toml"))
            .next_steps(|p| p.display().to_string());
        assert!(steps.contains("/h/anthropic.key") && steps.contains("/h/openai.key"));
        assert!(
            steps.contains(
                "  2. Check the config's secrets, files, and upstreams:\n     wye doctor\n"
            ),
            "{steps}"
        );
        assert!(!steps.contains(" -c "), "{steps}");
    }
}
