//! Responses Wye generates itself, in the error shape of the request's API.

use bytes::Bytes;
use gateway_core::api::Api;
use http::{HeaderName, HeaderValue, Response, StatusCode, header::CONTENT_TYPE};
use http_body_util::{Either, Full};

use super::ResBody;
use crate::{
    request_body::HoldError,
    upstream::{ForwardError, PolicyRefusal},
};

/// Anthropic's status for an overloaded API; clients back off and retry.
/// OpenAI clients get 503, which they retry likewise.
const OVERLOADED: u16 = 529;
/// Seconds a client waits before retrying an overload Wye generated. Integer
/// seconds at most 60: Claude Code stops retrying above that.
const OVERLOAD_RETRY_AFTER: HeaderValue = HeaderValue::from_static("1");
const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");
const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");

pub(super) fn hold_error(api: Api, e: HoldError) -> Response<ResBody> {
    match e {
        HoldError::Model(e) => error(
            api,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            e.as_str(),
        ),
        HoldError::Overloaded => overloaded(api, "gateway is holding too many request bytes"),
        HoldError::TooLarge => error(
            api,
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds the gateway limit",
        ),
        HoldError::Body => error(
            api,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request body could not be read",
        ),
    }
}

pub(super) fn forward_error(api: Api, e: ForwardError) -> Response<ResBody> {
    match e {
        ForwardError::HeaderTimeout => error(
            api,
            StatusCode::GATEWAY_TIMEOUT,
            "api_error",
            "upstream did not respond in time",
        ),
        ForwardError::Connect | ForwardError::Upstream => error(
            api,
            StatusCode::BAD_GATEWAY,
            "api_error",
            "upstream request failed",
        ),
        ForwardError::Policy(PolicyRefusal::Invalid(e)) => error(
            api,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            e.as_str(),
        ),
        ForwardError::Policy(PolicyRefusal::TokenLimit(message)) => {
            error(api, StatusCode::FORBIDDEN, "permission_error", &message)
        }
    }
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

pub(super) fn empty(status: StatusCode) -> Response<ResBody> {
    let mut res = Response::new(Either::Left(Full::new(Bytes::new())));
    *res.status_mut() = status;
    res
}

/// A missing or invalid gateway credential. One message for every failure:
/// the reason is not the caller's business.
pub(super) fn unauthorized(api: Api) -> Response<ResBody> {
    error(
        api,
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "invalid or missing gateway credential",
    )
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
    let mut res = Response::new(Either::Left(Full::new(Bytes::from(body))));
    *res.status_mut() = status;
    res.headers_mut().insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
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
}
