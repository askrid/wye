//! Identity-provider claims: read as a JSON object, because providers name
//! claims freely, then checked like Wye's own and mapped to a subject and
//! granted models.

use serde_json::{Map, Value};

use super::{Audience, CredentialError, Identity, Registered, Source, Verifier};
use crate::{config::Grant, policy::ModelPattern};

impl Audience {
    /// `aud` from a claims object: a string or a list of strings.
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(aud) => Some(Self::One(aud.clone())),
            Value::Array(items) => items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<_>>()
                .map(Self::Many),
            _ => None,
        }
    }
}

impl Verifier {
    /// Identity providers name claims freely, so the claims are read as a
    /// JSON object and the configured subject and grant claims looked up.
    pub(super) fn provider_identity(
        &self,
        source: &Source,
        claims_json: &[u8],
        subject_claim: &str,
        grants: &[Grant],
        kid: &str,
        now: u64,
    ) -> Result<Identity, CredentialError> {
        let claims: Map<String, Value> =
            serde_json::from_slice(claims_json).map_err(|_| CredentialError::Malformed)?;
        let number = |name: &str| claims.get(name).map(Value::as_u64);
        let (Some(Some(exp)), Some(Some(iat))) = (number("exp"), number("iat")) else {
            return Err(CredentialError::Malformed);
        };
        let nbf = match number("nbf") {
            None => None,
            Some(Some(nbf)) => Some(nbf),
            Some(None) => return Err(CredentialError::Malformed),
        };
        let iss = claims
            .get("iss")
            .and_then(Value::as_str)
            .ok_or(CredentialError::Malformed)?;
        let aud = claims
            .get("aud")
            .and_then(Audience::from_value)
            .ok_or(CredentialError::Malformed)?;
        self.check(
            source,
            &Registered {
                iss,
                aud: &aud,
                exp,
                iat,
                nbf,
            },
            now,
        )?;
        let subject = claims
            .get(subject_claim)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(CredentialError::MissingSubject)?;
        Ok(Identity {
            subject: subject.into(),
            key_id: kid.into(),
            expires_at: exp,
            models: granted(grants, &claims),
            max_output_tokens: None,
        })
    }
}

/// The models an identity provider's token is granted: `None` without
/// grants, otherwise the union of the matching grants (empty if none match).
fn granted(grants: &[Grant], claims: &Map<String, Value>) -> Option<Box<[ModelPattern]>> {
    if grants.is_empty() {
        return None;
    }
    let has = |grant: &Grant| match claims.get(&grant.claim) {
        Some(Value::String(value)) => *value == grant.value,
        Some(Value::Array(values)) => values.iter().any(|v| v.as_str() == Some(&grant.value)),
        _ => false,
    };
    Some(
        grants
            .iter()
            .filter(|grant| has(grant))
            .flat_map(|grant| grant.models.iter().cloned())
            .collect(),
    )
}
