//! Forwarding a proxied request: the concurrency permit, body admission,
//! sampling, the upstream choice and failover, and the response or gateway
//! error.

use std::{ops::Range, time::Duration};

use bytes::Bytes;
use gateway_core::{api::Api, prompt, route};
use http::{
    HeaderValue, Request, Response, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};
use http_body_util::Either;
use hyper::body::Incoming;

use super::{
    Gateway, ResBody,
    admit::ModelAt,
    responses::{error, forward_error, overloaded},
};
use crate::{
    diagnostics::PromptSlot,
    failover::{Bounds, FailoverBody},
    upgrade_check::UpgradeSlot,
    upstream::{ForwardError, Upstream},
    usage_log::{BodyKind, Failover, Observed, RequestInfo, UsageLog},
};

/// How long a refused attempt may take to hand its body back: hyper drops a
/// fully sent body as the response arrives, so this is a race, not a wait.
const HANDBACK_WAIT: Duration = Duration::from_millis(100);

impl Gateway {
    pub(super) async fn forward(
        &self,
        req: Request<Incoming>,
        mut info: RequestInfo,
        log: &UsageLog,
    ) -> Response<ResBody> {
        let api = info.endpoint.api;
        let Some(permit) = self.in_flight.try_hold(self.max_concurrent_requests) else {
            let res = overloaded(api, "gateway is at its concurrent request limit");
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
                api,
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "request body exceeds the gateway limit",
            );
            log.gateway_error(info, res.status());
            return res;
        }
        let (mut parts, body) = req.into_parts();
        let (body, model_at) = match self.admit_body(body, &mut info, &mut parts.headers).await {
            Ok(admitted) => admitted,
            Err(res) => {
                log.gateway_error(info, res.status());
                return *res;
            }
        };
        let req = Request::from_parts(parts, body);
        // Prompt hashes and upgrade checks read Anthropic request bodies.
        let messages = api == Api::Messages;
        if messages
            && self.prompt_key.is_some()
            && prompt::session_sampled(
                info.session_id.as_ref().map(HeaderValue::as_bytes),
                self.prompt_sample_percent,
            )
        {
            info.prompt = Some(PromptSlot::default());
        }
        // Sampled by session, or by client for SDK callers without one.
        if messages
            && let Some(check) = &self.upgrade_check
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
        let Some(route) = self.route(api) else {
            unreachable!("handle only forwards endpoints with a route")
        };
        let ranking = route.route.rank(key);
        // Failover keeps the body for resending, unless it is declared too large.
        let failover = route
            .route
            .failover()
            .filter(|f| declared_len.is_none_or(|len| len <= f.max_body_bytes))
            .copied();
        let (parts, mut body) = req.into_parts();
        // The model as the first upstream's provider spells it.
        let base_len = content_length(&parts.headers);
        // The provider prefix the body's model carries now.
        let mut spelled = None;
        if let (Some(at), Some(upstream)) = (&model_at, route.upstreams.get(ranking.first()))
            && let Some((prefix, value)) = respelling(at, upstream, spelled)
            && body.respell(&at.span, &value).is_some()
        {
            spelled = prefix;
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
            let (Some(index), Some(attempt)) = (
                order.next(),
                parts.take().or_else(|| received.as_ref().map(copy_parts)),
            ) else {
                unreachable!("an attempt has an upstream and the request")
            };
            let Some(upstream) = route.upstreams.get(index) else {
                unreachable!("the route and its upstreams have the same length")
            };
            let mut attempt = attempt;
            if let (Some(prefix), Some(len)) = (spelled, base_len) {
                attempt
                    .headers
                    .insert(CONTENT_LENGTH, HeaderValue::from(len + prefix.len() as u64));
            }
            info.upstream = upstream.id.clone();
            // Prompt hashes and upgrade checks read the body once, on the first attempt.
            let first = info.failovers.is_empty();
            let result = upstream
                .forward(
                    Request::from_parts(attempt, body),
                    self.max_request_body_bytes,
                    info.prompt.clone().filter(|_| first),
                    info.upgrade.clone().filter(|_| first),
                )
                .await;
            let refusal = match (&result, &failover) {
                (Ok(res), Some(f)) if f.retries_status(res.status().as_u16()) => {
                    Some(Some(res.status().as_u16()))
                }
                (Err(ForwardError::Connect), Some(f)) if f.on_connect => Some(None),
                _ => None,
            };
            if let (Some(status), Some(f), Some(handback), true) = (
                refusal,
                &failover,
                handback.as_mut(),
                order.peek().is_some(),
            ) {
                // Resend only a body handed back whole. A refusal that came
                // while the upload still runs is answered as it is: dropping
                // it first could lose both the body and the answer.
                let unsent = match handback.try_recv() {
                    Ok(unsent) => Some(unsent),
                    Err(_) => tokio::time::timeout(HANDBACK_WAIT, &mut *handback)
                        .await
                        .ok()
                        .and_then(Result::ok),
                };
                // The next upstream's provider may spell the model differently.
                let next = order.peek().and_then(|&i| route.upstreams.get(i));
                let unsent = match (unsent, &model_at, next) {
                    (Some(mut unsent), Some(at), Some(next)) => {
                        match respelling(at, next, spelled) {
                            None => Some(unsent),
                            // A value split between the sent bytes and the
                            // unread rest cannot be re-spelled: answer as is.
                            Some((prefix, value)) => {
                                unsent.respell(&spelled_span(at, spelled), &value).then(|| {
                                    spelled = prefix;
                                    unsent
                                })
                            }
                        }
                    }
                    (unsent, _, _) => unsent,
                };
                if let Some(unsent) = unsent {
                    drop(result);
                    info.failovers.push(Failover {
                        upstream: upstream.id.clone(),
                        status,
                    });
                    let (next, next_handback) = FailoverBody::resume(unsent, bounds(f));
                    body = next;
                    *handback = next_handback;
                    continue;
                }
            }
            return match result {
                Ok(res) => {
                    let status = res.status();
                    let kind =
                        BodyKind::of(res.headers().get(CONTENT_TYPE).map(HeaderValue::as_bytes));
                    let log = log.clone();
                    res.map(|body| {
                        Either::Right(Observed::new(body, info, status, kind, log, permit))
                    })
                }
                Err(e) => {
                    let res = forward_error(api, e);
                    log.gateway_error(info, res.status());
                    res
                }
            };
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
/// carries `spelled` and that differs. A client that sent a provider-spelled
/// id keeps it.
fn respelling(
    at: &ModelAt,
    upstream: &Upstream,
    spelled: Option<&'static str>,
) -> Option<(Option<&'static str>, Bytes)> {
    let wanted = upstream
        .model_prefix
        .filter(|prefix| !at.model.starts_with(prefix));
    (wanted != spelled).then(|| {
        let value = match wanted {
            Some(prefix) => Bytes::from(format!("{prefix}{}", at.model)),
            None => Bytes::copy_from_slice(at.model.as_bytes()),
        };
        (wanted, value)
    })
}

/// Where the model's value is in the body while it carries `spelled`.
fn spelled_span(at: &ModelAt, spelled: Option<&'static str>) -> Range<u64> {
    at.span.start..at.span.end + spelled.map_or(0, |p| p.len() as u64)
}

fn content_length(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}
