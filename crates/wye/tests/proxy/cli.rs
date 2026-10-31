//! The binary: `wye claude`, help and exit codes, the commands that need no
//! argument, and `serve`'s two output streams.

use std::{
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
};

use wye_testkit::TempDir;

use super::harness::*;

/// How long a `wye` process may stay silent when a test waits for its output.
const OUTPUT_WAIT: Duration = Duration::from_secs(30);

fn wye() -> Command {
    Command::new(env!("CARGO_BIN_EXE_wye"))
}

/// A running `wye`, stopped however the test ends.
pub(super) struct Running(Child);

impl Running {
    /// Starts `command` with its output piped.
    pub(super) fn spawn(mut command: Command) -> Self {
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self(child)
    }

    /// The first line on stderr; a process that writes none fails the test.
    pub(super) fn stderr_line(&mut self) -> String {
        first_line(self.0.stderr.take().unwrap())
    }

    /// The first line on stdout; a process that writes none fails the test.
    fn stdout_line(&mut self) -> String {
        first_line(self.0.stdout.take().unwrap())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The first line of `pipe`, within [`OUTPUT_WAIT`]. The rest is read and
/// discarded on a thread that ends with the writer, so a later write neither
/// blocks nor fails.
fn first_line(pipe: impl Read + Send + 'static) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    rx.recv_timeout(OUTPUT_WAIT)
        .expect("the process wrote nothing")
}

/// A directory with a user's home and a repository in it.
pub(super) struct Setup {
    pub(super) home: TempDir,
    pub(super) repo: PathBuf,
}

impl Setup {
    pub(super) fn new() -> Self {
        let home = TempDir::new("home");
        let repo = home.join("repo");
        std::fs::create_dir_all(home.join(".wye")).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("wye.toml"),
            "[server]\nlisten = \"127.0.0.1:0\"\n\
             [upstream]\nbase_url = \"http://127.0.0.1:1\"\napi_key_env = \"WYE_TEST_UNSET\"\n",
        )
        .unwrap();
        Self { home, repo }
    }

    /// With the user's own config, which needs no key.
    pub(super) fn with_subscription() -> Self {
        let setup = Self::new();
        std::fs::write(
            setup.home.join(".wye/wye.toml"),
            "[server]\nlisten = \"127.0.0.1:0\"\n\
             [upstream]\nbase_url = \"http://127.0.0.1:1\"\nforward_caller_credential = true\n",
        )
        .unwrap();
        setup
    }

    /// `wye`, run from the repository.
    pub(super) fn wye(&self, args: &[&str]) -> Command {
        let mut command = wye();
        command
            .args(args)
            .current_dir(&self.repo)
            .env("HOME", &*self.home)
            .env_remove("WYE_HOME")
            .env_remove("WYE_CONFIG")
            .env_remove("WYE_TEST_UNSET");
        command
    }

    pub(super) fn run(&self, args: &[&str]) -> Output {
        self.wye(args).output().unwrap()
    }
}

pub(super) fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub(super) fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test]
async fn probe_succeeds_only_for_a_2xx_answer() {
    let h = Harness::api_key(ok()).await;
    let base = format!("http://{}", h.gateway);
    let health = format!("{base}/healthz");
    assert_eq!(wye::probe(&health).await.unwrap(), health);
    // Without a path, a URL names a gateway: its `/` serves nothing.
    assert_eq!(wye::probe(&base).await.unwrap(), health);
    assert_eq!(wye::probe(&format!("{base}/")).await.unwrap(), health);
    let ready = format!("{base}/readyz");
    assert_eq!(wye::probe(&ready).await.unwrap(), ready);
    let not_found = wye::probe(&format!("http://{}/nope", h.gateway))
        .await
        .unwrap_err();
    assert!(not_found.contains("404"), "{not_found}");
    // https is accepted (a remote gateway for `wye claude`); other schemes are not.
    let ftp = wye::probe("ftp://127.0.0.1/healthz").await.unwrap_err();
    assert!(ftp.contains("http:// or https://"), "{ftp}");
    // Nothing listens on this port.
    let refused = wye::probe("http://127.0.0.1:9/healthz").await.unwrap_err();
    assert!(
        refused.starts_with("http://127.0.0.1:9/healthz:"),
        "{refused}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn wye_claude_runs_claude_against_the_gateway() {
    let h = Harness::api_key(ok()).await;
    let dir = TempDir::new("claude");
    // A stand-in for claude that records what it was given and exits 3.
    let fake = dir.join("claude");
    let seen = dir.join("seen");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$ANTHROPIC_BASE_URL\" \"$ANTHROPIC_AUTH_TOKEN\" \"${{ANTHROPIC_API_KEY-unset}}\" \"$*\" > {}\nexit 3\n",
            seen.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&fake, permissions).unwrap();
    let credential = dir.join("credential");
    std::fs::write(&credential, "gateway-token\n").unwrap();

    let gateway = format!("http://{}", h.gateway);
    // Run the binary off the runtime, which also serves the gateway.
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let fake = fake.clone();
        tokio::task::spawn_blocking(move || {
            wye()
                .args(args)
                .env("WYE_CLAUDE_BIN", fake)
                .env("ANTHROPIC_API_KEY", "sk-should-be-removed")
                .env_remove("CLAUDE_CODE_GATEWAY_HINT_HEADERS")
                .output()
                .unwrap()
        })
    };
    let credential = credential.to_str().unwrap();
    let output = run(&[
        "claude",
        "--url",
        &gateway,
        "--token-file",
        credential,
        "-p",
        "fix tests",
    ])
    .await
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "claude's exit code passes through"
    );
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        // The URL and the hints also go in --settings, for background sessions; the
        // credential never goes on the command line.
        format!(
            "{gateway}|gateway-token|unset|--settings {{\"env\":{{\"ANTHROPIC_BASE_URL\":\"{gateway}\",\"CLAUDE_CODE_GATEWAY_HINT_HEADERS\":\"1\"}}}} -p fix tests\n"
        )
    );

    // Without a reachable gateway, claude is not started.
    let output = run(&["claude", "--url", "http://127.0.0.1:9"])
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("wye serve"));
}

