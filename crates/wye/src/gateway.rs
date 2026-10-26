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
    config::{Config, UpgradeCheck},
    credential::Verifier,
    policy::ModelPattern,
    pricing::Pricing,
    prompt::{self, HashKey},
    route::Route,
};
use http::{
    HeaderName, HeaderValue, Method, Request, Response, StatusCode,
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
    upgrade_check::UpgradeSlot,
    upstream::{Upstream, UpstreamBody},
    usage_log::{AgentHeaders, Observed, OtlpTarget, RequestInfo, Sinks, UsageLog},
};

mod admit;
mod responses;

use responses::{empty, error, forward_error, overloaded, unauthorized};

pub(crate) type ResBody = Either<Full<Bytes>, Observed<UpstreamBody<Incoming>>>;

const SESSION_ID: HeaderName = HeaderName::from_static("x-claude-code-session-id");
/// W3C trace context: the parent of the request's OTLP span.
const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");

/// Per-process gateway state shared by every connection. Immutable after
/// startup except the in-flight counter and the draining flag.
pub struct Gateway {
    /// Checks gateway credentials; `None` on a loopback-only gateway without `[auth]`.
    verifier: Option<Verifier>,
    /// Picks an upstream per request; the same order as `upstreams`.
    route: Route,
    upstreams: Box<[Upstream]>,
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
    /// Prices usage records in the writer task, when `[pricing]` is configured.
    pricing: Option<Arc<Pricing>>,
    /// `[upgrade_check]`: target models and the share of sessions checked.
    upgrade_check: Option<Arc<UpgradeCheck>>,
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
        if resources.upstreams.len() != config.route.upstreams().len() {
            return Err(StartupError::UpstreamCount);
        }
        let otlp_roots = resources.all_extra_roots();
        let upstreams = config
            .route
            .upstreams()
            .iter()
            .zip(resources.upstreams)
            .map(|(upstream, r)| Upstream::new(upstream, r.auth, &r.extra_roots))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            verifier: resources.verifier,
            route: config.route.clone(),
            upstreams,
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
                    &otlp_roots,
                    resources.otlp_headers.clone(),
                )?)),
                None => None,
            },
            prompt_key: resources.prompt_key.map(Arc::new),
            pricing: config.pricing.clone().map(Arc::new),
            upgrade_check: config.upgrade_check.clone().map(Arc::new),
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
            pricing: self.pricing.clone(),
            upgrade_targets: self
                .upgrade_check
                .as_ref()
                .map(|check| Arc::from(&*check.targets)),
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
            upgrade: None,
            upstream: None,
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
        // Sampled by session, or by client for SDK callers without one.
        if let Some(check) = &self.upgrade_check
            && prompt::session_sampled(
                info.session_id
                    .as_ref()
                    .map(HeaderValue::as_bytes)
                    .or(info.identity.as_ref().map(|i| i.subject.as_bytes())),
                check.sample_percent,
            )
        {
            info.upgrade = Some(UpgradeSlot::default());
        }
        // Session affinity: prompt caches are per provider account.
        let key = info
            .session_id
            .as_ref()
            .map(HeaderValue::as_bytes)
            .or(info.identity.as_ref().map(|i| i.subject.as_bytes()))
            .unwrap_or_default();
        let Some(upstream) = self.upstreams.get(self.route.pick(key)) else {
            unreachable!("the route and its upstreams have the same length")
        };
        info.upstream = upstream.id.clone();
        match upstream
            .forward(
                req,
                self.max_request_body_bytes,
                info.prompt.clone(),
                info.upgrade.clone(),
            )
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
