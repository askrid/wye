//! Model aliases end to end: resolution, splicing, rollout, permissions.

use super::harness::*;

const ALIASES: &str = "\n[[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n\
     when = [{ subjects = [\"alice\"], model = \"claude-haiku-5-5\" }]\n\
     [[aliases]]\nname = \"big\"\nmodel = \"claude-opus-5-5\"\n";

/// Gateway whose upstream reports each request's headers and body.
async fn harness(upstream_extra: &str) -> (Harness, mpsc::Receiver<(HeaderMap, Bytes)>) {
    let (handler, rx) = recording(|seen| (seen.headers, seen.body));
    (Harness::keyed(handler, upstream_extra).await, rx)
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

    // No ceiling applies, so the output-token field is the provider's to judge.
    for tokens in ["null", "4096.0"] {
        let body = format!(r#"{{"model":"claude-sonnet-5-5","max_tokens":{tokens}}}"#);
        let res = h.send(messages(), body.clone()).await;
        assert_eq!(res.status(), StatusCode::OK, "{tokens}");
        read_body(res).await;
        assert_eq!(seen.recv().await.unwrap().1, body);
        h.usage_record().await;
    }

    // A name that is not an alias passes through byte for byte.
    let plain = r#"{"model":"claude-sonnet-5-5","max_tokens":1}"#;
    read_body(h.send(messages(), plain).await).await;
    assert_eq!(seen.recv().await.unwrap().1, plain);
    assert!(h.usage_record().await.get("requested_model").is_none());
}

#[tokio::test]
async fn rollout_follows_the_subject_and_permissions_apply_to_the_resolved_model() {
    let key = keypair();
    let auth = format!(
        "{}\n{ALIASES}",
        auth_section(&key, "models = [\"claude-haiku-*\"]")
    );
    let (h, mut seen) = harness(&auth).await;
    let send = |sub: &'static str, model: &'static str| {
        let req = bearer(&token(&key, sub, None, None));
        let body = format!(r#"{{"model":"{model}"}}"#);
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
