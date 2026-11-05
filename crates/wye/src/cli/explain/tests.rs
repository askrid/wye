use gateway_core::token::{self, NewToken};
use ring::signature::KeyPair;

use super::*;

const NOW: u64 = 1_800_000_000;

/// `explain` for a `/v1/messages` request without a session, at `NOW`.
fn explain_messages(config: &Config, caller: CallerToken<'_>, body: &[u8]) -> Explanation {
    explain(config, "/v1/messages", caller, &none(), body, NOW, &[])
}

/// No headers: a local client's, without a session.
fn none() -> HeaderMap {
    HeaderMap::new()
}

/// A local client's headers for `path`, with the session id `id`.
fn session(path: &str, id: &str) -> HeaderMap {
    request_headers(path, Some(id), &[]).unwrap()
}

fn config(key: &ring::signature::Ed25519KeyPair, extra_auth: &str) -> Config {
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\nmax_request_body_bytes = 200\n\
         [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key_env = \"ANTHROPIC_API_KEY\"\n\
         [auth]\nissuer = \"wye\"\naudience = \"wye-prod\"\n{extra_auth}\n\
         [[auth.signing_keys]]\nid = \"k1\"\ned25519_public_key = \"{}\"\n\
         [cache_diagnostics]\nhash_key_env = \"K\"",
        token::encode_public_key(key.public_key().as_ref())
    ))
    .unwrap()
}

fn verifier(config: &Config) -> Verifier {
    Verifier::new(config.auth.as_ref().unwrap(), Vec::new()).unwrap()
}

