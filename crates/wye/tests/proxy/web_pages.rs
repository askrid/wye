//! A gateway without `[auth]` serves programs on this machine, not web
//! pages: a page the user visits reaches the loopback port too, through DNS
//! rebinding or a cross-origin fetch, and must not spend its provider key or
//! read the replies.

use gateway_core::admission::Refusal;
use wye::{CallerToken, explain, request_headers};

use super::harness::*;

/// What a page's request carries: the attacker's name for 127.0.0.1 in
/// `Host` (DNS rebinding), an `Origin`, or a cross-site `Sec-Fetch-Site`.
fn from_pages(port: u16) -> [(&'static str, String); 4] {
    [
        ("host", format!("rebound.evil.example:{port}")),
        ("origin", "http://evil.example".to_owned()),
        ("origin", "null".to_owned()),
        ("sec-fetch-site", "cross-site".to_owned()),
    ]
}

#[tokio::test]
async fn a_gateway_without_auth_refuses_what_a_web_page_sends() {
    let h = Harness::provider_key(ok()).await;
    let port = h.gateway.port();
    for (name, value) in from_pages(port) {
        let res = h.send(messages().header(name, value.as_str()), "{}").await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{name}: {value}");
        let body: serde_json::Value = serde_json::from_slice(&read_body(res).await).unwrap();
        assert_eq!(body["error"]["type"], "permission_error");
        assert_eq!(
            body["error"]["message"].as_str(),
            Refusal::WebPage.fixed_message()
        );
        let record = h.usage_record().await;
        assert_eq!(record["outcome"], "refused");
        assert_eq!(record["status"], 403);
    }
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 0);

    // Programs name the gateway by a loopback host, as `ANTHROPIC_BASE_URL`
    // has it; a navigation the user typed says `none`.
    for host in [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        "localhost".to_owned(),
    ] {
        let req = messages()
            .header("host", host.as_str())
            .header("sec-fetch-site", "none");
        assert_eq!(h.send(req, "{}").await.status(), StatusCode::OK, "{host}");
        h.usage_record().await;
    }

    // Health checks answer anyone: they carry nothing.
    for path in ["/healthz", "/readyz"] {
        let req = Request::get(path)
            .header("host", "rebound.evil.example")
            .header("origin", "http://evil.example");
        assert_eq!(h.send(req, "").await.status(), StatusCode::OK, "{path}");
    }
}

/// The catalog says which models a credential may use: a page may not read
/// it either.
#[tokio::test]
async fn model_discovery_without_auth_refuses_a_web_page() {
    let catalog = "[discovery]\nmodels = [{ id = \"claude-sonnet-4-6\" }]";
    let h = Harness::keyed(ok(), catalog).await;
    let res = h
        .send(
            Request::get("/v1/models").header("origin", "http://evil.example"),
            "",
        )
        .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        h.send(Request::get("/v1/models"), "").await.status(),
        StatusCode::OK
    );
}

/// With gateway tokens, the token is the credential, wherever the request
/// comes from.
#[tokio::test]
async fn a_gateway_with_auth_checks_the_token_not_the_host() {
    let key = keypair();
    let h = Harness::keyed(ok(), &auth_section(&key, "")).await;
    let token = token(&key, "alice", None, None);
    for (name, value) in from_pages(h.gateway.port()) {
        let res = h
            .send(bearer(&token).header(name, value.as_str()), "{}")
            .await;
        assert_eq!(res.status(), StatusCode::OK, "{name}: {value}");
    }
}

/// `wye explain` refuses what the server refuses, word for word, and
/// explains a request without headers as a local client's.
#[tokio::test]
async fn explain_and_the_server_answer_alike_for_web_pages() {
    let h = Harness::provider_key(ok()).await;
    let body = r#"{"model":"claude-haiku-4-5","max_tokens":5}"#;
    let mut cases: Vec<(&str, String)> = from_pages(h.gateway.port()).into();
    cases.push(("sec-fetch-site", "none".to_owned()));
    for (name, value) in cases {
        let line = format!("{name}: {value}");
        let headers = request_headers("/v1/messages", None, &[&line]).unwrap();
        let explained = explain(
            &h.config,
            "/v1/messages",
            CallerToken::None,
            &headers,
            body.as_bytes(),
            now(),
            &[],
        )
        .refused
        .map(|(status, kind, message)| (status, kind.to_owned(), message));
        let res = h.send(messages().header(name, value.as_str()), body).await;
        let status = res.status().as_u16();
        let answer = read_body(res).await;
        let served = (status != 200).then(|| {
            let error: serde_json::Value = serde_json::from_slice(&answer).unwrap();
            (
                status,
                error["error"]["type"].as_str().unwrap().to_owned(),
                error["error"]["message"].as_str().unwrap().to_owned(),
            )
        });
        assert_eq!(served, explained, "{line}");
        h.usage_record().await;
    }
    let local = explain(
        &h.config,
        "/v1/messages",
        CallerToken::None,
        &HeaderMap::new(),
        body.as_bytes(),
        now(),
        &[],
    );
    assert_eq!(local.refused, None, "{}", local.report());
}
