//! Request routing, forwarding, and gateway-generated responses.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use bytes::Bytes;
use gateway_core::{config::Config, credential::Verifier};
use http::{
    HeaderName, Method, Request, Response, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};
use http_body_util::{Either, Full};
use hyper::body::Incoming;

use crate::{
    Resources, StartupError,
    auth::{ConnectionAuth, authenticate},
    gauge::Gauge,
    upstream::{ForwardError, Upstream, UpstreamBody},
    usage_log::{Observed, RequestInfo, UsageLog},
};

pub(crate) type ResBody = Either<Full<Bytes>, Observed<UpstreamBody<Incoming>>>;

const SESSION_ID: HeaderName = HeaderName::from_static("x-claude-code-session-id");

/// Anthropic's status for an overloaded API; clients back off and retry.
const OVERLOADED: u16 = 529;

/// Per-process gateway state shared by every connection. Immutable after
/// startup except the in-flight counter and the draining flag.
pub struct Gateway {
    /// Checks gateway credentials; `None` on a loopback-only gateway without `[auth]`.
    verifier: Option<Verifier>,
    upstream: Upstream,
    max_request_body_bytes: usize,
    in_flight: Gauge,
    max_concurrent_requests: usize,
    draining: AtomicBool,
    pub(crate) drain_delay: Duration,
    pub(crate) shutdown_grace: Duration,
}

impl Gateway {
    pub fn new(config: &Config, resources: Resources) -> Result<Self, StartupError> {
        Ok(Self {
            verifier: config.auth.as_ref().map(Verifier::new),
            upstream: Upstream::new(&config.upstream, resources.auth, &resources.extra_roots)?,
            max_request_body_bytes: usize::try_from(config.server.max_request_body_bytes)
                .map_err(|_| StartupError::BodyLimitTooLarge)?,
            in_flight: Gauge::default(),
            max_concurrent_requests: config.server.max_concurrent_requests,
            draining: AtomicBool::new(false),
            drain_delay: config.server.drain_delay,
            shutdown_grace: config.server.shutdown_grace,
        })
    }

    /// Endpoints Wye answers itself.
    fn local(&self, method: &Method, path: &str) -> Response<ResBody> {
        match (method, path) {
            // Claude Code's best-effort connection warm-up probe; carries no data.
            (&Method::HEAD, "/api/hello") => empty(StatusCode::OK),
            (&Method::GET, "/healthz") => empty(StatusCode::OK),
            (&Method::GET, "/readyz") if self.draining.load(Ordering::Relaxed) => {
                empty(StatusCode::SERVICE_UNAVAILABLE)
            }
            (&Method::GET, "/readyz") => empty(StatusCode::OK),
            _ => error(
                StatusCode::NOT_FOUND,
                "not_found_error",
                "unsupported endpoint",
            ),
        }
    }

    /// From now on `/readyz` fails; requests are still served.
    ///
    /// `Relaxed`: the flag publishes no other memory, and a readiness probe
    /// that races the signal may see either answer.
    pub(crate) fn start_draining(&self) {
        self.draining.store(true, Ordering::Relaxed);
    }

    pub(crate) async fn handle(
        &self,
        req: Request<Incoming>,
        log: &UsageLog,
        connection: &ConnectionAuth,
    ) -> Response<ResBody> {
        let path = match (req.method(), req.uri().path()) {
            (&Method::POST, "/v1/messages") => "/v1/messages",
            (&Method::POST, "/v1/messages/count_tokens") => "/v1/messages/count_tokens",
            (method, path) => return self.local(method, path),
        };
        let mut info = RequestInfo {
            path,
            session_id: req.headers().get(SESSION_ID).cloned(),
            subject: None,
            started: Instant::now(),
        };
        if let Some(verifier) = &self.verifier {
            match authenticate(verifier, req.headers(), connection) {
                Ok(identity) => info.subject = Some(identity.subject),
                Err(_) => {
                    // One message for every failure: the reason is not the caller's business.
                    let res = error(
                        StatusCode::UNAUTHORIZED,
                        "authentication_error",
                        "invalid or missing gateway credential",
                    );
                    log.gateway_error(info, res.status());
                    return res;
                }
            }
        }
        self.forward(req, info, log).await
    }

    async fn forward(
        &self,
        req: Request<Incoming>,
        info: RequestInfo,
        log: &UsageLog,
    ) -> Response<ResBody> {
        let Some(permit) = self.in_flight.try_hold(self.max_concurrent_requests) else {
            let res = error(
                StatusCode::from_u16(OVERLOADED).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                "overloaded_error",
                "gateway is at its concurrent request limit",
            );
            log.gateway_error(info, res.status());
            return res;
        };
        let declared_len = req
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared_len.is_some_and(|len| len > self.max_request_body_bytes as u64) {
            let res = error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "request body exceeds the gateway limit",
            );
            log.gateway_error(info, res.status());
            return res;
        }
        match self
            .upstream
            .forward(req, self.max_request_body_bytes)
            .await
        {
            Ok(res) => {
                let status = res.status();
                let is_event_stream = res
                    .headers()
                    .get(CONTENT_TYPE)
                    .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
                let log = log.clone();
                res.map(|body| {
                    Either::Right(Observed::new(
                        body,
                        info,
                        status,
                        is_event_stream,
                        log,
                        permit,
                    ))
                })
            }
            Err(e) => {
                let res = forward_error(e);
                log.gateway_error(info, res.status());
                res
            }
        }
    }
}

fn forward_error(e: ForwardError) -> Response<ResBody> {
    match e {
        ForwardError::HeaderTimeout => error(
            StatusCode::GATEWAY_TIMEOUT,
            "api_error",
            "upstream did not respond in time",
        ),
        ForwardError::Upstream => error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "upstream request failed",
        ),
    }
}

fn empty(status: StatusCode) -> Response<ResBody> {
    let mut res = Response::new(Either::Left(Full::new(Bytes::new())));
    *res.status_mut() = status;
    res
}

/// Gateway-generated errors use Anthropic's error shape so clients handle them like upstream errors.
fn error(status: StatusCode, kind: &'static str, message: &'static str) -> Response<ResBody> {
    let body = format!(r#"{{"type":"error","error":{{"type":"{kind}","message":"{message}"}}}}"#);
    let mut res = Response::new(Either::Left(Full::new(Bytes::from(body))));
    *res.status_mut() = status;
    res.headers_mut().insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    res
}
