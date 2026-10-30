//! The TLS client every outbound connection uses: upstreams, the OTLP
//! exporter, `wye doctor`, and `wye probe`. Built-in web PKI roots plus the
//! configured extra roots, ring as the crypto provider.

use std::sync::Arc;

use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use rustls::{ClientConfig, RootCertStore, pki_types::CertificateDer};

use crate::StartupError;

/// An HTTP/1.1 client connector for `http` and `https` URLs, trusting
/// `extra_roots` besides the built-in roots: the OTLP exporter and `wye probe`.
pub(crate) fn https_connector(
    extra_roots: &[CertificateDer<'static>],
) -> Result<HttpsConnector<HttpConnector>, StartupError> {
    Ok(HttpsConnectorBuilder::new()
        .with_tls_config(client_config(extra_roots)?)
        .https_or_http()
        .enable_http1()
        .build())
}

/// Built-in web PKI roots plus the configured extra roots.
pub(crate) fn client_config(
    extra_roots: &[CertificateDer<'static>],
) -> Result<ClientConfig, StartupError> {
    let mut roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    for cert in extra_roots {
        roots
            .add(cert.clone())
            .map_err(|e| StartupError::Tls(format!("invalid CA certificate: {e}")))?;
    }
    Ok(
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| StartupError::Tls(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}
