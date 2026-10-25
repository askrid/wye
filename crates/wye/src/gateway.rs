//! Request routing, forwarding, and gateway-generated responses.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use gateway_core::{
    alias::Aliases,
    config::Config,
    credential::Verifier,
    policy::{self, ModelPattern},
    prompt::{self, HashKey},
};
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};
use http_body_util::{Either, Full};
use hyper::body::Incoming;

use crate::{
    Resources, StartupError,
    auth::{ConnectionAuth, authenticate},
    diagnostics::PromptSlot,
    discovery::ModelCatalog,
    gauge::Gauge,
    permissions::{OverTokenLimit, RequestBody, hold_until_model},
    upstream::{Upstream, UpstreamBody},
    usage_log::{AgentHeaders, Observed, OtlpTarget, RequestInfo, Sinks, UsageLog},
};

mod responses;

use responses::{empty, error, forward_error, hold_error, overloaded, unauthorized};

pub(crate) type ResBody = Either<Full<Bytes>, Observed<UpstreamBody<Incoming>>>;

const SESSION_ID: HeaderName = HeaderName::from_static("x-claude-code-session-id");
/// W3C trace context: the parent of the request's OTLP span.
const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");

/// Per-process gateway state shared by every connection. Immutable after
/// startup except the in-flight counter and the draining flag.
pub struct Gateway {
    /// Checks gateway credentials; `None` on a loopback-only gateway without `[auth]`.
    verifier: Option<Verifier>,
    upstream: Upstream,
    /// Models any credential may use; `None` allows every model.
    models: Option<Vec<ModelPattern>>,
    /// Largest `max_tokens` any credential may request.
    max_output_tokens: Option<u64>,
    /// `GET /v1/models`, when `[discovery]` is configured.
    catalog: Option<ModelCatalog>,
    /// `[[aliases]]`, resolved while the body's `model` is held.
    aliases: Option<Aliases>,
    /// Each model an alias resolves to, built once so a splice shares it.
    alias_models: HashMap<Arc<str>, Bytes>,
    /// Request bytes held while reading `model`, across all requests.
    held_bytes: Gauge,
    max_held_request_bytes: usize,
    /// Keys prompt hashes in the usage log; `None` when cache diagnostics are off.
    prompt_key: Option<Arc<HashKey>>,
    /// Percentage of sessions whose requests are diagnosed.
    prompt_sample_percent: u8,
    /// OTLP export of usage records, when `[telemetry]` is configured.
    otlp: Option<Arc<OtlpTarget>>,
    max_request_body_bytes: usize,
    in_flight: Gauge,
    max_concurrent_requests: usize,
    draining: AtomicBool,
    pub(crate) drain_delay: Duration,
    pub(crate) shutdown_grace: Duration,
    pub(crate) client_write_timeout: Duration,
}

