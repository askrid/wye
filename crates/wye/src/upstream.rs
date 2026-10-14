//! Forwarding to the configured upstream.
//!
//! Request and response bodies stream through without buffering or changes;
//! the request body is read in passing only when cache diagnostics are on.
//! Only the URI, hop-by-hop headers, and (in API-key mode) the credential
//! headers are changed.

mod body;
mod pool;

use std::{fmt, sync::Arc, time::Duration};

use gateway_core::config;
use http::{
    HeaderMap, HeaderName, HeaderValue, Request, Response, Uri, Version,
    header::{
        AUTHORIZATION, CONNECTION, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER,
        TRANSFER_ENCODING, UPGRADE,
    },
    uri::{Authority, PathAndQuery, Scheme},
};
use http_body_util::Limited;
use hyper::body::Incoming;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use rustls::{ClientConfig, RootCertStore, pki_types::CertificateDer};

pub(crate) use body::{BoxError, UpstreamBody};
use pool::ClientPool;

use gateway_core::policy::PolicyError;

use crate::{
    StartupError,
    diagnostics::{PromptBody, PromptSlot},
    permissions::{OverTokenLimit, RequestBody},
};

type UpstreamClient =
    Client<HttpsConnector<HttpConnector>, PromptBody<Limited<RequestBody<Incoming>>>>;

const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");

/// Headers that apply to a single connection and are never forwarded (RFC 9110 §7.6.1).
const HOP_BY_HOP: [HeaderName; 9] = [
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TE,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
];

/// Credential Wye presents upstream.
pub enum UpstreamAuth {
    /// Replace the caller's credential headers with this `x-api-key` value.
    ApiKey(HeaderValue),
    /// Forward the caller's credential headers unchanged.
    ForwardCaller,
}

impl UpstreamAuth {
    pub fn api_key(key: &str) -> Result<Self, StartupError> {
        let mut value = HeaderValue::from_str(key).map_err(|_| StartupError::InvalidApiKey)?;
        value.set_sensitive(true);
        Ok(Self::ApiKey(value))
    }
}

