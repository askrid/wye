//! The command line: every command's options in one table, parsed the GNU
//! way (options in any order, `--name=value`, `-c value`, `--` ends the
//! options) and rendered as help from the same table, so help and parsing
//! cannot disagree.

use std::path::{Path, PathBuf};

/// One option of a command.
#[derive(Debug)]
pub struct Opt {
    pub long: &'static str,
    pub short: Option<char>,
    /// The value's name in help (`path`); `None` for a flag.
    pub value: Option<&'static str>,
    pub help: &'static str,
    /// May be given more than once; every value is kept.
    pub repeats: bool,
}

impl Opt {
    const fn flag(long: &'static str, help: &'static str) -> Self {
        Self {
            long,
            short: None,
            value: None,
            help,
            repeats: false,
        }
    }

    const fn value(long: &'static str, value: &'static str, help: &'static str) -> Self {
        Self {
            long,
            short: None,
            value: Some(value),
            help,
            repeats: false,
        }
    }
}

/// `-c, --config <path>`, on every command that reads a config.
const CONFIG: Opt = Opt {
    long: "config",
    short: Some('c'),
    value: Some("path"),
    help: "The config (default: $WYE_CONFIG, ./wye.toml, ~/.wye/wye.toml)",
    repeats: false,
};

/// One command: what it does and what it takes.
#[derive(Debug)]
pub struct Command {
    /// As typed after `wye`: `serve`, `token issue`.
    pub name: &'static str,
    pub summary: &'static str,
    /// Positional arguments as help shows them: `<dir>`, `[<log>]`.
    pub args: &'static str,
    /// Fewest and most positional arguments.
    pub arity: (usize, usize),
    pub options: &'static [Opt],
    /// The first argument the command does not know, and everything after
    /// it, goes to the program it starts (`wye claude -p "fix the tests"`).
    pub passthrough: bool,
    /// Help text after the options.
    pub notes: &'static str,
}

