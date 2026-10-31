//! End-to-end forwarding tests against a local mock upstream. No network access.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod aliases;
mod cli;
mod config_lookup;
mod credentials;
mod explain;
mod failover;
mod forwarding;
mod harness;
mod identity_providers;
mod lifecycle;
mod openai;
mod routes;
mod streaming;
mod tls;
mod usage_log;
