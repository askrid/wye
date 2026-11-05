//! Body admission: what config and a gateway token decide about one request
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
    policy::{self, ModelPattern, PolicyError, PolicyScanner},
    token::Identity,
};

/// The scanner for a request body under `output_ceiling`: the field is read only
/// when a ceiling applies, so a value only a ceiling would mind is not refused.
pub fn scanner(api: Api, output_ceiling: Option<u64>) -> PolicyScanner {
    let scanner = PolicyScanner::for_api(api);
    if output_ceiling.is_some() {
        scanner
    } else {
        scanner.without_token_field()
    }
}

/// The checks that apply to one request's body.
#[derive(Debug, Clone, Copy)]
pub struct BodyChecks<'a> {
    /// `[auth] models`: models any gateway token may use.
    pub configured: Option<&'a [ModelPattern]>,
    /// The token's own list, which can only narrow `configured`.
    pub granted: Option<&'a [ModelPattern]>,
    /// The output-token ceiling, the stricter of config's and the
    /// token's; `None` for an endpoint that generates nothing.
    pub output_ceiling: Option<u64>,
    pub aliases: Option<&'a Aliases>,
    /// An upstream of the route has its own model ids (Bedrock), so the
    /// model's place in the body is needed.
    pub prefixes_model_ids: bool,
}

impl<'a> BodyChecks<'a> {
    /// The checks config's `[auth]` and `aliases`, and `identity`, set for
    /// one request. `generates`: the endpoint generates output (not
    /// `count_tokens`), so the output-token ceiling applies.
    /// `prefixes_model_ids`: an upstream of the route has its own model ids.
    pub fn new(
        configured: Option<&'a [ModelPattern]>,
        configured_max_output_tokens: Option<u64>,
        aliases: Option<&'a Aliases>,
        identity: Option<&'a Identity>,
        generates: bool,
        prefixes_model_ids: bool,
    ) -> Self {
        let output_ceiling = generates
            .then(|| {
                policy::output_ceiling(
                    configured_max_output_tokens,
                    identity.and_then(|i| i.max_output_tokens),
                )
            })
            .flatten();
        Self {
            configured,
            granted: identity.and_then(|i| i.models.as_deref()),
            output_ceiling,
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
        self.configured.is_some() || self.granted.is_some() || self.output_ceiling.is_some()
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

    /// Whether the token may use `model`, the model that runs (an
    /// alias already resolved). The refusal names the model as the client
    /// sent it (`sent`): what an alias maps to is config's business.
    pub fn permit(&self, model: &str, sent: &str) -> Result<(), Refusal> {
        if policy::permitted(self.configured, self.granted, model) {
            Ok(())
        } else {
            Err(Refusal::Model(sent.into()))
        }
    }

    /// The output-token check ([`OutputCeiling::check`]).
    pub fn check_output_ceiling(
        &self,
        api: Api,
        requested: Option<u64>,
        body_read: bool,
    ) -> Result<(), Refusal> {
        match self.output_ceiling {
            Some(limit) => OutputCeiling { api, limit }
                .check(requested, body_read)
                .map_or(Ok(()), |e| Err(Refusal::OutputCeiling(e))),
            None => Ok(()),
        }
    }
}

/// One request's output-token ceiling, on its API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCeiling {
    pub api: Api,
    pub limit: u64,
}

impl OutputCeiling {
    /// The check as a body is read: a value is final once read, a missing
    /// field only once `body_read` (the whole body was read without one).
    pub fn check(self, requested: Option<u64>, body_read: bool) -> Option<OutputCeilingError> {
        let refused = match requested {
            Some(requested) => requested > self.limit,
            None => body_read && self.api.output_token_field_required(),
        };
        refused.then_some(OutputCeilingError {
            field: self.api.output_token_field(),
            requested,
            limit: self.limit,
        })
    }
}

/// A policy refusal, wherever it is decided: the status, error type, and
/// message are the same from `serve` and from `wye explain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The body's `model` or a top-level key cannot be checked, or the
    /// output-token field is malformed.
    Invalid(PolicyError),
    /// The token may not use this model, as the client named it.
    Model(Box<str>),
    /// The output-token maximum is above the ceiling, or missing where the
    /// ceiling requires it.
    OutputCeiling(OutputCeilingError),
    /// A known endpoint whose API no route serves.
    NoRoute { api: Api, path: Box<str> },
    /// A path or method the gateway does not proxy.
    NotProxied,
    /// A missing or invalid gateway token.
    Unauthenticated,
    /// A body past `server.max_request_body_bytes`, declared or sent.
    TooLarge,
}

impl Refusal {
    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::Unauthenticated => 401,
            Self::Model(_) | Self::OutputCeiling(_) => 403,
            Self::NoRoute { .. } | Self::NotProxied => 404,
            Self::TooLarge => 413,
        }
    }

    /// The message of a refusal that never varies, so a response needs no
    /// formatting. A token failure has one message for every reason: the
    /// reason is not the caller's business.
    pub fn fixed_message(&self) -> Option<&'static str> {
        match self {
            Self::NotProxied => Some("unsupported endpoint"),
            Self::Unauthenticated => Some("invalid or missing gateway token"),
            Self::TooLarge => Some("request body exceeds the gateway limit"),
            Self::Invalid(_) | Self::Model(_) | Self::OutputCeiling(_) | Self::NoRoute { .. } => {
                None
            }
        }
    }

    /// Anthropic's error type; on OpenAI's APIs the `code`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request_error",
            Self::Unauthenticated => "authentication_error",
            Self::Model(_) | Self::OutputCeiling(_) => "permission_error",
            Self::NoRoute { .. } | Self::NotProxied => "not_found_error",
            Self::TooLarge => "request_too_large",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(e) => f.write_str(e.as_str()),
            Self::Model(model) => write!(f, "this gateway token may not use the model {model:?}"),
            Self::OutputCeiling(e) => e.fmt(f),
            Self::NoRoute { api, path } => {
                write!(f, "no route serves {path}: add {}", api.route().section())
            }
            Self::NotProxied | Self::Unauthenticated | Self::TooLarge => {
                f.write_str(self.fixed_message().unwrap_or_default())
            }
        }
    }
}

/// An output-token maximum above the request's ceiling, or missing where
/// the ceiling requires it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCeilingError {
    /// The output-token field's name.
    pub field: &'static str,
    /// `None`: the request did not set the field.
    pub requested: Option<u64>,
    pub limit: u64,
}

impl fmt::Display for OutputCeilingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.requested {
            Some(requested) => write!(
                f,
                "{} {requested} exceeds the output-token ceiling of {}",
                self.field, self.limit
            ),
            None => write!(
                f,
                "{} is required: the output-token ceiling is {}",
                self.field, self.limit
            ),
        }
    }
}

impl std::error::Error for OutputCeilingError {}

#[cfg(test)]
mod tests;
