//! Responses Wye generates itself, in the error shape of the request's API.

use bytes::Bytes;
use gateway_core::{admission::Refusal, api::Api};
use http::{HeaderName, HeaderValue, Response, StatusCode, header::CONTENT_TYPE};
use http_body_util::{Either, Full};

use super::ResBody;
use crate::{
    request_body::HoldError,
    upstream::{BodyRefusal, ForwardError, UpstreamError},
};

/// Anthropic's status for an overloaded API; clients back off and retry.
/// OpenAI clients get 503, which they retry likewise.
const OVERLOADED: u16 = 529;
/// Seconds a client waits before retrying an overload Wye generated. Integer
/// seconds at most 60: Claude Code stops retrying above that.
const OVERLOAD_RETRY_AFTER: HeaderValue = HeaderValue::from_static("1");
const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");
const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");

/// Wye's answer to a request refused while its model was read.
pub(super) fn hold_error(api: Api, e: HoldError) -> Response<ResBody> {
    match e {
        HoldError::Model(e) => refuse(api, &Refusal::Invalid(e)),
        HoldError::Overloaded => overloaded(api, "gateway is holding too many request bytes"),
        HoldError::TooLarge => too_large(api),
        HoldError::Stalled => error(
            api,
            StatusCode::REQUEST_TIMEOUT,
            "invalid_request_error",
            "request body stalled while the gateway read its model",
        ),
        HoldError::Broken => unreadable_body(api),
    }
}

/// The client's body failed: bad framing, or cut off mid-upload.
fn unreadable_body(api: Api) -> Response<ResBody> {
    error(
        api,
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "request body could not be read",
    )
}

/// Wye's answer to an attempt that got no response to relay.
pub(super) fn forward_error(api: Api, e: ForwardError) -> Response<ResBody> {
    match e {
        ForwardError::Upstream(UpstreamError::Timeout) => error(
            api,
            StatusCode::GATEWAY_TIMEOUT,
            "timeout_error",
            "upstream did not respond in time",
        ),
        ForwardError::Upstream(UpstreamError::Connect | UpstreamError::Reset) => error(
            api,
            StatusCode::BAD_GATEWAY,
            "api_error",
            "upstream request failed",
        ),
        ForwardError::Body(BodyRefusal::Invalid(e)) => refuse(api, &Refusal::Invalid(e)),
        ForwardError::Body(BodyRefusal::OutputCeiling(e)) => {
            refuse(api, &Refusal::OutputCeiling(e))
        }
        ForwardError::Body(BodyRefusal::TooLarge) => too_large(api),
        ForwardError::Body(BodyRefusal::Broken) => unreadable_body(api),
    }
}

/// A policy refusal: the same status, type, and message `wye explain`
/// reports.
pub(super) fn refuse(api: Api, refusal: &Refusal) -> Response<ResBody> {
    let status = StatusCode::from_u16(refusal.status()).unwrap_or(StatusCode::FORBIDDEN);
    match refusal.fixed_message() {
        Some(message) => error(api, status, refusal.kind(), message),
        None => error(api, status, refusal.kind(), &refusal.to_string()),
    }
}

/// A body past `server.max_request_body_bytes`, declared or sent.
pub(super) fn too_large(api: Api) -> Response<ResBody> {
    refuse(api, &Refusal::TooLarge)
}

/// An overload Wye generated before contacting the provider: always safe to
/// retry, after a short wait.
pub(super) fn overloaded(api: Api, message: &str) -> Response<ResBody> {
    let status = match api {
        Api::Messages => {
            StatusCode::from_u16(OVERLOADED).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
        }
        Api::Responses | Api::ChatCompletions => StatusCode::SERVICE_UNAVAILABLE,
    };
    let mut res = error(api, status, "overloaded_error", message);
    let headers = res.headers_mut();
    headers.insert(RETRY_AFTER, OVERLOAD_RETRY_AFTER);
    headers.insert(SHOULD_RETRY, HeaderValue::from_static("true"));
    res
}

