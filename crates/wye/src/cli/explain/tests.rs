#![allow(clippy::unwrap_used)]

use gateway_core::credential::{self, NewCredential};
use ring::signature::KeyPair;

use super::*;

const NOW: u64 = 1_800_000_000;

fn config(key: &ring::signature::Ed25519KeyPair, extra_auth: &str) -> Config {
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\nmax_request_body_bytes = 200\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n\
         [auth]\nissuer = \"wye\"\naudience = \"wye-prod\"\n{extra_auth}\n\
         [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{}\"\n\
         [cache_diagnostics]\nhash_key_env = \"K\"",
        credential::encode_public_key(key.public_key().as_ref())
    ))
    .unwrap()
}

fn verifier(config: &Config) -> Verifier {
    Verifier::new(config.auth.as_ref().unwrap(), Vec::new()).unwrap()
}

fn token(key: &ring::signature::Ed25519KeyPair, models: Option<&[&str]>) -> String {
    credential::issue(
        key,
        "k1",
        &NewCredential {
            iss: "wye",
            sub: "alice",
            aud: "wye-prod",
            iat: NOW,
            exp: NOW + 600,
            models,
            max_output_tokens: None,
        },
    )
}

#[test]
fn explains_forwarding_and_each_refusal() {
    let key = wye_testkit::ed25519_keypair();
    let config = config(&key, "models = [\"claude-*\"]");
    let haiku = br#"{"model":"claude-haiku-4-5","messages":[]}"#;
    let t = token(&key, Some(&["claude-haiku-*"]));

    let ok = explain(
        &config,
        "/v1/messages",
        Credential::Token(&t, &verifier(&config)),
        Some("s-1"),
        haiku,
        NOW,
        &[],
    );
    assert!(ok.refused.is_none(), "{}", ok.report());
    let report = ok.report();
    assert!(report.contains("subject alice"), "{report}");
    assert!(report.contains("by credential: claude-haiku-*"), "{report}");
    assert!(
        report.contains("upstream: POST https://api.anthropic.com/v1/messages"),
        "{report}"
    );
    assert!(report.ends_with("decision: forward\n"), "{report}");

    let sonnet = br#"{"model":"claude-sonnet-4-5"}"#;
    let refused = |e: Explanation| e.refused.map(|(status, kind, _)| (status, kind));
    assert_eq!(
        refused(explain(
            &config,
            "/v1/messages",
            Credential::Token(&t, &verifier(&config)),
            None,
            sonnet,
            NOW,
            &[]
        )),
        Some((403, "permission_error"))
    );
    assert_eq!(
        refused(explain(
            &config,
            "/v1/messages",
            Credential::None,
            None,
            haiku,
            NOW,
            &[]
        )),
        Some((401, "authentication_error"))
    );
    assert_eq!(
        refused(explain(
            &config,
            "/v1/messages",
            Credential::Token(&t, &verifier(&config)),
            None,
            haiku,
            NOW + 601,
            &[]
        )),
        Some((401, "authentication_error"))
    );
    assert_eq!(
        refused(explain(
            &config,
            "/v1/messages",
            Credential::Token(&t, &verifier(&config)),
            None,
            &[b' '; 201],
            NOW,
            &[]
        )),
        Some((413, "request_too_large"))
    );
    assert_eq!(
        refused(explain(
            &config,
            "/v1/messages",
            Credential::Token(&t, &verifier(&config)),
            None,
            b"{}",
            NOW,
            &[]
        )),
        Some((400, "invalid_request_error"))
    );
    assert_eq!(
        refused(explain(
            &config,
            "/v1/other",
            Credential::Token(&t, &verifier(&config)),
            None,
            haiku,
            NOW,
            &[]
        )),
        Some((404, "not_found_error"))
    );
    let mut limited = config.clone();
    limited.auth.as_mut().unwrap().max_output_tokens = Some(100);
    let over = br#"{"model":"claude-haiku-4-5","max_tokens":101}"#;
    assert_eq!(
        refused(explain(
            &limited,
            "/v1/messages",
            Credential::Token(&t, &verifier(&config)),
            None,
            over,
            NOW,
            &[]
        )),
        Some((403, "permission_error"))
    );
    let report = explain(
        &limited,
        "/v1/messages",
        Credential::Token(&t, &verifier(&config)),
        None,
        br#"{"model":"claude-haiku-4-5","max_tokens":100}"#,
        NOW,
        &[],
    )
    .report();
    assert!(report.contains("max_tokens: 100 (limit 100)"), "{report}");

    // A token ceiling alone still makes the server read the body, so a
    // request without a model is refused, as `serve` refuses it.
    let mut only_ceiling = limited.clone();
    only_ceiling.auth.as_mut().unwrap().models = None;
    let t = token(&key, None);
    assert_eq!(
        refused(explain(
            &only_ceiling,
            "/v1/messages",
            Credential::Token(&t, &verifier(&only_ceiling)),
            None,
            br#"{"max_tokens":10}"#,
            NOW,
            &[]
        )),
        Some((400, "invalid_request_error"))
    );
}

