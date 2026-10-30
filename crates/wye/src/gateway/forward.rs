//! Forwarding a proxied request: the concurrency permit, body admission,
//! sampling, the upstream choice and failover, and the response or gateway
//! error.

use std::{ops::Range, time::Duration};

use bytes::Bytes;
use gateway_core::{
    api::Api,
    prompt,
    route::{self, FailoverReason, affinity_key},
};
use http::{
    HeaderValue, Request, Response,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};
use http_body_util::Either;
use hyper::body::Incoming;

use super::{
    ApiRoute, Gateway, ResBody,
    admit::ModelSpan,
    content_length,
    responses::{forward_error, overloaded, too_large},
};
use crate::{
    diagnostics::PromptSlot,
    failover::{Bounds, FailoverBody, Handback, Resend},
    request_body::{ClientBody, RequestBody},
    upgrade_check::UpgradeSlot,
    upstream::{ForwardError, Upstream, UpstreamBody},
    usage_log::{BodyKind, Observed, RefusedAttempt, RequestInfo, Unanswered, UsageLog},
};

/// How long a refused attempt may take to hand its body back: hyper drops a
/// fully sent body as the response arrives, so this is a race, not a wait.
const HANDBACK_WAIT: Duration = Duration::from_millis(100);

impl Gateway {
    pub(super) async fn forward(
        &self,
        req: Request<Incoming>,
        info: RequestInfo,
        log: &UsageLog,
    ) -> Response<ResBody> {
        let api = info.endpoint.api;
        let Some(permit) = self.in_flight.try_hold(self.max_concurrent_requests) else {
            let res = overloaded(api, "gateway is at its concurrent request limit");
            log.gateway_error(info, res.status());
            return res;
        };
        let declared_len = content_length(req.headers());
        if declared_len.is_some_and(|len| len > self.max_request_body_bytes as u64) {
            let res = too_large(api);
            log.gateway_error(info, res.status());
            return res;
        }
        // The awaits below read the body and wait for the upstream; if the
        // client leaves meanwhile, hyper drops this future and `pending`
        // records the request.
        let mut pending = Unanswered::new(info, log);
        let (mut parts, body) = req.into_parts();
        let admitted = self
            .admit_body(body, pending.info(), &mut parts.headers)
            .await;
        let (body, model_span) = match admitted {
            Ok(admitted) => admitted,
            Err(res) => {
                log.gateway_error(pending.answered(), res.status());
                return *res;
            }
        };
        self.sample(pending.info());
        let Some(route) = self.route(api) else {
            unreachable!("handle only forwards endpoints with a route")
        };
        let result = self
            .attempts(route, parts, body, model_span, declared_len, pending.info())
            .await;
        let info = pending.answered();
        match result {
            Ok(res) => {
                let status = res.status();
                let kind = BodyKind::of(res.headers().get(CONTENT_TYPE).map(HeaderValue::as_bytes));
                let log = log.clone();
                res.map(|body| Either::Right(Observed::new(body, info, status, kind, log, permit)))
            }
            Err(e) => {
                let res = forward_error(api, e);
                log.gateway_error(info, res.status());
                res
            }
        }
    }

