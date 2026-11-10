//! The command line: every command's options in one table, parsed the GNU
//! way (options in any order, `--name=value`, `-c value`, `--` ends the
//! options) and rendered as help from the same table, so help and parsing
//! cannot disagree.

mod commands;

use super::text::wrap;
pub use commands::COMMANDS;
use commands::{FAMILIES, GROUPS};

/// One option of a command.
#[derive(Debug)]
pub struct Opt {
    /// The name after `--`.
    pub long: &'static str,
    /// The letter after `-`, if it has one.
    pub short: Option<char>,
    /// The value's name in help (`path`); `None` for a flag.
    pub value: Option<&'static str>,
    /// What it does, in help.
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

/// Every variable a user sets, each beaten by its flag; docs/operations.md
/// has the full table, with those set only by tests and agents.
const ENVIRONMENT: &str = "Environment:
  WYE_CONFIG   The config, when -c is not given
  WYE_HOME     Your files (default: ~/.wye)
  WYE_URL      The gateway for claude, codex, and probe
  WYE_TOKEN    The gateway token for claude, codex, and explain
  NO_COLOR     No color, even on a terminal
";

/// Where option descriptions start.
const DESCRIPTION_COLUMN: usize = 28;

/// Which command a [`Command`] is: what `main` dispatches on, so a command
/// the tables list cannot go unhandled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Id {
    Serve,
    ServiceInstall,
    ServiceStatus,
    ServiceRestart,
    ServiceUninstall,
    Doctor,
    Claude,
    Codex,
    Report,
    Init,
    TokenKeygen,
    TokenIssue,
    Explain,
    PolicyTest,
    PolicyDiff,
    Probe,
}

/// One command: what it does and what it takes.
#[derive(Debug)]
pub struct Command {
    /// Which command it is.
    pub id: Id,
    /// As typed after `wye`: `serve`, `token issue`.
    pub name: &'static str,
    /// What it does, in one line of help.
    pub summary: &'static str,
    /// Positional arguments as help shows them: `<dir>`, `[<usage-log>]`.
    pub args: &'static str,
    /// Fewest and most positional arguments.
    pub arity: (usize, usize),
    /// Its options, in the order help lists them.
    pub options: &'static [Opt],
    /// The first argument the command does not know, and everything after
    /// it, goes to the program it starts (`wye claude -p "fix the tests"`).
    pub passthrough: bool,
    /// Help text after the options.
    pub notes: &'static str,
    /// What the required arguments are, with a command to copy: the whole
    /// error when they are missing.
    pub missing: &'static str,
}

/// What a command was given.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed<'a> {
    values: Vec<(&'static str, &'a str)>,
    /// The positional arguments, in order.
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
    /// A required argument is missing; the message says what to run.
    Missing(String),
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
                (None, None)
            };
            let Some(opt) = opt else {
                if self.passthrough {
                    parsed
                        .positional
                        .extend(args.get(i - 1..).unwrap_or_default());
                    break;
                }
                if arg != "-" && arg.starts_with('-') {
                    return Err(ArgsError::Usage(format!("unknown option {arg}")));
                }
                parsed.positional.push(arg);
                continue;
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
            return Err(ArgsError::Missing(format!(
                "missing {}: {}",
                self.args, self.missing
            )));
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
            out.push_str(&wrap(self.notes, 0));
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
    out.push_str(wrap(text, DESCRIPTION_COLUMN).trim_start());
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
    out.push_str(ENVIRONMENT);
    out.push('\n');
    out.push_str(&wrap(
        "`wye <command> --help` shows a command's options. Exit status: 0 done; 1 failed, \
         or a check found problems; 2 wrong usage; 3 done, with something to act on: \
         explain's request would be refused, policy-diff found widened access, or service \
         status found nothing installed.",
        0,
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
    if typed.starts_with('-') {
        return format!("options go after the command: `wye <command> {typed}`");
    }
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

#[cfg(test)]
mod tests;
