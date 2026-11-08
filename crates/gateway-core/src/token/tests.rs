use std::time::Duration;

use ring::signature::{Ed25519KeyPair, KeyPair};

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
                audience: "wye-prod".into(),
                keys: vec![PublicKey::new(public(key))],
            }),
            identity_providers: Vec::new(),
            max_gateway_token_ttl: Duration::from_secs(86_400),
        },
        Vec::new(),
    )
    .unwrap()
}

fn public(key: &Ed25519KeyPair) -> [u8; 32] {
    key.public_key().as_ref().try_into().unwrap()
}

/// The key id Wye's tokens signed with `key` carry.
fn kid(key: &Ed25519KeyPair) -> String {
    thumbprint(&public(key))
}

/// A token of `header` and `claims` (JSON), signed with `key` as issued.
fn signed(key: &Ed25519KeyPair, header: &str, claims: &str) -> String {
    let signed = format!("{}.{}", B64.encode(header), B64.encode(claims));
    format!("{signed}.{}", B64.encode(key.sign(signed.as_bytes())))
}

fn claims<'a>() -> NewToken<'a> {
    NewToken {
        sub: "alice",
        aud: "wye-prod",
        iat: NOW,
        exp: NOW + 3_600,
        models: None,
        max_output_tokens: None,
    }
}

#[test]
fn issued_token_verifies() {
    let key = keypair();
    let token = issue(&key, &claims());
    let header = B64.decode(token.split('.').next().unwrap()).unwrap();
    assert_eq!(
        String::from_utf8(header).unwrap(),
        format!(r#"{{"alg":"EdDSA","kid":"{}","typ":"JWT"}}"#, kid(&key))
    );
    let claims_json = B64.decode(token.split('.').nth(1).unwrap()).unwrap();
    assert!(
        String::from_utf8(claims_json)
            .unwrap()
            .starts_with(r#"{"iss":"wye","#)
    );
    let identity = verifier(&key).verify(&token, NOW + 10).unwrap();
    assert_eq!(&*identity.subject, "alice");
    assert_eq!(*identity.key_id, kid(&key));
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
        v.verify(&issue(&key, &c), NOW + 10)
    };
    let identity = with_models(&["claude-haiku-*", "claude-sonnet-4-5"]).unwrap();
    let models = identity.models.unwrap();
    assert!(models[0].matches("claude-haiku-4-5"));
    assert!(!models[1].matches("claude-sonnet-4-5-20250929"));
    assert_eq!(with_models(&[]), Err(TokenError::Malformed));
    assert_eq!(with_models(&["a*b"]), Err(TokenError::Malformed));

    let with_limit = |limit: u64| {
        let mut c = claims();
        c.max_output_tokens = Some(limit);
        v.verify(&issue(&key, &c), NOW + 10)
    };
    assert_eq!(with_limit(4000).unwrap().max_output_tokens, Some(4000));
    assert_eq!(with_limit(0), Err(TokenError::Malformed));
}

#[test]
fn rejects_by_time_issuer_audience_and_lifetime() {
    let key = keypair();
    let v = verifier(&key);
    let with = |f: fn(&mut NewToken<'_>)| {
        let mut c = claims();
        f(&mut c);
        issue(&key, &c)
    };
    assert_eq!(
        v.verify(&issue(&key, &claims()), NOW + 3_600),
        Err(TokenError::Expired)
    );
    let other_issuer = signed(
        &key,
        &format!(r#"{{"alg":"EdDSA","kid":"{}"}}"#, kid(&key)),
        &format!(
            r#"{{"iss":"other","sub":"a","aud":"wye-prod","iat":{NOW},"exp":{}}}"#,
            NOW + 60
        ),
    );
    assert_eq!(v.verify(&other_issuer, NOW), Err(TokenError::WrongIssuer));
    assert_eq!(
        v.verify(&with(|c| c.aud = "wye-dev"), NOW),
        Err(TokenError::WrongAudience)
    );
    assert_eq!(
        v.verify(&with(|c| c.exp = NOW + 2 * 86_400), NOW),
        Err(TokenError::LifetimeTooLong)
    );
    assert_eq!(
        v.verify(&with(|c| c.iat = NOW + 3_000), NOW),
        Err(TokenError::NotYetValid)
    );
    // Within the clock-skew allowance.
    assert!(v.verify(&with(|c| c.iat = NOW + 30), NOW).is_ok());
}

#[test]
fn rejects_other_keys_and_tampering() {
    let key = keypair();
    let v = verifier(&key);
    // Another key under this key's id.
    let header = format!(r#"{{"alg":"EdDSA","kid":"{}"}}"#, kid(&key));
    let body = format!(
        r#"{{"iss":"wye","sub":"a","aud":"wye-prod","iat":{NOW},"exp":{}}}"#,
        NOW + 60
    );
    assert_eq!(
        v.verify(&signed(&keypair(), &header, &body), NOW),
        Err(TokenError::BadSignature)
    );
    assert_eq!(
        v.verify(&issue(&keypair(), &claims()), NOW),
        Err(TokenError::UnknownKey)
    );
    // Swap in claims with another subject, keeping the original signature.
    let token = issue(&key, &claims());
    let mut parts: Vec<&str> = token.split('.').collect();
    let mallory = issue(
        &key,
        &NewToken {
            sub: "mallory",
            ..claims()
        },
    );
    let forged_claims = mallory.split('.').nth(1).unwrap().to_owned();
    parts[1] = &forged_claims;
    assert_eq!(
        v.verify(&parts.join("."), NOW),
        Err(TokenError::BadSignature)
    );
}

#[test]
fn rejects_unsupported_algorithms_and_headers() {
    let key = keypair();
    let v = verifier(&key);
    let token = issue(&key, &claims());
    let rest = token.split_once('.').unwrap().1;
    let kid = kid(&key);
    for header in [
        format!(r#"{{"alg":"none","kid":"{kid}"}}"#),
        format!(r#"{{"alg":"HS256","kid":"{kid}"}}"#),
        format!(r#"{{"alg":"EdDSA","kid":"{kid}","crit":["exp"]}}"#),
    ] {
        let forged = format!("{}.{rest}", B64.encode(&header));
        let err = v.verify(&forged, NOW).unwrap_err();
        assert!(
            matches!(
                err,
                TokenError::UnsupportedAlgorithm | TokenError::Malformed
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
        Err(TokenError::TooLarge)
    );
}

#[test]
fn audience_may_be_a_list() {
    let key = keypair();
    let token = signed(
        &key,
        &format!(r#"{{"alg":"EdDSA","kid":"{}"}}"#, kid(&key)),
        &format!(
            r#"{{"iss":"wye","sub":"ci","aud":["x","wye-prod"],"iat":{NOW},"exp":{}}}"#,
            NOW + 60
        ),
    );
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

#[test]
fn a_key_id_is_the_rfc_7638_thumbprint() {
    // RFC 8037, appendix A.3.
    let x = decode_public_key("11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo").unwrap();
    assert_eq!(
        thumbprint(&x),
        "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k"
    );
}

mod identity_provider;
