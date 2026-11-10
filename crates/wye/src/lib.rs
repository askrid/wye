//! Wye runtime: startup resolution, HTTP server, upstream selection, and
//! forwarding.

#[cfg(feature = "alloc-stats")]
pub mod alloc_stats;
mod auth;
pub mod cli;
mod diagnostics;
mod discovery;
mod failover;
mod gateway;
mod gauge;
mod idle;
mod log_files;
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

pub use cli::{CallerToken, doctor, explain, policy_diff, policy_test, probe, request_headers};
pub use gateway::Gateway;
pub use server::serve;
pub(crate) use startup::StartupError;
pub use startup::{Resources, UpstreamAuth, UpstreamResources, key_file_warnings, verifier};
pub use usage_log::{UsageFile, UsageOut};

/// An error from a request body or the connection under it.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A value one side sets once and the other reads after: a body's scan
/// result or a policy refusal, set by the request body in hyper's client
/// and read by the request's own task. An error or a dropped body leaves it
/// empty.
pub(crate) type Slot<T> = std::sync::Arc<std::sync::OnceLock<T>>;

/// Where the gateway answers that it is up; `wye probe`, `wye claude`,
/// `wye codex`, and `wye service` ask here.
pub(crate) const HEALTH_PATH: &str = "/healthz";

/// Anthropic's API-key header: a gateway token arrives in it, and
/// Wye's own key leaves in it.
pub(crate) const X_API_KEY: http::HeaderName = http::HeaderName::from_static("x-api-key");
