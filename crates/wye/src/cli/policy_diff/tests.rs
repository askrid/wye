#![allow(clippy::unwrap_used)]

use gateway_core::credential;

use super::*;

/// A config listening on `listen`, with `auth` after `[auth]` (none when
/// empty) and `rest` appended.
fn config(listen: &str, auth: Option<&str>, rest: &str) -> Config {
    let auth = auth.map_or(String::new(), |auth| format!("[auth]\n{auth}\n"));
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"{listen}\"\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
         {auth}{rest}"
    ))
    .unwrap()
}

fn own(models: &str, extra: &str) -> String {
    format!(
        "issuer = \"wye\"\naudience = \"wye-prod\"\n{models}\n{extra}\n\
         [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{}\"\n",
        credential::encode_public_key(&[7; 32])
    )
}

#[test]
fn narrowing_a_model_list_is_not_widening_and_a_new_model_is() {
    let old = config(
        "127.0.0.1:8080",
        Some(&own(r#"models = ["claude-*"]"#, "")),
        "",
    );
    let narrower = config(
        "127.0.0.1:8080",
        Some(&own(
            r#"models = ["claude-opus-*", "claude-haiku-5-5"]"#,
            "",
        )),
        "",
    );
    let diff = policy_diff(&old, &narrower);
    assert!(!diff.widens(), "{}", diff.report());
    assert!(
        diff.report()
            .contains("narrows: [auth] models: claude-* no longer allowed"),
        "{}",
        diff.report()
    );

    let diff = policy_diff(&narrower, &old);
    assert!(diff.widens());
    assert!(
        diff.report()
            .contains("widens: [auth] models: claude-* newly allowed"),
        "{}",
        diff.report()
    );

    let unlimited = config("127.0.0.1:8080", Some(&own("", "")), "");
    let report = policy_diff(&old, &unlimited).report();
    assert!(
        report.contains("widens: [auth] models: every model allowed"),
        "{report}"
    );

    let diff = policy_diff(&old, &old);
    assert!(!diff.widens());
    assert_eq!(diff.report(), "result: no access widened\n");
}

#[test]
fn ceilings_signers_and_exposure_widen() {
    let old = config(
        "127.0.0.1:8080",
        Some(&own(
            "",
            "max_output_tokens = 32000\nmax_credential_ttl_secs = 86400",
        )),
        "",
    );
    let new = config(
        "0.0.0.0:8080",
        Some(&format!(
            "{}[[auth.keys]]\nid = \"k2\"\ned25519_public_key = \"{}\"\n",
            own(
                "",
                "max_output_tokens = 64000\nmax_credential_ttl_secs = 604800"
            ),
            credential::encode_public_key(&[8; 32])
        )),
        "",
    );
    let report = policy_diff(&old, &new).report();
    for line in [
        "widens: server.listen: 127.0.0.1:8080 -> 0.0.0.0:8080, reachable beyond this machine",
        "widens: [auth] max_output_tokens: 32000 -> 64000",
        "widens: [auth] max_credential_ttl_secs: 86400 -> 604800",
        "widens: [[auth.keys]] k2: a new signing key is trusted",
        "result: access widened (4)",
    ] {
        assert!(report.contains(line), "{line}\n{report}");
    }
    let report = policy_diff(&new, &old).report();
    assert!(report.contains("narrows: [[auth.keys]] k2: no longer trusted"));
    assert!(report.contains("result: no access widened"), "{report}");

    let open = config("127.0.0.1:8080", None, "");
    let report = policy_diff(&old, &open).report();
    assert!(
        report.contains("widens: [auth] removed: no credential is required"),
        "{report}"
    );
}

#[test]
fn identity_provider_grants_compare_per_claim_value() {
    let issuer = |grants: &str| {
        format!(
            "[[auth.issuers]]\nissuer = \"https://idp.example\"\naudience = \"wye\"\n\
             algorithm = \"EdDSA\"\njwks_file = \"idp.jwks\"\n{grants}"
        )
    };
    let grant = |value: &str, models: &str| {
        format!(
            "[[auth.issuers.grants]]\nclaim = \"groups\"\nvalue = \"{value}\"\nmodels = {models}\n"
        )
    };
    let old = config(
        "127.0.0.1:8080",
        Some(&issuer(&grant("eng", r#"["claude-sonnet-*"]"#))),
        "",
    );
    let new = config(
        "127.0.0.1:8080",
        Some(&issuer(&format!(
            "{}{}",
            grant("eng", r#"["claude-sonnet-*", "claude-opus-*"]"#),
            grant("ops", r#"["claude-haiku-*"]"#)
        ))),
        "",
    );
    let report = policy_diff(&old, &new).report();
    for line in [
        "widens: [[auth.issuers]] https://idp.example grant groups=eng: claude-opus-* newly allowed",
        "widens: [[auth.issuers]] https://idp.example grant groups=ops: claude-haiku-* newly allowed",
    ] {
        assert!(report.contains(line), "{line}\n{report}");
    }
    let ungranted = config("127.0.0.1:8080", Some(&issuer("")), "");
    let report = policy_diff(&old, &ungranted).report();
    assert!(report.contains("grants removed"), "{report}");
    let other = config(
        "127.0.0.1:8080",
        Some(&issuer("").replace("idp.example", "other.example")),
        "",
    );
    let report = policy_diff(&old, &other).report();
    assert!(
        report.contains("widens: [[auth.issuers]] https://other.example: tokens from this identity provider accepted"),
        "{report}"
    );
}

#[test]
fn routes_are_reported_without_widening() {
    let route = |extra: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\n\
             [[routes.claude.upstreams]]\nid = \"a\"\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"A\"\n{extra}"
        ))
        .unwrap()
    };
    let old = route("");
    let new = route(
        "[[routes.claude.upstreams]]\nid = \"bedrock\"\nprovider = \"bedrock\"\n\
         base_url = \"https://bedrock-mantle.us-east-1.api.aws/anthropic\"\napi_key_env = \"B\"\n",
    );
    let diff = policy_diff(&old, &new);
    assert!(!diff.widens());
    assert!(
        diff.report().contains(
            "changes: routes.claude: upstream bedrock added (https://bedrock-mantle.us-east-1.api.aws/anthropic)"
        ),
        "{}",
        diff.report()
    );
    assert!(
        policy_diff(&new, &old)
            .report()
            .contains("changes: routes.claude: upstream bedrock removed")
    );
}

#[test]
fn exposure_new_routes_and_the_operators_key_widen() {
    let auth = own("", "");
    let inside = config("10.0.0.5:8080", Some(&auth), "");
    let everywhere = config("0.0.0.0:8080", Some(&auth), "");
    assert!(
        policy_diff(&inside, &everywhere)
            .report()
            .contains("widens: server.listen: 10.0.0.5:8080 -> 0.0.0.0:8080, on every interface")
    );

    let claude = config("127.0.0.1:8080", None, "");
    let both = config(
        "127.0.0.1:8080",
        None,
        "[routes.openai]
[[routes.openai.upstreams]]
id = \"openai\"
provider = \"openai\"
\
         base_url = \"https://api.openai.com\"\napi_key_env = \"O\"\n",
    );
    let report = policy_diff(&claude, &both).report();
    assert!(
        report.contains("widens: routes.openai added: its API now reaches a provider"),
        "{report}"
    );
    let report = policy_diff(&both, &claude).report();
    assert!(
        report.contains("narrows: routes.openai removed"),
        "{report}"
    );

    let upstream = |credential: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\n{credential}\n"
        ))
        .unwrap()
    };
    let forwarded = upstream("forward_caller_credential = true");
    let keyed = upstream("api_key_env = \"K\"");
    let report = policy_diff(&forwarded, &keyed).report();
    assert!(
        report.contains(
            "widens: [upstream] sends the operator's key, where callers brought their own"
        ),
        "{report}"
    );
    assert!(!policy_diff(&keyed, &forwarded).widens());
    let report = policy_diff(&keyed, &upstream("api_key_env = \"OTHER\"")).report();
    assert!(
        report.contains("changes: [upstream] key: $K -> $OTHER"),
        "{report}"
    );
}

#[test]
fn a_key_set_compares_by_its_keys() {
    use wye_testkit::jwt::{IdpKey, jwks};

    let (a, b, c) = (IdpKey::ed25519(), IdpKey::ed25519(), IdpKey::ed25519());
    let dir = std::env::temp_dir().join(format!("wye-policy-diff-{}", std::process::id()));
    let side = |name: &str, keys: &[(&IdpKey, &str)]| {
        let at = dir.join(name);
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join("idp.jwks"), jwks(keys)).unwrap();
        at
    };
    let issuer = "[[auth.issuers]]\nissuer = \"https://idp.example\"\naudience = \"wye\"\n\
                  algorithm = \"EdDSA\"\njwks_file = \"idp.jwks\"\n";
    let config = config("127.0.0.1:8080", Some(issuer), "");
    let old = side("old", &[(&a, "a")]);
    let added = side("added", &[(&a, "a"), (&b, "b")]);
    let replaced = side("replaced", &[(&c, "a")]);

    let mut diff = policy_diff(&config, &config);
    diff.key_sets(&config, &old, &config, &added).unwrap();
    assert!(
        diff.report()
            .contains("widens: [[auth.issuers]] https://idp.example: key b newly trusted"),
        "{}",
        diff.report()
    );
    let mut diff = policy_diff(&config, &config);
    diff.key_sets(&config, &old, &config, &replaced).unwrap();
    let report = diff.report();
    assert!(report.contains("key a newly trusted"), "{report}");
    assert!(
        report.contains("narrows: [[auth.issuers]] https://idp.example: key a no longer trusted"),
        "{report}"
    );
    let mut diff = policy_diff(&config, &config);
    diff.key_sets(&config, &old, &config, &old).unwrap();
    assert!(!diff.widens());
    std::fs::remove_dir_all(&dir).unwrap();
}