fn token(key: &ring::signature::Ed25519KeyPair, models: Option<&[&str]>) -> String {
    token::issue(
        key,
        "k1",
        &NewToken {
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
        CallerToken::Token(&t, &verifier(&config)),
        &session("/v1/messages", "s-1"),
        haiku,
        NOW,
        &[],
    );
    assert!(ok.refused.is_none(), "{}", ok.report());
    let report = ok.report();
    assert!(
        report.contains("\ngateway token: valid; subject alice, key id k1, expires at "),
        "{report}"
    );
    assert!(report.contains("by token: claude-haiku-*"), "{report}");
    assert!(
        report.contains("upstream: POST https://api.anthropic.com/v1/messages"),
        "{report}"
    );
    assert!(report.ends_with("decision: forward\n"), "{report}");

    let sonnet = br#"{"model":"claude-sonnet-4-5"}"#;
    let refused = |e: Explanation| e.refused.map(|(status, kind, _)| (status, kind));
    let forbidden = explain_messages(&config, CallerToken::Token(&t, &verifier(&config)), sonnet);
    // The server's message: the model as the client sent it.
    assert_eq!(
        forbidden.refused.as_ref().map(|r| r.2.as_str()),
        Some("this gateway token may not use the model \"claude-sonnet-4-5\"")
    );
    assert_eq!(refused(forbidden), Some((403, "permission_error")));
    assert_eq!(
        refused(explain_messages(&config, CallerToken::None, haiku)),
        Some((401, "authentication_error"))
    );
    assert_eq!(
        refused(explain(
            &config,
            "/v1/messages",
            CallerToken::Token(&t, &verifier(&config)),
            &none(),
            haiku,
            NOW + 601,
            &[]
        )),
        Some((401, "authentication_error"))
    );
    assert_eq!(
        refused(explain_messages(
            &config,
            CallerToken::Token(&t, &verifier(&config)),
            &[b' '; 201]
        )),
        Some((413, "request_too_large"))
    );
    assert_eq!(
        refused(explain_messages(
            &config,
            CallerToken::Token(&t, &verifier(&config)),
            b"{}"
        )),
        Some((400, "invalid_request_error"))
    );
    assert_eq!(
        refused(explain(
            &config,
            "/v1/other",
            CallerToken::Token(&t, &verifier(&config)),
            &none(),
            haiku,
            NOW,
            &[]
        )),
        Some((404, "not_found_error"))
    );
    let other = explain(
        &config,
        "/v1/other",
        CallerToken::None,
        &none(),
        haiku,
        NOW,
        &[],
    );
    assert_eq!(
        other.refused.map(|(_, _, message)| message).as_deref(),
        Some("/v1/other is not an endpoint Wye serves")
    );
    let mut limited = config.clone();
    limited.auth.as_mut().unwrap().max_output_tokens = Some(100);
    let over = br#"{"model":"claude-haiku-4-5","max_tokens":101}"#;
    assert_eq!(
        refused(explain_messages(
            &limited,
            CallerToken::Token(&t, &verifier(&config)),
            over
        )),
        Some((403, "permission_error"))
    );
    let report = explain_messages(
        &limited,
        CallerToken::Token(&t, &verifier(&config)),
        br#"{"model":"claude-haiku-4-5","max_tokens":100}"#,
    )
    .report();
    assert!(report.contains("max_tokens: 100 (ceiling 100)"), "{report}");

    // An output-token ceiling alone still makes the server read the body, so a
    // request without a model is refused, as `serve` refuses it.
    let mut only_ceiling = limited.clone();
    only_ceiling.auth.as_mut().unwrap().models = None;
    let t = token(&key, None);
    assert_eq!(
        refused(explain_messages(
            &only_ceiling,
            CallerToken::Token(&t, &verifier(&only_ceiling)),
            br#"{"max_tokens":10}"#
        )),
        Some((400, "invalid_request_error"))
    );
}

#[test]
fn explains_alias_resolution_before_permissions() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key_env = \"K\"\n\
         [[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n",
    )
    .unwrap();
    let report = explain_messages(&config, CallerToken::None, br#"{"model":"coding"}"#).report();
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
         [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key_env = \"K\"\n\
         [upgrade_check]\ntargets = [\"claude-opus-4-7\"]\n",
    )
    .unwrap();
    let body = br#"{"model":"claude-opus-4-6","thinking":{"type":"enabled","budget_tokens":9},"messages":[{"role":"system","content":"x"}]}"#;
    let configured = explain_messages(&config, CallerToken::None, body);
    assert!(configured.refused.is_none(), "report only");
    let report = configured.report();
    for rule in ["budget_tokens", "system_role"] {
        assert!(
            report.contains(&format!(
                "upgrade check: claude-opus-4-7 would reject it: {rule} ("
            )),
            "{report}"
        );
    }
    assert_eq!(report.matches("would reject it").count(), 2, "{report}");
    let named = explain(
        &config,
        "/v1/messages",
        CallerToken::None,
        &none(),
        body,
        NOW,
        &["claude-haiku-4-5"],
    );
    assert!(!named.report().contains("would reject it"));
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
         [[routes.claude.upstreams]]\nid = \"team-a\"\nbase_url = \"https://a.example\"\nprovider_key_env = \"A\"\n\
         [[routes.claude.upstreams]]\nid = \"team-b\"\nbase_url = \"https://b.example\"\nprovider_key_env = \"B\"\n",
    )
    .unwrap();
    for session in ["s-1", "s-2", "s-3", "s-4"] {
        let route = config.routes.claude.as_ref().unwrap();
        let picked = &route.upstreams()[route.pick(session.as_bytes())];
        let id = picked.id.as_deref().unwrap();
        let out = explain(
            &config,
            "/v1/messages",
            CallerToken::None,
            &self::session("/v1/messages", session),
            b"{}",
            NOW,
            &[],
        );
        let report = out.report();
        assert!(
            report.contains(&format!(
                "selection: upstream {id} of 2 (cache_affine by session id)"
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
         base_url = \"https://bedrock-mantle.us-east-1.api.aws/anthropic\"\nprovider_key_env = \"B\"\n",
    )
    .unwrap();
    for (body, step) in [
        (
            &br#"{"model":"claude-opus-5-5"}"#[..],
            Some("model id: the body names anthropic.claude-opus-5-5 upstream"),
        ),
        (&br#"{"model":"anthropic.claude-opus-5-5"}"#[..], None),
    ] {
        let report = explain_messages(&config, CallerToken::None, body).report();
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
         [[auth.signing_keys]]\nid = \"k1\"\ned25519_public_key = \"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo\"\n\
         [routes.openai]\n[[routes.openai.upstreams]]\nid = \"oa\"\n\
         base_url = \"https://api.openai.com\"\nprovider_key_env = \"OPENAI_API_KEY\"\n",
    )
    .unwrap();
    let dev = || {
        CallerToken::Trusted(Identity {
            subject: "dev".into(),
            key_id: "k1".into(),
            expires_at: NOW + 60,
            models: None,
            max_output_tokens: None,
        })
    };
    let run = |path: &str, body: &[u8]| {
        explain(
            &config,
            path,
            dev(),
            &session(path, "codex-1"),
            body,
            NOW,
            &[],
        )
    };
    let ok = run(
        "/v1/responses",
        br#"{"model":"gpt-5.5","max_output_tokens":500,"input":[]}"#,
    );
    let report = ok.report();
    assert!(ok.refused.is_none(), "{report}");
    assert!(
        report.contains("max_output_tokens: 500 (ceiling 1000)"),
        "{report}"
    );
    assert!(
        report.contains("provider key: Authorization: Bearer from $OPENAI_API_KEY"),
        "{report}"
    );
    assert!(
        report.contains("upstream: POST https://api.openai.com/v1/responses"),
        "{report}"
    );

    let missing = run("/v1/responses", br#"{"model":"gpt-5.5","input":[]}"#);
    assert_eq!(
        missing.refused.as_ref().map(|r| r.2.as_str()),
        Some("max_output_tokens is required: the output-token ceiling is 1000")
    );

    let no_claude = run(
        "/v1/messages",
        br#"{"model":"claude-opus-5-5","max_tokens":1}"#,
    );
    assert_eq!(
        no_claude.refused.as_ref().map(|r| r.2.as_str()),
        Some("no route serves /v1/messages: add [routes.claude]")
    );
}

#[test]
fn shows_the_price_that_applies_at_the_upstream() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\nselect = \"ordered\"\n\
         [[routes.claude.upstreams]]\nid = \"aws\"\nbase_url = \"https://a.example\"\nprovider_key_env = \"A\"\n\
         [[routes.claude.upstreams]]\nid = \"direct\"\nbase_url = \"https://b.example\"\nprovider_key_env = \"B\"\n\
         [pricing]\nversion = \"v1\"\n\
         [[pricing.prices]]\nmodels = [\"claude-opus-*\"]\nupstreams = [\"aws\"]\n\
         input = \"5.50\"\noutput = \"27.50\"\n\
         [[pricing.prices]]\nmodels = [\"claude-opus-*\"]\ninput = \"5.00\"\noutput = \"25.00\"\n\
         cache_read = \"0.50\"\n",
    )
    .unwrap();
    let report = |body: &[u8]| explain_messages(&config, CallerToken::None, body).report();
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
         [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key_env = \"K\"\n\
         [[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n",
    )
    .unwrap();
    let anonymous = |path: &str, body: &[u8]| {
        explain(&aliased, path, CallerToken::None, &none(), body, NOW, &[]).refused
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
        let credential = CallerToken::Token(&t, &verifier);
        explain(&config, "/v1/messages", credential, &none(), body, NOW, &[])
            .refused
            .map(|(status, ..)| status)
    };
    assert_eq!(
        status(br#"{"model":"claude-opus-5-5","max_tokens":4096.0}"#),
        Some(403)
    );
    // No ceiling: the field is the provider's to judge.
    assert_eq!(
        status(br#"{"model":"claude-haiku-4-5","max_tokens":4096.0}"#),
        None
    );
    // Under a ceiling, a value it cannot read is refused.
    let capped = self::config(&key, "max_output_tokens = 1000");
    let verifier = self::verifier(&capped);
    let credential = CallerToken::Token(&t, &verifier);
    let body = br#"{"model":"claude-haiku-4-5","max_tokens":4096.0}"#;
    let refused = explain(&capped, "/v1/messages", credential, &none(), body, NOW, &[]).refused;
    assert_eq!(refused.map(|(status, ..)| status), Some(400));
}

#[test]
fn chat_completions_carries_no_session_to_route_by() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.openai]\n\
         [[routes.openai.upstreams]]\nid = \"a\"\nbase_url = \"https://api.openai.com\"\nprovider_key_env = \"A\"\n\
         [[routes.openai.upstreams]]\nid = \"b\"\nbase_url = \"https://api.openai.com\"\nprovider_key_env = \"B\"\n",
    )
    .unwrap();
    let report = explain(
        &config,
        "/v1/chat/completions",
        CallerToken::None,
        // Both agents' session headers: Chat Completions reads neither.
        &request_headers("/v1/messages", Some("s-1"), &["session-id: s-1"]).unwrap(),
        br#"{"model":"gpt-5"}"#,
        NOW,
        &[],
    )
    .report();
    assert!(
        report.contains("cache_affine by nothing: no session id or token"),
        "{report}"
    );
}

#[test]
fn a_passthrough_names_the_callers_credential() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n",
    )
    .unwrap();
    let report = explain_messages(&config, CallerToken::None, b"{}").report();
    assert!(
        report.contains(
            "\ngateway token: not required (no [auth]); a local client: a loopback Host, \
             no Origin\n"
        ),
        "{report}"
    );
    assert!(
        report.contains("\ncredential: the caller's own (passthrough)\n"),
        "{report}"
    );
    assert!(report.ends_with("decision: forward\n"), "{report}");
}

