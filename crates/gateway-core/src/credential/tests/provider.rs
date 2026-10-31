//! Identity-provider tokens: pinned algorithms, JWKS key sets, subject
//! claims, and grants.

use std::time::Duration;

use ring::signature::KeyPair;
use wye_testkit::jwt::{IdpKey, jwks};

use super::super::*;
use crate::config::{Grant, Issuer, OwnIssuer, PublicKey};

const NOW: u64 = 1_800_000_000;
const ISS: &str = "https://idp.example.com";

fn issuer(algorithm: Algorithm, grants: Vec<Grant>) -> Issuer {
    Issuer {
        issuer: ISS.into(),
        audience: "wye-prod".into(),
        algorithm,
        jwks_file: "idp.jwks".into(),
        subject_claim: "sub".into(),
        grants,
    }
}

fn auth(issuers: Vec<Issuer>) -> Auth {
    Auth {
        own: None,
        issuers,
        max_token_ttl: Duration::from_secs(86_400),
        models: None,
        max_output_tokens: None,
    }
}

fn grant(claim: &str, value: &str, models: &[&str]) -> Grant {
    Grant {
        claim: claim.into(),
        value: value.into(),
        models: models
            .iter()
            .map(|m| ModelPattern::parse(m).unwrap())
            .collect(),
    }
}

/// A verifier trusting `key` (kid `idp-1`) for `issuer`.
fn verifier_for(key: &IdpKey, issuer: Issuer) -> Verifier {
    let set = ProviderKeys::parse(jwks(&[(key, "idp-1")]).as_bytes(), issuer.algorithm).unwrap();
    Verifier::new(&auth(vec![issuer]), vec![set]).unwrap()
}

fn claims(extra: &str) -> String {
    format!(
        r#"{{"iss":"{ISS}","aud":"wye-prod","sub":"user-1","iat":{NOW},"exp":{}{extra}}}"#,
        NOW + 600
    )
}

fn algorithm_of(key: &IdpKey) -> Algorithm {
    match key {
        IdpKey::Rs256(_) => Algorithm::Rs256,
        IdpKey::Es256(_) => Algorithm::Es256,
        IdpKey::EdDsa(_) => Algorithm::EdDsa,
    }
}

#[test]
fn each_algorithm_verifies_and_names_the_subject() {
    for key in [IdpKey::rs256(), IdpKey::es256(), IdpKey::ed25519()] {
        let v = verifier_for(&key, issuer(algorithm_of(&key), Vec::new()));
        let identity = v.verify(&key.token("idp-1", &claims("")), NOW).unwrap();
        assert_eq!(&*identity.subject, "user-1", "{}", key.alg());
        assert_eq!(&*identity.key_id, "idp-1");
        assert_eq!(identity.models, None, "no grants: the ceiling decides");
        assert_eq!(identity.max_output_tokens, None);
    }
}

