use super::*;
use crate::cli::text::WIDTH;

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
    assert_eq!(usage("init", &["--shared=yes"]), "--shared takes no value");
    // A repeated option would leave one of its values unused.
    assert_eq!(
        usage("token issue", &["a", "--models", "x", "--models=y"]),
        "--models given more than once"
    );
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
    assert_eq!(
        unknown_command("-c"),
        "options go after the command: `wye <command> -c`"
    );
}

#[test]
fn a_missing_argument_is_one_line_that_says_what_to_run() {
    assert_eq!(
        command("token issue").parse(&[]),
        Err(ArgsError::Missing(
            "missing <subject>: who the token is for, as in \
             `wye token issue alice --out alice.token`"
                .into()
        ))
    );
    let Err(ArgsError::Missing(diff)) = command("policy-diff").parse(&["old.toml"]) else {
        panic!("policy-diff takes two configs");
    };
    assert!(
        diff.starts_with("missing <old> <new>: ") && diff.contains("`wye policy-diff "),
        "{diff}"
    );
    // Every command that requires an argument says what it is.
    for command in COMMANDS.iter().filter(|c| c.arity.0 > 0) {
        assert!(command.missing.contains("`wye "), "{}", command.name);
    }
}

#[test]
fn every_command_has_its_own_id() {
    // BASE's placeholder id would otherwise hide a command that set none.
    for (i, a) in COMMANDS.iter().enumerate() {
        for b in COMMANDS.iter().skip(i + 1) {
            assert_ne!(a.id, b.id, "{} and {}", a.name, b.name);
        }
    }
}