/// The commands, in the order help lists them.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "serve",
        summary: "Run the gateway in the foreground",
        args: "",
        arity: (0, 0),
        options: &[CONFIG],
        passthrough: false,
        notes: "Usage records go to stderr, one JSON line per request.",
    },
    Command {
        name: "service install",
        summary: "Run the gateway in the background, now and at every login",
        args: "",
        arity: (0, 0),
        options: &[
            CONFIG,
            Opt {
                short: Some('y'),
                ..Opt::flag("yes", "Install without asking")
            },
            Opt::flag("dry-run", "Show the plan and the file; change nothing"),
        ],
        passthrough: false,
        notes: "On macOS a LaunchAgent (launchd), on Linux a systemd user unit: both run \
                as you, without sudo. Shows what it writes and runs, and asks first.",
    },
    Command {
        name: "service status",
        summary: "Show whether the background gateway is installed and answering",
        args: "",
        arity: (0, 0),
        options: &[],
        passthrough: false,
        notes: "",
    },
    Command {
        name: "service restart",
        summary: "Restart the background gateway, after checking its config",
        args: "",
        arity: (0, 0),
        options: &[],
        passthrough: false,
        notes: "Run it after editing the config or installing a new wye.",
    },
    Command {
        name: "service uninstall",
        summary: "Stop the background gateway and remove it",
        args: "",
        arity: (0, 0),
        options: &[],
        passthrough: false,
        notes: "The config and the usage log stay.",
    },
    Command {
        name: "validate",
        summary: "Check a config without starting the gateway",
        args: "",
        arity: (0, 0),
        options: &[CONFIG],
        passthrough: false,
        notes: "Reads the identity providers' key sets too; secrets in the environment are \
                left to `wye doctor`.",
    },
    Command {
        name: "doctor",
        summary: "Check a config's keys, files, and endpoints",
        args: "",
        arity: (0, 0),
        options: &[CONFIG],
        passthrough: false,
        notes: "",
    },
    Command {
        name: "claude",
        summary: "Start Claude Code through the gateway",
        args: "[<claude args>...]",
        arity: (0, usize::MAX),
        options: &WRAPPER,
        passthrough: true,
        notes: WRAPPER_NOTES,
    },
    Command {
        name: "codex",
        summary: "Start Codex through the gateway",
        args: "[<codex args>...]",
        arity: (0, usize::MAX),
        options: &WRAPPER,
        passthrough: true,
        notes: WRAPPER_NOTES,
    },
    Command {
        name: "report",
        summary: "Summarize a usage log",
        args: "[<log>]",
        arity: (0, 1),
        options: &[],
        passthrough: false,
        notes: "Reads ~/.wye/usage.jsonl, the log `wye service install` keeps, unless \
                given a file; `-` reads standard input.",
    },
    Command {
        name: "init",
        summary: "Write a starter config",
        args: "",
        arity: (0, 0),
        options: &[
            Opt::flag(
                "subscription",
                "Forward Claude Code's own claude.ai login instead of an API key",
            ),
            Opt::flag(
                "auth",
                "For a team: gateway tokens, a signing key, listen on all interfaces",
            ),
            Opt::flag("openai", "Also serve the OpenAI APIs (for Codex)"),
            Opt::value(
                "out",
                "path",
                "Where to write it (default: ~/.wye/wye.toml)",
            ),
        ],
        passthrough: false,
        notes: "Never overwrites a file.",
    },
    Command {
        name: "token keygen",
        summary: "Make a signing key for gateway tokens",
        args: "",
        arity: (0, 0),
        options: &[Opt::value(
            "out",
            "path",
            "Where to write it (default: ~/.wye/issuer.key)",
        )],
        passthrough: false,
        notes: "Prints the public half for [[auth.keys]]. `wye init --auth` makes the first \
                key; this one is for rotation.",
    },
    Command {
        name: "token issue",
        summary: "Issue a gateway token",
        args: "<subject>",
        arity: (1, 1),
        options: &[
            CONFIG,
            Opt::value("key", "path", "Signing key (default: ~/.wye/issuer.key)"),
            Opt::value(
                "ttl",
                "duration",
                "Lifetime: 30m, 8h, 7d (default: the config's longest)",
            ),
            Opt::value(
                "models",
                "list",
                "Models it may use: names, or prefixes ending in *",
            ),
            Opt::value("max-output-tokens", "n", "A lower output-token ceiling"),
            Opt::value("issuer", "iss", "Without a config: the issuer"),
            Opt::value("audience", "aud", "Without a config: the audience"),
            Opt::value("kid", "id", "Without a config: the key's id"),
        ],
        passthrough: false,
        notes: "The issuer, audience, and key id come from the config's [auth], the key id \
                found by the signing key's public half.",
    },
    Command {
        name: "explain",
        summary: "Show what the gateway would do with one request",
        args: "<request.json>",
        arity: (1, 1),
        options: &[
            CONFIG,
            Opt::value("token-file", "path", "The caller's gateway token"),
            Opt::value("session", "id", "The Claude Code session id"),
            Opt::value("path", "path", "The endpoint (default: /v1/messages)"),
            Opt {
                repeats: true,
                ..Opt::value(
                    "target-model",
                    "model",
                    "Check against this model's breaking changes too",
                )
            },
        ],
        passthrough: false,
        notes: "Offline: calls what the server calls. `-` reads the request from standard \
                input.",
    },
    Command {
        name: "policy-test",
        summary: "Check sample requests against expected decisions",
        args: "<dir>",
        arity: (1, 1),
        options: &[CONFIG],
        passthrough: false,
        notes: "",
    },
    Command {
        name: "policy-diff",
        summary: "Show what a config change widens or narrows",
        args: "<old> <new>",
        arity: (2, 2),
        options: &[],
        passthrough: false,
        notes: "",
    },
    Command {
        name: "probe",
        summary: "Check that a URL answers 2xx, for container health checks",
        args: "<url>",
        arity: (1, 1),
        options: &[],
        passthrough: false,
        notes: "",
    },
];

const WRAPPER: [Opt; 2] = [
    Opt::value(
        "url",
        "url",
        "The gateway (default: $WYE_URL, else http://127.0.0.1:8080)",
    ),
    Opt::value(
        "token-file",
        "path",
        "A file holding a gateway token (default: $WYE_TOKEN)",
    ),
];

