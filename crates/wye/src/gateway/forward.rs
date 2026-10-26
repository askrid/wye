//! Forwarding a proxied request: the concurrency permit, body admission,
//! sampling, the upstream choice, and the response or gateway error.

use gateway_core::{api::Api, prompt};
use http::{
    HeaderValue, Request, Response, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};
use http_body_util::Either;
use hyper::body::Incoming;

use super::{
    Gateway, ResBody,
    responses::{error, forward_error, overloaded},
};
use crate::{
    diagnostics::PromptSlot,
    upgrade_check::UpgradeSlot,
    usage_log::{BodyKind, Observed, RequestInfo, UsageLog},
};

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
        let body = match self.admit_body(body, &mut info, &mut parts.headers).await {
            Ok(body) => body,
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
        let Some(upstream) = route.upstreams.get(route.route.pick(key)) else {
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
}
