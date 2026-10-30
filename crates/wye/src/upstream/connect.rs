//! The connect timeout over the whole connect: TCP and the TLS handshake.
//!
//! hyper-util's connect timeout covers the TCP connect only (per address, so
//! a host with several addresses moves on from one that hangs). A peer that
//! accepts the connection and then stalls the handshake, such as an
//! overloaded load balancer, would hold the request until the response header
//! timeout, as an upstream failure, not a refused connection, so failover
//! would not apply. Wrapping the whole connector makes the handshake part of
//! the connect: past the timeout it fails as a connect error.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use http::Uri;
use tower_service::Service;

use super::BoxError;

/// `inner`, failing any connect that takes longer than `timeout`.
#[derive(Clone)]
pub(super) struct ConnectTimeout<C> {
    inner: C,
    timeout: Duration,
}

impl<C> ConnectTimeout<C> {
    pub(super) fn new(inner: C, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

impl<C> Service<Uri> for ConnectTimeout<C>
where
    C: Service<Uri>,
    C::Future: Send + 'static,
    C::Error: Into<BoxError>,
{
    type Response = C::Response;
    type Error = BoxError;
    // Boxed: once per new connection, never per request. hyper-rustls boxes
    // its own future too.
    type Future = Pin<Box<dyn Future<Output = Result<C::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connecting = self.inner.call(uri);
        let timeout = self.timeout;
        Box::pin(async move {
            match tokio::time::timeout(timeout, connecting).await {
                Ok(connected) => connected.map_err(Into::into),
                Err(_) => Err("connect (TCP and TLS) timed out".into()),
            }
        })
    }
}
