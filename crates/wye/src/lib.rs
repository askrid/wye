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
mod tls;
mod upgrade_check;
mod upstream;
mod usage_log;

pub use cli::{
    Agent, Credential, Explanation, InitOptions, Launch, PolicyDiff, claude, codex, doctor,
    explain, init, policy_diff, policy_test, probe, report,
};
pub use gateway::Gateway;
pub use server::serve;
pub use startup::{Resources, StartupError, UpstreamAuth, UpstreamResources, verifier};

/// An error from a request body or the connection under it.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Anthropic's API-key header: a gateway credential arrives in it, and
/// Wye's own key leaves in it.
pub(crate) const X_API_KEY: http::HeaderName = http::HeaderName::from_static("x-api-key");
