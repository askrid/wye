//! Body admission: what config and a credential decide about one request
//! body, before and once its `model` is read.
//!
//! `serve` (`crates/wye/src/gateway/admit.rs`) and `wye explain` both decide
//! through [`BodyChecks`], so an offline answer cannot drift from the
//! server's. Reading the body (holding it, splicing an alias in) stays with
//! the caller; this module only decides.

use std::{fmt, sync::Arc};

use crate::{
    alias::Aliases,
    api::Api,
    credential::Identity,
    policy::{self, ModelPattern, PolicyError, PolicyScanner},
};

/// The scanner for a request body under `token_limit`: the field is read only
/// when a ceiling applies, so a value only a ceiling would mind is not refused.
pub fn scanner(api: Api, token_limit: Option<u64>) -> PolicyScanner {
    let scanner = PolicyScanner::for_api(api);
    if token_limit.is_some() {
        scanner
    } else {
        scanner.without_token_field()
    }
}

/// The checks that apply to one request's body.
#[derive(Debug, Clone, Copy)]
pub struct BodyChecks<'a> {
    /// `[auth] models`: models any credential may use.
    pub configured: Option<&'a [ModelPattern]>,
    /// The credential's own list, which can only narrow `configured`.
    pub granted: Option<&'a [ModelPattern]>,
    /// The output-token ceiling, the stricter of config's and the
    /// credential's; `None` for an endpoint that generates nothing.
    pub token_limit: Option<u64>,
    pub aliases: Option<&'a Aliases>,
    /// An upstream of the route has its own model ids (Bedrock), so the
    /// model's place in the body is needed.
    pub prefixes_model_ids: bool,
}

impl<'a> BodyChecks<'a> {
    /// The checks config's `[auth]` and `aliases`, and `identity`, set for a
    /// request to `api`. `generates`: the endpoint generates output (not
    /// `count_tokens`), so the output-token ceiling applies.
    pub fn new(
        configured: Option<&'a [ModelPattern]>,
        configured_max_output_tokens: Option<u64>,
        aliases: Option<&'a Aliases>,
        identity: Option<&'a Identity>,
        generates: bool,
        prefixes_model_ids: bool,
    ) -> Self {
        let token_limit = generates
            .then(|| {
                policy::token_limit(
                    configured_max_output_tokens,
                    identity.and_then(|i| i.max_output_tokens),
                )
            })
            .flatten();
        Self {
            configured,
            granted: identity.and_then(|i| i.models.as_deref()),
            token_limit,
            aliases,
            prefixes_model_ids,
        }
    }

    /// Whether the body is held until its `model` is read; otherwise it is
    /// forwarded unread.
    pub fn hold(&self) -> bool {
        self.checks_rest() || self.aliases.is_some() || self.prefixes_model_ids
    }

    /// Whether the rest of the body is checked as it streams (a repeated
    /// `model`, an output-token field after the hold): for permissions and
    /// ceilings, not for an alias or a provider's model id alone, since
    /// lexing the rest costs CPU per uploaded MB.
    pub fn checks_rest(&self) -> bool {
        self.configured.is_some() || self.granted.is_some() || self.token_limit.is_some()
    }

    /// The alias `sent` names and the model it resolves to for `subject`;
    /// `None` when `sent` is no alias and runs as it is.
    pub fn resolve(
        &self,
        sent: &str,
        subject: Option<&str>,
    ) -> Option<(&'a Arc<str>, &'a Arc<str>)> {
        let alias = self.aliases?.get(sent)?;
        Some((alias.name(), alias.model_for(subject)))
    }

    /// Whether the credential may use `model`, the model that runs (an
    /// alias already resolved). The refusal names the model as the client
    /// sent it (`sent`): what an alias maps to is config's business.
    pub fn permit(&self, model: &str, sent: &str) -> Result<(), Refusal> {
        if policy::permitted(self.configured, self.granted, model) {
            Ok(())
        } else {
            Err(Refusal::Model(sent.into()))
        }
    }

    /// The output-token check: a value is final once read, a missing field
    /// only once `body_read` (the whole body was read without one).
    pub fn check_tokens(
        &self,
        api: Api,
        requested: Option<u64>,
        body_read: bool,
    ) -> Result<(), Refusal> {
        match self.token_limit {
            Some(limit) if requested.is_some() || body_read => {
                TokenLimitError::check(api, limit, requested)
                    .map_or(Ok(()), |e| Err(Refusal::TokenLimit(e)))
            }
            _ => Ok(()),
        }
    }
}

/// A policy refusal, wherever it is decided: the status, error type, and
/// message are the same from `serve` and from `wye explain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The body's `model` or a top-level key cannot be checked, or the
    /// output-token field is malformed.
    Invalid(PolicyError),
    /// The credential may not use this model, as the client named it.
    Model(Box<str>),
    /// The output-token maximum is above the ceiling, or missing where the
    /// ceiling requires it.
    TokenLimit(TokenLimitError),
    /// A known endpoint whose API no route serves.
    NoRoute { api: Api, path: Box<str> },
}

impl Refusal {
    /// The HTTP status.
    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::Model(_) | Self::TokenLimit(_) => 403,
            Self::NoRoute { .. } => 404,
        }
    }

    /// Anthropic's error type; on OpenAI's APIs the `code`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request_error",
            Self::Model(_) | Self::TokenLimit(_) => "permission_error",
            Self::NoRoute { .. } => "not_found_error",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(e) => f.write_str(e.as_str()),
            Self::Model(model) => write!(f, "this gateway token may not use the model {model:?}"),
            Self::TokenLimit(e) => e.fmt(f),
            // Names the sections that would serve it.
            Self::NoRoute { api, path } => {
                write!(f, "no route serves {path}: add {}", api.route_sections())
            }
        }
    }
}

/// An output-token maximum above the request's ceiling, or missing where
/// the ceiling requires it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenLimitError {
    /// The field's name: `max_tokens`, `max_output_tokens`, or
    /// `max_completion_tokens`.
    pub field: &'static str,
    /// `None`: the request did not set the field.
    pub requested: Option<u64>,
    pub limit: u64,
}

impl TokenLimitError {
    /// The refusal for `requested` under `limit` on `api`, if any.
    pub fn check(api: Api, limit: u64, requested: Option<u64>) -> Option<Self> {
        let refused = match requested {
            Some(requested) => requested > limit,
            None => api.token_field_required(),
        };
        refused.then_some(Self {
            field: api.token_field(),
            requested,
            limit,
        })
    }
}

impl fmt::Display for TokenLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.requested {
            Some(requested) => write!(
                f,
                "{} {requested} exceeds this gateway token's limit of {}",
                self.field, self.limit
            ),
            None => write!(
                f,
                "{} is required: this gateway token's limit is {}",
                self.field, self.limit
            ),
        }
    }
}

impl std::error::Error for TokenLimitError {}

#[cfg(test)]
mod tests;
