//! Wye runtime: HTTP server, upstream forwarding, and startup glue.

use std::fmt;

mod server;
mod upstream;
mod usage_log;

pub use server::{Gateway, serve};
pub use upstream::UpstreamAuth;

/// Failures that prevent the gateway from starting.
#[derive(Debug)]
pub enum StartupError {
    MissingEnv(String),
    InvalidApiKey,
    Tls(String),
    BodyLimitTooLarge,
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv(name) => write!(
                f,
                "environment variable {name} is not set or not valid UTF-8"
            ),
            Self::InvalidApiKey => {
                f.write_str("upstream API key contains characters not allowed in an HTTP header")
            }
            Self::Tls(msg) => write!(f, "TLS setup failed: {msg}"),
            Self::BodyLimitTooLarge => {
                f.write_str("server.max_request_body_bytes exceeds this platform's address space")
            }
        }
    }
}

impl std::error::Error for StartupError {}
