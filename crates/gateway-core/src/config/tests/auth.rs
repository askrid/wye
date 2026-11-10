//! `[auth]`: Wye's own signing keys, identity providers, and the limits
//! they put on tokens.

use super::*;

#[test]
fn auth_allows_network_listen_and_validates_keys() {
    let with_auth = |listen: &str, upstream: &str, auth: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"{listen}\"\n[[upstreams]]\nbase_url = \"https://api.anthropic.com\"\n\
             {upstream}\n[auth]\naudience = \"wye-prod\"\n{auth}"
        ))
    };
    let keys = format!("signing_keys = [\"{PUBLIC_KEY}\"]");
    let config = with_auth("0.0.0.0:8080", "provider_key = { env = \"K\" }", &keys).unwrap();
    let auth = config.auth.unwrap();
    let own = auth.own.unwrap();
    assert_eq!(own.keys.len(), 1);
    // Named by its RFC 7638 thumbprint (RFC 8037, appendix A.3).
    assert_eq!(
        own.keys[0].thumbprint,
        "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k"
    );
    assert_eq!(
        crate::token::encode_public_key(&own.keys[0].ed25519_public_key),
        PUBLIC_KEY
    );
    assert_eq!(
        auth.max_gateway_token_ttl,
        Duration::from_secs(7 * 24 * 3600)
    );

    let error = |upstream: &str, auth: &str| {
        with_auth("127.0.0.1:8080", upstream, auth)
            .unwrap_err()
            .to_string()
    };
    let key = "provider_key = { env = \"K\" }";
    assert!(
        error("forward_caller_credential = true", &keys)
            .starts_with("auth: cannot be combined with forward_caller_credential")
    );
    assert_eq!(
        error(key, "signing_keys = []"),
        "auth: audience needs at least one key in signing_keys"
    );
    assert_eq!(
        error(key, "signing_keys = [\"not-a-key\"]"),
        "auth: signing_keys: \"not-a-key\" is not an Ed25519 public key, 32 bytes in \
         unpadded base64url"
    );
    assert_eq!(
        error(
            key,
            &format!("signing_keys = [\"{PUBLIC_KEY}\", \"{PUBLIC_KEY}\"]")
        ),
        format!("auth: signing_keys: \"{PUBLIC_KEY}\" is listed twice")
    );
    // Wye's own tokens always name the issuer "wye", and keys need no id.
    assert!(error(key, &format!("{keys}\nissuer = \"wye\"")).contains("unknown field"));
    let table =
        format!("[[auth.signing_keys]]\nid = \"k1\"\ned25519_public_key = \"{PUBLIC_KEY}\"");
    assert!(error(key, &table).starts_with("invalid configuration"));
}

/// Gateway tokens cannot be revoked one by one, so how long one may live
/// has a hard cap.
#[test]
fn gateway_token_lifetimes_are_capped_at_30_days() {
    let with_ttl = |ttl: &str| {
        config(&format!(
            "[auth]\naudience = \"wye-prod\"\nmax_gateway_token_ttl = \"{ttl}\"\n\
             signing_keys = [\"{PUBLIC_KEY}\"]"
        ))
    };
    let ttl = |text| with_ttl(text).unwrap().auth.unwrap().max_gateway_token_ttl;
    assert_eq!(ttl("30d"), Duration::from_secs(2_592_000));
    assert_eq!(ttl("1h"), Duration::from_secs(3600));
    assert_eq!(
        with_ttl("2592001s").unwrap_err(),
        ConfigError::TooLong {
            field: "auth.max_gateway_token_ttl",
            max: MAX_GATEWAY_TOKEN_TTL
        }
    );
    assert_eq!(
        with_ttl("31d").unwrap_err().to_string(),
        "auth.max_gateway_token_ttl must be at most 30d"
    );
    assert_eq!(
        with_ttl("0s").unwrap_err(),
        ConfigError::Zero("auth.max_gateway_token_ttl")
    );
}

