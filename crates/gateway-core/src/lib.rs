//! Portable gateway policy: configuration types and validation.
//!
//! No Tokio, sockets, filesystem, or environment access, so the same policy can
//! later back a WASM adapter.
#![forbid(unsafe_code)]

pub mod config;
