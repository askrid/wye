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
    /// Scheme and authority, e.g. `http://127.0.0.1:8080`.
    origin: Arc<str>,
    /// Prefix before `/v1/messages`; empty except for gateways that mount it elsewhere.
    api_prefix: Arc<str>,
    /// Added to every request, e.g. a gateway's own credential.
    extra_headers: Arc<[(&'static str, String)]>,
    /// Prepended to the mock's control header names, for a gateway that
    /// forwards extra headers only under its own prefix.
    control_prefix: &'static str,
    /// Whether requests carry `anthropic-version` (a gateway may add it itself).
    anthropic_version: bool,
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

    /// A gateway endpoint that serves `/v1/messages` under `api_prefix` and
    /// requires `extra_headers`.
    pub fn gateway(
        addr: SocketAddr,
        api_prefix: &str,
        extra_headers: Vec<(&'static str, String)>,
    ) -> Self {
        Self {
            api_prefix: api_prefix.into(),
            extra_headers: extra_headers.into(),
            ..Self::plain(addr)
        }
    }

    /// Sends the mock's control headers as `{prefix}{name}` and leaves
    /// `anthropic-version` to the gateway.
    pub fn with_control_prefix(self, prefix: &'static str) -> Self {
        Self {
            control_prefix: prefix,
            anthropic_version: false,
            ..self
        }
    }

    fn new(origin: String, root: Option<CertificateDer<'static>>) -> Self {
        Self {
            origin: origin.into(),
            api_prefix: "".into(),
            extra_headers: Arc::new([]),
            control_prefix: "",
            anthropic_version: true,
            clients: (0..POOLS).map(|_| build_client(root.as_ref())).collect(),
            root,
            next: Arc::default(),
        }
    }

    /// Same endpoint with its own connection pool.
    pub fn fresh(&self) -> Self {
        Self {
            api_prefix: Arc::clone(&self.api_prefix),
            extra_headers: Arc::clone(&self.extra_headers),
            control_prefix: self.control_prefix,
            anthropic_version: self.anthropic_version,
            ..Self::new(self.origin.to_string(), self.root.clone())
        }
    }

    /// The next client, round-robin over the pools.
    pub fn client(&self) -> &HttpClient {
        &self.clients[self.next.fetch_add(1, Ordering::Relaxed) % POOLS]
    }

    /// `POST /v1/messages` with the mock's control headers and a small body:
    /// `"stream":true` unless the mock is asked for a unary (JSON) response,
    /// as a real client would, since gateways that parse requests honor it.
    pub fn messages(&self, headers: &[(&'static str, String)]) -> Request<Full<Bytes>> {
        let unary = headers.iter().any(|(name, _)| *name == "x-bench-unary");
        let body: &'static [u8] = if unary {
            br#"{"model":"anthropic/bench","max_tokens":1,"messages":[{"role":"user","content":"x"}]}"#
        } else {
            br#"{"model":"anthropic/bench","max_tokens":1,"stream":true,"messages":[{"role":"user","content":"x"}]}"#
        };
        self.messages_with_body(headers, Bytes::from_static(body))
    }

    /// `POST /v1/messages` with the mock's control headers and `body`.
    pub fn messages_with_body(
        &self,
        headers: &[(&'static str, String)],
        body: Bytes,
    ) -> Request<Full<Bytes>> {
        let url = format!("{}{}/v1/messages?beta=true", self.origin, self.api_prefix);
        let mut req = Request::post(url).header("content-type", "application/json");
        if self.anthropic_version {
            req = req.header("anthropic-version", "2023-06-01");
        }
        for (name, value) in headers {
            req = if self.control_prefix.is_empty() {
                req.header(*name, value.as_str())
            } else {
                req.header(format!("{}{name}", self.control_prefix), value.as_str())
            };
        }
        for (name, value) in self.extra_headers.iter() {
            req = req.header(*name, value.as_str());
        }
        req.body(Full::new(body)).unwrap()
    }

    /// `GET` of `path` on the origin, without the API prefix or extra headers.
    pub fn get(&self, path: &str) -> Request<Full<Bytes>> {
        Request::get(format!("{}{path}", self.origin))
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
