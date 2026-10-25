//! Model aliases end to end: resolution, splicing, rollout, permissions.

use gateway_core::credential::{self, NewCredential};
use ring::signature::{Ed25519KeyPair, KeyPair};
use wye_testkit::ed25519_keypair as keypair;

use super::harness::*;

const ALIASES: &str = "\n[[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n\
     when = [{ subjects = [\"alice\"], model = \"claude-haiku-5-5\" }]\n\
     [[aliases]]\nname = \"big\"\nmodel = \"claude-opus-5-5\"\n";

/// Gateway whose upstream reports each request's headers and body.
async fn harness(upstream_extra: &str) -> (Harness, mpsc::Receiver<(HeaderMap, Bytes)>) {
    let (tx, rx) = mpsc::channel(8);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send((seen.headers, seen.body)).await.unwrap();
            full(StatusCode::OK, "{}")
        })
    });
    let config = format!("api_key_env = \"UNUSED\"\n{upstream_extra}");
    let h = Harness::start(handler, &config, UpstreamAuth::api_key(API_KEY).unwrap()).await;
    (h, rx)
}

#[tokio::test]
async fn an_alias_is_replaced_in_place_and_recorded() {
    let (h, mut seen) = harness(ALIASES).await;
    let body = r#"{"model":"coding","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#;
    let res = h.send(messages(), body).await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    let (headers, forwarded) = seen.recv().await.unwrap();
    assert_eq!(forwarded, body.replace("coding", "claude-opus-5-5"));
    assert_eq!(headers["content-length"], forwarded.len().to_string());
    assert_eq!(h.usage_record().await["requested_model"], "coding");

    // A name that is not an alias passes through byte for byte.
    let plain = r#"{"model":"claude-sonnet-5-5","max_tokens":1}"#;
    read_body(h.send(messages(), plain).await).await;
    assert_eq!(seen.recv().await.unwrap().1, plain);
    assert!(h.usage_record().await.get("requested_model").is_none());
}

fn token(key: &Ed25519KeyPair, sub: &str) -> String {
    credential::issue(
        key,
        "k1",
        &NewCredential {
            iss: "wye",
            sub,
            aud: "wye-prod",
            iat: now(),
            exp: now() + 600,
            models: None,
            max_output_tokens: None,
        },
    )
}

#[tokio::test]
async fn rollout_follows_the_subject_and_permissions_apply_to_the_resolved_model() {
    let key = keypair();
    let auth = format!(
        "\n[auth]\nissuer = \"wye\"\naudience = \"wye-prod\"\nmodels = [\"claude-haiku-*\"]\n\
         [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{}\"\n{ALIASES}",
        credential::encode_public_key(key.public_key().as_ref())
    );
    let (h, mut seen) = harness(&auth).await;
    let send = |sub: &'static str, model: &'static str| {
        let req = messages().header("authorization", format!("Bearer {}", token(&key, sub)));
        let body: &'static str = Box::leak(format!(r#"{{"model":"{model}"}}"#).into_boxed_str());
        h.send(req, body)
    };

    // Alice's rule resolves `coding` to Haiku, which the configured list allows.
    let res = send("alice", "coding").await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    assert_eq!(
        seen.recv().await.unwrap().1,
        r#"{"model":"claude-haiku-5-5"}"#
    );

    // Everyone else gets Opus, which the list refuses before upstream contact.
    for (sub, model) in [("bob", "coding"), ("alice", "big")] {
        let res = send(sub, model).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{sub} {model}");
    }
    assert!(seen.try_recv().is_err());
}
