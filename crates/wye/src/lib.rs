//! Wye runtime: startup resolution, HTTP server, routing, and upstream forwarding.
//!
//! - `startup`: secrets and files referenced by the config
//! - `server`: listener, connections, graceful shutdown
//! - `auth`: gateway credential checks, cached per connection
//! - `gateway`: routing and gateway-generated responses
//! - `permissions`: model permissions, checked before forwarding
//! - `upstream`: forwarding to the provider
//! - `usage_log`: per-request usage records
//! - `diagnostics`: prompt hashes for cache-miss diagnostics
//! - `gauge`: in-flight counters released on drop
//! - `alloc_stats`: heap allocation counters (feature `alloc-stats`, benchmarks only)

#[cfg(feature = "alloc-stats")]
pub mod alloc_stats;
mod auth;
mod diagnostics;
mod gateway;
mod gauge;
mod permissions;
mod server;
mod startup;
mod upstream;
mod usage_log;

pub use gateway::Gateway;
pub use server::serve;
pub use startup::{Resources, StartupError};
pub use upstream::UpstreamAuth;
