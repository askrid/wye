//! Test and benchmark support shared by `wye`'s tests and `wye-bench`.
//!
//! - [`TestCa`]: a throwaway CA and a server certificate for `127.0.0.1`.
//! - [`ed25519_keypair`]: a fresh signing key for gateway credentials.
//! - [`spawn_upstream`]: a mock upstream over plain HTTP/1.1, or over TLS with
//!   HTTP/2 or HTTP/1.1 negotiated by ALPN, like a real provider.
//!
//! Not for production use: keys are generated per process and never protected.
// Test support: setup failures abort the test or benchmark that called them.
#![allow(clippy::expect_used)]

use std::{convert::Infallible, error::Error, future::Future, net::SocketAddr, sync::Arc};

use bytes::Bytes;
use http::{Request, Response};
use hyper::{
    body::{Body, Incoming},
    service::service_fn,
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use ring::{rand::SystemRandom, signature::Ed25519KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// A CA and a server certificate it signed for `127.0.0.1`.
pub struct TestCa {
    ca_cert: CertificateDer<'static>,
    ca_pem: String,
    acceptor: TlsAcceptor,
}

impl TestCa {
    pub fn generate() -> Self {
        let ca_key = KeyPair::generate().expect("CA key");
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        // Distinct subject names: rcgen's default is the same for every certificate,
        // and OpenSSL treats a leaf whose subject equals its issuer as self-signed.
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "wye test CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        // Python 3.13+ (strict X.509 verification) rejects CAs without key usage.
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = ca_params.self_signed(&ca_key).expect("CA certificate");
        let issuer = Issuer::new(ca_params, ca_key);

        let server_key = KeyPair::generate().expect("server key");
        let mut server_params =
            CertificateParams::new(vec!["127.0.0.1".to_owned()]).expect("server params");
        server_params
            .distinguished_name
            .push(DnType::CommonName, "127.0.0.1");
        // Python 3.13+ (strict X.509 verification) rejects leaf certificates
        // without an Authority Key Identifier.
        server_params.use_authority_key_identifier_extension = true;
        let server = server_params
            .signed_by(&server_key, &issuer)
            .expect("server certificate");

        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![server.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
        )
        .expect("server TLS config");
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        Self {
            ca_cert: ca.der().clone(),
            ca_pem: ca.pem(),
            acceptor: TlsAcceptor::from(Arc::new(config)),
        }
    }

    /// The CA certificate, for a client root store.
    pub fn ca_cert(&self) -> CertificateDer<'static> {
        self.ca_cert.clone()
    }

    /// The CA certificate as PEM, for `upstream.ca_file`.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }
}

/// Serves `handler` on `127.0.0.1` until the runtime stops; returns the address.
///
/// With `tls`, connections use TLS and negotiate HTTP/2 or HTTP/1.1 by ALPN;
/// without it, plain HTTP/1.1 (or HTTP/2 with prior knowledge).
/// `max_streams` sets the HTTP/2 concurrent stream limit per connection (hyper's default: 200).
pub async fn spawn_upstream<F, Fut, B>(
    handler: F,
    tls: Option<&TestCa>,
    max_streams: Option<u32>,
) -> SocketAddr
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<B>> + Send + 'static,
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let acceptor = tls.map(|ca| ca.acceptor.clone());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let _ = stream.set_nodelay(true);
            let handler = handler.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let response = handler(req);
                    async move { Ok::<_, Infallible>(response.await) }
                });
                let mut builder = auto::Builder::new(TokioExecutor::new());
                if let Some(max) = max_streams {
                    builder.http2().max_concurrent_streams(max);
                }
                // Connection errors (client went away, failed handshake) end only that connection.
                let _ = match acceptor {
                    Some(acceptor) => match acceptor.accept(stream).await {
                        Ok(tls) => builder.serve_connection(TokioIo::new(tls), service).await,
                        Err(_) => return,
                    },
                    None => {
                        builder
                            .serve_connection(TokioIo::new(stream), service)
                            .await
                    }
                };
            });
        }
    });
    addr
}

/// A fresh Ed25519 key, for issuing test and benchmark gateway credentials.
pub fn ed25519_keypair() -> Ed25519KeyPair {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).expect("key generation");
    Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("generated key")
}