const WRAPPER_NOTES: &str = "Without a token the agent keeps its own login. Arguments \
    after `--`, or from the first one wye does not know, go to the agent unchanged. A \
    gateway beyond this machine must be https.";

/// Commands with subcommands, as the command list shows them.
const FAMILIES: &[(&str, &str)] = &[
    (
        "service",
        "Run the gateway in the background (launchd or systemd, as you)",
    ),
    ("token", "Make signing keys and gateway tokens"),
];

/// The command list, grouped.
const GROUPS: &[(&str, &[&str])] = &[
    ("Run", &["serve", "service"]),
    ("Use", &["claude", "codex", "report"]),
    ("Set up", &["init", "token"]),
    (
        "Check",
        &[
            "validate",
            "doctor",
            "explain",
            "policy-test",
            "policy-diff",
            "probe",
        ],
    ),
];

/// Help is wrapped to this width, which fits a split terminal.
pub const WIDTH: usize = 80;
/// Where option descriptions start.
const DESCRIPTION_COLUMN: usize = 28;

/// What a command was given.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed<'a> {
    values: Vec<(&'static str, &'a str)>,
    pub positional: Vec<&'a str>,
}

impl<'a> Parsed<'a> {
    /// The value of `--long`, if given.
    pub fn value(&self, long: &str) -> Option<&'a str> {
        self.values
            .iter()
            .find(|(l, _)| *l == long)
            .map(|(_, v)| *v)
    }

    /// Every value of a repeatable `--long`, in order.
    pub fn values(&self, long: &str) -> Vec<&'a str> {
        self.values
            .iter()
            .filter(|(l, _)| *l == long)
            .map(|(_, v)| *v)
            .collect()
    }

    /// Whether the flag `--long` was given.
    pub fn flag(&self, long: &str) -> bool {
        self.values.iter().any(|(l, _)| *l == long)
    }
}

/// Why a command did not run.
#[derive(Debug, PartialEq, Eq)]
pub enum ArgsError {
    /// `-h` or `--help`: print the command's help.
    Help,
    /// The arguments do not fit; the message names what is wrong.
    Usage(String),
}

