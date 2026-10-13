//! Load client for the mock (plain or TLS) and for Wye.

use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use http::Request;
use http_body_util::Full;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use rustls::{ClientConfig, RootCertStore, pki_types::CertificateDer};

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// An endpoint to load: base URL plus a pooled client that trusts the test CA.
#[derive(Clone)]
pub struct Target {
    base: Arc<str>,
    root: Option<CertificateDer<'static>>,
    client: HttpClient,
}

impl Target {
    pub fn plain(addr: SocketAddr) -> Self {
        Self::new(format!("http://{addr}"), None)
    }

    /// TLS endpoint; HTTP/2 is negotiated by ALPN.
    pub fn tls(addr: SocketAddr, root: CertificateDer<'static>) -> Self {
        Self::new(format!("https://{addr}"), Some(root))
    }

    fn new(base: String, root: Option<CertificateDer<'static>>) -> Self {
        let client = build_client(root.as_ref());
        Self {
            base: base.into(),
            root,
            client,
        }
    }

    /// Same endpoint with its own connection pool.
    pub fn fresh(&self) -> Self {
        Self::new(self.base.to_string(), self.root.clone())
    }

    pub fn client(&self) -> &HttpClient {
        &self.client
    }

    /// `POST /v1/messages` with the mock's control headers.
    pub fn messages(&self, headers: &[(&'static str, String)]) -> Request<Full<Bytes>> {
        let mut req = Request::post(format!("{}/v1/messages?beta=true", self.base))
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");
        for (name, value) in headers {
            req = req.header(*name, value.as_str());
        }
        req.body(Full::new(Bytes::from_static(
            br#"{"model":"bench","max_tokens":1,"messages":[]}"#,
        )))
        .unwrap()
    }

    pub fn get(&self, path: &str) -> Request<Full<Bytes>> {
        Request::get(format!("{}{path}", self.base))
            .body(Full::new(Bytes::new()))
            .unwrap()
    }
}

fn build_client(root: Option<&CertificateDer<'static>>) -> HttpClient {
    let mut roots = RootCertStore::empty();
    if let Some(root) = root {
        roots.add(root.clone()).unwrap();
    }
    let tls =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(4096)
        .build(https)
}
