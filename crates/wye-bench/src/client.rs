//! Load client for the mock (plain or TLS) and for Wye.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

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

/// Independent connection pools per target. Each HTTP/2 pool holds one
/// connection, so without this the direct TLS baseline would be limited by a
/// single connection, the same limit Wye's upstream pool works around.
const POOLS: usize = 8;

/// An endpoint to load: base URL plus pooled clients that trust the test CA.
#[derive(Clone)]
pub struct Target {
    base: Arc<str>,
    root: Option<CertificateDer<'static>>,
    clients: Arc<[HttpClient]>,
    next: Arc<AtomicUsize>,
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
        Self {
            base: base.into(),
            clients: (0..POOLS).map(|_| build_client(root.as_ref())).collect(),
            root,
            next: Arc::default(),
        }
    }

    /// Same endpoint with its own connection pool.
    pub fn fresh(&self) -> Self {
        Self::new(self.base.to_string(), self.root.clone())
    }

    /// The next client, round-robin over the pools.
    pub fn client(&self) -> &HttpClient {
        &self.clients[self.next.fetch_add(1, Ordering::Relaxed) % POOLS]
    }

    /// `POST /v1/messages` with the mock's control headers and a small body.
    pub fn messages(&self, headers: &[(&'static str, String)]) -> Request<Full<Bytes>> {
        self.messages_with_body(
            headers,
            Bytes::from_static(br#"{"model":"bench","max_tokens":1,"messages":[]}"#),
        )
    }

    /// `POST /v1/messages` with the mock's control headers and `body`.
    pub fn messages_with_body(
        &self,
        headers: &[(&'static str, String)],
        body: Bytes,
    ) -> Request<Full<Bytes>> {
        let mut req = Request::post(format!("{}/v1/messages?beta=true", self.base))
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");
        for (name, value) in headers {
            req = req.header(*name, value.as_str());
        }
        req.body(Full::new(body)).unwrap()
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
