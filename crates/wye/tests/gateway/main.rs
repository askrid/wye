//! End-to-end tests of the binary and a gateway, against local mock
//! upstreams. No network access.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod aliases;
mod cli;
mod config_lookup;
mod explain;
mod failover;
mod forwarding;
mod gateway_tokens;
mod harness;
mod identity_providers;
mod lifecycle;
mod openai;
mod routes;
mod streaming;
mod telemetry;
mod tls;
mod usage_log;
mod web_pages;
