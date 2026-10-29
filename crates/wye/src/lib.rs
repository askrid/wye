//! Wye runtime: startup resolution, HTTP server, routing, and upstream
//! forwarding. The module map is in `docs/architecture.md`.

#[cfg(feature = "alloc-stats")]
pub mod alloc_stats;
mod auth;
pub mod cli;
mod diagnostics;
mod discovery;
mod failover;
mod gateway;
mod gauge;
mod request_body;
mod scanned_body;
mod server;
mod startup;
#[cfg(test)]
mod test_body;
mod upgrade_check;
mod upstream;
mod usage_log;

pub use cli::{
    Agent, Credential, Explanation, InitOptions, Launch, PolicyDiff, claude, codex, doctor,
    explain, init, policy_diff, policy_test, probe, report,
};
pub use gateway::Gateway;
pub use server::serve;
pub use startup::{Resources, StartupError, UpstreamResources, verifier};
pub use upstream::UpstreamAuth;