impl Command {
    /// The command named by the start of `args` (`token issue ...`), and the
    /// arguments after its name.
    pub fn find<'a, 'b>(args: &'b [&'a str]) -> Option<(&'static Self, &'b [&'a str])> {
        let first = *args.first()?;
        let rest = args.get(1..).unwrap_or_default();
        if let Some(command) = COMMANDS.iter().find(|c| c.name == first) {
            return Some((command, rest));
        }
        let sub = *rest.first()?;
        let command = COMMANDS
            .iter()
            .find(|c| c.name.split_once(' ') == Some((first, sub)))?;
        Some((command, rest.get(1..).unwrap_or_default()))
    }

    /// Parses `args`, the arguments after the command's name.
    pub fn parse<'a>(&self, args: &[&'a str]) -> Result<Parsed<'a>, ArgsError> {
        let mut parsed = Parsed::default();
        let mut i = 0;
        while let Some(&arg) = args.get(i) {
            i += 1;
            if arg == "--" {
                parsed.positional.extend(args.get(i..).unwrap_or_default());
                break;
            }
            if arg == "-h" || arg == "--help" {
                return Err(ArgsError::Help);
            }
            let (opt, inline) = if let Some(long) = arg.strip_prefix("--") {
                let (name, inline) = long
                    .split_once('=')
                    .map_or((long, None), |(n, v)| (n, Some(v)));
                (self.options.iter().find(|o| o.long == name), inline)
            } else if let Some(short) = arg.strip_prefix('-').filter(|s| !s.is_empty()) {
                let mut chars = short.chars();
                let letter = chars.next();
                let rest = chars.as_str();
                (
                    self.options.iter().find(|o| o.short == letter),
                    (!rest.is_empty()).then_some(rest),
                )
            } else {
                if self.passthrough {
                    parsed
                        .positional
                        .extend(args.get(i - 1..).unwrap_or_default());
                    break;
                }
                parsed.positional.push(arg);
                continue;
            };
            let Some(opt) = opt else {
                if self.passthrough {
                    parsed
                        .positional
                        .extend(args.get(i - 1..).unwrap_or_default());
                    break;
                }
                return Err(ArgsError::Usage(format!("unknown option {arg}")));
            };
            let value = match (opt.value, inline) {
                (None, None) => "",
                (None, Some(_)) => {
                    return Err(ArgsError::Usage(format!("--{} takes no value", opt.long)));
                }
                (Some(_), Some(value)) => value,
                (Some(name), None) => {
                    let value = args.get(i).ok_or_else(|| {
                        ArgsError::Usage(format!("--{} needs a <{name}>", opt.long))
                    })?;
                    i += 1;
                    value
                }
            };
            // A repeated option would leave one of its values unused: a
            // token issued with the first, perhaps broader, `--models`.
            if !opt.repeats && parsed.flag(opt.long) {
                return Err(ArgsError::Usage(format!(
                    "--{} given more than once",
                    opt.long
                )));
            }
            parsed.values.push((opt.long, value));
        }
        let (fewest, most) = self.arity;
        let n = parsed.positional.len();
        if n < fewest {
            return Err(ArgsError::Usage(format!("missing {}", self.args)));
        }
        if n > most {
            let extra = parsed.positional.get(most).copied().unwrap_or_default();
            return Err(ArgsError::Usage(format!("unexpected argument {extra}")));
        }
        Ok(parsed)
    }

    /// `wye serve [options]`.
    pub fn usage(&self) -> String {
        let mut usage = format!("wye {}", self.name);
        if !self.options.is_empty() {
            usage.push_str(" [options]");
        }
        if !self.args.is_empty() {
            usage.push(' ');
            usage.push_str(self.args);
        }
        usage
    }

    /// The command's help: what it does, its usage, options, and notes.
    pub fn help(&self) -> String {
        let mut out = format!("{}.\n\nUsage: {}\n", self.summary, self.usage());
        out.push_str("\nOptions:\n");
        for opt in self.options {
            let mut left = match opt.short {
                Some(short) => format!("  -{short}, --{}", opt.long),
                None => format!("      --{}", opt.long),
            };
            if let Some(value) = opt.value {
                left.push_str(&format!(" <{value}>"));
            }
            out.push_str(&row(&left, opt.help));
        }
        out.push_str(&row("  -h, --help", "Show this help"));
        if !self.notes.is_empty() {
            out.push('\n');
            out.push_str(&wrap(self.notes, 0, WIDTH));
        }
        out
    }
}

/// `left`, then `text` from the description column, wrapped under itself.
fn row(left: &str, text: &str) -> String {
    let mut out = left.to_owned();
    if left.len() + 2 > DESCRIPTION_COLUMN {
        out.push('\n');
        out.push_str(&" ".repeat(DESCRIPTION_COLUMN));
    } else {
        out.push_str(&" ".repeat(DESCRIPTION_COLUMN - left.len()));
    }
    out.push_str(wrap(text, DESCRIPTION_COLUMN, WIDTH).trim_start());
    out
}

/// `text` wrapped at word boundaries to `width`, every line indented by
/// `indent`; ends with a newline.
pub fn wrap(text: &str, indent: usize, width: usize) -> String {
    let mut out = String::new();
    let mut line = 0;
    for word in text.split_whitespace() {
        if line > indent && line + 1 + word.len() > width {
            out.push('\n');
            line = 0;
        }
        if line == 0 {
            out.push_str(&" ".repeat(indent));
            line = indent;
        } else {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += word.len();
    }
    out.push('\n');
    out
}

/// `wye --help`: the commands, grouped.
pub fn overview() -> String {
    let mut out = format!(
        "wye {}: a stateless gateway between agents or apps and model providers.\n\n\
         Usage: wye <command> [options]\n",
        env!("CARGO_PKG_VERSION")
    );
    for (group, names) in GROUPS {
        out.push_str(&format!("\n{group}:\n"));
        for name in *names {
            out.push_str(&format!("  {name:<13}{}\n", summary(name)));
        }
    }
    out.push('\n');
    out.push_str(&wrap(
        "`wye <command> --help` shows a command's options. Your files live in ~/.wye \
         ($WYE_HOME). Exit status 3: a check found a problem, a request would be refused, \
         a policy test failed, or a config change widens access.",
        0,
        WIDTH,
    ));
    out
}

/// `wye token --help`: a family's subcommands.
pub fn family_help(family: &str) -> Option<String> {
    let summary = FAMILIES.iter().find(|(name, _)| *name == family)?.1;
    let mut out = format!("{summary}.\n\nUsage: wye {family} <command> [options]\n\nCommands:\n");
    for command in COMMANDS {
        if let Some((_, sub)) = command.name.split_once(' ').filter(|(f, _)| *f == family) {
            out.push_str(&format!("  {sub:<13}{}\n", command.summary));
        }
    }
    out.push_str(&format!(
        "\n`wye {family} <command> --help` shows a command's options.\n"
    ));
    Some(out)
}

/// The one-line summary help gives `name`, a command or a family.
fn summary(name: &str) -> &'static str {
    COMMANDS
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.summary)
        .or_else(|| FAMILIES.iter().find(|(f, _)| *f == name).map(|(_, s)| *s))
        .unwrap_or_default()
}