impl Gateway {
    pub fn new(config: &Config, resources: Resources) -> Result<Self, StartupError> {
        Ok(Self {
            verifier: resources.verifier,
            upstream: Upstream::new(&config.upstream, resources.auth, &resources.extra_roots)?,
            models: config.auth.as_ref().and_then(|auth| auth.models.clone()),
            max_output_tokens: config.auth.as_ref().and_then(|auth| auth.max_output_tokens),
            catalog: config.discovery.as_ref().map(|discovery| {
                ModelCatalog::new(
                    discovery,
                    config.aliases.as_ref(),
                    config.auth.as_ref().and_then(|auth| auth.models.as_deref()),
                )
            }),
            aliases: config.aliases.clone(),
            alias_models: config
                .aliases
                .iter()
                .flat_map(Aliases::iter)
                .flat_map(|alias| alias.models())
                .map(|model| (Arc::clone(model), Bytes::copy_from_slice(model.as_bytes())))
                .collect(),
            held_bytes: Gauge::default(),
            max_held_request_bytes: usize::try_from(config.server.max_held_request_bytes)
                .unwrap_or(usize::MAX),
            otlp: match &config.telemetry {
                Some(telemetry) => Some(Arc::new(otlp_target(
                    telemetry,
                    &resources.extra_roots,
                    resources.otlp_headers.clone(),
                )?)),
                None => None,
            },
            prompt_key: resources.prompt_key.map(Arc::new),
            prompt_sample_percent: config
                .cache_diagnostics
                .as_ref()
                .map_or(100, |d| d.sample_percent),
            max_request_body_bytes: usize::try_from(config.server.max_request_body_bytes)
                .map_err(|_| StartupError::BodyLimitTooLarge)?,
            in_flight: Gauge::default(),
            max_concurrent_requests: config.server.max_concurrent_requests,
            draining: AtomicBool::new(false),
            drain_delay: config.server.drain_delay,
            shutdown_grace: config.server.shutdown_grace,
            client_write_timeout: config.server.client_write_timeout,
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
            #[cfg(feature = "alloc-stats")]
            (&Method::GET, "/debug/alloc") => {
                let mut res = Response::new(Either::Left(Full::new(Bytes::from(
                    crate::alloc_stats::snapshot().to_json(),
                ))));
                res.headers_mut().insert(
                    CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                res
            }
            _ => error(
                StatusCode::NOT_FOUND,
                "not_found_error",
                "unsupported endpoint",
            ),
        }
    }

    /// What the usage log writes besides stderr.
    pub(crate) fn sinks(&self) -> Sinks {
        Sinks {
            prompt_key: self.prompt_key.clone(),
            otlp: self.otlp.clone(),
            active: self.in_flight.clone(),
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
            (&Method::GET, "/v1/models") if self.catalog.is_some() => {
                return self.models(&req, connection);
            }
            (method, path) => return self.local(method, path),
        };
        let mut info = RequestInfo {
            path,
            session_id: req.headers().get(SESSION_ID).cloned(),
            traceparent: req.headers().get(TRACEPARENT).cloned(),
            agent: AgentHeaders::from_headers(req.headers()),
            identity: None,
            prompt: None,
            requested_model: None,
            started: Instant::now(),
        };
        if let Some(verifier) = &self.verifier {
            match authenticate(verifier, req.headers(), connection) {
                Ok(identity) => info.identity = Some(identity),
                Err(_) => {
                    let res = unauthorized();
                    log.gateway_error(info, res.status());
                    return res;
                }
            }
        }
        self.forward(req, info, log).await
    }

    /// The catalog this caller's credential may use. Authenticated like a
    /// proxied request when `[auth]` is set, since the answer reveals the
    /// credential's permissions. Not a usage record: nothing reaches the
    /// provider.
    fn models(&self, req: &Request<Incoming>, connection: &ConnectionAuth) -> Response<ResBody> {
        let Some(catalog) = &self.catalog else {
            return self.local(req.method(), req.uri().path());
        };
        let identity = match &self.verifier {
            None => None,
            Some(verifier) => match authenticate(verifier, req.headers(), connection) {
                Ok(identity) => Some(identity),
                Err(_) => return unauthorized(),
            },
        };
        let body = catalog.list(
            identity.as_ref().and_then(|i| i.models.as_deref()),
            identity.as_ref().map(|i| &*i.subject),
        );
        let mut res = Response::new(Either::Left(Full::new(body)));
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        res
    }

    async fn forward(
        &self,
        req: Request<Incoming>,
        mut info: RequestInfo,
        log: &UsageLog,
    ) -> Response<ResBody> {
        let Some(permit) = self.in_flight.try_hold(self.max_concurrent_requests) else {
            let res = overloaded("gateway is at its concurrent request limit");
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
        let (mut parts, body) = req.into_parts();
        let body = match self.admit_body(body, &mut info, &mut parts.headers).await {
            Ok(body) => body,
            Err(res) => {
                log.gateway_error(info, res.status());
                return *res;
            }
        };
        let req = Request::from_parts(parts, body);
        if self.prompt_key.is_some()
            && prompt::session_sampled(
                info.session_id.as_ref().map(HeaderValue::as_bytes),
                self.prompt_sample_percent,
            )
        {
            info.prompt = Some(PromptSlot::default());
        }
        match self
            .upstream
            .forward(req, self.max_request_body_bytes, info.prompt.clone())
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

    /// Resolves model aliases and checks the request body against model
    /// permissions and the output-token ceiling. Without any of them, the
    /// body passes untouched; otherwise it is held until its `model` is read,
    /// then replayed (with an alias replaced by its model, and `headers`'
    /// `content-length` adjusted) and checked as it streams.
    async fn admit_body(
        &self,
        mut body: Incoming,
        info: &mut RequestInfo,
        headers: &mut HeaderMap,
    ) -> Result<RequestBody<Incoming>, Box<Response<ResBody>>> {
        let credential_models = info.identity.as_ref().and_then(|i| i.models.as_deref());
        // `max_tokens` bounds generation; count_tokens generates nothing.
        let token_limit = (info.path == "/v1/messages")
            .then(|| {
                policy::token_limit(
                    self.max_output_tokens,
                    info.identity.as_ref().and_then(|i| i.max_output_tokens),
                )
            })
            .flatten();
        if self.models.is_none()
            && credential_models.is_none()
            && token_limit.is_none()
            && self.aliases.is_none()
        {
            return Ok(RequestBody::new(body));
        }
        let mut held = hold_until_model(
            &mut body,
            &self.held_bytes,
            self.max_held_request_bytes,
            self.max_request_body_bytes,
            token_limit,
        )
        .await
        .map_err(|e| Box::new(hold_error(e)))?;
        // Permissions and pricing apply to the model that runs.
        let alias = self
            .aliases
            .as_ref()
            .zip(held.model())
            .and_then(|(aliases, model)| aliases.get(model));
        let resolved = alias.map(|alias| {
            let subject = info.identity.as_ref().map(|i| &*i.subject);
            (
                Arc::clone(alias.name()),
                Arc::clone(alias.model_for(subject)),
            )
        });
        if let Some((name, model)) = &resolved {
            let spliced = self
                .alias_models
                .get(model)
                .and_then(|bytes| held.splice_model(bytes));
            let Some(delta) = spliced else {
                return Err(Box::new(error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "model alias could not be applied",
                )));
            };
            adjust_content_length(headers, delta);
            info.requested_model = Some(Arc::clone(name));
        }
        if self.models.is_none() && credential_models.is_none() && token_limit.is_none() {
            held.skip_rest_check();
        }
        let model = resolved.as_ref().map(|(_, m)| &**m).or(held.model());
        let allowed =
            model.is_some_and(|m| policy::permitted(self.models.as_deref(), credential_models, m));
        if !allowed {
            return Err(Box::new(error(
                StatusCode::FORBIDDEN,
                "permission_error",
                "this credential may not use the requested model",
            )));
        }
        if let (Some(limit), Some(requested)) = (token_limit, held.max_tokens())
            && requested > limit
        {
            let message = OverTokenLimit { requested, limit }.to_string();
            return Err(Box::new(error(
                StatusCode::FORBIDDEN,
                "permission_error",
                &message,
            )));
        }
        Ok(RequestBody::after_hold(held, body))
    }
}

/// Keeps a declared `content-length` true after the body changed by `delta`
/// bytes; a chunked body has none to fix.
fn adjust_content_length(headers: &mut HeaderMap, delta: i64) {
    if delta == 0 {
        return;
    }
    let Some(len) = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return;
    };
    if let Some(len) = len.checked_add_signed(delta) {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
    }
}

/// The OTLP export client trusts the same roots as the upstream connection.
fn otlp_target(
    telemetry: &gateway_core::config::Telemetry,
    extra_roots: &[rustls::pki_types::CertificateDer<'static>],
    headers: http::HeaderMap,
) -> Result<OtlpTarget, StartupError> {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(crate::upstream::https_connector(extra_roots)?);
    Ok(OtlpTarget::new(
        telemetry.otlp_logs_uri.clone(),
        telemetry.otlp_metrics_uri.clone(),
        telemetry.otlp_traces_uri.clone(),
        headers,
        client,
        &telemetry.service_name,
    ))
}
