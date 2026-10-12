//! Forwarding to the configured upstream.
//!
//! Request and response bodies stream through without buffering or parsing.
//! Only the URI, hop-by-hop headers, and (in API-key mode) the credential
//! headers are changed.

use std::{
    error::Error as StdError,
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
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
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use tokio::time::{Instant, Sleep};

use crate::StartupError;

type BoxError = Box<dyn StdError + Send + Sync>;
type UpstreamClient = Client<HttpsConnector<HttpConnector>, Limited<Incoming>>;

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
}

pub(crate) struct Upstream {
    client: UpstreamClient,
    scheme: Scheme,
    authority: Authority,
    path_prefix: Box<str>,
    auth: UpstreamAuth,
    response_header_timeout: Duration,
    stream_idle_timeout: Duration,
}

impl Upstream {
    pub(crate) fn new(config: &config::Upstream, auth: UpstreamAuth) -> Result<Self, StartupError> {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(config.connect_timeout));
        http.set_nodelay(true);
        let https = HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(Arc::new(rustls::crypto::ring::default_provider()))
            .map_err(|e| StartupError::Tls(e.to_string()))?
            // Plain http is accepted by config validation only for loopback hosts.
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(https),
            scheme: config.scheme.clone(),
            authority: config.authority.clone(),
            path_prefix: config.path_prefix.as_str().into(),
            auth,
            response_header_timeout: config.response_header_timeout,
            stream_idle_timeout: config.stream_idle_timeout,
        })
    }

    /// Forwards `req` and returns the upstream response with a streaming body.
    pub(crate) async fn forward(
        &self,
        req: Request<Incoming>,
        max_body_bytes: usize,
    ) -> Result<Response<IdleTimeout<Incoming>>, ForwardError> {
        let (mut parts, body) = req.into_parts();
        parts.uri = self.upstream_uri(parts.uri.path_and_query())?;
        parts.version = Version::HTTP_11;
        parts.extensions.clear();
        strip_hop_by_hop(&mut parts.headers);
        parts.headers.remove(HOST);
        if let UpstreamAuth::ApiKey(key) = &self.auth {
            parts.headers.remove(AUTHORIZATION);
            parts.headers.insert(X_API_KEY, key.clone());
        }
        let req = Request::from_parts(parts, Limited::new(body, max_body_bytes));

        let res = tokio::time::timeout(self.response_header_timeout, self.client.request(req))
            .await
            .map_err(|_| ForwardError::HeaderTimeout)?
            .map_err(|_| ForwardError::Upstream)?;

        let (mut parts, body) = res.into_parts();
        strip_hop_by_hop(&mut parts.headers);
        Ok(Response::from_parts(
            parts,
            IdleTimeout::new(body, self.stream_idle_timeout),
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

/// Fails the stream when the upstream sends nothing for `timeout`.
///
/// The timer runs only while this body waits on the upstream. Time spent
/// while the downstream is not polling (backpressure) never counts.
pub(crate) struct IdleTimeout<B> {
    inner: B,
    timeout: Duration,
    sleep: Pin<Box<Sleep>>,
    waiting: bool,
}

impl<B> IdleTimeout<B> {
    fn new(inner: B, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            sleep: Box::pin(tokio::time::sleep(timeout)),
            waiting: false,
        }
    }
}

#[derive(Debug)]
struct StreamIdleTimeout;

impl fmt::Display for StreamIdleTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("upstream stream idle timeout")
    }
}

impl StdError for StreamIdleTimeout {}

impl<B> Body for IdleTimeout<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(frame) => {
                this.waiting = false;
                Poll::Ready(frame.map(|f| f.map_err(Into::into)))
            }
            Poll::Pending => {
                if !this.waiting {
                    this.waiting = true;
                    this.sleep.as_mut().reset(Instant::now() + this.timeout);
                }
                match this.sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Some(Err(StreamIdleTimeout.into()))),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
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
