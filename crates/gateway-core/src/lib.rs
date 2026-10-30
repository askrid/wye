//! Portable gateway policy: configuration types and validation, gateway
//! credentials, model aliases, upstream selection and failover order,
//! request policy (model permissions, output-token ceilings), upgrade
//! checks, agent headers (Claude Code's and Codex's), prompt hashes for
//! cache-miss diagnostics, usage extraction from provider responses, and
//! pricing.
//!
//! No Tokio, sockets, filesystem, or environment access, so the same policy can
//! later back a WASM adapter.
#![forbid(unsafe_code)]

pub mod admission;
pub mod agent;
pub mod alias;
pub mod api;
pub mod config;
pub mod credential;
mod json;
pub mod policy;
pub mod pricing;
pub mod prompt;
pub mod route;
pub mod upgrade;
pub mod usage;
