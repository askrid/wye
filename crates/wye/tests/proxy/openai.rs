//! The OpenAI route: `/v1/responses` passthrough for Codex and the OpenAI
//! SDKs, with the provider key as a bearer token.

use gateway_core::credential::{self, NewCredential};
use ring::signature::{Ed25519KeyPair, KeyPair};
use wye_testkit::ed25519_keypair as keypair;

use super::harness::*;

const OPENAI_KEY: &str = "sk-openai-test";
const EVENTS: &str = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n";

/// A gateway with only `[routes.openai]` (plus `extra` config), whose
/// upstream reports what it received and answers with an SSE stream.
async fn harness(extra: &str) -> (Harness, mpsc::Receiver<Seen>) {
    let (tx, rx) = mpsc::channel(8);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(seen).await.unwrap();
            let mut res = full(StatusCode::OK, EVENTS);
            res.headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            res
        })
    });
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n{extra}\n\
         [routes.openai]\n[[routes.openai.upstreams]]\nid = \"openai\"\n\
         base_url = \"http://{upstream}\"\napi_key_env = \"UNUSED\"\n"
    );
    let resources = Resources {
        upstreams: vec![UpstreamResources::new(
            UpstreamAuth::bearer(OPENAI_KEY).unwrap(),
        )],
        verifier: None,
        prompt_key: None,
        otlp_headers: HeaderMap::new(),
    };
    (Harness::launch_config(&config, resources, hits).await, rx)
}

fn responses() -> http::request::Builder {
    Request::post("/v1/responses")
        .header("content-type", "application/json")
        .header("session-id", "codex-session-1")
        .header("originator", "codex_cli_rs")
}

const BODY: &str = r#"{"model":"gpt-5.5-codex","stream":true,"input":[]}"#;

#[tokio::test]
async fn responses_reach_openai_with_the_provider_key() {
    let (h, mut seen) = harness("").await;
    let res = h
        .send(responses().header("x-api-key", "client-junk"), BODY)
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(read_body(res).await, EVENTS);
    let got = seen.recv().await.unwrap();
    assert_eq!(got.uri.path(), "/v1/responses");
    assert_eq!(got.headers["authorization"], format!("Bearer {OPENAI_KEY}"));
    assert!(got.headers.get("x-api-key").is_none());
    assert_eq!(got.headers["session-id"], "codex-session-1");
    assert_eq!(got.body, BODY);
    let record = h.usage_record().await;
    assert_eq!(record["path"], "/v1/responses");
    assert_eq!(record["session_id"], "codex-session-1");
    assert_eq!(record["upstream"], "openai");

    // No Claude route configured: Messages is not proxied.
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

fn token(key: &Ed25519KeyPair, models: Option<&[&str]>, max_output_tokens: Option<u64>) -> String {
    credential::issue(
        key,
        "k1",
        &NewCredential {
            iss: "wye",
            sub: "dev",
            aud: "wye-prod",
            iat: now(),
            exp: now() + 600,
            models,
            max_output_tokens,
        },
    )
}

#[tokio::test]
async fn gateway_credentials_and_policy_apply_with_openai_errors() {
    let key = keypair();
    let auth = format!(
        "[auth]\nissuer = \"wye\"\naudience = \"wye-prod\"\n\
         [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{}\"",
        credential::encode_public_key(key.public_key().as_ref())
    );
    let (h, mut seen) = harness(&auth).await;

    // Codex sends its env_key value as a bearer token: here, a Wye credential.
    let t = token(&key, Some(&["gpt-5.5-*"]), None);
    let res = h
        .send(
            responses().header("authorization", format!("Bearer {t}")),
            BODY,
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    let got = seen.recv().await.unwrap();
    assert_eq!(got.headers["authorization"], format!("Bearer {OPENAI_KEY}"));
    assert_eq!(h.usage_record().await["subject"], "dev");

    let refused = |res: Response<Incoming>, status: StatusCode| async move {
        assert_eq!(res.status(), status);
        let body: serde_json::Value = serde_json::from_slice(&read_body(res).await).unwrap();
        assert!(body["error"]["message"].is_string(), "{body}");
        assert!(
            body.get("type").is_none(),
            "OpenAI shape, not Anthropic's: {body}"
        );
        body["error"]["code"].as_str().unwrap().to_owned()
    };
    let res = h.send(responses(), BODY).await;
    assert_eq!(
        refused(res, StatusCode::UNAUTHORIZED).await,
        "authentication_error"
    );

    let other = token(&key, Some(&["claude-*"]), None);
    let res = h
        .send(
            responses().header("authorization", format!("Bearer {other}")),
            BODY,
        )
        .await;
    assert_eq!(
        refused(res, StatusCode::FORBIDDEN).await,
        "permission_error"
    );

    // Under a token ceiling, Responses must say max_output_tokens.
    let capped = token(&key, None, Some(1000));
    let res = h
        .send(
            responses().header("authorization", format!("Bearer {capped}")),
            BODY,
        )
        .await;
    assert_eq!(
        refused(res, StatusCode::FORBIDDEN).await,
        "permission_error"
    );
    let res = h
        .send(
            responses().header("authorization", format!("Bearer {capped}")),
            r#"{"model":"gpt-5.5-codex","max_output_tokens":800,"input":[]}"#,
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 2);
}
