//! Portable gateway policy: configuration types and validation, gateway
//! credentials, model aliases, upstream selection, request policy (model
//! permissions, output-token ceilings), Claude Code's agent headers, prompt
//! hashes for cache-miss diagnostics, and usage extraction from provider
//! responses.
//!
//! No Tokio, sockets, filesystem, or environment access, so the same policy can
//! later back a WASM adapter.
#![forbid(unsafe_code)]

pub mod agent;
pub mod alias;
pub mod config;
pub mod credential;
mod json;
pub mod policy;
pub mod pricing;
pub mod prompt;
pub mod route;
pub mod upgrade;
pub mod usage;