/// Without `[auth]`, a request a web page may have sent is refused, as the
/// server refuses it; without headers, the request is a local client's.
#[test]
fn a_web_pages_request_is_refused_without_auth() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n",
    )
    .unwrap();
    let with = |lines: &[&str]| {
        let headers = request_headers("/v1/messages", None, lines).unwrap();
        explain(
            &config,
            "/v1/messages",
            CallerToken::None,
            &headers,
            b"{}",
            NOW,
            &[],
        )
    };
    assert_eq!(with(&[]).refused, None);
    assert_eq!(with(&["Host: localhost:8080"]).refused, None);
    for lines in [
        &["Host: rebound.evil.example:8080"][..],
        &["Origin: http://evil.example"][..],
        &["Sec-Fetch-Site: cross-site"][..],
    ] {
        let out = with(lines);
        let (status, kind, message) = out.refused.clone().unwrap();
        assert_eq!((status, kind), (403, "permission_error"), "{lines:?}");
        assert_eq!(Some(&*message), Refusal::WebPage.fixed_message());
        assert!(
            out.report()
                .contains("decision: refuse 403 permission_error: a gateway without [auth]")
        );
    }
}

#[test]
fn request_headers_take_curls_form_and_place_the_session() {
    let headers = request_headers(
        "/v1/responses?x=1",
        Some("s-1"),
        &["Origin: http://evil.example", "x-a:b"],
    )
    .unwrap();
    assert_eq!(headers["origin"], "http://evil.example");
    assert_eq!(headers["x-a"], "b");
    assert_eq!(headers["session-id"], "s-1");
    assert!(request_headers("/v1/messages", None, &["no colon"]).is_err());
    assert!(request_headers("/v1/messages", None, &["bad name: x"]).is_err());
}
