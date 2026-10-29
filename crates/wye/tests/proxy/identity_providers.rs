//! Identity-provider tokens end to end: a JWKS file from the config, grants
//! checked before the upstream, the provider key forwarded instead.

use wye_testkit::jwt::{IdpKey, jwks};

use super::harness::*;

const ISS: &str = "https://token.actions.githubusercontent.com";

/// Gateway trusting `key` (kid `gh-1`) as a GitHub Actions-style issuer,
/// with subject `repository` and one grant for the `acme` owner.
async fn harness(key: &IdpKey) -> (Harness, mpsc::Receiver<HeaderMap>) {
    // One directory per harness: tests run in parallel and remove theirs.
    static HARNESSES: AtomicUsize = AtomicUsize::new(0);
    let n = HARNESSES.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("wye-idp-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks_file = dir.join(format!("{}.jwks", key.alg()));
    std::fs::write(&jwks_file, jwks(&[(key, "gh-1")])).unwrap();
    let (handler, rx) = recording(|seen| seen.headers);
    let auth = format!(
        "api_key_env = \"UNUSED\"\n\n[auth]\n\
         [[auth.issuers]]\nissuer = \"{ISS}\"\naudience = \"wye-prod\"\nalgorithm = \"{}\"\n\
         jwks_file = \"{}\"\nsubject_claim = \"repository\"\n\
         [[auth.issuers.grants]]\nclaim = \"repository_owner\"\nvalue = \"acme\"\n\
         models = [\"claude-haiku-*\"]",
        key.alg(),
        jwks_file.display()
    );
    let h = Harness::start(handler, &auth, UpstreamAuth::api_key(API_KEY).unwrap()).await;
    // Key sets are read once, at startup.
    std::fs::remove_dir_all(&dir).unwrap();
    (h, rx)
}

fn claims(owner: &str) -> String {
    let now = now();
    format!(
        r#"{{"iss":"{ISS}","aud":"wye-prod","sub":"repo:{owner}/app:ref:refs/heads/main","repository":"{owner}/app","repository_owner":"{owner}","iat":{now},"exp":{}}}"#,
        now + 300
    )
}

const HAIKU: &str = r#"{"model":"claude-haiku-4-5","max_tokens":1,"messages":[]}"#;
const OPUS: &str = r#"{"model":"claude-opus-4-1","max_tokens":1,"messages":[]}"#;

#[tokio::test]
async fn granted_provider_token_reaches_upstream_with_the_provider_key() {
    for key in [IdpKey::rs256(), IdpKey::es256()] {
        let (h, mut seen) = harness(&key).await;
        let t = key.token("gh-1", &claims("acme"));
        let res = h
            .send(
                messages().header("authorization", format!("Bearer {t}")),
                HAIKU,
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK, "{}", key.alg());
        let headers = seen.recv().await.unwrap();
        assert_eq!(headers["x-api-key"], API_KEY);
        assert!(headers.get("authorization").is_none());
        assert_eq!(h.usage_record().await["subject"], "acme/app");

        // A model outside the grant is refused before the upstream.
        let res = h
            .send(
                messages().header("authorization", format!("Bearer {t}")),
                OPUS,
            )
            .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn ungranted_or_foreign_provider_tokens_never_reach_upstream() {
    let key = IdpKey::rs256();
    let (h, _seen) = harness(&key).await;
    // Valid, but matching no grant: no model is allowed.
    let other_owner = key.token("gh-1", &claims("someone-else"));
    let res = h
        .send(
            messages().header("authorization", format!("Bearer {other_owner}")),
            HAIKU,
        )
        .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // Signed by another key under the same kid, or with the algorithm swapped.
    let forged = IdpKey::rs256_other();
    for t in [
        forged.token("gh-1", &claims("acme")),
        key.sign(r#"{"alg":"ES256","kid":"gh-1"}"#, &claims("acme")),
        key.sign(r#"{"alg":"none","kid":"gh-1"}"#, &claims("acme")),
    ] {
        let res = h
            .send(
                messages().header("authorization", format!("Bearer {t}")),
                HAIKU,
            )
            .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 0);
}
