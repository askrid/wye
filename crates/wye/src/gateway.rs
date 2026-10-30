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
    api::{self, Api},
    config::{Config, DEFAULT_SAMPLE_PERCENT, UpgradeCheck},
    credential::Verifier,
    policy::ModelPattern,
    pricing::Pricing,
    prompt::HashKey,
    route::Route,
};
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, header::CONTENT_TYPE};
use http_body_util::{Either, Full};
use hyper::body::Incoming;

use crate::{
    Resources, StartupError,
    auth::{ConnectionAuth, authenticate},
    discovery::ModelCatalog,
    gauge::Gauge,
    upstream::{Upstream, UpstreamBody},
    usage_log::{AgentHeaders, Observed, OtlpTarget, RequestInfo, Sinks, UsageLog},
};

mod admit;
mod forward;
mod responses;

use responses::{empty, error, unauthorized};

pub(crate) type ResBody = Either<Full<Bytes>, Observed<UpstreamBody<Incoming>>>;

/// W3C trace context: the parent of the request's OTLP span.
const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");

/// Per-process gateway state shared by every connection. Immutable after
/// startup except the in-flight counter and the draining flag.
pub struct Gateway {
    /// Checks gateway credentials; `None` on a loopback-only gateway without `[auth]`.
    verifier: Option<Verifier>,
    /// Anthropic Messages, when `[upstream]` or `[routes.claude]` is set.
    claude: Option<ApiRoute>,
    /// OpenAI Responses and Chat Completions, when `[routes.openai]` is set.
    openai: Option<ApiRoute>,
    /// Models any credential may use; `None` allows every model.
    models: Option<Vec<ModelPattern>>,
    /// Largest output-token maximum any request may ask for.
    max_output_tokens: Option<u64>,
    /// `GET /v1/models`, when `[discovery]` is configured.
    catalog: Option<ModelCatalog>,
    /// `[[aliases]]`, resolved while the body's `model` is held.
    aliases: Option<Aliases>,
    /// Each model an alias resolves to, built once so a splice shares it.
    alias_models: HashMap<Arc<str>, Bytes>,
    /// Request bytes held while reading `model`, across all requests.
    held_bytes: Gauge,
    /// Request bytes kept for failover, across all requests; bounded per
    /// route by its `failover_max_held_bytes`.
    failover_held: Gauge,
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

/// One API's route: how a request picks an upstream, and the upstreams, in
/// the route's order.
struct ApiRoute {
    route: Route,
    upstreams: Box<[Upstream]>,
    /// An upstream's provider has its own model ids (Bedrock): the
    /// model is held and re-prefixed per attempt.
    prefixes_model_ids: bool,
}

impl Gateway {
    pub fn new(config: &Config, resources: Resources) -> Result<Self, StartupError> {
        if resources.upstreams.len() != config.routes.upstreams().count() {
            return Err(StartupError::UpstreamCount);
        }
        let otlp_roots = resources.all_extra_roots();
        // Resources come in `Routes::upstreams` order: Claude's, then OpenAI's.
        let mut resolved = resources.upstreams.into_iter();
        let mut build = |route: &Route| -> Result<ApiRoute, StartupError> {
            let upstreams = route
                .upstreams()
                .iter()
                .zip(resolved.by_ref())
                .map(|(upstream, r)| Upstream::new(upstream, r.auth, &r.extra_roots))
                .collect::<Result<_, _>>()?;
            Ok(ApiRoute {
                prefixes_model_ids: route.prefixes_model_ids(),
                route: route.clone(),
                upstreams,
            })
        };
        let claude = config.routes.claude.as_ref().map(&mut build).transpose()?;
        let openai = config.routes.openai.as_ref().map(&mut build).transpose()?;
        Ok(Self {
            verifier: resources.verifier,
            claude,
            openai,
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
            failover_held: Gauge::default(),
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
                .map_or(DEFAULT_SAMPLE_PERCENT, |d| d.sample_percent),
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

    fn route(&self, api: Api) -> Option<&ApiRoute> {
        match api {
            Api::Messages => self.claude.as_ref(),
            Api::Responses | Api::ChatCompletions => self.openai.as_ref(),
        }
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
                Api::Messages,
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
        let endpoint = match (req.method(), req.uri().path()) {
            (&Method::GET, "/v1/models")
                if self.catalog.is_some() && !is_codex_catalog(req.uri().query()) =>
            {
                return self.models(&req, connection);
            }
            (&Method::POST, path) => match api::endpoint(path) {
                Some(endpoint) if self.route(endpoint.api).is_some() => endpoint,
                Some(endpoint) => return no_route(endpoint.api, path),
                None => return self.local(req.method(), path),
            },
            (method, path) => return self.local(method, path),
        };
        let api = endpoint.api;
        let mut info = RequestInfo {
            endpoint,
            session_id: api
                .session_header()
                .and_then(|name| req.headers().get(name))
                .cloned(),
            traceparent: req.headers().get(TRACEPARENT).cloned(),
            agent: match api {
                Api::Messages => AgentHeaders::from_headers(req.headers()),
                Api::Responses => AgentHeaders::from_codex_headers(req.headers()),
                Api::ChatCompletions => AgentHeaders::default(),
            },
            identity: None,
            prompt: None,
            requested_model: None,
            upgrade: None,
            upstream: None,
            failovers: Vec::new(),
            started: Instant::now(),
        };
        if let Some(verifier) = &self.verifier {
            match authenticate(verifier, req.headers(), connection) {
                Ok(identity) => info.identity = Some(identity),
                Err(_) => {
                    let res = unauthorized(api);
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
                Err(_) => return unauthorized(Api::Messages),
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
}

/// A known endpoint whose API no route serves: the 404, in that API's shape,
/// names the sections that would.
fn no_route(api: Api, path: &str) -> Response<ResBody> {
    let message = format!("no route serves {path}: add {}", api.route_sections());
    error(api, StatusCode::NOT_FOUND, "not_found_error", &message)
}

/// Codex asks `GET {base_url}/models?client_version=...` for its own catalog
/// format, not Claude Code's: a 404 makes it use its bundled catalog.
fn is_codex_catalog(query: Option<&str>) -> bool {
    query.is_some_and(|q| q.split('&').any(|pair| pair.starts_with("client_version=")))
}

/// The OTLP export client trusts the same roots as the upstream connection.
fn otlp_target(
    telemetry: &gateway_core::config::Telemetry,
    extra_roots: &[rustls::pki_types::CertificateDer<'static>],
    headers: http::HeaderMap,
) -> Result<OtlpTarget, StartupError> {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(crate::tls::https_connector(extra_roots)?);
    Ok(OtlpTarget::new(
        telemetry.otlp_logs_uri.clone(),
        telemetry.otlp_metrics_uri.clone(),
        telemetry.otlp_traces_uri.clone(),
        headers,
        client,
        &telemetry.service_name,
    ))
}

/// A request's declared `content-length`, if it has a valid one.
fn content_length(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::is_codex_catalog;

    #[test]
    fn codex_catalog_requests_are_recognized() {
        assert!(is_codex_catalog(Some("client_version=0.130.0")));
        assert!(is_codex_catalog(Some("limit=5&client_version=1")));
        assert!(!is_codex_catalog(Some("limit=1000")));
        assert!(!is_codex_catalog(None));
    }
}
