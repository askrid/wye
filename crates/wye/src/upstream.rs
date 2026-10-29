//! Forwarding to an upstream of the route.
//!
//! Request and response bodies stream through without buffering. The one
//! change a request body can carry is the allowlisted `model` rewrite (an
//! alias, or the provider's own model id), made before forwarding; the body
//! is read in passing for model permissions and output-token ceilings, cache
//! diagnostics, and upgrade checks, when those are on. Of the headers, only
//! the URI, hop-by-hop headers, `accept-encoding` (always `identity`), the
//! credential headers (in API-key mode), and `anthropic-workspace-id` (Claude
//! Platform on AWS) are changed.

mod body;
mod pool;
mod refusal;

use std::{fmt, sync::Arc, time::Duration};

use gateway_core::config;
use http::{
    HeaderMap, HeaderName, HeaderValue, Request, Response, Uri, Version,
    header::{
        ACCEPT_ENCODING, AUTHORIZATION, CONNECTION, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION,
        TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
    },
    uri::{Authority, PathAndQuery, Scheme},
};
use hyper::body::Incoming;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use rustls::{ClientConfig, RootCertStore, pki_types::CertificateDer};

pub(crate) use body::{BoxError, UpstreamBody};
use pool::ClientPool;
pub(crate) use refusal::PolicyRefusal;
use refusal::{RefusalSlot, Refusing, policy_refusal};

use gateway_core::{prompt::PromptScanner, upgrade::UpgradeScanner};

use crate::{
    StartupError,
    diagnostics::PromptSlot,
    failover::FailoverBody,
    request_body::{ClientBody, RequestBody},
    scanned_body::ScannedBody,
    upgrade_check::UpgradeSlot,
};

/// The client's request body, checked for permissions and kept for failover.
pub(crate) type SendBody = FailoverBody<RequestBody<ClientBody>>;

/// Sends a [`SendBody`], read for upgrade checks and prompt hashes in
/// passing.
type UpstreamClient = Client<
    HttpsConnector<HttpConnector>,
    ScannedBody<ScannedBody<Refusing<SendBody>, UpgradeScanner>, PromptScanner>,
>;

const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const WORKSPACE_ID: HeaderName = HeaderName::from_static("anthropic-workspace-id");
/// Asked of the upstream instead of the caller's `accept-encoding`: Wye
/// reads usage, models, and tool calls from the response bytes it relays,
/// and Anthropic compresses responses (Brotli) when Claude Code asks for it.
/// Decompressing a copy would cost CPU per event and decoders for untrusted
/// input; an uncompressed response costs only bytes, and responses are small.
const IDENTITY: HeaderValue = HeaderValue::from_static("identity");

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
    /// Replace the caller's credential headers with this `x-api-key` value
    /// (Anthropic, Bedrock).
    ApiKey(HeaderValue),
    /// Replace the caller's credential headers with this `Authorization`
    /// value, `Bearer` and the key (OpenAI, Claude Platform on AWS).
    Bearer(HeaderValue),
    /// Forward the caller's credential headers unchanged.
    ForwardCaller,
}

impl UpstreamAuth {
    pub fn api_key(key: &str) -> Result<Self, StartupError> {
        Ok(Self::ApiKey(secret(key)?))
    }

    pub fn bearer(key: &str) -> Result<Self, StartupError> {
        Ok(Self::Bearer(secret(&format!("Bearer {key}"))?))
    }
}

fn secret(value: &str) -> Result<HeaderValue, StartupError> {
    let mut value = HeaderValue::from_str(value).map_err(|_| StartupError::InvalidApiKey)?;
    value.set_sensitive(true);
    Ok(value)
}