/// A response with `status` and no body.
pub(super) fn empty(status: StatusCode) -> Response<ResBody> {
    let mut res = Response::new(Either::Left(Full::new(Bytes::new())));
    *res.status_mut() = status;
    res
}

/// A path or method the gateway does not proxy, in the Messages shape.
pub(super) fn not_proxied() -> Response<ResBody> {
    refuse(Api::Messages, &Refusal::NotProxied)
}

/// Gateway-generated errors use the provider's error shape, so clients
/// handle them like upstream errors. `kind` is Anthropic's error type; on
/// OpenAI's APIs it is the `code`, under OpenAI's broader `type`.
pub(super) fn error(
    api: Api,
    status: StatusCode,
    kind: &'static str,
    message: &str,
) -> Response<ResBody> {
    let message = serde_json::to_string(message).unwrap_or_default();
    let body = match api {
        Api::Messages => {
            format!(r#"{{"type":"error","error":{{"type":"{kind}","message":{message}}}}}"#)
        }
        Api::Responses | Api::ChatCompletions => {
            let class = if status.is_server_error() {
                "server_error"
            } else {
                "invalid_request_error"
            };
            format!(
                r#"{{"error":{{"message":{message},"type":"{class}","param":null,"code":"{kind}"}}}}"#
            )
        }
    };
    let mut res = json(Bytes::from(body));
    *res.status_mut() = status;
    res
}

/// A `200` JSON response with `body`.
pub(super) fn json(body: Bytes) -> Response<ResBody> {
    let mut res = Response::new(Either::Left(Full::new(body)));
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    #[tokio::test]
    async fn error_bodies_are_valid_json_whatever_the_message() {
        let res = error(
            Api::Messages,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "a \"quoted\" \\ word",
        );
        let Either::Left(body) = res.into_body() else {
            panic!("a gateway error has a full body");
        };
        let bytes = body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["message"], "a \"quoted\" \\ word");
        assert_eq!(json["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn responses_errors_use_openai_shape() {
        let body = |res: Response<ResBody>| async move {
            let Either::Left(body) = res.into_body() else {
                panic!("a gateway error has a full body");
            };
            let bytes = body.collect().await.unwrap().to_bytes();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        let denied = body(error(
            Api::Responses,
            StatusCode::FORBIDDEN,
            "permission_error",
            "no",
        ))
        .await;
        assert_eq!(denied["error"]["message"], "no");
        assert_eq!(denied["error"]["type"], "invalid_request_error");
        assert_eq!(denied["error"]["code"], "permission_error");
        assert!(denied["error"]["param"].is_null());
        let busy = overloaded(Api::Responses, "busy");
        assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(busy.headers()["retry-after"], "1");
        assert_eq!(body(busy).await["error"]["type"], "server_error");
        assert_eq!(overloaded(Api::Messages, "busy").status().as_u16(), 529);
    }

    /// The API timing out is Anthropic's `timeout_error`; a client that stops
    /// sending its body is the client's request to fix.
    #[tokio::test]
    async fn an_upstream_timeout_and_a_stalled_client_have_their_own_types() {
        let body = |res: Response<ResBody>| async move {
            let Either::Left(body) = res.into_body() else {
                panic!("a gateway error has a full body");
            };
            let bytes = body.collect().await.unwrap().to_bytes();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        let timeout = forward_error(
            Api::Messages,
            ForwardError::Upstream(UpstreamError::Timeout),
        );
        assert_eq!(timeout.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body(timeout).await["error"]["type"], "timeout_error");
        let timeout = body(forward_error(
            Api::Responses,
            ForwardError::Upstream(UpstreamError::Timeout),
        ))
        .await;
        assert_eq!(timeout["error"]["type"], "server_error");
        assert_eq!(timeout["error"]["code"], "timeout_error");

        let stalled = hold_error(Api::Messages, HoldError::Stalled);
        assert_eq!(stalled.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(
            body(stalled).await["error"]["type"],
            "invalid_request_error"
        );
        let stalled = body(hold_error(Api::ChatCompletions, HoldError::Stalled)).await;
        assert_eq!(stalled["error"]["code"], "invalid_request_error");
    }
}
