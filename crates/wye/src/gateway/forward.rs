//! Forwarding a proxied request: the concurrency permit, body admission,
//! sampling, the upstream choice and failover, and the response or Wye's own
//! error answer, recorded by whose it is.

use std::{ops::Range, time::Duration};
use tokio::sync::oneshot::error::TryRecvError;

use bytes::Bytes;
use gateway_core::{
    api::Api,
    route::{FAILOVER_MAX_BODY_BYTES, FailoverReason, affinity_key, sampled},
};
use http::{
    HeaderValue, Request, Response,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};
use http_body_util::Either;
use hyper::body::Incoming;

use super::{
    ApiRoute, Gateway, ResBody,
    admit::{ModelSpan, NotAdmitted},
    content_length,
    responses::{forward_error, overloaded, too_large},
};
use crate::{
    diagnostics::PromptSlot,
    failover::{Bounds, FailoverBody, Handback, Resend},
    request_body::{ClientBody, RequestBody},
    upgrade_check::UpgradeSlot,
    upstream::{ForwardError, Upstream, UpstreamBody, UpstreamError},
    usage_log::{Observed, RejectedAttempt, RequestInfo, Unanswered, UsageLog},
};

/// How long a rejected attempt may take to hand its body back: hyper drops a
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
        let Some(permit) = self.in_flight.try_reserve(self.max_concurrent_requests) else {
            return log.refused(
                info,
                overloaded(api, "gateway is at its concurrent request limit"),
            );
        };
        let declared_len = content_length(req.headers());
        if declared_len.is_some_and(|len| len > self.max_request_body_bytes as u64) {
            return log.refused(info, too_large(api));
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
            Err(not_admitted) => {
                return match *not_admitted {
                    NotAdmitted::Refused(res) => log.refused(pending.answered(), res),
                    NotAdmitted::Fault(res) => log.gateway_error(pending.answered(), res),
                };
            }
        };
        self.sample(pending.info());
        let Some(route) = self.routes.get(api) else {
            unreachable!("handle only forwards endpoints with a route")
        };
        let result = self
            .attempts(route, parts, body, model_span, declared_len, pending.info())
            .await;
        let info = pending.answered();
        match result {
            Ok(res) => {
                let (parts, body) = res.into_parts();
                let content_type = parts.headers.get(CONTENT_TYPE);
                let body =
                    Observed::new(body, info, parts.status, content_type, log.clone(), permit);
                Response::from_parts(parts, Either::Right(body))
            }
            Err(e) => {
                let res = forward_error(api, e);
                match e {
                    ForwardError::Upstream(error) => log.upstream_error(info, error, res),
                    ForwardError::Body(_) => log.refused(info, res),
                }
            }
        }
    }

    /// Sends the request to the upstream its affinity key picks and, after a
    /// rejection that proves it was not processed, to the next upstream in its
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
        // Cache affinity: prompt caches are per provider account.
        let ranking = route.route.rank(
            affinity_key(
                info.session_id.as_ref().map(HeaderValue::as_bytes),
                info.identity.as_ref().map(|i| &*i.subject),
            )
            .unwrap_or_default(),
        );
        // Failover keeps the body for resending, unless it is declared too large.
        let failover = route.route.fails_over()
            && declared_len.is_none_or(|len| len <= FAILOVER_MAX_BODY_BYTES);
        // The declared length before any provider prefix is added to `model`.
        let base_len = content_length(&parts.headers);
        // The provider prefix the body's model carries now; the first
        // attempt gets its upstream's model id.
        let mut prefix_sent = None;
        if let Some(first) = route.upstreams.get(ranking.first()) {
            give_model_id(
                model_span.as_ref(),
                first,
                &mut prefix_sent,
                |span, value| body.splice(span, value),
            );
        }
        let (mut body, mut handback) = if failover {
            let (body, handback) = FailoverBody::kept(body, &self.kept_bytes, self.bounds());
            (body, Some(handback))
        } else {
            (FailoverBody::once(body), None)
        };
        // `forward` rewrites the URI and credential headers per upstream, so
        // another attempt starts from a copy of the request as received.
        let received = failover.then(|| copy_parts(&parts));
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
            info.upstream = Some(upstream.id.clone());
            info.provider = upstream.provider;
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
            if let (Some(reason), Some(handback), Some(next)) =
                (rejection(&result), handback.as_mut(), next)
            {
                let cut = matches!(result, Err(ForwardError::Upstream(UpstreamError::Reset)));
                if let Some(resend) = handed_back(handback, cut).await.and_then(|mut resend| {
                    let at = model_span.as_ref();
                    give_model_id(at, next, &mut prefix_sent, |span, value| {
                        resend.splice(span, value)
                    })
                    .then_some(resend)
                }) {
                    drop(result);
                    info.failovers.push(RejectedAttempt {
                        upstream: upstream.id.clone(),
                        reason,
                    });
                    (body, *handback) = FailoverBody::resume(resend, self.bounds());
                    continue;
                }
            }
            return result;
        }
    }

    /// The bounds a body kept for failover stays within: the per-request
    /// bound and the process-wide one.
    fn bounds(&self) -> Bounds {
        Bounds {
            max_body: usize::try_from(FAILOVER_MAX_BODY_BYTES).unwrap_or(usize::MAX),
            max_kept: self.max_total_kept_bytes,
        }
    }

    /// Whether this request's body is hashed for cache diagnostics and
    /// scanned by upgrade checks: both read Anthropic request bodies, sampled
    /// by session (upgrade checks: by client for SDK callers without one).
    fn sample(&self, info: &mut RequestInfo) {
        if info.endpoint.api != Api::Messages {
            return;
        }
        let session = info.session_id.as_ref().map(HeaderValue::as_bytes);
        if self.prompt_key.is_some() && sampled(session, self.prompt_sample_percent) {
            info.prompt = Some(PromptSlot::default());
        }
        if let Some(check) = &self.upgrade_check
            && sampled(
                affinity_key(session, info.identity.as_ref().map(|i| &*i.subject)),
                check.sample_percent,
            )
        {
            info.upgrade = Some(UpgradeSlot::default());
        }
    }
}