#[test]
fn the_algorithm_is_pinned_per_issuer() {
    let key = IdpKey::es256();
    let v = verifier_for(&key, issuer(Algorithm::Es256, Vec::new()));
    for alg in ["RS256", "HS256", "none", "EdDSA"] {
        let token = key.sign(&format!(r#"{{"alg":"{alg}","kid":"idp-1"}}"#), &claims(""));
        assert_eq!(
            v.verify(&token, NOW),
            Err(CredentialError::UnsupportedAlgorithm),
            "{alg}"
        );
    }
}

#[test]
fn provider_headers_may_carry_extra_fields_but_not_crit() {
    let key = IdpKey::rs256();
    let v = verifier_for(&key, issuer(Algorithm::Rs256, Vec::new()));
    let x5t = key.sign(
        r#"{"typ":"JWT","alg":"RS256","x5t":"abc","kid":"idp-1"}"#,
        &claims(""),
    );
    assert!(v.verify(&x5t, NOW).is_ok());
    let crit = key.sign(
        r#"{"alg":"RS256","kid":"idp-1","crit":["exp"]}"#,
        &claims(""),
    );
    assert_eq!(v.verify(&crit, NOW), Err(CredentialError::Malformed));
    let no_kid = key.sign(r#"{"alg":"RS256"}"#, &claims(""));
    assert_eq!(v.verify(&no_kid, NOW), Err(CredentialError::UnknownKey));
}

#[test]
fn registered_claims_are_checked_like_wyes_own() {
    let key = IdpKey::es256();
    let v = verifier_for(&key, issuer(Algorithm::Es256, Vec::new()));
    let token = |claims: &str| key.token("idp-1", claims);
    let exp = NOW + 600;
    let cases = [
        (
            format!(r#"{{"iss":"wye","aud":"wye-prod","sub":"u","iat":{NOW},"exp":{exp}}}"#),
            CredentialError::WrongIssuer,
        ),
        (
            format!(r#"{{"iss":"{ISS}","aud":["other"],"sub":"u","iat":{NOW},"exp":{exp}}}"#),
            CredentialError::WrongAudience,
        ),
        (
            format!(r#"{{"iss":"{ISS}","aud":"wye-prod","sub":"u","iat":{NOW},"exp":{NOW}}}"#),
            CredentialError::Expired,
        ),
        (
            format!(
                r#"{{"iss":"{ISS}","aud":"wye-prod","sub":"u","iat":{NOW},"nbf":{},"exp":{exp}}}"#,
                NOW + 120
            ),
            CredentialError::NotYetValid,
        ),
        (
            format!(
                r#"{{"iss":"{ISS}","aud":"wye-prod","sub":"u","iat":{NOW},"exp":{}}}"#,
                NOW + 86_401
            ),
            CredentialError::LifetimeTooLong,
        ),
        (
            format!(r#"{{"iss":"{ISS}","aud":"wye-prod","sub":"u","exp":{exp}}}"#),
            CredentialError::Malformed,
        ),
        (
            format!(r#"{{"iss":"{ISS}","aud":"wye-prod","sub":"u","iat":"{NOW}","exp":{exp}}}"#),
            CredentialError::Malformed,
        ),
        (
            format!(r#"{{"iss":"{ISS}","aud":"wye-prod","iat":{NOW},"exp":{exp}}}"#),
            CredentialError::MissingSubject,
        ),
        (
            format!(r#"{{"iss":"{ISS}","aud":"wye-prod","sub":"","iat":{NOW},"exp":{exp}}}"#),
            CredentialError::MissingSubject,
        ),
    ];
    for (claims, expected) in cases {
        assert_eq!(v.verify(&token(&claims), NOW), Err(expected), "{claims}");
    }
    // A key with the right kid but not the configured one.
    let forged = IdpKey::es256().token("idp-1", &self::claims(""));
    assert_eq!(v.verify(&forged, NOW), Err(CredentialError::BadSignature));
}

#[test]
fn subject_comes_from_the_configured_claim() {
    let key = IdpKey::rs256();
    let mut iss = issuer(Algorithm::Rs256, Vec::new());
    iss.subject_claim = "email".into();
    let v = verifier_for(&key, iss);
    let with_email = key.token("idp-1", &claims(r#","email":"dev@example.com""#));
    assert_eq!(
        &*v.verify(&with_email, NOW).unwrap().subject,
        "dev@example.com"
    );
    let without = key.token("idp-1", &claims(""));
    assert_eq!(
        v.verify(&without, NOW),
        Err(CredentialError::MissingSubject)
    );
}

#[test]
fn grants_give_the_union_of_matching_models() {
    let key = IdpKey::es256();
    let grants = vec![
        grant("groups", "ml-team", &["claude-sonnet-*"]),
        grant("groups", "infra", &["claude-haiku-*"]),
        grant("repository", "org/app", &["claude-opus-4-1"]),
    ];
    let v = verifier_for(&key, issuer(Algorithm::Es256, grants));
    let models = |extra: &str| {
        v.verify(&key.token("idp-1", &claims(extra)), NOW)
            .unwrap()
            .models
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        models(r#","groups":["infra","ml-team","other"]"#),
        ["claude-sonnet-*", "claude-haiku-*"]
    );
    assert_eq!(models(r#","repository":"org/app""#), ["claude-opus-4-1"]);
    assert_eq!(models(r#","groups":"infra""#), ["claude-haiku-*"]);
    // Matching no grant leaves an empty list: no model is allowed.
    assert!(models(r#","groups":["other"]"#).is_empty());
    assert!(models("").is_empty());
    assert!(models(r#","groups":[1,true]"#).is_empty());
}

#[test]
fn provider_tokens_cannot_claim_wye_policy() {
    let key = IdpKey::rs256();
    let v = verifier_for(&key, issuer(Algorithm::Rs256, Vec::new()));
    let token = key.token(
        "idp-1",
        &claims(r#","models":["claude-opus-*"],"max_output_tokens":99"#),
    );
    let identity = v.verify(&token, NOW).unwrap();
    assert_eq!(
        identity.models, None,
        "models claims count only from wye token issue"
    );
    assert_eq!(identity.max_output_tokens, None);
}

#[test]
fn own_and_provider_keys_coexist_by_kid() {
    let own = wye_testkit::ed25519_keypair();
    let idp = IdpKey::ed25519();
    let mut both = auth(vec![issuer(Algorithm::EdDsa, Vec::new())]);
    both.own = Some(OwnIssuer {
        issuer: "wye".into(),
        audience: "wye-prod".into(),
        keys: vec![PublicKey {
            id: "k1".into(),
            ed25519_public_key: own.public_key().as_ref().try_into().unwrap(),
        }],
    });
    let set =
        || ProviderKeys::parse(jwks(&[(&idp, "idp-1")]).as_bytes(), Algorithm::EdDsa).unwrap();
    let v = Verifier::new(&both, vec![set()]).unwrap();
    let own_token = issue(
        &own,
        "k1",
        &NewCredential {
            iss: "wye",
            sub: "alice",
            aud: "wye-prod",
            iat: NOW,
            exp: NOW + 60,
            models: None,
            max_output_tokens: None,
        },
    );
    assert_eq!(&*v.verify(&own_token, NOW).unwrap().subject, "alice");
    assert_eq!(
        &*v.verify(&idp.token("idp-1", &claims("")), NOW)
            .unwrap()
            .subject,
        "user-1"
    );
    // A provider token naming Wye's issuer is still the provider's.
    let posing = idp.token(
        "idp-1",
        &format!(
            r#"{{"iss":"wye","aud":"wye-prod","sub":"x","iat":{NOW},"exp":{}}}"#,
            NOW + 60
        ),
    );
    assert_eq!(v.verify(&posing, NOW), Err(CredentialError::WrongIssuer));
    // Wye's own header stays strict.
    let extra = idp.sign(r#"{"alg":"EdDSA","kid":"k1","x5t":"a"}"#, &claims(""));
    assert_eq!(v.verify(&extra, NOW), Err(CredentialError::Malformed));

    let clash = ProviderKeys::parse(jwks(&[(&idp, "k1")]).as_bytes(), Algorithm::EdDsa).unwrap();
    assert_eq!(
        Verifier::new(&both, vec![clash]).err(),
        Some(KeySetError::DuplicateKid("k1".into()))
    );
}

#[test]
fn issuers_without_key_sets_accept_nothing() {
    let key = IdpKey::es256();
    let v = Verifier::new(
        &auth(vec![issuer(Algorithm::Es256, Vec::new())]),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        v.verify(&key.token("idp-1", &claims("")), NOW),
        Err(CredentialError::UnknownKey)
    );
}

#[test]
fn key_sets_keep_only_signing_keys_for_the_algorithm() {
    let rsa = IdpKey::rs256();
    let ec = IdpKey::es256();
    let ed = IdpKey::ed25519();
    let enc = rsa.jwk("enc-1").replace(r#""use":"sig""#, r#""use":"enc""#);
    let other_alg = rsa
        .jwk("ps-1")
        .replace(r#""alg":"RS256""#, r#""alg":"PS256""#);
    let doc = format!(
        r#"{{"keys":[{},{},{},{enc},{other_alg},{{"kty":"oct","k":"c2VjcmV0"}}]}}"#,
        rsa.jwk("rsa-1"),
        ec.jwk("ec-1"),
        ed.jwk("ed-1"),
    );
    let ids = |alg| {
        ProviderKeys::parse(doc.as_bytes(), alg)
            .unwrap()
            .keys
            .iter()
            .map(|(kid, _)| kid.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(Algorithm::Rs256), ["rsa-1"]);
    assert_eq!(ids(Algorithm::Es256), ["ec-1"]);
    assert_eq!(ids(Algorithm::EdDsa), ["ed-1"]);
}

#[test]
fn unusable_key_sets_are_rejected() {
    let ec = IdpKey::es256();
    let parse = |doc: &str| ProviderKeys::parse(doc.as_bytes(), Algorithm::Es256).err();
    assert!(matches!(parse("{}"), Some(KeySetError::Json(_))));
    assert!(matches!(parse("not json"), Some(KeySetError::Json(_))));
    assert_eq!(
        parse(r#"{"keys":[]}"#),
        Some(KeySetError::NoUsableKey(Algorithm::Es256))
    );
    let no_kid = ec.jwk("x").replace(r#""kid":"x","#, "");
    assert_eq!(
        parse(&format!(r#"{{"keys":[{no_kid}]}}"#)),
        Some(KeySetError::MissingKid)
    );
    assert_eq!(
        parse(&jwks(&[(&ec, "a"), (&ec, "a")])),
        Some(KeySetError::DuplicateKid("a".into()))
    );
    let short = r#"{"keys":[{"kty":"EC","crv":"P-256","kid":"s","x":"AAAA","y":"AAAA"}]}"#;
    assert!(matches!(parse(short), Some(KeySetError::InvalidKey { .. })));
    let small_rsa = r#"{"keys":[{"kty":"RSA","kid":"r","n":"AQAB","e":"AQAB"}]}"#;
    assert!(matches!(
        ProviderKeys::parse(small_rsa.as_bytes(), Algorithm::Rs256),
        Err(KeySetError::InvalidKey { .. })
    ));
    let huge = format!(r#"{{"keys":[],"pad":"{}"}}"#, "a".repeat(1024 * 1024));
    assert_eq!(parse(&huge), Some(KeySetError::TooLarge));
}
