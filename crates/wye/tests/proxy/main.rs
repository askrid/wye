//! End-to-end forwarding tests against a local mock upstream. No network access.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod credentials;
mod forwarding;
mod harness;
mod lifecycle;
mod streaming;
mod tls;
mod usage_log;
