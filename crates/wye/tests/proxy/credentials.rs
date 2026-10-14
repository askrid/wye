//! Gateway credentials end to end.

use gateway_core::credential::{self, NewCredential};
use ring::signature::{Ed25519KeyPair, KeyPair};

use super::harness::*;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

use wye_testkit::ed25519_keypair as keypair;

fn token(key: &Ed25519KeyPair, iat: u64, exp: u64) -> String {
    credential::issue(
        key,
        "k1",
        &NewCredential {
            iss: "wye",
            sub: "alice",
            aud: "wye-prod",
            iat,
            exp,
            models: None,
            max_output_tokens: None,
        },
    )
}

/// Gateway with `[auth]` trusting `key`, recording what the upstream receives.
async fn harness(key: &Ed25519KeyPair) -> (Harness, mpsc::Receiver<HeaderMap>) {
    let (tx, rx) = mpsc::channel(8);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(seen.headers).await.unwrap();
            full(StatusCode::OK, "{}")
        })
    });
    let auth = format!(
        "api_key_env = \"UNUSED\"\n\n[auth]\nissuer = \"wye\"\naudience = \"wye-prod\"\n\
         [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{}\"",
        credential::encode_public_key(key.public_key().as_ref())
    );
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
    credential::issue(
        key,
        "k1",
        &NewCredential {
            iss: "wye",
            sub: "alice",
            aud: "wye-prod",
            iat: now(),
            exp: now() + 600,
            models: Some(models),
            max_output_tokens: None,
        },
    )
}

/// Gateway with `[auth]` trusting `key` and `auth_extra` lines, recording
/// the request bodies the upstream receives.
async fn body_harness(key: &Ed25519KeyPair, auth_extra: &str) -> (Harness, mpsc::Receiver<Bytes>) {
    let (tx, rx) = mpsc::channel(8);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(seen.body).await.unwrap();
            full(StatusCode::OK, "{}")
        })
    });
    let auth = format!(
        "api_key_env = \"UNUSED\"\n\n[auth]\nissuer = \"wye\"\naudience = \"wye-prod\"\n{auth_extra}\n\
         [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{}\"",
        credential::encode_public_key(key.public_key().as_ref())
    );
    let h = Harness::start(handler, &auth, UpstreamAuth::api_key(API_KEY).unwrap()).await;
    (h, rx)
}

const HAIKU: &str =
    r#"{"model":"claude-haiku-4-5","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#;
const SONNET: &str = r#"{"model":"claude-sonnet-4-5","max_tokens":1,"messages":[]}"#;

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
    assert!(
        read_body(res)
            .await
            .starts_with(br#"{"type":"error","error":{"type":"permission_error""#)
    );
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
    credential::issue(
        key,
        "k1",
        &NewCredential {
            iss: "wye",
            sub: "alice",
            aud: "wye-prod",
            iat: now(),
            exp: now() + 600,
            models: None,
            max_output_tokens: Some(max_output_tokens),
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
            .contains("max_tokens 6000 exceeds this credential's limit of 4000"),
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
    let t = token(&key, now(), now() + 600);
    // The client streams the body: model first, max_tokens in a later chunk.
    let (tx, rx) = mpsc::channel::<Bytes>(2);
    let client: Client<HttpConnector, ChannelBody> =
        Client::builder(TokioExecutor::new()).build_http();
    let req = Request::post(format!("http://{}/v1/messages", h.gateway))
        .header("authorization", format!("Bearer {t}"))
        .body(ChannelBody(rx))
        .unwrap();
    let response = tokio::spawn(async move { client.request(req).await });
    tx.send(Bytes::from_static(
        br#"{"model":"claude-haiku-4-5","messages":[],"#,
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(Bytes::from_static(br#""max_tokens":9000}"#))
        .await
        .unwrap();
    drop(tx);
    let res = tokio::time::timeout(WAIT, response)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let text = read_body(res).await;
    assert!(
        String::from_utf8_lossy(&text).contains("max_tokens 9000"),
        "{text:?}"
    );
    // The upstream saw the request start, never a complete body.
    assert!(
        bodies.try_recv().is_err(),
        "the upstream received a complete body"
    );
}
