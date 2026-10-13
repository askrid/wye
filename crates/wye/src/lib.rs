//! Wye runtime: startup resolution, HTTP server, routing, and upstream forwarding.
//!
//! - `startup`: secrets and files referenced by the config
//! - `server`: listener, connections, graceful shutdown
//! - `auth`: gateway credential checks, cached per connection
//! - `gateway`: routing and gateway-generated responses
//! - `upstream`: forwarding to the provider
//! - `usage_log`: per-request usage records
//! - `gauge`: in-flight counters released on drop

mod auth;
mod gateway;
mod gauge;
mod server;
mod startup;
mod upstream;
mod usage_log;

pub use gateway::Gateway;
pub use server::serve;
pub use startup::{Resources, StartupError};
pub use upstream::UpstreamAuth;