/// The failover reason `result` gives, if any. A lost connection qualifies
/// only if the attempt did not deliver the whole body (checked by
/// [`handed_back`]): a provider can reject early and close while the upload
/// runs, and an incomplete request cannot have been processed.
fn rejection(
    result: &Result<Response<UpstreamBody<Incoming>>, ForwardError>,
) -> Option<FailoverReason> {
    match result {
        Ok(res) => FailoverReason::of_status(res.status().as_u16()),
        Err(ForwardError::Upstream(UpstreamError::Connect | UpstreamError::Reset)) => {
            Some(FailoverReason::Connect)
        }
        Err(_) => None,
    }
}

/// The body a rejected attempt hands back: the bytes it sent plus the unread
/// rest, so a rejection during the upload moves on once hyper drops the body.
/// If the attempt still holds it after [`HANDBACK_WAIT`] (hyper still
/// writing), `None`: the rejection is answered as it is, because dropping the
/// answer first could lose both it and the body. Also `None` when the
/// connection was `cut` after the whole body was delivered.
async fn handed_back(
    handback: &mut Handback<RequestBody<ClientBody>>,
    cut: bool,
) -> Option<Resend<RequestBody<ClientBody>>> {
    let resend = match handback.try_recv() {
        Ok(resend) => Some(resend),
        // The attempt stopped keeping the body: nothing will come, and a
        // receiver that saw this must not be polled again.
        Err(TryRecvError::Closed) => None,
        Err(TryRecvError::Empty) => tokio::time::timeout(HANDBACK_WAIT, &mut *handback)
            .await
            .ok()
            .and_then(Result::ok),
    };
    resend.filter(|resend| !cut || !resend.sent_whole())
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

/// Gives the body's model, at `at`, the id `upstream`'s provider needs
/// through `splice`, when that differs from the `prefix_sent` it carries
/// now. `false` when `splice` cannot rewrite the value: a resend whose value
/// is split between the sent bytes and the unread rest, answered as it is
/// then. A client that sent an already-prefixed id keeps it.
fn give_model_id(
    at: Option<&ModelSpan>,
    upstream: &Upstream,
    prefix_sent: &mut Option<&'static str>,
    splice: impl FnOnce(&Range<u64>, Bytes) -> bool,
) -> bool {
    let Some(at) = at else { return true };
    let wanted = upstream.provider.prefix_for(&at.model);
    if wanted == *prefix_sent {
        return true;
    }
    let value = match wanted {
        // Exact capacity: the Vec becomes the Bytes without another allocation.
        Some(prefix) => Bytes::from([prefix.as_bytes(), at.model.as_bytes()].concat()),
        None => Bytes::copy_from_slice(at.model.as_bytes()),
    };
    // Where the value is while it carries the prefix sent.
    let span = at.span.start..at.span.end + prefix_sent.map_or(0, |p| p.len() as u64);
    let spliced = splice(&span, value);
    if spliced {
        *prefix_sent = wanted;
    }
    spliced
}
