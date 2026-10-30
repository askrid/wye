//! Gateway credential checks, with the last verified credential cached per connection.
//!
//! Clients such as Claude Code keep a connection open and send the same
//! credential on every request, so a full signature verification (about
//! 88 µs of CPU per request for Ed25519) repeats identical work. Each
//! connection remembers the last credential it verified; a request carrying
//! the same bytes skips signature verification and only rechecks expiry.
//! Answers equal a fresh verification: keys, issuers, and audiences are fixed
//! for the process lifetime.
//!
//! The cache belongs to one connection and dies with it: nothing is shared
//! between connections or replicas. It holds one credential, and only one that
//! verified, so its size is bounded by the token size limit.

use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use gateway_core::credential::{CredentialError, Identity, Verifier};
use http::{HeaderMap, header::AUTHORIZATION};

use crate::X_API_KEY;

/// The last credential verified on one connection.
#[derive(Default)]
pub(crate) struct ConnectionAuth {
    /// Uncontended: an HTTP/1.1 connection handles one request at a time.
    /// Held only to compare or replace, never during verification or `.await`.
    last: Mutex<Option<Verified>>,
}

struct Verified {
    token: Box<[u8]>,
    /// Shared with each request it authenticates, so a cache hit allocates nothing.
    identity: Arc<Identity>,
}

/// Checks the credential in `Authorization: Bearer` (Claude Code's
/// `ANTHROPIC_AUTH_TOKEN`) or else `x-api-key` (`ANTHROPIC_API_KEY`).
pub(crate) fn authenticate(
    verifier: &Verifier,
    headers: &HeaderMap,
    connection: &ConnectionAuth,
) -> Result<Arc<Identity>, CredentialError> {
    let token = credential_from(headers).ok_or(CredentialError::Malformed)?;
    // A clock before 1970 makes every credential look expired: fail closed.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |d| d.as_secs());
    authenticate_at(verifier, token, now, connection)
}

fn credential_from(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim());
    bearer.or_else(|| headers.get(X_API_KEY).and_then(|v| v.to_str().ok()))
}

fn authenticate_at(
    verifier: &Verifier,
    token: &str,
    now: u64,
    connection: &ConnectionAuth,
) -> Result<Arc<Identity>, CredentialError> {
    // A poisoned lock only means another request on this connection panicked;
    // the cached value is still a fully verified credential.
    let mut last = connection
        .last
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(verified) = last.as_ref()
        && constant_time_eq(&verified.token, token.as_bytes())
    {
        if now >= verified.identity.expires_at {
            *last = None;
            return Err(CredentialError::Expired);
        }
        return Ok(Arc::clone(&verified.identity));
    }
    drop(last);

    let identity = Arc::new(verifier.verify(token, now)?);
    *connection
        .last
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Verified {
        token: token.as_bytes().into(),
        identity: Arc::clone(&identity),
    });
    Ok(identity)
}

/// Compares without an early exit, so timing does not reveal how much of a
/// presented token matches the cached one.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && std::hint::black_box(a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y))) == 0
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gateway_core::{
        config::{Auth, OwnIssuer, PublicKey},
        credential::{self, NewCredential},
    };
    use ring::signature::{Ed25519KeyPair, KeyPair};

    use super::*;

    const NOW: u64 = 1_800_000_000;

    use wye_testkit::ed25519_keypair as keypair;

    fn verifier(key: &Ed25519KeyPair) -> Verifier {
        let auth = Auth {
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
            max_credential_ttl: Duration::from_secs(86_400),
        };
        Verifier::new(&auth, Vec::new()).unwrap()
    }

    fn token(key: &Ed25519KeyPair, sub: &str, exp: u64) -> String {
        credential::issue(
            key,
            "k1",
            &NewCredential {
                iss: "wye",
                sub,
                aud: "wye-prod",
                iat: NOW,
                exp,
                models: None,
                max_output_tokens: None,
            },
        )
    }

    #[test]
    fn repeated_credential_skips_signature_verification() {
        let key = keypair();
        let t = token(&key, "alice", NOW + 600);
        let connection = ConnectionAuth::default();
        authenticate_at(&verifier(&key), &t, NOW, &connection).unwrap();

        // A verifier that trusts no key would reject the token; the cache answers.
        let blind = verifier(&keypair());
        let identity = authenticate_at(&blind, &t, NOW + 1, &connection).unwrap();
        assert_eq!(&*identity.subject, "alice");
    }

    #[test]
    fn a_different_credential_is_fully_verified() {
        let key = keypair();
        let connection = ConnectionAuth::default();
        authenticate_at(
            &verifier(&key),
            &token(&key, "alice", NOW + 600),
            NOW,
            &connection,
        )
        .unwrap();
        let forged = token(&keypair(), "mallory", NOW + 600);
        assert_eq!(
            authenticate_at(&verifier(&key), &forged, NOW, &connection),
            Err(CredentialError::BadSignature)
        );
        // A token differing from the cached one in its last byte is not a hit.
        let mut near = token(&key, "alice", NOW + 600);
        let last = near.pop().unwrap();
        near.push(if last == 'A' { 'B' } else { 'A' });
        assert!(authenticate_at(&verifier(&key), &near, NOW, &connection).is_err());
    }

    #[test]
    fn cached_credential_still_expires() {
        let key = keypair();
        let t = token(&key, "alice", NOW + 60);
        let connection = ConnectionAuth::default();
        authenticate_at(&verifier(&key), &t, NOW, &connection).unwrap();
        assert_eq!(
            authenticate_at(&verifier(&key), &t, NOW + 60, &connection),
            Err(CredentialError::Expired)
        );
        // The expired entry is gone; a fresh check rejects it too.
        assert!(connection.last.lock().unwrap().is_none());
    }

    #[test]
    fn rejected_credentials_are_not_cached() {
        let key = keypair();
        let connection = ConnectionAuth::default();
        let foreign = token(&keypair(), "mallory", NOW + 600);
        assert!(authenticate_at(&verifier(&key), &foreign, NOW, &connection).is_err());
        assert!(connection.last.lock().unwrap().is_none());
    }

    #[test]
    fn constant_time_eq_matches_slice_equality() {
        for (a, b) in [
            ("", ""),
            ("a", "a"),
            ("a", "b"),
            ("ab", "a"),
            ("abc", "abd"),
        ] {
            assert_eq!(
                constant_time_eq(a.as_bytes(), b.as_bytes()),
                a == b,
                "{a:?} {b:?}"
            );
        }
    }
}