/// The binary's two streams: records are data, for a pipe or a file; messages
/// are for whoever runs it, and stay out of the data.
#[tokio::test]
async fn serve_writes_records_to_stdout_and_messages_to_stderr() {
    let (upstream, _) = spawn_counted(ok()).await;
    let dir = TempDir::new("streams");
    let config = dir.join("wye.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:0\"\n\
             [upstream]\nbase_url = \"http://{upstream}\"\nforward_caller_credential = true\n"
        ),
    )
    .unwrap();
    let mut command = wye();
    command.args(["serve", "-c", config.to_str().unwrap()]);
    let mut running = Running::spawn(command);
    // Off the runtime, which also serves the upstream.
    let record = tokio::task::spawn_blocking(move || {
        let message = running.stderr_line();
        let addr: SocketAddr = message
            .strip_prefix("wye: listening on http://")
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_else(|| panic!("{message}"))
            .parse()
            .unwrap();
        let mut request = std::net::TcpStream::connect(addr).unwrap();
        std::io::Write::write_all(
            &mut request,
            b"POST /v1/messages HTTP/1.1\r\nhost: wye\r\nx-api-key: k\r\n\
              content-length: 2\r\nconnection: close\r\n\r\n{}",
        )
        .unwrap();
        running.stdout_line()
    })
    .await
    .unwrap();
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["event"], "request");
    assert_eq!(record["path"], "/v1/messages");
}

/// The sample policy tests in examples/ pass against their config.
#[test]
fn example_policy_tests_pass() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy-tests");
    let config =
        Config::from_toml_str(&std::fs::read_to_string(dir.join("wye.toml")).unwrap()).unwrap();
    let (report, passed) = wye::policy_test(&config, &dir, 0).unwrap();
    assert!(passed, "{report}");
    assert!(report.ends_with("4 passed, 0 failed\n"), "{report}");
}

#[test]
fn the_cli_answers_help_and_version_and_names_an_unknown_command() {
    let run = |args: &[&str]| wye().args(args).output().unwrap();
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("\n  serve        Run the gateway"));
    let version = run(&["--version"]);
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("wye {}\n", env!("CARGO_PKG_VERSION"))
    );
    let typo = run(&["srve"]);
    assert_eq!(typo.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&typo.stderr),
        "wye: unknown command `srve`; did you mean `serve`?\n"
    );
    // A known command with wrong arguments gets its own usage.
    let wrong = run(&["serve", "--confg", "x"]);
    assert_eq!(
        String::from_utf8_lossy(&wrong.stderr),
        "wye: unknown option --confg\nusage: wye serve [options]\nmore: wye serve --help\n"
    );
}

