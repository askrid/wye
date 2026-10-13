//! Resolves what the configuration references: secrets from the environment
//! and files from disk. Runs once, before serving.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use gateway_core::config::{Config, UpstreamCredential};
use rustls::pki_types::{CertificateDer, pem::PemObject};

use crate::UpstreamAuth;

/// Failures that prevent the gateway from starting.
#[derive(Debug)]
pub enum StartupError {
    MissingEnv(String),
    InvalidApiKey,
    CaFile { path: PathBuf, reason: String },
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
            Self::CaFile { path, reason } => {
                write!(f, "upstream.ca_file {}: {reason}", path.display())
            }
            Self::Tls(msg) => write!(f, "TLS setup failed: {msg}"),
            Self::BodyLimitTooLarge => {
                f.write_str("server.max_request_body_bytes exceeds this platform's address space")
            }
        }
    }
}

impl std::error::Error for StartupError {}

/// Secrets and files the configuration references, loaded at startup.
#[derive(Debug)]
pub struct Resources {
    pub auth: UpstreamAuth,
    /// Root certificates trusted for the upstream in addition to the built-in roots.
    pub extra_roots: Vec<CertificateDer<'static>>,
}

impl Resources {
    /// Loads what `config` references. Relative paths resolve against `config_dir`.
    pub fn resolve(config: &Config, config_dir: &Path) -> Result<Self, StartupError> {
        let auth = match &config.upstream.credential {
            UpstreamCredential::ApiKeyEnv(name) => {
                let key =
                    std::env::var(name).map_err(|_| StartupError::MissingEnv(name.clone()))?;
                UpstreamAuth::api_key(&key)?
            }
            UpstreamCredential::ForwardCaller => UpstreamAuth::ForwardCaller,
        };
        let extra_roots = match &config.upstream.ca_file {
            Some(path) => load_certificates(&config_dir.join(path))?,
            None => Vec::new(),
        };
        Ok(Self { auth, extra_roots })
    }
}

fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, StartupError> {
    let error = |reason: String| StartupError::CaFile {
        path: path.to_owned(),
        reason,
    };
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| error(e.to_string()))?;
    if certs.is_empty() {
        return Err(error("no PEM certificates found".into()));
    }
    Ok(certs)
}
