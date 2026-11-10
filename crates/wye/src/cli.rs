//! The commands besides `wye serve`. Each runs once and exits; none runs
//! inside the server.

use std::path::Path;

use paint::Paint;

pub mod args;
pub mod doctor;
pub mod explain;
pub mod files;
pub mod init;
pub mod paint;
pub mod policy_diff;
pub mod policy_test;
pub mod probe;
pub mod report;
pub mod service;
mod text;
pub mod token;
pub mod wrapper;

pub use doctor::doctor;
pub use explain::{CallerToken, Explanation, explain, request_headers};
pub use policy_diff::{PolicyDiff, policy_diff};
pub use policy_test::policy_test;
pub use probe::probe;

/// The repository's files at this build's release tag, for every link the
/// CLI prints or writes: a link to `main` would describe another version's
/// config keys and commands.
pub const REPO_AT_RELEASE: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "/blob/v",
    env!("CARGO_PKG_VERSION")
);

/// The environment a command reads: the process's, or a test's stand-in.
pub trait Env: Fn(&str) -> Option<String> {
    /// The variable `name`, unless it is unset or empty.
    fn var(&self, name: &str) -> Option<String> {
        self(name).filter(|value| !value.is_empty())
    }
}

impl<F: Fn(&str) -> Option<String>> Env for F {}

/// How a command ended; the process exits with its code. Wrong usage
/// (exit 2) is an [`Error::Usage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Done.
    Done = 0,
    /// Failed: a check found problems.
    Failed = 1,
    /// Done, with something a script should act on.
    ActOn = 3,
}

/// `report`, headed by the config it is about.
fn about(config: &Path, env: &impl Env, report: &str) -> String {
    format!("config: {}\n{report}", files::tilde(config, env))
}

/// Prints a check's report; [`Exit::Failed`] when it found problems.
fn checked(report: &str, ok: bool) -> Exit {
    emit(&Paint::stdout().style(report));
    if ok { Exit::Done } else { Exit::Failed }
}

/// Prints a report; [`Exit::ActOn`] when it found something a script
/// should act on (a refusal, widened access).
fn act_on(report: &str, found: bool) -> Exit {
    emit(&Paint::stdout().style(report));
    if found { Exit::ActOn } else { Exit::Done }
}

/// Writes messages about running a command to standard error, styled as
/// [`Paint::style`] styles them.
fn tell(text: &str) {
    eprint!("{}", Paint::stderr().style(text));
}

/// A runtime for a command that makes a request or two.
fn current_thread() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// The time offline checks verify credentials at.
fn unix_secs() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|e| e.to_string())
}

/// A duration that runs, as [`gateway_core::duration::parse`] reads it, in
/// seconds.
fn duration_secs(text: &str) -> Option<u64> {
    gateway_core::duration::parse(text)
        .map(|d| d.as_secs())
        .filter(|&secs| secs > 0)
}

/// Writes to standard output. A reader that went away (`wye report | head`)
/// is the normal end of a command, not a failure.
pub fn emit(text: &str) {
    use std::io::Write;

    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => {
            eprintln!("wye: cannot write to standard output: {e}");
            std::process::exit(1);
        }
    }
}

/// Why a command stopped short; the exit status tells a script which.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Wrong usage: an argument or a value the command cannot take (exit 2).
    Usage(String),
    /// Anything else (exit 1).
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(why: String) -> Self {
        Self::Failed(why)
    }
}

impl From<&str> for Error {
    fn from(why: &str) -> Self {
        Self::Failed(why.to_owned())
    }
}

/// `println!` through [`emit`].
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::cli::emit(&format!("{}\n", format_args!($($arg)*)))
    };
}
pub(crate) use say;

/// Creates `dir` if missing, readable only by its owner: it holds keys.
fn create_private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

/// Writes `text` to the new file `path`, never over an existing one (or
/// through a symlink), making its directory, readable only by you, if it is
/// missing. A `secret` file is readable only by you too. Errors show the
/// path with `show`, and name `flag` as the way to choose another.
fn write_new(
    path: &Path,
    text: &str,
    secret: bool,
    flag: &str,
    show: &dyn Fn(&Path) -> String,
) -> Result<(), String> {
    use std::io::Write;

    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // 0o666, less the umask, is what a file gets by default.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, if secret { 0o600 } else { 0o666 });
    let mut file = options.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
            format!("{} exists; choose another path with {flag}", show(path))
        }
        _ => format!("cannot create {}: {e}", show(path)),
    })?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", show(path)))
}

/// An error with its causes, which carry the useful part (refused,
/// certificate, DNS).
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_written_and_run() {
        for (text, secs) in [("45s", 45), ("30m", 1800), ("8h", 28_800), ("7d", 604_800)] {
            assert_eq!(duration_secs(text), Some(secs), "{text}");
        }
        for bad in ["", "d", "7w", "-1d", "0s", "90", "1.5h"] {
            assert_eq!(duration_secs(bad), None, "{bad}");
        }
    }
}
