//! Gateway credentials end to end.

use super::harness::*;

fn token(key: &Ed25519KeyPair, iat: u64, exp: u64) -> String {
    issue(
        key,
        &NewCredential {
            iat,
            exp,
            ..claims("alice")
        },
    )
}

/// Gateway with `[auth]` trusting `key`, recording what the upstream receives.
async fn harness(key: &Ed25519KeyPair) -> (Harness, mpsc::Receiver<HeaderMap>) {
    let (handler, rx) = recording(|seen| seen.headers);
    let auth = format!("api_key_env = \"UNUSED\"\n{}", auth_section(key, ""));
    let h = Harness::start(handler, &auth, UpstreamAuth::api_key(API_KEY).unwrap()).await;
    (h, rx)
}

#[tokio::test]
async fn valid_credential_is_replaced_by_the_provider_key() {
    let key = keypair();
    let (h, mut seen) = harness(&key).await;
    let t = token(&key, now(), now() + 600);

    let res = h
        .send(
            messages().header("authorization", format!("Bearer {t}")),
            "{}",
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let headers = seen.recv().await.unwrap();
    assert_eq!(headers["x-api-key"], API_KEY);
    assert!(headers.get("authorization").is_none());
    let record = h.usage_record().await;
    assert_eq!(record["subject"], "alice");

    // Claude Code's ANTHROPIC_API_KEY sends the credential as x-api-key.
    let res = h.send(messages().header("x-api-key", &t), "{}").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(seen.recv().await.unwrap()["x-api-key"], API_KEY);
}

#[tokio::test]
async fn missing_expired_or_foreign_credentials_never_reach_upstream() {
    let key = keypair();
    let (h, _seen) = harness(&key).await;
    let expired = token(&key, now() - 600, now() - 1);
    let foreign = token(&keypair(), now(), now() + 600);
    for header in [
        None,
        Some(format!("Bearer {expired}")),
        Some(format!("Bearer {foreign}")),
        Some("Bearer not-a-token".to_owned()),
        Some(format!("Basic {}", token(&key, now(), now() + 600))),
    ] {
        let req = match &header {
            Some(value) => messages().header("authorization", value),
            None => messages(),
        };
        let res = h.send(req, "{}").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{header:?}");
        assert!(
            read_body(res)
                .await
                .starts_with(br#"{"type":"error","error":{"type":"authentication_error""#)
        );
    }
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 0);
    // Probes stay open without a credential.
    assert_eq!(
        h.send(Request::head("/api/hello"), "").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        h.send(Request::get("/healthz"), "").await.status(),
        StatusCode::OK
    );
}

fn token_with_models(key: &Ed25519KeyPair, models: &[&str]) -> String {
    issue(
        key,
        &NewCredential {
            models: Some(models),
            ..claims("alice")
        },
    )
}

/// Gateway with `[auth]` trusting `key` and `auth_extra` lines, recording
/// the request bodies the upstream receives.
async fn body_harness(key: &Ed25519KeyPair, auth_extra: &str) -> (Harness, mpsc::Receiver<Bytes>) {
    let (handler, rx) = recording(|seen| seen.body);
    let auth = format!(
        "api_key_env = \"UNUSED\"\n{}",
        auth_section(key, auth_extra)
    );
    let h = Harness::start(handler, &auth, UpstreamAuth::api_key(API_KEY).unwrap()).await;
    (h, rx)
}

const HAIKU: &str =
    r#"{"model":"claude-haiku-4-5","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#;
const SONNET: &str = r#"{"model":"claude-sonnet-4-5","max_tokens":1,"messages":[]}"#;

#[tokio::test]
async fn a_body_wye_cannot_read_as_utf8_json_is_refused_under_a_policy() {
    let key = keypair();
    let (h, _bodies) = body_harness(&key, "").await;
    let t = token_with_models(&key, &["claude-haiku-*"]);
    // Every line counts: a provider may honour the second.
    let with_lines = |name: &'static str, values: &[&'static str]| {
        let mut req = messages().header("authorization", format!("Bearer {t}"));
        let headers = req.headers_mut().unwrap();
        headers.remove(name);
        for value in values {
            headers.append(name, http::HeaderValue::from_static(value));
        }
        req
    };
    let with = |name: &'static str, value: &'static str| with_lines(name, &[value]);
    for (name, values) in [
        ("content-encoding", &["br"][..]),
        ("content-encoding", &["identity", "br"]),
        ("content-encoding", &["identity, br"]),
        ("content-type", &["application/json; charset=utf-16"]),
        (
            "content-type",
            &["application/json", "application/json; charset=utf-7"],
        ),
    ] {
        let res = h.send(with_lines(name, values), HAIKU).await;
        assert_eq!(
            res.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{name}: {values:?}"
        );
    }
    let utf8 = with("content-type", "application/json; charset=UTF-8");
    assert_eq!(h.send(utf8, HAIKU).await.status(), StatusCode::OK);
    assert_eq!(h.upstream_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn credential_models_allow_or_refuse_before_upstream() {
    let key = keypair();
    let (h, mut bodies) = body_harness(&key, "").await;
    let t = token_with_models(&key, &["claude-haiku-*"]);
    let bearer = || messages().header("authorization", format!("Bearer {t}"));

    let res = h.send(bearer(), HAIKU).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(bodies.recv().await.unwrap(), HAIKU, "forwarded unchanged");
    h.usage_record().await;

    let res = h.send(bearer(), SONNET).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let refusal = read_body(res).await;
    assert!(refusal.starts_with(br#"{"type":"error","error":{"type":"permission_error""#));
    // The refusal names the model, so the client knows what to change.
    let refusal = String::from_utf8(refusal.to_vec()).unwrap();
    assert!(refusal.contains("claude-sonnet"), "{refusal}");
    assert_eq!(h.upstream_hits.load(Ordering::SeqCst), 1);
    assert_eq!(h.usage_record().await["status"], 403);

    for (body, status) in [
        (r#"{"max_tokens":1}"#, StatusCode::BAD_REQUEST),
        (
            r#"{"model":"claude-haiku-4-5","model":"claude-opus-4-1"}"#,
            StatusCode::BAD_REQUEST,
        ),
        ("not json", StatusCode::BAD_REQUEST),
    ] {
        let res = h.send(bearer(), body).await;
        assert_eq!(res.status(), status, "{body}");
        h.usage_record().await;
    }
    assert_eq!(h.upstream_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn configured_models_are_a_ceiling_credentials_cannot_widen() {
    let key = keypair();
    let (h, mut bodies) = body_harness(&key, "models = [\"claude-haiku-*\"]").await;
    for t in [
        token(&key, now(), now() + 600),
        token_with_models(&key, &["*"]),
    ] {
        let bearer = || messages().header("authorization", format!("Bearer {t}"));
        assert_eq!(h.send(bearer(), HAIKU).await.status(), StatusCode::OK);
        assert_eq!(bodies.recv().await.unwrap(), HAIKU);
        assert_eq!(
            h.send(bearer(), SONNET).await.status(),
            StatusCode::FORBIDDEN
        );
    }
}

#[tokio::test]
async fn credentials_without_any_model_list_skip_the_check() {
    let key = keypair();
    let (h, mut bodies) = body_harness(&key, "").await;
    let t = token(&key, now(), now() + 600);
    // Not even JSON: without a restriction, Wye does not read the body.
    let res = h
        .send(
            messages().header("authorization", format!("Bearer {t}")),
            "not json",
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(bodies.recv().await.unwrap(), "not json");
}

fn token_with_output_limit(key: &Ed25519KeyPair, max_output_tokens: u64) -> String {
    issue(
        key,
        &NewCredential {
            max_output_tokens: Some(max_output_tokens),
            ..claims("alice")
        },
    )
}

#[tokio::test]
async fn max_tokens_above_the_stricter_limit_is_refused() {
    let key = keypair();
    let (h, mut bodies) = body_harness(&key, "max_output_tokens = 8000").await;
    let t = token_with_output_limit(&key, 4000);
    let bearer = || messages().header("authorization", format!("Bearer {t}"));
    let within = r#"{"model":"claude-haiku-4-5","max_tokens":4000,"messages":[]}"#;
    assert_eq!(h.send(bearer(), within).await.status(), StatusCode::OK);
    assert_eq!(bodies.recv().await.unwrap(), within);
    h.usage_record().await;

    // The credential's 4000 is stricter than the configured 8000.
    let res = h
        .send(
            bearer(),
            r#"{"model":"claude-haiku-4-5","max_tokens":6000,"messages":[]}"#,
        )
        .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let text = read_body(res).await;
    assert!(
        String::from_utf8_lossy(&text)
            .contains("max_tokens 6000 exceeds this gateway token's limit of 4000"),
        "{text:?}"
    );
    assert_eq!(h.upstream_hits.load(Ordering::SeqCst), 1);
    h.usage_record().await;

    // count_tokens generates nothing; the ceiling does not apply.
    let count =
        Request::post("/v1/messages/count_tokens").header("authorization", format!("Bearer {t}"));
    let res = h
        .send(
            count,
            r#"{"model":"claude-haiku-4-5","max_tokens":999999,"messages":[]}"#,
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_late_max_tokens_above_the_limit_aborts_the_upstream_request() {
    let key = keypair();
    let (h, mut bodies) = body_harness(&key, "max_output_tokens = 4000").await;
    late_max_tokens_is_refused(&h, &key, &mut bodies).await;
}

#[tokio::test]
async fn a_late_refusal_gets_the_same_answer_from_an_http2_upstream() {
    // hyper's HTTP/2 client drops the request body's error and resets the
    // stream; the refusal must still reach the client.
    let key = keypair();
    let ca = TestCa::generate();
    let (handler, mut bodies) = recording(|seen| seen.body);
    let auth = auth_section(&key, "max_output_tokens = 4000");
    let h = Harness::start_tls_with(handler, &ca, true, None, &auth).await;
    late_max_tokens_is_refused(&h, &key, &mut bodies).await;
}

#[tokio::test]
async fn a_late_second_model_aborts_the_upstream_request() {
    let key = keypair();
    let (h, mut bodies) = body_harness(&key, "").await;
    let t = token_with_models(&key, &["claude-haiku-*"]);
    let req = messages().header("authorization", format!("Bearer {t}"));
    let chunks: [&[u8]; 2] = [
        br#"{"model":"claude-haiku-4-5","messages":[],"#,
        br#""model":"claude-opus-5-5"}"#,
    ];
    let res = h.send_chunked(req, &chunks).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let text = read_body(res).await;
    assert!(
        String::from_utf8_lossy(&text).contains("request names model more than once"),
        "{text:?}"
    );
    assert!(
        bodies.try_recv().is_err(),
        "the upstream received a complete body"
    );
}

/// The client streams the body, model first and max_tokens in a later
/// chunk; the gateway refuses it and the upstream never sees it complete.
async fn late_max_tokens_is_refused(
    h: &Harness,
    key: &Ed25519KeyPair,
    bodies: &mut mpsc::Receiver<Bytes>,
) {
    let t = token(key, now(), now() + 600);
    let req = messages().header("authorization", format!("Bearer {t}"));
    let chunks: [&[u8]; 2] = [
        br#"{"model":"claude-haiku-4-5","messages":[],"#,
        br#""max_tokens":9000}"#,
    ];
    let res = h.send_chunked(req, &chunks).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let text = read_body(res).await;
    assert!(
        String::from_utf8_lossy(&text).contains("max_tokens 9000"),
        "{text:?}"
    );
    assert!(
        bodies.try_recv().is_err(),
        "the upstream received a complete body"
    );
}

#[tokio::test]
async fn model_discovery_lists_only_permitted_models() {
    let key = keypair();
    let (h, _bodies) = body_harness(
        &key,
        "models = [\"claude-*\"]\n\
         [discovery]\nmodels = [{ id = \"claude-sonnet-4-6\", display_name = \"Sonnet\" }, \
         { id = \"claude-haiku-4-5\" }, { id = \"gpt-5\" }]",
    )
    .await;
    let list = |t: Option<String>| {
        let mut req = Request::get("/v1/models?limit=1000");
        if let Some(t) = t {
            req = req.header("x-api-key", t);
        }
        h.send(req, "")
    };
    let ids = |body: Bytes| -> Vec<String> {
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect()
    };

    let res = list(Some(token(&key, now(), now() + 600))).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(
        ids(read_body(res).await),
        ["claude-sonnet-4-6", "claude-haiku-4-5"],
        "the configured ceiling drops gpt-5"
    );

    let res = list(Some(token_with_models(&key, &["claude-haiku-*"]))).await;
    assert_eq!(ids(read_body(res).await), ["claude-haiku-4-5"]);

    assert_eq!(list(None).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(h.upstream_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn model_discovery_is_absent_without_a_catalog() {
    let key = keypair();
    let (h, _bodies) = body_harness(&key, "").await;
    let req = Request::get("/v1/models").header("x-api-key", token(&key, now(), now() + 600));
    assert_eq!(h.send(req, "").await.status(), StatusCode::NOT_FOUND);
}
