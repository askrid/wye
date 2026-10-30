//! The commands besides `wye serve`. Each runs once and exits; none runs
//! inside the server.
//!
//! - `explain`: the server's decisions for one request, offline
//! - `policy_test`: sample requests checked against expected decisions
//! - `policy_diff`: what a config change widens, narrows, or changes
//! - `report`: a usage-log summary by request class, upstream, tool, session,
//!   and prompt
//! - `wrapper`: `wye claude` and `wye codex`, an agent pointed at a gateway
//! - `probe`: a one-shot health check client
//! - `doctor`: a config's secrets, files, and endpoints checked before serving
//! - `service`: the gateway as a per-user launchd or systemd service
//! - `init`: a minimal working config
//! - `token`: offline key generation and token issuance
//! - `args`: every command's options, parsed and rendered as help
//! - `paint`: color on a terminal, where it carries meaning

pub mod args;
mod doctor;
mod explain;
mod init;
pub mod paint;
mod policy_diff;
mod policy_test;
mod probe;
mod report;
pub mod service;
pub mod token;
mod wrapper;

pub use doctor::doctor;
pub use explain::{Credential, Explanation, explain};
pub use init::{InitOptions, Mode as InitMode, init};
pub use policy_diff::{PolicyDiff, policy_diff};
pub use policy_test::policy_test;
pub use probe::{health_url, probe};
pub use report::report;
pub use wrapper::{Agent, Launch, gateway_url, run as run_agent};

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

/// `println!` through [`emit`].
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::cli::emit(&format!("{}\n", format_args!($($arg)*)))
    };
}
pub(crate) use say;

/// Creates `dir` if missing, readable only by its owner: it holds keys.
fn create_private_dir(dir: &std::path::Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

/// Writes a secret to a new file only its owner can read, making the
/// directory if it is missing; never over an existing file.
pub fn write_secret(path: &std::path::Path, text: &str) -> Result<(), String> {
    use std::io::Write;

    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
            format!("{} exists; choose another path with --out", path.display())
        }
        _ => format!("cannot create {}: {e}", path.display()),
    })?;
    writeln!(file, "{text}").map_err(|e| format!("cannot write {}: {e}", path.display()))
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

/// `n` and the noun in the number it takes: "1 session", "2 sessions".
fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A configured size as people write it: "32 MiB", "64 KiB", or "1000 bytes".
fn size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    match bytes {
        0 => "0 bytes".to_owned(),
        b if b % MIB == 0 => format!("{} MiB", b / MIB),
        b if b % KIB == 0 => format!("{} KiB", b / KIB),
        b => plural(b, "byte", "bytes"),
    }
}
