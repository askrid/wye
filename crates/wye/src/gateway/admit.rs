//! Body admission: model aliases, model permissions, and the output-token
//! ceiling, decided while the request body's `model` is held.

use std::sync::Arc;

use gateway_core::policy;
use http::{HeaderMap, HeaderValue, Response, StatusCode, header::CONTENT_LENGTH};
use hyper::body::Incoming;

use super::{
    Gateway, ResBody,
    responses::{error, hold_error},
};
use crate::{
    permissions::{OverTokenLimit, RequestBody, hold_until_model},
    usage_log::RequestInfo,
};

impl Gateway {
    /// Resolves model aliases and checks the request body against model
    /// permissions and the output-token ceiling. Without any of them, the
    /// body passes untouched; otherwise it is held until its `model` is read,
    /// then replayed (with an alias replaced by its model, and `headers`'
    /// `content-length` adjusted) and checked as it streams.
    pub(super) async fn admit_body(
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
