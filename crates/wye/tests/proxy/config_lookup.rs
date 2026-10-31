//! Which config the commands read: the binary, run from a directory holding
//! an API-key `wye.toml`, for a user whose own config forwards a subscription
//! login, with no key in the environment.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

/// A directory with a user's home and a repository in it.
struct Setup {
    home: PathBuf,
    repo: PathBuf,
}

impl Setup {
    fn new(name: &str) -> Self {
        let home = std::env::temp_dir().join(format!("wye-{name}-{}", std::process::id()));
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
    fn with_subscription(name: &str) -> Self {
        let setup = Self::new(name);
        std::fs::write(
            setup.home.join(".wye/wye.toml"),
            "[server]\nlisten = \"127.0.0.1:0\"\n\
             [upstream]\nbase_url = \"http://127.0.0.1:1\"\nforward_caller_credential = true\n",
        )
        .unwrap();
        setup
    }

    /// `wye`, run from the repository.
    fn wye(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wye"));
        command
            .args(args)
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env_remove("WYE_HOME")
            .env_remove("WYE_CONFIG")
            .env_remove("WYE_TEST_UNSET");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.wye(args).output().unwrap()
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The first line `command` writes to stderr, which ends it.
fn first_stderr_line(mut command: Command) -> String {
    let mut child = command.stderr(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    line
}

#[test]
fn a_config_in_the_working_directory_applies_only_when_named() {
    let setup = Setup::with_subscription("lookup");

    // Every command reads the user's own, which asks for no key.
    let line = first_stderr_line(setup.wye(&["serve"]));
    assert!(line.starts_with("wye: listening on"), "{line}");
    assert!(line.contains(".wye/wye.toml)"), "{line}");
    let validate = setup.run(&["validate"]);
    assert!(validate.status.success(), "{}", err(&validate));
    assert!(
        out(&validate).starts_with("~/.wye/wye.toml: "),
        "{}",
        out(&validate)
    );
    let doctor = out(&setup.run(&["doctor"]));
    assert!(doctor.starts_with("config: ~/.wye/wye.toml\n"), "{doctor}");
    assert!(!doctor.contains("WYE_TEST_UNSET"), "{doctor}");
    assert!(
        doctor.contains("forwards the caller's own credential"),
        "{doctor}"
    );
    let request = setup.repo.join("request.json");
    std::fs::write(&request, r#"{"model":"m","max_tokens":1}"#).unwrap();
    let explain = out(&setup.run(&["explain", "request.json"]));
    assert!(
        explain.starts_with("config: ~/.wye/wye.toml\n"),
        "{explain}"
    );
    assert!(!explain.contains("WYE_TEST_UNSET"), "{explain}");

    // Named, it applies, and what is missing says which config asked.
    for named in [setup.wye(&["serve", "-c", "wye.toml"]), {
        let mut by_env = setup.wye(&["serve"]);
        by_env.env("WYE_CONFIG", "wye.toml");
        by_env
    }] {
        let line = first_stderr_line(named);
        assert_eq!(
            line,
            "wye: wye.toml: environment variable WYE_TEST_UNSET is not set or not valid UTF-8\n"
        );
    }
    let doctor = setup.run(&["doctor", "-c", "wye.toml"]);
    assert_eq!(doctor.status.code(), Some(3));
    assert!(
        out(&doctor).starts_with("config: wye.toml\n"),
        "{}",
        out(&doctor)
    );
    assert!(out(&doctor).contains("WYE_TEST_UNSET"), "{}", out(&doctor));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_service_installs_the_users_own_config() {
    let setup = Setup::with_subscription("service");
    let plan = setup.run(&["service", "install", "--dry-run"]);
    assert!(plan.status.success(), "{}", err(&plan));
    assert!(
        out(&plan).contains(" serve --config ~/.wye/wye.toml\n"),
        "{}",
        out(&plan)
    );
    let named = setup.run(&["service", "install", "--dry-run", "-c", "wye.toml"]);
    assert_eq!(named.status.code(), Some(1));
    assert!(err(&named).contains("$WYE_TEST_UNSET"), "{}", err(&named));
    assert!(!Path::new(&setup.home).join("Library").exists());
}

#[test]
fn without_a_config_of_your_own_the_error_says_what_to_do() {
    let setup = Setup::new("none");
    let failed = setup.run(&["validate"]);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        err(&failed),
        "wye: no config at ~/.wye/wye.toml, and ./wye.toml is read only when named: -c wye.toml\n"
    );
    std::fs::remove_file(setup.repo.join("wye.toml")).unwrap();
    assert_eq!(
        err(&setup.run(&["serve"])),
        "wye: no config at ~/.wye/wye.toml: write one with `wye init`, or pass -c <path>\n"
    );
}

/// The commands that need no argument, on a fresh home.
#[test]
fn the_defaults_work_on_a_fresh_home() {
    let setup = Setup::new("fresh");
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
    let tested = out(&setup.run(&["policy-test", "-c", "wye.toml"]));
    assert!(tested.contains("1 passed"), "{tested}");
}

#[test]
fn a_reader_going_away_is_not_a_failure() {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let status = Command::new(env!("CARGO_BIN_EXE_wye"))
        .arg("--help")
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(status.status.code(), Some(0), "{}", err(&status));
}

#[test]
fn help_answers_the_usual_ways_of_asking() {
    let setup = Setup::new("help");
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
