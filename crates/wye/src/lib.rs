//! Wye runtime: startup resolution, HTTP server, routing, and upstream forwarding.
//!
//! - `startup`: secrets and files referenced by the config
//! - `server`: listener, connections, graceful shutdown
//! - `auth`: gateway credential checks, cached per connection
//! - `discovery`: `GET /v1/models`, the model catalog filtered by permissions
//! - `gateway`: routing and gateway-generated responses
//! - `cli`: the commands besides `serve` (`explain`, `policy-test`, `report`,
//!   `claude`, `probe`, `token`)
//! - `permissions`: model permissions and token ceilings, checked before forwarding
//! - `upstream`: forwarding to the provider
//! - `usage_log`: per-request usage records
//! - `diagnostics`: prompt hashes for cache-miss diagnostics
//! - `gauge`: in-flight counters released on drop
//! - `alloc_stats`: heap allocation counters (feature `alloc-stats`, benchmarks only)

#[cfg(feature = "alloc-stats")]
pub mod alloc_stats;
mod auth;
pub mod cli;
mod diagnostics;
mod discovery;
mod gateway;
mod gauge;
mod permissions;
mod server;
mod startup;
mod upgrade_check;
mod upstream;
mod usage_log;

pub use cli::{ClaudeLaunch, Credential, Explanation, claude, explain, policy_test, probe, report};
pub use gateway::Gateway;
pub use server::serve;
pub use startup::{Resources, StartupError, verifier};
pub use upstream::UpstreamAuth;
