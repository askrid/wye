//! Body admission: model aliases, model permissions, and the output-token
//! ceiling, decided while the request body's `model` is held; and where the
//! model's value is, for providers with their own model ids.

use std::{ops::Range, sync::Arc};

use gateway_core::{
    admission::{BodyChecks, Refusal},
    policy::PolicyError,
};
use http::{
    HeaderMap, HeaderValue, Response, StatusCode,
    header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE},
};
use http_body_util::Limited;
use hyper::body::{Body, Incoming};

use super::{
    Gateway, ResBody, content_length,
    responses::{self, error, hold_error},
};
use crate::{
    request_body::{ClientBody, RequestBody, hold_until_model},
    usage_log::RequestInfo,
};

/// Where the `model` value is in the body as forwarded, and what it says:
/// the model that runs (an alias already resolved).
pub(super) struct ModelSpan {
    pub(super) span: Range<u64>,
    pub(super) model: Arc<str>,
}

impl Gateway {
    /// Resolves model aliases and checks the request body against model
    /// permissions and the output-token ceiling. Without any of them, the
    /// body passes untouched; otherwise it is held until its `model` is read,
    /// then replayed (with an alias replaced by its model, and `headers`'
    /// `content-length` adjusted) and checked as it streams.
    pub(super) async fn admit_body(
        &self,
        body: Incoming,
        info: &mut RequestInfo,
        headers: &mut HeaderMap,
    ) -> Result<(RequestBody<ClientBody>, Option<ModelSpan>), Box<Response<ResBody>>> {
        let api = info.endpoint.api;
        let mut body = Limited::new(body, self.max_request_body_bytes);
        let route = self.route(api);
        let checks = BodyChecks::new(
            self.models.as_deref(),
            self.max_output_tokens,
            self.aliases.as_ref(),
            info.identity.as_deref(),
            info.endpoint.generates,
            route.is_some_and(|r| r.prefixes_model_ids),
        );
        if !checks.hold() {
            return Ok((RequestBody::new(body), None));
        }
        if let Some(reason) = unreadable(headers) {
            return Err(Box::new(error(
                api,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "invalid_request_error",
                reason,
            )));
        }
        let mut held = hold_until_model(
            &mut body,
            api,
            &self.held_bytes,
            self.max_held_request_bytes,
            checks.token_limit,
        )
        .await
        .map_err(|e| Box::new(hold_error(api, e)))?;
        if !checks.checks_rest() {
            held.skip_rest_check();
        }
        let refuse = |refusal| Box::new(responses::refuse(api, &refusal));
        let Some(sent) = held.model() else {
            return Err(refuse(Refusal::Invalid(PolicyError::Missing)));
        };
        // Permissions and pricing apply to the model that runs.
        let subject = info.identity.as_ref().map(|i| &*i.subject);
        let resolved = checks
            .resolve(sent, subject)
            .map(|(name, model)| (Arc::clone(name), Arc::clone(model)));
        if let Some((name, _)) = &resolved {
            info.requested_model = Some(Arc::clone(name));
        }
        let model = resolved.as_ref().map_or(sent, |(_, model)| &**model);
        checks.permit(model, sent).map_err(refuse)?;
        if let Some((_, model)) = &resolved {
            let spliced = self
                .alias_models
                .get(model)
                .and_then(|bytes| held.splice_model(bytes.clone()));
            let Some(delta) = spliced else {
                return Err(Box::new(error(
                    api,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "model alias could not be applied",
                )));
            };
            adjust_content_length(headers, delta);
        }
        let body_read = held.ended() || body.is_end_stream();
        checks
            .check_tokens(api, held.max_output_tokens(), body_read)
            .map_err(refuse)?;
        let model_span = checks
            .prefixes_model_ids
            .then(|| {
                let start = held.model_span()?.start;
                let model: Arc<str> = match &resolved {
                    Some((_, model)) => Arc::clone(model),
                    None => Arc::from(held.model()?),
                };
                Some(ModelSpan {
                    span: start..start + model.len() as u64,
                    model,
                })
            })
            .flatten();
        Ok((RequestBody::after_hold(held, body), model_span))
    }
}

/// Why the body cannot be read as the provider would read it: Wye checks
/// UTF-8 JSON, and a provider that honours a content encoding or another
/// charset could find other keys in the same bytes.
fn unreadable(headers: &HeaderMap) -> Option<&'static str> {
    // Every line and every listed coding: a provider may apply any of them.
    let encoded = headers.get_all(CONTENT_ENCODING).iter().any(|v| {
        v.to_str().map_or(true, |v| {
            v.split(',')
                .any(|coding| !coding.trim().eq_ignore_ascii_case("identity"))
        })
    });
    if encoded {
        return Some("the request body must not be compressed");
    }
    let mut types = headers.get_all(CONTENT_TYPE).iter();
    let content_type = types.next();
    if types.next().is_some() {
        // Which one a provider reads is not defined.
        return Some("the request body must be UTF-8 JSON");
    }
    let charset = content_type.and_then(|v| v.to_str().ok()).and_then(|v| {
        v.split(';').skip(1).find_map(|param| {
            let (name, value) = param.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches('"'))
        })
    });
    match charset {
        Some(c) if !c.eq_ignore_ascii_case("utf-8") && !c.eq_ignore_ascii_case("utf8") => {
            Some("the request body must be UTF-8 JSON")
        }
        _ => None,
    }
}

/// Keeps a declared `content-length` true after the body changed by `delta`
/// bytes; a chunked body has none to fix.
fn adjust_content_length(headers: &mut HeaderMap, delta: i64) {
    if delta == 0 {
        return;
    }
    let Some(len) = content_length(headers) else {
        return;
    };
    if let Some(len) = len.checked_add_signed(delta) {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
    }
}