#[test]
fn explains_alias_resolution_before_permissions() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
         [[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n",
    )
    .unwrap();
    let report = explain(
        &config,
        "/v1/messages",
        Credential::None,
        None,
        br#"{"model":"coding"}"#,
        NOW,
        &[],
    )
    .report();
    assert!(
        report.contains("alias: coding resolves to claude-opus-5-5;"),
        "{report}"
    );
    assert!(report.contains("model: claude-opus-5-5;"), "{report}");
    assert!(report.ends_with("decision: forward\n"), "{report}");
}

#[test]
fn explains_upgrade_issues_per_target_without_refusing() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
         [upgrade_check]\ntargets = [\"claude-opus-4-7\"]\n",
    )
    .unwrap();
    let body = br#"{"model":"claude-opus-4-6","thinking":{"type":"enabled","budget_tokens":9},"messages":[{"role":"system","content":"x"}]}"#;
    let configured = explain(
        &config,
        "/v1/messages",
        Credential::None,
        None,
        body,
        NOW,
        &[],
    );
    assert!(configured.refused.is_none(), "report only");
    assert_eq!(
        configured.upgrade_issues,
        [
            ("claude-opus-4-7".to_owned(), "budget_tokens"),
            ("claude-opus-4-7".to_owned(), "system_role")
        ]
    );
    let named = explain(
        &config,
        "/v1/messages",
        Credential::None,
        None,
        body,
        NOW,
        &["claude-opus-4-6"],
    );
    assert!(named.upgrade_issues.is_empty());
    assert!(
        named
            .report()
            .contains("upgrade check: claude-opus-4-6 accepts this request")
    );
}

#[test]
fn shows_the_upstream_a_session_goes_to() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\n\
         [[routes.claude.upstreams]]\nid = \"team-a\"\nbase_url = \"https://a.example\"\napi_key_env = \"A\"\n\
         [[routes.claude.upstreams]]\nid = \"team-b\"\nbase_url = \"https://b.example\"\napi_key_env = \"B\"\n",
    )
    .unwrap();
    for session in ["s-1", "s-2", "s-3", "s-4"] {
        let picked = &config.route.upstreams()[config.route.pick(session.as_bytes())];
        let id = picked.id.as_deref().unwrap();
        let out = explain(
            &config,
            "/v1/messages",
            Credential::None,
            Some(session),
            b"{}",
            NOW,
            &[],
        );
        let report = out.report();
        assert!(
            report.contains(&format!(
                "route: upstream {id} of 2 (cache_affine by session id)"
            )),
            "{report}"
        );
        assert!(
            report.contains(&format!(
                "upstream: POST https://{}/v1/messages",
                picked.authority
            )),
            "{report}"
        );
        assert!(report.contains(&format!(
            "x-api-key from ${}",
            if id == "team-a" { "A" } else { "B" }
        )));
    }
}