impl fmt::Debug for UpstreamAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            Self::ForwardCaller => f.write_str("ForwardCaller"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ForwardError {
    /// No response headers within the configured timeout.
    HeaderTimeout,
    /// Connection, TLS, protocol, or request-body failure.
    Upstream,
    /// The request body broke the policy after forwarding began; the
    /// upstream request was aborted.
    Policy(PolicyRefusal),
}

/// A policy failure found in the streamed part of the body.
#[derive(Debug)]
pub(crate) enum PolicyRefusal {
    /// A repeated `model`, an escaped top-level key, or a bad `max_tokens`.
    Invalid(PolicyError),
    /// `max_tokens` above the request's ceiling.
    OverTokenLimit(String),
}

pub(crate) struct Upstream {
    clients: ClientPool,
    scheme: Scheme,
    authority: Authority,
    path_prefix: Box<str>,
    auth: UpstreamAuth,
    response_header_timeout: Duration,
    stream_idle_timeout: Duration,
}

impl Upstream {
    pub(crate) fn new(
        config: &config::Upstream,
        auth: UpstreamAuth,
        extra_roots: &[CertificateDer<'static>],
    ) -> Result<Self, StartupError> {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(config.connect_timeout));
        http.set_nodelay(true);
        let https = HttpsConnectorBuilder::new()
            .with_tls_config(tls_config(extra_roots)?)
            // Plain http is accepted by config validation only for loopback hosts.
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        Ok(Self {
            clients: ClientPool::new(|| Client::builder(TokioExecutor::new()).build(https.clone())),
            scheme: config.scheme.clone(),
            authority: config.authority.clone(),
            path_prefix: config.path_prefix.as_str().into(),
            auth,
            response_header_timeout: config.response_header_timeout,
            stream_idle_timeout: config.stream_idle_timeout,
        })
    }

    /// Forwards `req` and returns the upstream response with a streaming body.
    /// With a `prompt` slot, the request body's prompt hashes land there.
    pub(crate) async fn forward(
        &self,
        req: Request<RequestBody<Incoming>>,
        max_body_bytes: usize,
        prompt: Option<PromptSlot>,
    ) -> Result<Response<UpstreamBody<Incoming>>, ForwardError> {
        let (mut parts, body) = req.into_parts();
        parts.uri = self.upstream_uri(parts.uri.path_and_query())?;
        // The client negotiates HTTP/2 or HTTP/1.1 with the upstream through TLS ALPN
        // (Anthropic: HTTP/2, so concurrent streams share one connection); this only
        // resets the caller's version so it cannot select the upstream protocol.
        parts.version = Version::HTTP_11;
        parts.extensions.clear();
        strip_hop_by_hop(&mut parts.headers);
        parts.headers.remove(HOST);
        if let UpstreamAuth::ApiKey(key) = &self.auth {
            parts.headers.remove(AUTHORIZATION);
            parts.headers.insert(X_API_KEY, key.clone());
        }
        let body = PromptBody::new(Limited::new(body, max_body_bytes), prompt);
        let req = Request::from_parts(parts, body);

        let (client, slot) = self.clients.acquire();
        let res = tokio::time::timeout(self.response_header_timeout, client.request(req))
            .await
            .map_err(|_| ForwardError::HeaderTimeout)?
            .map_err(|e| policy_refusal(&e).map_or(ForwardError::Upstream, ForwardError::Policy))?;

        let (mut parts, body) = res.into_parts();
        strip_hop_by_hop(&mut parts.headers);
        Ok(Response::from_parts(
            parts,
            UpstreamBody::new(body, self.stream_idle_timeout, slot),
        ))
    }

    fn upstream_uri(&self, path_and_query: Option<&PathAndQuery>) -> Result<Uri, ForwardError> {
        let path_and_query = match (self.path_prefix.is_empty(), path_and_query) {
            // Cheap clone: `PathAndQuery` is backed by shared `Bytes`.
            (true, Some(pq)) => pq.clone(),
            (true, None) => PathAndQuery::from_static("/"),
            (false, pq) => {
                let suffix = pq.map_or("/", PathAndQuery::as_str);
                PathAndQuery::try_from(format!("{}{suffix}", self.path_prefix))
                    .map_err(|_| ForwardError::Upstream)?
            }
        };
        Uri::builder()
            .scheme(self.scheme.clone())
            .authority(self.authority.clone())
            .path_and_query(path_and_query)
            .build()
            .map_err(|_| ForwardError::Upstream)
    }
}

/// The policy failure our request body raised, if that is why the upstream
/// request failed (hyper keeps the body's error as a source).
fn policy_refusal(error: &(dyn std::error::Error + 'static)) -> Option<PolicyRefusal> {
    let mut cause = Some(error);
    while let Some(e) = cause {
        if let Some(e) = e.downcast_ref::<PolicyError>() {
            return Some(PolicyRefusal::Invalid(*e));
        }
        if let Some(e) = e.downcast_ref::<OverTokenLimit>() {
            return Some(PolicyRefusal::OverTokenLimit(e.to_string()));
        }
        cause = e.source();
    }
    None
}

/// Built-in web PKI roots plus the configured extra roots.
pub(crate) fn tls_config(
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

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // Allocates only when `Connection` names extra headers, which is rare.
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    for name in named.iter().chain(HOP_BY_HOP.iter()) {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_hop_by_hop_and_connection_named_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONNECTION,
            HeaderValue::from_static("keep-alive, x-private"),
        );
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("x-private", HeaderValue::from_static("1"));
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        headers.insert("anthropic-beta", HeaderValue::from_static("a,b"));
        strip_hop_by_hop(&mut headers);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["anthropic-beta"], "a,b");
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let auth = UpstreamAuth::api_key("sk-secret").unwrap();
        assert_eq!(format!("{auth:?}"), "ApiKey(<redacted>)");
    }
}