    /// Sends the request to the upstream its affinity key picks and, after a
    /// refusal that proves it was not processed, to the next upstream in its
    /// ranking, as long as failover can resend the body.
    async fn attempts(
        &self,
        route: &ApiRoute,
        parts: http::request::Parts,
        mut body: RequestBody<ClientBody>,
        model_span: Option<ModelSpan>,
        declared_len: Option<u64>,
        info: &mut RequestInfo,
    ) -> Result<Response<UpstreamBody<Incoming>>, ForwardError> {
        // Session affinity: prompt caches are per provider account.
        let ranking = route.route.rank(
            affinity_key(
                info.session_id.as_ref().map(HeaderValue::as_bytes),
                info.identity.as_ref().map(|i| &*i.subject),
            )
            .unwrap_or_default(),
        );
        // Failover keeps the body for resending, unless it is declared too large.
        let failover = route
            .route
            .failover()
            .filter(|f| declared_len.is_none_or(|len| len <= f.max_body_bytes))
            .copied();
        // The declared length before any provider prefix is added to `model`.
        let base_len = content_length(&parts.headers);
        // The provider prefix the body's model carries now; the first
        // attempt gets its upstream's model id.
        let mut prefix_sent = None;
        if let (Some(at), Some(upstream)) = (&model_span, route.upstreams.get(ranking.first()))
            && let Some((prefix, value)) = model_id_for(at, upstream, prefix_sent)
            && body.splice(&at.span, value).is_some()
        {
            prefix_sent = prefix;
        }
        let (mut body, mut handback) = match &failover {
            Some(f) => {
                let (body, handback) = FailoverBody::kept(body, &self.failover_held, bounds(f));
                (body, Some(handback))
            }
            None => (FailoverBody::once(body), None),
        };
        // `forward` rewrites the URI and credential headers per upstream, so
        // another attempt starts from a copy of the request as received.
        let received = failover.as_ref().map(|_| copy_parts(&parts));
        let mut parts = Some(parts);
        let mut order = ranking.iter().peekable();
        loop {
            let (Some(index), Some(mut attempt)) = (
                order.next(),
                parts.take().or_else(|| received.as_ref().map(copy_parts)),
            ) else {
                unreachable!("an attempt has an upstream and the request")
            };
            let Some(upstream) = route.upstreams.get(index) else {
                unreachable!("the route and its upstreams have the same length")
            };
            if let (Some(prefix), Some(len)) = (prefix_sent, base_len) {
                attempt
                    .headers
                    .insert(CONTENT_LENGTH, HeaderValue::from(len + prefix.len() as u64));
            }
            info.upstream = upstream.id.clone();
            // Prompt hashes and upgrade checks read the body once: on the first
            // attempt that sends it whole (a resend replays it from the start).
            let result = upstream
                .forward(
                    Request::from_parts(attempt, body),
                    info.prompt.clone().filter(|slot| slot.get().is_none()),
                    info.upgrade.clone().filter(|slot| slot.get().is_none()),
                )
                .await;
            let next = order.peek().and_then(|&i| route.upstreams.get(i));
            if let (Some(reason), Some(f), Some(handback), Some(next)) = (
                refusal(&result, failover.as_ref()),
                &failover,
                handback.as_mut(),
                next,
            ) {
                let cut = matches!(result, Err(ForwardError::Upstream));
                if let Some(resend) = handed_back(handback, cut).await.and_then(|resend| {
                    with_model_id(resend, model_span.as_ref(), next, &mut prefix_sent)
                }) {
                    drop(result);
                    info.failovers.push(RefusedAttempt {
                        upstream: upstream.id.clone(),
                        reason,
                    });
                    (body, *handback) = FailoverBody::resume(resend, bounds(f));
                    continue;
                }
            }
            return result;
        }
    }
}

/// The failover reason `result` gives, if the route acts on it. A lost connection
/// qualifies only if the attempt did not deliver the whole body (checked by
/// [`handed_back`]): a provider can refuse early and close while the upload
/// runs, and an incomplete request cannot have been processed.
fn refusal(
    result: &Result<Response<UpstreamBody<Incoming>>, ForwardError>,
    failover: Option<&route::Failover>,
) -> Option<FailoverReason> {
    let reason = match result {
        Ok(res) => FailoverReason::of_status(res.status().as_u16())?,
        Err(ForwardError::Connect | ForwardError::Upstream) => FailoverReason::Connect,
        Err(_) => return None,
    };
    failover?.retries(reason).then_some(reason)
}

