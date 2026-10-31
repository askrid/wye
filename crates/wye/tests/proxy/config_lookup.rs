//! Which config the commands read: the binary, run from a directory holding
//! an API-key `wye.toml`, for a user whose own config forwards a subscription
//! login, with no key in the environment.

use super::cli::{Running, Setup, err, out};

#[test]
fn a_config_in_the_working_directory_applies_only_when_named() {
    let setup = Setup::with_subscription();

    // Every command reads the user's own, which asks for no key.
    let line = Running::spawn(setup.wye(&["serve"])).stderr_line();
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
        let line = Running::spawn(named).stderr_line();
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
    let setup = Setup::with_subscription();
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
    for written in ["Library", ".config/systemd/user"] {
        assert!(!setup.home.join(written).exists(), "{written}");
    }
}

#[test]
fn without_a_config_of_your_own_the_error_says_what_to_do() {
    let setup = Setup::new();
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
