//! Wye runtime: startup resolution, HTTP server, routing, and upstream forwarding.
//!
//! - `startup`: secrets and files referenced by the config
//! - `server`: listener, connections, graceful shutdown
//! - `gateway`: routing and gateway-generated responses
//! - `upstream`: forwarding to the provider
//! - `usage_log`: per-request usage records

mod gateway;
mod server;
mod startup;
mod upstream;
mod usage_log;

pub use gateway::Gateway;
pub use server::serve;
pub use startup::{Resources, StartupError};
pub use upstream::UpstreamAuth;
