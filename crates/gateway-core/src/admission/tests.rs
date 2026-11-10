use super::*;
use crate::test_support::{config, patterns};

fn aliases() -> Aliases {
    config(
        "[[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n\
         when = [{ subjects = [\"canary-*\"], model = \"claude-sonnet-5-5\" }]\n",
    )
    .unwrap()
    .aliases
    .unwrap()
}

const NONE: BodyChecks<'static> = BodyChecks {
    configured: None,
    granted: None,
    output_ceiling: None,
    aliases: None,
    prefixes_model_ids: false,
};

#[test]
fn the_body_is_held_for_any_check_and_lexed_on_for_permissions_and_ceilings() {
    assert!(!NONE.hold());
    let aliases = aliases();
    let alias_only = BodyChecks {
        aliases: Some(&aliases),
        ..NONE
    };
    assert!(alias_only.hold() && !alias_only.checks_rest());
    let prefixed = BodyChecks {
        prefixes_model_ids: true,
        ..NONE
    };
    assert!(prefixed.hold() && !prefixed.checks_rest());
    let haiku = patterns(&["claude-haiku-*"]);
    for checks in [
        BodyChecks {
            configured: Some(&haiku),
            ..NONE
        },
        BodyChecks {
            granted: Some(&haiku),
            ..NONE
        },
        BodyChecks {
            output_ceiling: Some(100),
            ..NONE
        },
    ] {
        assert!(checks.hold() && checks.checks_rest(), "{checks:?}");
    }
}

#[test]
fn permissions_apply_to_the_resolved_model_and_name_the_sent_one() {
    let aliases = aliases();
    let sonnet = patterns(&["claude-sonnet-*"]);
    let checks = BodyChecks {
        configured: Some(&sonnet),
        aliases: Some(&aliases),
        ..NONE
    };
    let (name, model) = checks.resolve("coding", Some("canary-1")).unwrap();
    assert_eq!((&**name, &**model), ("coding", "claude-sonnet-5-5"));
    assert_eq!(checks.permit(model, "coding"), Ok(()));
    let (_, model) = checks.resolve("coding", None).unwrap();
    let refusal = checks.permit(model, "coding").unwrap_err();
    assert_eq!(refusal.status(), 403);
    assert_eq!(refusal.kind(), "permission_error");
    assert_eq!(
        refusal.to_string(),
        "this gateway token may not use the model \"coding\""
    );
    assert!(checks.resolve("claude-sonnet-5-5", None).is_none());
}

#[test]
fn a_token_value_is_final_when_read_and_a_missing_one_at_the_end() {
    let checks = BodyChecks {
        output_ceiling: Some(100),
        ..NONE
    };
    assert_eq!(
        checks.check_output_ceiling(Api::Messages, Some(100), false),
        Ok(())
    );
    let refusal = checks
        .check_output_ceiling(Api::Messages, Some(101), false)
        .unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "max_tokens 101 exceeds the output-token ceiling of 100"
    );
    // The OpenAI APIs let the field out; under a ceiling it is required,
    // once the whole body shows it is missing.
    assert_eq!(
        checks.check_output_ceiling(Api::Responses, None, false),
        Ok(())
    );
    let refusal = checks
        .check_output_ceiling(Api::Responses, None, true)
        .unwrap_err();
    assert_eq!(
        (refusal.status(), refusal.kind()),
        (403, "permission_error")
    );
    assert_eq!(
        NONE.check_output_ceiling(Api::Responses, None, true),
        Ok(())
    );
}

#[test]
fn the_ceiling_is_the_stricter_one_and_only_where_output_is_generated() {
    let checks = |configured, generates| {
        BodyChecks::new(None, configured, None, None, generates, false).output_ceiling
    };
    assert_eq!(checks(Some(100), true), Some(100));
    assert_eq!(checks(Some(100), false), None, "count_tokens");
    assert_eq!(checks(None, true), None);
}

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.append(*name, value.parse().unwrap());
    }
    headers
}

#[test]
fn a_local_client_names_a_loopback_host_with_or_without_a_port() {
    for host in [
        "localhost",
        "localhost:8080",
        "LOCALHOST:8080",
        "127.0.0.1",
        "127.0.0.1:8080",
        "[::1]",
        "[::1]:8080",
    ] {
        assert_eq!(
            check_local_client(&headers(&[("host", host)])),
            Ok(()),
            "{host}"
        );
    }
}

/// DNS rebinding: the attacker's name resolves to loopback, and the browser
/// sends that name in `Host`.
#[test]
fn any_other_host_is_refused_as_a_web_page() {
    for host in [
        "evil.example",
        "evil.example:8080",
        "localhost.evil.example:8080",
        "127.0.0.1.nip.io:8080",
        "192.168.1.10:8080",
        "[::2]:8080",
        "::1",
        "[::1]x",
        "[::1",
        "localhost:80a",
        "",
    ] {
        assert_eq!(
            check_local_client(&headers(&[("host", host)])),
            Err(Refusal::WebPage),
            "{host:?}"
        );
    }
    assert_eq!(check_local_client(&HeaderMap::new()), Err(Refusal::WebPage));
}

#[test]
fn an_origin_or_a_cross_site_fetch_is_refused_even_on_a_loopback_host() {
    let host = ("host", "127.0.0.1:8080");
    for page in [
        ("origin", "http://evil.example"),
        ("origin", "null"),
        ("origin", "http://localhost:8080"),
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-site", "same-site"),
        ("sec-fetch-site", "same-origin"),
    ] {
        let refused = check_local_client(&headers(&[host, page]));
        assert_eq!(refused, Err(Refusal::WebPage), "{page:?}");
        assert_eq!(refused.unwrap_err().status(), 403);
    }
    // A navigation the user typed sends `none`.
    assert_eq!(
        check_local_client(&headers(&[host, ("sec-fetch-site", "none")])),
        Ok(())
    );
    assert_eq!(
        check_local_client(&headers(&[
            host,
            ("sec-fetch-site", "none"),
            ("sec-fetch-site", "cross-site")
        ])),
        Err(Refusal::WebPage)
    );
}
