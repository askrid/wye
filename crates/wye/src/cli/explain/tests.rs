#![allow(clippy::unwrap_used)]

use gateway_core::credential::{self, NewCredential};
use ring::signature::KeyPair;

use super::*;

const NOW: u64 = 1_800_000_000;

/// `explain` for a `/v1/messages` request without a session, at `NOW`.
fn explain_messages(config: &Config, credential: Credential<'_>, body: &[u8]) -> Explanation {
    explain(config, "/v1/messages", credential, None, body, NOW, &[])
}

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
    let forbidden = explain_messages(&config, Credential::Token(&t, &verifier(&config)), sonnet);
    // The server's message: the model as the client sent it.
    assert_eq!(
        forbidden.refused.as_ref().map(|r| r.2.as_str()),
        Some("this credential may not use the model \"claude-sonnet-4-5\"")
    );
    assert_eq!(refused(forbidden), Some((403, "permission_error")));
    assert_eq!(
        refused(explain_messages(&config, Credential::None, haiku)),
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
        refused(explain_messages(
            &config,
            Credential::Token(&t, &verifier(&config)),
            &[b' '; 201]
        )),
        Some((413, "request_too_large"))
    );
    assert_eq!(
        refused(explain_messages(
            &config,
            Credential::Token(&t, &verifier(&config)),
            b"{}"
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
        refused(explain_messages(
            &limited,
            Credential::Token(&t, &verifier(&config)),
            over
        )),
        Some((403, "permission_error"))
    );
    let report = explain_messages(
        &limited,
        Credential::Token(&t, &verifier(&config)),
        br#"{"model":"claude-haiku-4-5","max_tokens":100}"#,
    )
    .report();
    assert!(report.contains("max_tokens: 100 (limit 100)"), "{report}");

    // An output-token ceiling alone still makes the server read the body, so a
    // request without a model is refused, as `serve` refuses it.
    let mut only_ceiling = limited.clone();
    only_ceiling.auth.as_mut().unwrap().models = None;
    let t = token(&key, None);
    assert_eq!(
        refused(explain_messages(
            &only_ceiling,
            Credential::Token(&t, &verifier(&only_ceiling)),
            br#"{"max_tokens":10}"#
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
    let report = explain_messages(&config, Credential::None, br#"{"model":"coding"}"#).report();
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
    let configured = explain_messages(&config, Credential::None, body);
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
        &["claude-haiku-4-5"],
    );
    assert!(named.upgrade_issues.is_empty());
    assert!(
        named
            .report()
            .contains("upgrade check: claude-haiku-4-5 accepts this request")
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
        let route = config.routes.claude.as_ref().unwrap();
        let picked = &route.upstreams()[route.pick(session.as_bytes())];
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

#[test]
fn shows_the_providers_own_model_id() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\n\
         [[routes.claude.upstreams]]\nid = \"bedrock\"\nprovider = \"bedrock\"\n\
         base_url = \"https://bedrock-mantle.us-east-1.api.aws/anthropic\"\napi_key_env = \"B\"\n",
    )
    .unwrap();
    for (body, step) in [
        (
            &br#"{"model":"claude-opus-5-5"}"#[..],
            Some("model id: the body names anthropic.claude-opus-5-5 upstream"),
        ),
        (&br#"{"model":"anthropic.claude-opus-5-5"}"#[..], None),
    ] {
        let report = explain_messages(&config, Credential::None, body).report();
        assert!(
            report.contains(
                "upstream: POST https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages"
            ),
            "{report}"
        );
        assert!(report.contains("x-api-key from $B"), "{report}");
        assert!(!report.contains("the body is not read"), "{report}");
        assert_eq!(report.contains("model id:"), step.is_some(), "{report}");
        if let Some(step) = step {
            assert!(report.contains(step), "{report}");
        }
    }
}

#[test]
fn explains_responses_on_the_openai_route() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[auth]\nissuer = \"wye\"\naudience = \"wye\"\n\
         max_output_tokens = 1000\n\
         [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo\"\n\
         [routes.openai]\n[[routes.openai.upstreams]]\nid = \"oa\"\n\
         base_url = \"https://api.openai.com\"\napi_key_env = \"OPENAI_API_KEY\"\n",
    )
    .unwrap();
    let dev = || {
        Credential::Trusted(Identity {
            subject: "dev".into(),
            key_id: "k1".into(),
            expires_at: NOW + 60,
            models: None,
            max_output_tokens: None,
        })
    };
    let run =
        |path: &str, body: &[u8]| explain(&config, path, dev(), Some("codex-1"), body, NOW, &[]);
    let ok = run(
        "/v1/responses",
        br#"{"model":"gpt-5.5","max_output_tokens":500,"input":[]}"#,
    );
    let report = ok.report();
    assert!(ok.refused.is_none(), "{report}");
    assert!(
        report.contains("max_output_tokens: 500 (limit 1000)"),
        "{report}"
    );
    assert!(
        report.contains("provider credential: Authorization: Bearer from $OPENAI_API_KEY"),
        "{report}"
    );
    assert!(
        report.contains("upstream: POST https://api.openai.com/v1/responses"),
        "{report}"
    );

    let missing = run("/v1/responses", br#"{"model":"gpt-5.5","input":[]}"#);
    assert_eq!(
        missing.refused.as_ref().map(|r| r.2.as_str()),
        Some("max_output_tokens is required: this credential's limit is 1000")
    );

    let no_claude = run(
        "/v1/messages",
        br#"{"model":"claude-opus-5-5","max_tokens":1}"#,
    );
    assert_eq!(
        no_claude.refused.as_ref().map(|r| r.2.as_str()),
        Some("no route serves /v1/messages: add [upstream] or [routes.claude]")
    );
}

#[test]
fn shows_the_price_that_applies_at_the_upstream() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\nselect = \"ordered\"\n\
         [[routes.claude.upstreams]]\nid = \"aws\"\nbase_url = \"https://a.example\"\napi_key_env = \"A\"\n\
         [[routes.claude.upstreams]]\nid = \"direct\"\nbase_url = \"https://b.example\"\napi_key_env = \"B\"\n\
         [pricing]\nversion = \"v1\"\n\
         [[pricing.models]]\nmodels = [\"claude-opus-*\"]\nupstreams = [\"aws\"]\n\
         input = \"5.50\"\noutput = \"27.50\"\n\
         [[pricing.models]]\nmodels = [\"claude-opus-*\"]\ninput = \"5.00\"\noutput = \"25.00\"\n\
         cache_read = \"0.50\"\n",
    )
    .unwrap();
    let report = |body: &[u8]| explain_messages(&config, Credential::None, body).report();
    let priced = report(br#"{"model":"claude-opus-5-5"}"#);
    assert!(
        priced.contains(
            "price: claude-opus-5-5 at aws: input $5.50, output $27.50 per million tokens \
             (pricing version v1)"
        ),
        "{priced}"
    );
    let unpriced = report(br#"{"model":"claude-haiku-5-5"}"#);
    assert!(
        unpriced.contains("price: none for claude-haiku-5-5 at aws"),
        "{unpriced}"
    );
}

#[test]
fn reads_the_body_in_the_servers_order() {
    let aliased = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
         [[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n",
    )
    .unwrap();
    let anonymous = |path: &str, body: &[u8]| {
        explain(&aliased, path, Credential::None, None, body, NOW, &[]).refused
    };
    // With an alias alone the server reads only to the model: the provider
    // answers the rest.
    let late_error = br#"{"model":"coding","max_tokens":4096.0,"messages":[]}"#;
    assert_eq!(anonymous("/v1/messages", late_error), None);
    // The query string Claude Code sends is not part of the path.
    assert_eq!(
        anonymous("/v1/messages?beta=true", br#"{"model":"m"}"#),
        None
    );

    // Under permissions, the model is decided first; the rest streams after.
    let key = wye_testkit::ed25519_keypair();
    let config = config(&key, "");
    let verifier = verifier(&config);
    let t = token(&key, Some(&["claude-haiku-*"]));
    let status = |body: &[u8]| {
        let credential = Credential::Token(&t, &verifier);
        explain(&config, "/v1/messages", credential, None, body, NOW, &[])
            .refused
            .map(|(status, ..)| status)
    };
    assert_eq!(
        status(br#"{"model":"claude-opus-5-5","max_tokens":4096.0}"#),
        Some(403)
    );
    assert_eq!(
        status(br#"{"model":"claude-haiku-4-5","max_tokens":4096.0}"#),
        Some(400)
    );
}

#[test]
fn chat_completions_carries_no_session_to_route_by() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.openai]\n\
         [[routes.openai.upstreams]]\nid = \"a\"\nbase_url = \"https://api.openai.com\"\napi_key_env = \"A\"\n\
         [[routes.openai.upstreams]]\nid = \"b\"\nbase_url = \"https://api.openai.com\"\napi_key_env = \"B\"\n",
    )
    .unwrap();
    let report = explain(
        &config,
        "/v1/chat/completions",
        Credential::None,
        Some("s-1"),
        br#"{"model":"gpt-5"}"#,
        NOW,
        &[],
    )
    .report();
    assert!(
        report.contains("cache_affine by nothing: no session id or credential"),
        "{report}"
    );
}
