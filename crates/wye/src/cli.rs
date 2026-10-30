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
pub use probe::probe;
pub use report::report;
pub use wrapper::{Agent, Launch, run as run_agent};

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
