//! Portable gateway policy: configuration types and validation, and usage
//! extraction from provider responses.
//!
//! No Tokio, sockets, filesystem, or environment access, so the same policy can
//! later back a WASM adapter.
#![forbid(unsafe_code)]

pub mod config;
pub mod credential;
mod json;
pub mod model;
pub mod prompt;
pub mod usage;
