//! Admission, upstream selection, forwarding, and gateway-generated responses.

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
    admission::{self, Refusal},
    alias::Aliases,
    api::{self, Api},
    config::{
        CLIENT_WRITE_TIMEOUT, Config, DEFAULT_SAMPLE_PERCENT, MAX_TOTAL_KEPT_BYTES, Provider,
        Routes, UpgradeCheck,
    },
    policy::ModelPattern,
    pricing::Pricing,
    prompt::HashKey,
    route::Route,
    token::{Identity, Verifier},
};
use http::{HeaderName, Method, Request, Response, StatusCode};
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

use responses::empty;

pub(crate) type ResBody = Either<Full<Bytes>, Observed<UpstreamBody<Incoming>>>;

/// W3C trace context: the parent of the request's OTLP span.
const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");

/// Per-process gateway state shared by every connection. Immutable after
/// startup except its gauges (in flight, held bytes, failover bytes) and the
/// draining flag.
pub struct Gateway {
    /// Checks gateway tokens; `None` on a loopback-only gateway without `[auth]`.
    verifier: Option<Verifier>,
    /// The configured routes, ready to forward.
    routes: Routes<ApiRoute>,
    /// Models any gateway token may use; `None` allows every model.
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
    /// Request bytes kept for failover, across all requests.
    failover_kept: Gauge,
    max_total_held_bytes: usize,
    max_total_kept_bytes: usize,
    /// Keys prompt hashes in the usage log; `None` when cache diagnostics are off.
    prompt_key: Option<Arc<HashKey>>,
    /// Percentage of sessions whose requests are diagnosed.
    prompt_sample_percent: u8,
    /// OTLP export of usage records, when `[telemetry]` is configured.
    otlp: Option<Arc<OtlpTarget>>,
    /// Prices usage records in the writer task, when `[pricing]` is configured.
    pricing: Option<Arc<Pricing>>,
    /// `[upgrade_check]`: target models and the share of sessions checked.
    upgrade_check: Option<UpgradeCheck>,
    max_request_body_bytes: usize,
    in_flight: Gauge,
    max_concurrent_requests: usize,
    draining: AtomicBool,
    /// Open client connections `serve` accepts at once.
    pub(crate) max_connections: usize,
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
    /// `resources` come from [`Resources::resolve`] on the same `config`.
    pub fn new(config: &Config, resources: Resources) -> Result<Self, StartupError> {
        if resources.upstreams.len() != config.routes.upstreams().count() {
            return Err(StartupError::UpstreamCount);
        }
        let otlp_roots = resources.all_extra_roots();
        // Resources come in `Routes::upstreams` order: Claude's, then OpenAI's.
        let mut resolved = resources.upstreams.into_iter();
        let build = |route: &Route| -> Result<ApiRoute, StartupError> {
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
        let routes = config.routes.try_map(build)?;
        Ok(Self {
            verifier: resources.verifier,
            routes,
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
            failover_kept: Gauge::default(),
            max_total_held_bytes: usize::try_from(config.server.max_total_held_bytes())
                .unwrap_or(usize::MAX),
            max_total_kept_bytes: usize::try_from(MAX_TOTAL_KEPT_BYTES).unwrap_or(usize::MAX),
            otlp: config
                .telemetry
                .as_ref()
                .map(|telemetry| OtlpTarget::new(telemetry, &otlp_roots, resources.otlp_headers))
                .transpose()?
                .map(Arc::new),
            prompt_key: resources.prompt_key.map(Arc::new),
            pricing: config.pricing.clone().map(Arc::new),
            upgrade_check: config.upgrade_check.clone(),
            prompt_sample_percent: config
                .cache_diagnostics
                .as_ref()
                .map_or(DEFAULT_SAMPLE_PERCENT, |d| d.sample_percent),
            max_request_body_bytes: usize::try_from(config.server.max_request_body_bytes)
                .map_err(|_| StartupError::BodyLimitTooLarge)?,
            in_flight: Gauge::default(),
            max_concurrent_requests: config.server.max_concurrent_requests,
            draining: AtomicBool::new(false),
            max_connections: config.server.max_connections(),
            drain_delay: config.server.drain_delay,
            shutdown_grace: config.server.shutdown_grace,
            client_write_timeout: CLIENT_WRITE_TIMEOUT,
        })
    }

    /// The gateway with another client write timeout than
    /// [`CLIENT_WRITE_TIMEOUT`], so a test need not wait a minute for one.
    #[doc(hidden)]
    pub fn with_client_write_timeout(self, client_write_timeout: Duration) -> Self {
        Self {
            client_write_timeout,
            ..self
        }
    }

    /// The caller's identity when gateway tokens are configured, `None`
    /// when they are not; `Err` for a missing or invalid token, or, without
    /// tokens, for a request a web page may have sent.
    fn identify(
        &self,
        headers: &http::HeaderMap,
        connection: &ConnectionAuth,
    ) -> Result<Option<Arc<Identity>>, Refusal> {
        match &self.verifier {
            Some(verifier) => authenticate(verifier, headers, connection)
                .map(Some)
                .map_err(|_| Refusal::Unauthenticated),
            None => admission::check_local_client(headers).map(|()| None),
        }
    }

    /// Endpoints Wye answers itself.
    fn local(&self, method: &Method, path: &str) -> Response<ResBody> {
        match (method, path) {
            // Claude Code's best-effort connection warm-up probe; carries no data.
            (&Method::HEAD, "/api/hello") => empty(StatusCode::OK),
            (&Method::GET, crate::HEALTH_PATH) => empty(StatusCode::OK),
            (&Method::GET, "/readyz") if self.draining.load(Ordering::Relaxed) => {
                empty(StatusCode::SERVICE_UNAVAILABLE)
            }
            (&Method::GET, "/readyz") => empty(StatusCode::OK),
            #[cfg(feature = "alloc-stats")]
            (&Method::GET, "/debug/alloc") => {
                responses::json(Bytes::from(crate::alloc_stats::snapshot().to_json()))
            }
            _ => responses::not_proxied(),
        }
    }

    /// What the usage log writes besides stdout.
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
        let endpoint = match (req.method(), req.uri().path(), &self.catalog) {
            (&Method::GET, "/v1/models", Some(catalog)) if !is_codex_catalog(req.uri().query()) => {
                return self.list_models(catalog, &req, connection);
            }
            (&Method::POST, path, _) => match api::endpoint(path) {
                Some(endpoint) if self.routes.get(endpoint.api).is_some() => endpoint,
                Some(endpoint) => {
                    let refusal = Refusal::NoRoute {
                        api: endpoint.api,
                        path: path.into(),
                    };
                    return responses::refuse(endpoint.api, &refusal);
                }
                None => return self.local(req.method(), path),
            },
            (method, path, _) => return self.local(method, path),
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
                Api::Messages => AgentHeaders::from_claude_code_headers(req.headers()),
                Api::Responses => AgentHeaders::from_codex_headers(req.headers()),
                Api::ChatCompletions => AgentHeaders::default(),
            },
            identity: None,
            prompt: None,
            requested_model: None,
            upgrade: None,
            upstream: None,
            provider: Provider::of_route(api.route()),
            failovers: Vec::new(),
            started: Instant::now(),
        };
        match self.identify(req.headers(), connection) {
            Ok(identity) => info.identity = identity,
            Err(refusal) => return log.refused(info, responses::refuse(api, &refusal)),
        }
        self.forward(req, info, log).await
    }

    /// The catalog this caller's credential may use. Admitted like a
    /// proxied request, since the answer reveals the credential's
    /// permissions. Not a usage record: nothing reaches the provider.
    fn list_models(
        &self,
        catalog: &ModelCatalog,
        req: &Request<Incoming>,
        connection: &ConnectionAuth,
    ) -> Response<ResBody> {
        let identity = match self.identify(req.headers(), connection) {
            Ok(identity) => identity,
            Err(refusal) => return responses::refuse(Api::Messages, &refusal),
        };
        let body = catalog.list(
            identity.as_ref().and_then(|i| i.models.as_deref()),
            identity.as_ref().map(|i| &*i.subject),
        );
        responses::json(body)
    }
}

/// Codex asks `GET {base_url}/models?client_version=...` for its own catalog
/// format, not Claude Code's: a 404 makes it use its bundled catalog.
fn is_codex_catalog(query: Option<&str>) -> bool {
    query.is_some_and(|q| q.split('&').any(|pair| pair.starts_with("client_version=")))
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