#[test]
fn review_commands_exit_3_on_what_they_report() {
    let dir = TempDir::new("exit");
    let write = |name: &str, text: &str| {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    };
    // Nothing listens on port 1: `doctor` stays on this machine.
    let loopback = write(
        "loopback.toml",
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"http://127.0.0.1:1\"\nforward_caller_credential = true\n",
    );
    let keyed = write(
        "keyed.toml",
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"http://127.0.0.1:1\"\napi_key_env = \"WYE_TEST_UNSET\"\n",
    );
    let request = write("request.json", r#"{"messages":[]}"#);
    let status = |args: &[&str]| {
        wye()
            .args(args)
            .env_remove("WYE_TEST_UNSET")
            .output()
            .unwrap()
            .status
            .code()
    };
    // The operator's key where callers brought their own widens access.
    assert_eq!(status(&["policy-diff", &loopback, &keyed]), Some(3));
    assert_eq!(status(&["policy-diff", &keyed, &loopback]), Some(0));
    // explain: a path no route serves is a refusal; the key is not read.
    let explain = |path: &str| status(&["explain", "--config", &keyed, "--path", path, &request]);
    assert_eq!(explain("/v1/messages"), Some(0));
    assert_eq!(explain("/v1/unknown"), Some(3));
    // policy-test: a case whose expectation is wrong fails.
    let cases = dir.join("cases");
    std::fs::create_dir_all(&cases).unwrap();
    std::fs::write(cases.join("wrong.json"), r#"{"model":"m"}"#).unwrap();
    std::fs::write(cases.join("wrong.expect.toml"), "decision = \"refuse\"\n").unwrap();
    let cases = cases.display().to_string();
    assert_eq!(
        status(&["policy-test", "--config", &keyed, &cases]),
        Some(3)
    );
    // The provider key is not set.
    assert_eq!(status(&["doctor", "--config", &keyed]), Some(3));
}

/// The commands that need no argument, on a fresh home.
#[test]
fn the_defaults_work_on_a_fresh_home() {
    let setup = Setup::new();
    std::fs::remove_dir_all(setup.home.join(".wye")).unwrap();
    // keygen's default path is in a home nothing has made yet.
    let keygen = setup.run(&["token", "keygen"]);
    assert!(keygen.status.success(), "{}", err(&keygen));
    std::fs::remove_file(setup.home.join(".wye/issuer.key")).unwrap();

    // `init` is for you on this machine, and needs no key.
    let init = setup.run(&["init"]);
    assert!(init.status.success(), "{}", err(&init));
    let config = std::fs::read_to_string(setup.home.join(".wye/wye.toml")).unwrap();
    assert!(
        config.contains("forward_caller_credential = true"),
        "{config}"
    );
    assert!(setup.run(&["validate"]).status.success());
    // `service status` when not installed is a state a script can test.
    assert_eq!(setup.run(&["service", "status"]).status.code(), Some(3));
    // The next steps' `wye report` has nothing yet, and says how to get it.
    let report = err(&setup.run(&["report"]));
    assert!(report.contains("wye service install"), "{report}");

    // `init --team` writes where a team reviews it, and keeps the key out.
    std::fs::remove_file(setup.repo.join("wye.toml")).unwrap();
    assert!(setup.run(&["init", "--team"]).status.success());
    assert!(setup.repo.join("wye.toml").exists());
    assert!(setup.home.join(".wye/issuer.key").exists());
    let issued = setup.run(&["token", "issue", "alice", "-c", "wye.toml"]);
    assert!(out(&issued).starts_with("eyJ"), "{}", err(&issued));
    let written = setup.run(&[
        "token",
        "issue",
        "bob",
        "-c",
        "wye.toml",
        "--out",
        "bob.token",
    ]);
    assert!(written.status.success(), "{}", err(&written));
    assert!(
        out(&written).is_empty(),
        "a token file, not a token on screen"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(setup.repo.join("bob.token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
    // A bad signing key is not the config's fault.
    let no_key = err(&setup.run(&["token", "issue", "a", "-c", "wye.toml", "--key", "nokey"]));
    assert!(no_key.starts_with("wye: cannot read nokey"), "{no_key}");

    // `policy-test` looks next to the config.
    let cases = setup.repo.join("policy-tests");
    std::fs::create_dir_all(&cases).unwrap();
    std::fs::write(cases.join("a.json"), r#"{"model":"m","max_tokens":1}"#).unwrap();
    std::fs::write(cases.join("a.expect.toml"), "decision = \"refuse\"\n").unwrap();
    let tested = setup.run(&["policy-test", "-c", "wye.toml"]);
    assert!(tested.status.success(), "{}", err(&tested));
    assert!(out(&tested).contains("1 passed"), "{}", out(&tested));
}

#[test]
fn a_reader_going_away_is_not_a_failure() {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let status = wye()
        .arg("--help")
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(status.status.code(), Some(0), "{}", err(&status));
}

#[test]
fn help_answers_the_usual_ways_of_asking() {
    let setup = Setup::new();
    for args in [
        &["help", "doctor"][..],
        &["--help", "doctor"][..],
        &["doctor", "--help"][..],
    ] {
        assert!(
            out(&setup.run(args)).starts_with("Check a config's keys"),
            "{args:?}"
        );
    }
    for args in [
        &["token", "help", "issue"][..],
        &["help", "token", "issue"][..],
    ] {
        assert!(
            out(&setup.run(args)).starts_with("Issue a gateway token"),
            "{args:?}"
        );
    }
    assert!(out(&setup.run(&["version"])).starts_with("wye "));
    // Color is for terminals: a pipe, as here, stays plain.
    assert!(!out(&setup.run(&["--help"])).contains('\x1b'));
    let leading = err(&setup.run(&["-c", "x", "doctor"]));
    assert!(
        leading.contains("options go after the command"),
        "{leading}"
    );
}
