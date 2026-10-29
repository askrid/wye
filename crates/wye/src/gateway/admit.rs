//! Body admission: model aliases, model permissions, and the output-token
//! ceiling, decided while the request body's `model` is held; and where the
//! model's value is, for providers with their own model ids.

use std::{ops::Range, sync::Arc};

use gateway_core::policy;
use http::{HeaderMap, HeaderValue, Response, StatusCode, header::CONTENT_LENGTH};
use http_body_util::Limited;
use hyper::body::{Body, Incoming};

use super::{
    Gateway, ResBody, content_length,
    responses::{error, hold_error},
};
use crate::{
    request_body::{ClientBody, RequestBody, TokenLimitError, hold_until_model},
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
        let credential_models = info.identity.as_ref().and_then(|i| i.models.as_deref());
        // The ceiling bounds generation; count_tokens generates nothing.
        let token_limit = info
            .endpoint
            .generates
            .then(|| {
                policy::token_limit(
                    self.max_output_tokens,
                    info.identity.as_ref().and_then(|i| i.max_output_tokens),
                )
            })
            .flatten();
        // A provider with its own model ids on the route: the model's
        // place in the body is needed.
        let prefixes_model_ids = self.route(api).is_some_and(|r| r.prefixes_model_ids);
        if self.models.is_none()
            && credential_models.is_none()
            && token_limit.is_none()
            && self.aliases.is_none()
            && !prefixes_model_ids
        {
            return Ok((RequestBody::new(body), None));
        }
        let mut held = hold_until_model(
            &mut body,
            api,
            &self.held_bytes,
            self.max_held_request_bytes,
            token_limit,
        )
        .await
        .map_err(|e| Box::new(hold_error(api, e)))?;
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
            info.requested_model = Some(Arc::clone(name));
        }
        if self.models.is_none() && credential_models.is_none() && token_limit.is_none() {
            held.skip_rest_check();
        }
        let model = resolved.as_ref().map(|(_, m)| &**m).or(held.model());
        let allowed =
            model.is_some_and(|m| policy::permitted(self.models.as_deref(), credential_models, m));
        if !allowed {
            // The name the client sent: an alias's model stays config's business.
            let sent = resolved
                .as_ref()
                .map(|(name, _)| &**name)
                .or(held.model())
                .unwrap_or_default();
            return Err(Box::new(error(
                api,
                StatusCode::FORBIDDEN,
                "permission_error",
                &format!("this credential may not use the model {sent:?}"),
            )));
        }
        // A value is final now; a missing field only once the whole body was read.
        let requested = held.max_output_tokens();
        let body_read = held.ended() || body.is_end_stream();
        if let Some(limit) = token_limit
            && (requested.is_some() || body_read)
            && let Some(refusal) = TokenLimitError::check(api, limit, requested)
        {
            let message = refusal.to_string();
            return Err(Box::new(error(
                api,
                StatusCode::FORBIDDEN,
                "permission_error",
                &message,
            )));
        }
        let model_span = prefixes_model_ids
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
