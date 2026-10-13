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
