//! Per-request policy: which models a request may name, its output-token
//! ceiling, and reading `model` and the output-token field from a streamed
//! request body.
//!
//! [`PolicyScanner`] follows the body's structure (strings, escapes, nesting)
//! with constant memory and chunk boundaries anywhere. It reports the
//! top-level `model` string as soon as it has been read, so the caller holds
//! only the bytes up to it, and keeps checking the rest: JSON allows a
//! repeated key, and a provider may honor a later `"model"` than the one that
//! was checked. A second top-level `model` key (compared ignoring ASCII case),
//! or a top-level key written with escapes or non-ASCII bytes (which could
//! spell `model` to some decoder), is an error the caller turns into an
//! aborted request.

mod scanner;

use std::fmt;

pub use scanner::PolicyScanner;

/// Longest accepted model name.
pub const MAX_MODEL_BYTES: usize = 256;

/// A name a client can send and a permission can match exactly: what the
/// policy scanner accepts as `model` (no escapes, so no `"` or `\`). Model
/// names in config (aliases, discovery ids, upgrade targets) must be one.
pub(crate) fn is_model_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_MODEL_BYTES
        && !name.contains(['*', '"', '\\'])
        && !name.chars().any(char::is_control)
}

/// An allowed model: an exact name, or a prefix written with a trailing `*`
/// (`claude-haiku-*` matches every dated Haiku model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPattern {
    /// The name, or the prefix without its `*`.
    text: Box<str>,
    prefix: bool,
}

impl ModelPattern {
    /// `None` for an empty pattern, a `*` anywhere but at the end, or one
    /// longer than [`MAX_MODEL_BYTES`].
    pub fn parse(pattern: &str) -> Option<Self> {
        if pattern.is_empty() || pattern.len() > MAX_MODEL_BYTES {
            return None;
        }
        let (text, prefix) = match pattern.strip_suffix('*') {
            Some(text) => (text, true),
            None => (pattern, false),
        };
        (!text.contains('*')).then(|| Self {
            text: text.into(),
            prefix,
        })
    }

    /// Whether every model `other` matches is matched by this pattern.
    pub fn covers(&self, other: &Self) -> bool {
        match (self.prefix, other.prefix) {
            (true, _) => other.text.starts_with(&*self.text),
            (false, false) => self.text == other.text,
            (false, true) => false,
        }
    }

    /// Whether `model` is this name, or starts with this prefix.
    pub fn matches(&self, model: &str) -> bool {
        if self.prefix {
            model.starts_with(&*self.text)
        } else {
            model == &*self.text
        }
    }
}

impl fmt::Display for ModelPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)?;
        if self.prefix {
            f.write_str("*")?;
        }
        Ok(())
    }
}

/// The output-token ceiling of a request: the stricter of the configured
/// and the token's limit, if either is set.
pub(crate) fn output_ceiling(configured: Option<u64>, token: Option<u64>) -> Option<u64> {
    configured.into_iter().chain(token).min()
}

/// Whether `model` is allowed by both lists; an absent list allows every
/// model. Model permissions are the configured list narrowed by the token's:
/// a token's list can only narrow it, so the stricter one always applies.
pub fn permitted(
    configured: Option<&[ModelPattern]>,
    token: Option<&[ModelPattern]>,
    model: &str,
) -> bool {
    let allows = |list: Option<&[ModelPattern]>| {
        list.is_none_or(|patterns| patterns.iter().any(|p| p.matches(model)))
    };
    allows(configured) && allows(token)
}

/// Why a request body's `model` cannot be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyError {
    /// The body ended without a top-level `model`.
    Missing,
    /// `model` is not a plain string of at most [`MAX_MODEL_BYTES`] bytes
    /// without escapes.
    Invalid,
    /// A second top-level `model` key.
    Repeated,
    /// A top-level key with escapes or non-ASCII bytes, which could spell
    /// `model` once decoded or case-folded.
    EscapedKey,
    /// The body is not a JSON object, or its brackets do not match.
    Malformed,
    /// The output-token field is not a non-negative integer.
    InvalidOutputTokens,
    /// A second top-level output-token field.
    RepeatedOutputTokens,
}

impl PolicyError {
    /// A client-facing description.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "request has no model",
            Self::Invalid => "model must be a short string without escapes",
            Self::Repeated => "request names model more than once",
            Self::EscapedKey => "top-level keys must be ASCII without escapes",
            Self::Malformed => "request body is not a JSON object",
            Self::InvalidOutputTokens => {
                "the output-token field (max_tokens, max_output_tokens, or max_completion_tokens) must be a non-negative integer"
            }
            Self::RepeatedOutputTokens => "request names its output-token field more than once",
        }
    }
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for PolicyError {}

#[cfg(test)]
mod tests;
