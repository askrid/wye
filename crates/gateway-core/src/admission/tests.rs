#![allow(clippy::unwrap_used)]

use super::*;
use crate::config::Config;

fn aliases() -> Aliases {
    Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
         [[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n\
         when = [{ subjects = [\"canary-*\"], model = \"claude-sonnet-5-5\" }]\n",
    )
    .unwrap()
    .aliases
    .unwrap()
}

fn patterns(list: &[&str]) -> Vec<ModelPattern> {
    list.iter()
        .map(|p| ModelPattern::parse(p).unwrap())
        .collect()
}

const NONE: BodyChecks<'static> = BodyChecks {
    configured: None,
    granted: None,
    token_limit: None,
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
            token_limit: Some(100),
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
        "this credential may not use the model \"coding\""
    );
    assert!(checks.resolve("claude-sonnet-5-5", None).is_none());
}

#[test]
fn a_token_value_is_final_when_read_and_a_missing_one_at_the_end() {
    let checks = BodyChecks {
        token_limit: Some(100),
        ..NONE
    };
    assert_eq!(checks.check_tokens(Api::Messages, Some(100), false), Ok(()));
    let refusal = checks
        .check_tokens(Api::Messages, Some(101), false)
        .unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "max_tokens 101 exceeds this credential's limit of 100"
    );
    // The OpenAI APIs let the field out; under a ceiling it is required,
    // once the whole body shows it is missing.
    assert_eq!(checks.check_tokens(Api::Responses, None, false), Ok(()));
    let refusal = checks.check_tokens(Api::Responses, None, true).unwrap_err();
    assert_eq!(
        (refusal.status(), refusal.kind()),
        (403, "permission_error")
    );
    assert_eq!(NONE.check_tokens(Api::Responses, None, true), Ok(()));
}

#[test]
fn the_ceiling_is_the_stricter_one_and_only_where_output_is_generated() {
    let checks = |configured, generates| {
        BodyChecks::new(None, configured, None, None, generates, false).token_limit
    };
    assert_eq!(checks(Some(100), true), Some(100));
    assert_eq!(checks(Some(100), false), None, "count_tokens");
    assert_eq!(checks(None, true), None);
}