/// The body a refused attempt hands back: the bytes it sent plus the unread
/// rest, so a refusal during the upload moves on once hyper drops the body.
/// If the attempt still holds it after [`HANDBACK_WAIT`] (hyper still
/// writing), `None`: the refusal is answered as it is, because dropping the
/// answer first could lose both it and the body. Also `None` when the
/// connection was `cut` after the whole body was delivered.
async fn handed_back(
    handback: &mut Handback<RequestBody<ClientBody>>,
    cut: bool,
) -> Option<Resend<RequestBody<ClientBody>>> {
    let resend = match handback.try_recv() {
        Ok(resend) => Some(resend),
        Err(_) => tokio::time::timeout(HANDBACK_WAIT, &mut *handback)
            .await
            .ok()
            .and_then(Result::ok),
    };
    resend.filter(|resend| !cut || !resend.sent_whole())
}

/// `resend` with the model id `next`'s provider needs, or `None` when the
/// value is split between the sent bytes and the unread rest and cannot be
/// rewritten: the refusal is then answered as it is.
fn with_model_id(
    mut resend: Resend<RequestBody<ClientBody>>,
    at: Option<&ModelSpan>,
    next: &Upstream,
    prefix_sent: &mut Option<&'static str>,
) -> Option<Resend<RequestBody<ClientBody>>> {
    let Some(at) = at else { return Some(resend) };
    match model_id_for(at, next, *prefix_sent) {
        None => Some(resend),
        Some((prefix, value)) => {
            resend
                .splice(&model_id_span(at, *prefix_sent), value)
                .then(|| {
                    *prefix_sent = prefix;
                    resend
                })
        }
    }
}

impl Gateway {
    /// Whether this request's body is hashed for cache diagnostics and
    /// scanned by upgrade checks: both read Anthropic request bodies, sampled
    /// by session (upgrade checks: by client for SDK callers without one).
    fn sample(&self, info: &mut RequestInfo) {
        if info.endpoint.api != Api::Messages {
            return;
        }
        let session = info.session_id.as_ref().map(HeaderValue::as_bytes);
        if self.prompt_key.is_some() && prompt::session_sampled(session, self.prompt_sample_percent)
        {
            info.prompt = Some(PromptSlot::default());
        }
        if let Some(check) = &self.upgrade_check
            && prompt::session_sampled(
                affinity_key(session, info.identity.as_ref().map(|i| &*i.subject)),
                check.sample_percent,
            )
        {
            info.upgrade = Some(UpgradeSlot::default());
        }
    }
}

fn bounds(failover: &route::Failover) -> Bounds {
    Bounds {
        max_body: usize::try_from(failover.max_body_bytes).unwrap_or(usize::MAX),
        max_held: usize::try_from(failover.max_held_bytes).unwrap_or(usize::MAX),
    }
}

/// The request's method, URI, version, and headers; extensions are not
/// forwarded anyway.
fn copy_parts(parts: &http::request::Parts) -> http::request::Parts {
    let (mut copy, ()) = Request::new(()).into_parts();
    copy.method = parts.method.clone();
    copy.uri = parts.uri.clone();
    copy.version = parts.version;
    copy.headers = parts.headers.clone();
    copy
}

/// The prefix and value the body's model needs for `upstream`, when it now
/// carries `prefix_sent` and that differs. A client that sent an
/// already-prefixed id keeps it.
fn model_id_for(
    at: &ModelSpan,
    upstream: &Upstream,
    prefix_sent: Option<&'static str>,
) -> Option<(Option<&'static str>, Bytes)> {
    let wanted = upstream
        .model_prefix
        .filter(|prefix| !at.model.starts_with(prefix));
    (wanted != prefix_sent).then(|| {
        let value = match wanted {
            // Exact capacity: the Vec becomes the Bytes without another allocation.
            Some(prefix) => Bytes::from([prefix.as_bytes(), at.model.as_bytes()].concat()),
            None => Bytes::copy_from_slice(at.model.as_bytes()),
        };
        (wanted, value)
    })
}

/// Where the model's value is in the body while it carries `prefix_sent`.
fn model_id_span(at: &ModelSpan, prefix_sent: Option<&'static str>) -> Range<u64> {
    at.span.start..at.span.end + prefix_sent.map_or(0, |p| p.len() as u64)
}