#[test]
fn auth_models_are_validated_patterns() {
    let with_models = |models: &str| {
        config(&format!(
            "[auth]\naudience = \"wye-prod\"\n{models}\nsigning_keys = [\"{PUBLIC_KEY}\"]"
        ))
    };
    assert!(with_models("").unwrap().auth.unwrap().models.is_none());
    let models = with_models("models = [\"claude-haiku-*\", \"claude-sonnet-4-5\"]")
        .unwrap()
        .auth
        .unwrap()
        .models
        .unwrap();
    assert!(models[0].matches("claude-haiku-4-5"));
    for bad in ["models = []", "models = [\"a*b\"]", "models = [\"\"]"] {
        assert!(
            matches!(with_models(bad), Err(ConfigError::Auth(_))),
            "{bad}"
        );
    }
    assert_eq!(
        with_models("max_output_tokens = 8000")
            .unwrap()
            .auth
            .unwrap()
            .max_output_tokens,
        Some(8000)
    );
    assert_eq!(
        with_models("max_output_tokens = 0").unwrap_err(),
        ConfigError::Zero("auth.max_output_tokens")
    );
}

#[test]
fn identity_provider_issuers_are_validated() {
    let parse_auth = |auth: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"0.0.0.0:8080\"\n[[upstreams]]\nbase_url = \"https://api.anthropic.com\"\n\
             provider_key = {{ env = \"K\" }}\n[auth]\n{auth}"
        ))
    };
    let idp = "[[auth.identity_providers]]\nissuer = \"https://token.actions.githubusercontent.com\"\n\
               audience = \"wye-prod\"\nalgorithm = \"RS256\"\njwks_file = \"github.jwks\"\n";
    // Issuers alone are enough: Wye's own keys are optional.
    let auth = parse_auth(idp).unwrap().auth.unwrap();
    assert!(auth.own.is_none());
    let issuer = &auth.identity_providers[0];
    assert_eq!(issuer.algorithm, Algorithm::Rs256);
    assert_eq!(issuer.subject_claim, "sub");
    assert!(issuer.grants.is_empty());

    let grants = format!(
        "{idp}subject_claim = \"repository\"\n[[auth.identity_providers.grants]]\nclaim = \"repository_owner\"\n\
         value = \"acme\"\nmodels = [\"claude-haiku-*\"]\n"
    );
    let auth = parse_auth(&grants).unwrap().auth.unwrap();
    assert_eq!(auth.identity_providers[0].subject_claim, "repository");
    assert_eq!(
        auth.identity_providers[0].grants[0].models[0].to_string(),
        "claude-haiku-*"
    );

    let own = format!("audience = \"wye-prod\"\nsigning_keys = [\"{PUBLIC_KEY}\"]\n");
    let auth = parse_auth(&format!("{own}{idp}")).unwrap().auth.unwrap();
    assert!(auth.own.is_some() && auth.identity_providers.len() == 1);

    for bad in [
        String::new(),                          // neither own keys nor identity providers
        "audience = \"wye-prod\"\n".to_owned(), // own audience without keys
        format!("signing_keys = [\"{PUBLIC_KEY}\"]\n"), // own keys without an audience
        idp.replace("RS256", "HS256"),          // HMAC is never accepted
        idp.replace("RS256", "none"),
        idp.replace("github.jwks", ""),
        format!("{idp}{idp}"), // duplicate issuer
        // Wye's own tokens name the issuer "wye".
        format!(
            "{own}{}",
            idp.replace("https://token.actions.githubusercontent.com", "wye")
        ),
        format!("{idp}subject_claim = \"\"\n"),
        format!(
            "{idp}[[auth.identity_providers.grants]]\nclaim = \"groups\"\nvalue = \"x\"\nmodels = []\n"
        ),
        format!(
            "{idp}[[auth.identity_providers.grants]]\nclaim = \"\"\nvalue = \"x\"\nmodels = [\"a\"]\n"
        ),
        format!(
            "{idp}[[auth.identity_providers.grants]]\nclaim = \"g\"\nvalue = \"x\"\nmodels = [\"a*b\"]\n"
        ),
        format!("{idp}jwks_url = \"https://example.com\"\n"), // no runtime fetching
    ] {
        assert!(parse_auth(&bad).is_err(), "accepted:\n{bad}");
    }
}