impl fmt::Debug for UpstreamAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Self::ForwardCaller => f.write_str("ForwardCaller"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ForwardError {
    /// No response headers within the configured timeout.
    HeaderTimeout,
    /// The connection could not be made: the request never reached the
    /// provider.
    Connect,
    /// TLS, protocol, or request-body failure, or a connection lost.
    Upstream,
    /// The request body broke the policy or the size limit after
    /// forwarding began; the upstream request was aborted.
    Policy(PolicyRefusal),
}

/// HTTP/2 receive window per upstream stream: what one response may buffer
/// in Wye while its client is not reading (heap per stalled stream: 0.56 MB;
/// 2.2 MB at 1 MiB). It sustains about 5 MB/s per response at 50 ms round
/// trip, far above token pace; bulk relays cost ~10% more CPU per MB than
/// with 1 MiB.
const STREAM_WINDOW: u32 = 256 * 1024;
/// The largest HTTP/2 window, so the connection never limits: streams share
/// an upstream connection, and with a finite window a few clients that stop
/// reading fill it and stall every other stream on it. Buffering stays bounded
/// by `STREAM_WINDOW` per stream.
const CONNECTION_WINDOW: u32 = (1 << 31) - 1;

pub(crate) struct Upstream {
    /// The route's name for it, in usage records; `None` for `[upstream]`.
    pub(crate) id: Option<Arc<str>>,
    /// `anthropic-workspace-id` for Claude Platform on AWS, built once.
    workspace: Option<HeaderValue>,
    /// The prefix the provider's model ids carry (`anthropic.` on Bedrock).
    pub(crate) model_prefix: Option<&'static str>,
    clients: ClientPool,
    scheme: Scheme,
    authority: Authority,
    path_prefix: Box<str>,
    auth: UpstreamAuth,
    response_header_timeout: Duration,
    stream_idle_timeout: Duration,
    /// TLS ALPN may pick HTTP/2, whose client drops request-body errors.
    may_use_http2: bool,
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
        // Plain http is accepted by config validation only for loopback hosts.
        let protocols = HttpsConnectorBuilder::new()
            .with_tls_config(tls_config(extra_roots)?)
            .https_or_http()
            .enable_http1();
        let https = match config.http_version {
            config::UpstreamHttpVersion::Auto => protocols.enable_http2().wrap_connector(http),
            config::UpstreamHttpVersion::Http1 => protocols.wrap_connector(http),
        };
        Ok(Self {
            id: config.id.as_deref().map(Arc::from),
            model_prefix: config.provider.model_prefix(),
            workspace: match config.workspace_id.as_deref() {
                Some(id) => {
                    Some(HeaderValue::from_str(id).map_err(|_| StartupError::InvalidApiKey)?)
                }
                None => None,
            },
            clients: ClientPool::new(|| {
                Client::builder(TokioExecutor::new())
                    .http2_initial_stream_window_size(STREAM_WINDOW)
                    .http2_initial_connection_window_size(CONNECTION_WINDOW)
                    .build(https.clone())
            }),
            scheme: config.scheme.clone(),
            authority: config.authority.clone(),
            path_prefix: config.path_prefix.as_str().into(),
            auth,
            response_header_timeout: config.response_header_timeout,
            stream_idle_timeout: config.stream_idle_timeout,
            may_use_http2: config.scheme == Scheme::HTTPS
                && config.http_version == config::UpstreamHttpVersion::Auto,
        })
    }

    /// Forwards `req` and returns the upstream response with a streaming body.
    /// With a `prompt` slot, the request body's prompt hashes land there;
    /// with an `upgrade` slot, its upgrade-check findings.
    pub(crate) async fn forward(
        &self,
        req: Request<SendBody>,
        prompt: Option<PromptSlot>,
        upgrade: Option<UpgradeSlot>,
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
        parts.headers.insert(ACCEPT_ENCODING, IDENTITY);
        // The caller's headers carried the gateway credential, in either
        // header: neither may reach the provider.
        match &self.auth {
            UpstreamAuth::ApiKey(key) => {
                parts.headers.remove(AUTHORIZATION);
                parts.headers.insert(X_API_KEY, key.clone());
            }
            UpstreamAuth::Bearer(value) => {
                parts.headers.remove(X_API_KEY);
                parts.headers.insert(AUTHORIZATION, value.clone());
            }
            UpstreamAuth::ForwardCaller => {}
        }
        if let Some(workspace) = &self.workspace {
            parts.headers.insert(WORKSPACE_ID, workspace.clone());
        }
        let refused = self.may_use_http2.then(RefusalSlot::default);
        let body = ScannedBody::new(
            ScannedBody::new(Refusing::new(body, refused.clone()), upgrade),
            prompt,
        );
        let req = Request::from_parts(parts, body);

        let (client, slot) = self.clients.acquire();
        let res = tokio::time::timeout(self.response_header_timeout, client.request(req))
            .await
            .map_err(|_| ForwardError::HeaderTimeout)?
            .map_err(|e| {
                let refusal = refused.as_ref().and_then(|r| r.get().copied());
                match refusal.or_else(|| policy_refusal(&e)) {
                    Some(refusal) => ForwardError::Policy(refusal),
                    None if e.is_connect() => ForwardError::Connect,
                    None => ForwardError::Upstream,
                }
            })?;

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

/// An HTTP/1.1 client connector for `http` and `https` URLs, trusting
/// `extra_roots` besides the built-in roots: the OTLP exporter and `wye probe`.
pub(crate) fn https_connector(
    extra_roots: &[CertificateDer<'static>],
) -> Result<HttpsConnector<HttpConnector>, StartupError> {
    Ok(HttpsConnectorBuilder::new()
        .with_tls_config(tls_config(extra_roots)?)
        .https_or_http()
        .enable_http1()
        .build())
}

/// Built-in web PKI roots plus the configured extra roots.
fn tls_config(extra_roots: &[CertificateDer<'static>]) -> Result<ClientConfig, StartupError> {
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
        let auth = UpstreamAuth::bearer("sk-secret").unwrap();
        assert_eq!(format!("{auth:?}"), "Bearer(<redacted>)");
    }
}