/// The error for an unknown command, with the closest known one.
pub fn unknown_command(typed: &str) -> String {
    let names = GROUPS.iter().flat_map(|(_, names)| names.iter());
    let closest = names
        .map(|name| (distance(typed, name), *name))
        .min()
        .filter(|(d, _)| *d <= 2);
    match closest {
        Some((_, name)) => format!("unknown command `{typed}`; did you mean `{name}`?"),
        None => format!("unknown command `{typed}`; see `wye --help`"),
    }
}

/// Edit distance between two short words.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row.first().copied().unwrap_or_default();
        if let Some(first) = row.first_mut() {
            *first = i + 1;
        }
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous + usize::from(ca != *cb);
            let deletion = row.get(j + 1).copied().unwrap_or_default() + 1;
            let insertion = row.get(j).copied().unwrap_or_default() + 1;
            previous = row.get(j + 1).copied().unwrap_or_default();
            if let Some(cell) = row.get_mut(j + 1) {
                *cell = substitution.min(deletion).min(insertion);
            }
        }
    }
    row.last().copied().unwrap_or_default()
}

/// Where a user's files live: `$WYE_HOME`, else `~/.wye`.
pub fn home(env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let set = |name| env(name).filter(|v| !v.is_empty());
    set("WYE_HOME")
        .map(PathBuf::from)
        .or_else(|| set("HOME").map(|home| Path::new(&home).join(".wye")))
}

/// The config a command reads: `--config`, else `$WYE_CONFIG`, else
/// `./wye.toml`, else `wye.toml` in the home directory.
pub fn find_config(
    flag: Option<&str>,
    env: &impl Fn(&str) -> Option<String>,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    if let Some(path) = flag
        .map(str::to_owned)
        .or_else(|| env("WYE_CONFIG").filter(|v| !v.is_empty()))
    {
        return Ok(path.into());
    }
    let mut candidates = vec![PathBuf::from("wye.toml")];
    candidates.extend(home(env).map(|home| home.join("wye.toml")));
    candidates.into_iter().find(|p| exists(p)).ok_or_else(|| {
        "no config: none in ./wye.toml or ~/.wye/wye.toml. Pass -c <path>, set WYE_CONFIG, \
         or write one with `wye init`"
            .to_owned()
    })
}

/// `path` with the home directory shown as `~`.
pub fn tilde(path: &Path, env: &impl Fn(&str) -> Option<String>) -> String {
    match env("HOME").filter(|h| !h.is_empty()) {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

/// A duration as people write it: `45s`, `30m`, `8h`, `7d`, or seconds.
pub fn duration_secs(text: &str) -> Option<u64> {
    let (digits, unit) = match text.char_indices().last()? {
        (i, c) if c.is_ascii_alphabetic() => (text.get(..i)?, c),
        _ => (text, 's'),
    };
    let n: u64 = digits.parse().ok()?;
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return None,
    };
    n.checked_mul(scale).filter(|&secs| secs > 0)
}

/// Seconds as `duration_secs` reads them back: `7d`, `8h`, `90s`.
pub fn show_duration(secs: u64) -> String {
    match secs {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests;
