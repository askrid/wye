//! Responses Wye generates itself, in Anthropic's error shape.

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Response, StatusCode, header::CONTENT_TYPE};
use http_body_util::{Either, Full};

use super::ResBody;
use crate::{
    permissions::HoldError,
    upstream::{ForwardError, PolicyRefusal},
};

/// Anthropic's status for an overloaded API; clients back off and retry.
const OVERLOADED: u16 = 529;
/// Seconds a client waits before retrying an overload Wye generated. Integer
/// seconds at most 60: Claude Code stops retrying above that.
const OVERLOAD_RETRY_AFTER: HeaderValue = HeaderValue::from_static("1");
const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");
const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");

pub(super) fn hold_error(e: HoldError) -> Response<ResBody> {
    match e {
        HoldError::Model(e) => error(StatusCode::BAD_REQUEST, "invalid_request_error", e.as_str()),
        HoldError::Overloaded => overloaded("gateway is holding too many request bytes"),
        HoldError::TooLarge => error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds the gateway limit",
        ),
        HoldError::Body => error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request body could not be read",
        ),
    }
}

pub(super) fn forward_error(e: ForwardError) -> Response<ResBody> {
    match e {
        ForwardError::HeaderTimeout => error(
            StatusCode::GATEWAY_TIMEOUT,
            "api_error",
            "upstream did not respond in time",
        ),
        ForwardError::Upstream => error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "upstream request failed",
        ),
        ForwardError::Policy(PolicyRefusal::Invalid(e)) => {
            error(StatusCode::BAD_REQUEST, "invalid_request_error", e.as_str())
        }
        ForwardError::Policy(PolicyRefusal::OverTokenLimit(message)) => {
            error(StatusCode::FORBIDDEN, "permission_error", &message)
        }
    }
}

/// An overload Wye generated before contacting the provider: always safe to
/// retry, after a short wait.
pub(super) fn overloaded(message: &str) -> Response<ResBody> {
    let mut res = error(
        StatusCode::from_u16(OVERLOADED).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        "overloaded_error",
        message,
    );
    let headers = res.headers_mut();
    headers.insert(RETRY_AFTER, OVERLOAD_RETRY_AFTER);
    headers.insert(SHOULD_RETRY, HeaderValue::from_static("true"));
    res
}

pub(super) fn empty(status: StatusCode) -> Response<ResBody> {
    let mut res = Response::new(Either::Left(Full::new(Bytes::new())));
    *res.status_mut() = status;
    res
}

/// A missing or invalid gateway credential. One message for every failure:
/// the reason is not the caller's business.
pub(super) fn unauthorized() -> Response<ResBody> {
    error(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "invalid or missing gateway credential",
    )
}

/// Gateway-generated errors use Anthropic's error shape so clients handle them like upstream errors.
pub(super) fn error(status: StatusCode, kind: &'static str, message: &str) -> Response<ResBody> {
    let message = serde_json::to_string(message).unwrap_or_default();
    let body = format!(r#"{{"type":"error","error":{{"type":"{kind}","message":{message}}}}}"#);
    let mut res = Response::new(Either::Left(Full::new(Bytes::from(body))));
    *res.status_mut() = status;
    res.headers_mut().insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    res
}
