//! Request routing, forwarding, and gateway-generated responses.

use std::time::Instant;

use bytes::Bytes;
use gateway_core::config::Config;
use http::{
    HeaderName, Method, Request, Response, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};
use http_body_util::{Either, Full};
use hyper::body::Incoming;

use crate::{
    Resources, StartupError,
    upstream::{ForwardError, Upstream, UpstreamBody},
    usage_log::{Observed, RequestInfo, UsageLog},
};

pub(crate) type ResBody = Either<Full<Bytes>, Observed<UpstreamBody<Incoming>>>;

const SESSION_ID: HeaderName = HeaderName::from_static("x-claude-code-session-id");

/// Immutable per-process gateway state shared by every connection.
pub struct Gateway {
    upstream: Upstream,
    max_request_body_bytes: usize,
}

impl Gateway {
    pub fn new(config: &Config, resources: Resources) -> Result<Self, StartupError> {
        Ok(Self {
            upstream: Upstream::new(&config.upstream, resources.auth, &resources.extra_roots)?,
            max_request_body_bytes: usize::try_from(config.server.max_request_body_bytes)
                .map_err(|_| StartupError::BodyLimitTooLarge)?,
        })
    }

    pub(crate) async fn handle(&self, req: Request<Incoming>, log: &UsageLog) -> Response<ResBody> {
        let path = match (req.method(), req.uri().path()) {
            (&Method::POST, "/v1/messages") => "/v1/messages",
            (&Method::POST, "/v1/messages/count_tokens") => "/v1/messages/count_tokens",
            (method, path) => return local(method, path),
        };
        let info = RequestInfo {
            path,
            session_id: req.headers().get(SESSION_ID).cloned(),
            started: Instant::now(),
        };
        self.forward(req, info, log).await
    }

    async fn forward(
        &self,
        req: Request<Incoming>,
        info: RequestInfo,
        log: &UsageLog,
    ) -> Response<ResBody> {
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
                    Either::Right(Observed::new(body, info, status, is_event_stream, log))
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

/// Endpoints Wye answers itself.
fn local(method: &Method, path: &str) -> Response<ResBody> {
    match (method, path) {
        // Claude Code's best-effort connection warm-up probe; carries no data.
        (&Method::HEAD, "/api/hello") => empty(StatusCode::OK),
        (&Method::GET, "/healthz" | "/readyz") => empty(StatusCode::OK),
        _ => error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "unsupported endpoint",
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
