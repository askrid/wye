//! HTTP listener, routing, and gateway error responses.

use std::{
    convert::Infallible,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use gateway_core::config::Config;
use http::{
    HeaderName, Method, Request, Response, StatusCode, header::CONTENT_LENGTH, header::CONTENT_TYPE,
};
use http_body_util::{Either, Full};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::graceful::GracefulShutdown,
};
use tokio::{io::AsyncWrite, net::TcpListener};

use crate::{
    StartupError,
    upstream::{ForwardError, IdleTimeout, Upstream, UpstreamAuth},
    usage_log::{Observed, RequestInfo, UsageLog},
};

type ResBody = Either<Full<Bytes>, Observed<IdleTimeout<Incoming>>>;

const SESSION_ID: HeaderName = HeaderName::from_static("x-claude-code-session-id");

/// How long `serve` waits for the usage log to be written after connections close.
const LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// Immutable per-process gateway state shared by every connection.
pub struct Gateway {
    upstream: Upstream,
    max_request_body_bytes: usize,
}

impl Gateway {
    pub fn new(config: &Config, auth: UpstreamAuth) -> Result<Self, StartupError> {
        Ok(Self {
            upstream: Upstream::new(&config.upstream, auth)?,
            max_request_body_bytes: usize::try_from(config.server.max_request_body_bytes)
                .map_err(|_| StartupError::BodyLimitTooLarge)?,
        })
    }

    async fn handle(&self, req: Request<Incoming>, log: &UsageLog) -> Response<ResBody> {
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
        let res = if declared_len.is_some_and(|len| len > self.max_request_body_bytes as u64) {
            error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "request body exceeds the gateway limit",
            )
        } else {
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
                    return res.map(|body| {
                        Either::Right(Observed::new(body, info, status, is_event_stream, log))
                    });
                }
                Err(ForwardError::HeaderTimeout) => error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "api_error",
                    "upstream did not respond in time",
                ),
                Err(ForwardError::Upstream) => error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "upstream request failed",
                ),
            }
        };
        log.gateway_error(info, res.status());
        res
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

/// Serves connections until `shutdown` resolves, then drains them for at most `grace`.
pub async fn serve(
    listener: TcpListener,
    gateway: Arc<Gateway>,
    shutdown: impl Future<Output = ()>,
    grace: Duration,
    usage_out: impl AsyncWrite + Send + Unpin + 'static,
) {
    let (log, writer) = UsageLog::start(usage_out);
    let graceful = GracefulShutdown::new();
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new());
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                // Transient accept errors (for example, file descriptor exhaustion) must not stop the server.
                let Ok((stream, _)) = accepted else { continue };
                let _ = stream.set_nodelay(true);
                let gateway = Arc::clone(&gateway);
                let log = log.clone();
                let service = service_fn(move |req| {
                    let gateway = Arc::clone(&gateway);
                    let log = log.clone();
                    async move { Ok::<_, Infallible>(gateway.handle(req, &log).await) }
                });
                let conn = graceful.watch(http.serve_connection(TokioIo::new(stream), service));
                // Owned by `graceful`: drained on shutdown, cancelled with the runtime after the deadline.
                tokio::spawn(conn);
            }
            () = &mut shutdown => break,
        }
    }
    drop(listener);
    let _ = tokio::time::timeout(grace, graceful.shutdown()).await;
    // The writer ends once the last handle is gone: here and in drained connections.
    drop(log);
    let _ = tokio::time::timeout(LOG_FLUSH_TIMEOUT, writer).await;
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
