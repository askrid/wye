//! Wye runtime: startup resolution, HTTP server, routing, and upstream forwarding.
//!
//! - `startup`: secrets and files referenced by the config
//! - `server`: listener, connections, graceful shutdown
//! - `auth`: gateway credential checks, cached per connection
//! - `gateway`: routing and gateway-generated responses
//! - `explain`: `wye explain`, the server's decisions for one request, offline
//! - `permissions`: model permissions, checked before forwarding
//! - `upstream`: forwarding to the provider
//! - `usage_log`: per-request usage records
//! - `diagnostics`: prompt hashes for cache-miss diagnostics
//! - `probe`: the `wye probe` health check client
//! - `wrapper`: `wye claude`, Claude Code pointed at a gateway
//! - `gauge`: in-flight counters released on drop
//! - `alloc_stats`: heap allocation counters (feature `alloc-stats`, benchmarks only)

#[cfg(feature = "alloc-stats")]
pub mod alloc_stats;
mod auth;
mod diagnostics;
mod explain;
mod gateway;
mod gauge;
mod permissions;
mod probe;
mod server;
mod startup;
mod upstream;
mod usage_log;
mod wrapper;

pub use explain::{Explanation, explain};
pub use gateway::Gateway;
pub use probe::probe;
pub use server::serve;
pub use startup::{Resources, StartupError};
pub use upstream::UpstreamAuth;
pub use wrapper::{ClaudeLaunch, claude};
