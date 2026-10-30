use std::time::Duration;

use ring::signature::KeyPair;

use super::*;
use crate::config::{OwnIssuer, PublicKey};

const NOW: u64 = 1_800_000_000;

use wye_testkit::ed25519_keypair as keypair;

fn verifier(key: &Ed25519KeyPair) -> Verifier {
    Verifier::new(
        &Auth {
            models: None,
            max_output_tokens: None,
            own: Some(OwnIssuer {
                issuer: "wye".into(),
                audience: "wye-prod".into(),
                keys: vec![PublicKey {
                    id: "k1".into(),
                    ed25519_public_key: key.public_key().as_ref().try_into().unwrap(),
                }],
            }),
            issuers: Vec::new(),
            max_token_ttl: Duration::from_secs(86_400),
        },
        Vec::new(),
    )
    .unwrap()
}

fn claims<'a>() -> NewCredential<'a> {
    NewCredential {
        iss: "wye",
        sub: "alice",
        aud: "wye-prod",
        iat: NOW,
        exp: NOW + 3_600,
        models: None,
        max_output_tokens: None,
    }
}

#[test]
fn issued_credential_verifies() {
    let key = keypair();
    let token = issue(&key, "k1", &claims());
    let identity = verifier(&key).verify(&token, NOW + 10).unwrap();
    assert_eq!(&*identity.subject, "alice");
    assert_eq!(&*identity.key_id, "k1");
    assert_eq!(identity.expires_at, NOW + 3_600);
    assert_eq!(identity.models, None);
}

#[test]
fn models_claim_becomes_patterns_and_bad_lists_fail_closed() {
    let key = keypair();
    let v = verifier(&key);
    let with_models = |models: &[&str]| {
        let mut c = claims();
        c.models = Some(models);
        v.verify(&issue(&key, "k1", &c), NOW + 10)
    };
    let identity = with_models(&["claude-haiku-*", "claude-sonnet-4-5"]).unwrap();
    let models = identity.models.unwrap();
    assert!(models[0].matches("claude-haiku-4-5"));
    assert!(!models[1].matches("claude-sonnet-4-5-20250929"));
    assert_eq!(with_models(&[]), Err(CredentialError::Malformed));
    assert_eq!(with_models(&["a*b"]), Err(CredentialError::Malformed));

    let with_limit = |limit: u64| {
        let mut c = claims();
        c.max_output_tokens = Some(limit);
        v.verify(&issue(&key, "k1", &c), NOW + 10)
    };
    assert_eq!(with_limit(4000).unwrap().max_output_tokens, Some(4000));
    assert_eq!(with_limit(0), Err(CredentialError::Malformed));
}

#[test]
fn rejects_by_time_issuer_audience_and_lifetime() {
    let key = keypair();
    let v = verifier(&key);
    let with = |f: fn(&mut NewCredential<'_>)| {
        let mut c = claims();
        f(&mut c);
        issue(&key, "k1", &c)
    };
    assert_eq!(
        v.verify(&issue(&key, "k1", &claims()), NOW + 3_600),
        Err(CredentialError::Expired)
    );
    assert_eq!(
        v.verify(&with(|c| c.iss = "other"), NOW),
        Err(CredentialError::WrongIssuer)
    );
    assert_eq!(
        v.verify(&with(|c| c.aud = "wye-dev"), NOW),
        Err(CredentialError::WrongAudience)
    );
    assert_eq!(
        v.verify(&with(|c| c.exp = NOW + 2 * 86_400), NOW),
        Err(CredentialError::LifetimeTooLong)
    );
    assert_eq!(
        v.verify(&with(|c| c.iat = NOW + 3_000), NOW),
        Err(CredentialError::NotYetValid)
    );
    // Within the clock-skew allowance.
    assert!(v.verify(&with(|c| c.iat = NOW + 30), NOW).is_ok());
}

#[test]
fn rejects_other_keys_and_tampering() {
    let key = keypair();
    let v = verifier(&key);
    assert_eq!(
        v.verify(&issue(&keypair(), "k1", &claims()), NOW),
        Err(CredentialError::BadSignature)
    );
    assert_eq!(
        v.verify(&issue(&key, "k2", &claims()), NOW),
        Err(CredentialError::UnknownKey)
    );
    // Swap in claims with another subject, keeping the original signature.
    let token = issue(&key, "k1", &claims());
    let mut parts: Vec<&str> = token.split('.').collect();
    let mallory = issue(
        &key,
        "k1",
        &NewCredential {
            sub: "mallory",
            ..claims()
        },
    );
    let forged_claims = mallory.split('.').nth(1).unwrap().to_owned();
    parts[1] = &forged_claims;
    assert_eq!(
        v.verify(&parts.join("."), NOW),
        Err(CredentialError::BadSignature)
    );
}

#[test]
fn rejects_unsupported_algorithms_and_headers() {
    let key = keypair();
    let v = verifier(&key);
    let token = issue(&key, "k1", &claims());
    let rest = token.split_once('.').unwrap().1;
    for header in [
        r#"{"alg":"none","kid":"k1"}"#,
        r#"{"alg":"HS256","kid":"k1"}"#,
        r#"{"alg":"EdDSA","kid":"k1","crit":["exp"]}"#,
    ] {
        let forged = format!("{}.{rest}", B64.encode(header));
        let err = v.verify(&forged, NOW).unwrap_err();
        assert!(
            matches!(
                err,
                CredentialError::UnsupportedAlgorithm | CredentialError::Malformed
            ),
            "{header}: {err:?}"
        );
    }
}

#[test]
fn rejects_malformed_and_oversized_tokens() {
    let v = verifier(&keypair());
    for token in ["", "a", "a.b", "a.b.c.d", "!!.!!.!!", "e30.e30.e30"] {
        assert!(v.verify(token, NOW).is_err(), "{token:?}");
    }
    assert_eq!(
        v.verify(&"a".repeat(MAX_TOKEN_BYTES + 1), NOW),
        Err(CredentialError::TooLarge)
    );
}

#[test]
fn audience_may_be_a_list() {
    let key = keypair();
    let header = B64.encode(r#"{"alg":"EdDSA","kid":"k1"}"#);
    let claims = B64.encode(format!(
        r#"{{"iss":"wye","sub":"ci","aud":["x","wye-prod"],"iat":{NOW},"exp":{}}}"#,
        NOW + 60
    ));
    let signed = format!("{header}.{claims}");
    let token = format!("{signed}.{}", B64.encode(key.sign(signed.as_bytes())));
    assert_eq!(&*verifier(&key).verify(&token, NOW).unwrap().subject, "ci");
}

#[test]
fn public_key_round_trips() {
    let key = keypair();
    let encoded = encode_public_key(key.public_key().as_ref());
    assert_eq!(
        decode_public_key(&encoded).unwrap(),
        key.public_key().as_ref()
    );
    assert!(decode_public_key("short").is_none());
}

mod provider;
