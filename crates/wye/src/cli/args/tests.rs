#![allow(clippy::unwrap_used)]

use super::*;

fn command(name: &str) -> &'static Command {
    COMMANDS.iter().find(|c| c.name == name).unwrap()
}

#[test]
fn options_parse_the_gnu_way() {
    let explain = command("explain");
    let parsed = explain
        .parse(&[
            "req.json",
            "--config=a.toml",
            "--target-model",
            "m1",
            "--target-model=m2",
            "--session",
            "s",
        ])
        .unwrap();
    assert_eq!(parsed.value("config"), Some("a.toml"));
    assert_eq!(parsed.values("target-model"), ["m1", "m2"]);
    assert_eq!(parsed.value("session"), Some("s"));
    assert_eq!(parsed.positional, ["req.json"]);
    let short = explain.parse(&["-c", "a.toml", "-"]).unwrap();
    assert_eq!(short.value("config"), Some("a.toml"));
    assert_eq!(short.positional, ["-"], "`-` is standard input");
    assert_eq!(
        explain.parse(&["-ca.toml", "r"]).unwrap().value("config"),
        Some("a.toml")
    );
    assert_eq!(
        explain
            .parse(&["--", "--weird-name.json"])
            .unwrap()
            .positional,
        ["--weird-name.json"]
    );
}

#[test]
fn mistakes_are_named() {
    let usage = |name: &str, args: &[&str]| match command(name).parse(args) {
        Err(ArgsError::Usage(why)) => why,
        other => panic!("{other:?}"),
    };
    assert_eq!(usage("serve", &["--confg", "x"]), "unknown option --confg");
    assert_eq!(usage("serve", &["-c"]), "--config needs a <path>");
    assert_eq!(usage("serve", &["extra"]), "unexpected argument extra");
    assert_eq!(usage("init", &["--auth=yes"]), "--auth takes no value");
    // A repeated option would leave one of its values unused.
    assert_eq!(
        usage("token issue", &["a", "--models", "x", "--models=y"]),
        "--models given more than once"
    );
    assert_eq!(usage("policy-diff", &["old.toml"]), "missing <old> <new>");
    assert_eq!(
        command("serve").parse(&["-c", "x", "--help"]),
        Err(ArgsError::Help)
    );
}

#[test]
fn wrappers_pass_what_they_do_not_know_to_the_agent() {
    let claude = command("claude");
    let parsed = claude
        .parse(&["--url", "https://gw", "-p", "fix", "--url", "x"])
        .unwrap();
    assert_eq!(parsed.value("url"), Some("https://gw"));
    assert_eq!(parsed.positional, ["-p", "fix", "--url", "x"]);
    let parsed = claude
        .parse(&["--", "--dangerously-skip-permissions", "--resume", "id"])
        .unwrap();
    assert_eq!(
        parsed.positional,
        ["--dangerously-skip-permissions", "--resume", "id"]
    );
    assert_eq!(claude.parse(&["resume"]).unwrap().positional, ["resume"]);
}

#[test]
fn every_command_is_listed_once_and_help_fits_80_columns() {
    let listed: Vec<&str> = GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    for command in COMMANDS {
        let top = command.name.split(' ').next().unwrap();
        assert_eq!(
            listed.iter().filter(|n| **n == top).count(),
            1,
            "{}",
            command.name
        );
        assert_eq!(
            Command::find(&command.name.split(' ').collect::<Vec<_>>())
                .unwrap()
                .0
                .name,
            command.name
        );
        for line in command.help().lines() {
            assert!(line.len() <= WIDTH, "{}: {line}", command.name);
        }
    }
    let mut pages = vec![overview()];
    pages.extend(FAMILIES.iter().map(|(f, _)| family_help(f).unwrap()));
    for page in pages {
        for line in page.lines() {
            assert!(line.len() <= WIDTH, "{line}");
        }
    }
    assert!(!summary("token").is_empty());
}

#[test]
fn help_shows_options_with_their_values() {
    let help = command("serve").help();
    assert!(help.starts_with("Run the gateway in the foreground.\n\nUsage: wye serve [options]\n"));
    assert!(
        help.contains("  -c, --config <path>       The config (default: $WYE_CONFIG, else\n"),
        "{help}"
    );
    assert!(
        help.contains("  -h, --help                Show this help\n"),
        "{help}"
    );
}

#[test]
fn a_typo_gets_the_closest_command() {
    assert_eq!(
        unknown_command("srve"),
        "unknown command `srve`; did you mean `serve`?"
    );
    assert_eq!(
        unknown_command("frobnicate"),
        "unknown command `frobnicate`; see `wye --help`"
    );
}

#[test]
fn the_config_is_found_in_order_and_never_in_the_working_directory() {
    let env = |vars: &'static [(&str, &str)]| {
        move |name: &str| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    let none = |_: &Path| false;
    let named = env(&[("HOME", "/u"), ("WYE_CONFIG", "/etc/wye.toml")]);
    assert_eq!(
        find_config(Some("x.toml"), &named, none).unwrap(),
        PathBuf::from("x.toml")
    );
    assert_eq!(
        find_config(None, &named, none).unwrap(),
        PathBuf::from("/etc/wye.toml")
    );
    let user = env(&[("HOME", "/u")]);
    // Both exist: the user's own, not the one in the working directory.
    assert_eq!(
        find_config(None, &user, |_| true).unwrap(),
        PathBuf::from("/u/.wye/wye.toml")
    );
    // Only the working directory's: not read, and the error says how.
    let here = find_config(None, &user, |p| p == Path::new("wye.toml")).unwrap_err();
    assert_eq!(
        here,
        "no config at ~/.wye/wye.toml, and ./wye.toml is read only when named: -c wye.toml"
    );
    assert_eq!(
        find_config(None, &user, none).unwrap_err(),
        "no config at ~/.wye/wye.toml: write one with `wye init`, or pass -c <path>"
    );
    let moved = env(&[("HOME", "/u"), ("WYE_HOME", "/data/wye")]);
    assert_eq!(home_of(&moved), "/data/wye");
    assert_eq!(
        find_config(None, &moved, |_| true).unwrap(),
        PathBuf::from("/data/wye/wye.toml")
    );
    assert_eq!(
        tilde(Path::new("/u/.wye/usage.jsonl"), &moved),
        "~/.wye/usage.jsonl"
    );
    assert_eq!(tilde(Path::new("/elsewhere"), &moved), "/elsewhere");
}

fn home_of(env: &impl Fn(&str) -> Option<String>) -> String {
    home(env).unwrap().display().to_string()
}

#[test]
fn durations_read_as_written() {
    for (text, secs) in [
        ("45s", 45),
        ("30m", 1800),
        ("8h", 28_800),
        ("7d", 604_800),
        ("90", 90),
    ] {
        assert_eq!(duration_secs(text), Some(secs), "{text}");
        assert_eq!(duration_secs(&show_duration(secs)), Some(secs));
    }
    for bad in ["", "d", "7w", "-1d", "0", "1.5h"] {
        assert_eq!(duration_secs(bad), None, "{bad}");
    }
}
