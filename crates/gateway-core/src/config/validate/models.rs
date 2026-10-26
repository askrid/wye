//! The model tables: `[[aliases]]`, `[pricing]`, and `[upgrade_check]`.

use serde::Deserialize;

use super::*;
use crate::{
    alias::{Alias, AliasError},
    pricing::{MAX_PRICED_MODELS, MAX_TIERS, PricedModels, Prices, Pricing, Tier, parse_price},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawUpgradeCheck {
    pub(crate) targets: Vec<String>,
    pub(crate) sample_percent: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPricing {
    pub(crate) version: String,
    pub(crate) models: Vec<RawPricedModels>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPricedModels {
    pub(crate) models: Vec<String>,
    pub(crate) input: String,
    pub(crate) output: String,
    pub(crate) cache_read: Option<String>,
    pub(crate) cache_write_5m: Option<String>,
    pub(crate) cache_write_1h: Option<String>,
    #[serde(default)]
    pub(crate) tiers: Vec<RawTier>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawTier {
    pub(crate) above_prompt_tokens: u64,
    pub(crate) input: Option<String>,
    pub(crate) output: Option<String>,
    pub(crate) cache_read: Option<String>,
    pub(crate) cache_write_5m: Option<String>,
    pub(crate) cache_write_1h: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAlias {
    pub(crate) name: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) when: Vec<RawAliasRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAliasRule {
    pub(crate) subjects: Vec<String>,
    pub(crate) model: String,
}

/// Longest `pricing.version`: it is recorded with every cost.
const MAX_PRICING_VERSION_BYTES: usize = 128;

pub(crate) fn validate_pricing(raw: RawPricing) -> Result<Pricing, ConfigError> {
    if raw.version.is_empty()
        || raw.version.len() > MAX_PRICING_VERSION_BYTES
        || raw.version.chars().any(char::is_control)
    {
        return Err(ConfigError::Invalid(
            "pricing.version must be 1-128 printable characters",
        ));
    }
    if raw.models.is_empty() || raw.models.len() > MAX_PRICED_MODELS {
        return Err(ConfigError::Invalid(
            "pricing.models: 1-256 entries; omit [pricing] to record no cost",
        ));
    }
    let price = |text: &str| {
        parse_price(text).map_err(|_| {
            ConfigError::Invalid(
                "pricing: a price is USD per million tokens as a decimal string with at most six decimals, such as \"4.00\"",
            )
        })
    };
    let optional = |text: Option<String>| text.as_deref().map(price).transpose();
    let mut entries = Vec::with_capacity(raw.models.len());
    for entry in raw.models {
        if entry.models.is_empty() {
            return Err(ConfigError::Invalid(
                "pricing.models: models must not be empty",
            ));
        }
        let models = entry
            .models
            .iter()
            .map(|m| ModelPattern::parse(m))
            .collect::<Option<Box<[_]>>>()
            .ok_or(ConfigError::Invalid(
                "pricing.models: a model is a name or a prefix ending in *",
            ))?;
        if entry.tiers.len() > MAX_TIERS {
            return Err(ConfigError::Invalid(
                "pricing.models.tiers: at most 8 tiers",
            ));
        }
        let mut tiers = entry
            .tiers
            .into_iter()
            .map(|t| {
                Ok(Tier {
                    above_prompt_tokens: t.above_prompt_tokens,
                    input: optional(t.input)?,
                    output: optional(t.output)?,
                    cache_read: optional(t.cache_read)?,
                    cache_write_5m: optional(t.cache_write_5m)?,
                    cache_write_1h: optional(t.cache_write_1h)?,
                })
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;
        tiers.sort_by_key(|t| t.above_prompt_tokens);
        if tiers.windows(2).any(|w| {
            w.first().map(|t| t.above_prompt_tokens) == w.get(1).map(|t| t.above_prompt_tokens)
        }) {
            return Err(ConfigError::Invalid(
                "pricing.models.tiers: duplicate above_prompt_tokens",
            ));
        }
        entries.push(PricedModels {
            models,
            prices: Prices {
                input: price(&entry.input)?,
                output: price(&entry.output)?,
                cache_read: optional(entry.cache_read)?,
                cache_write_5m: optional(entry.cache_write_5m)?,
                cache_write_1h: optional(entry.cache_write_1h)?,
            },
            tiers: tiers.into(),
        });
    }
    Ok(Pricing {
        version: raw.version.into(),
        entries: entries.into(),
    })
}

pub(crate) fn validate_upgrade_check(raw: RawUpgradeCheck) -> Result<UpgradeCheck, ConfigError> {
    if raw.targets.is_empty() || raw.targets.len() > MAX_UPGRADE_TARGETS {
        return Err(ConfigError::Invalid(
            "upgrade_check.targets: 1-16 model names; omit [upgrade_check] to disable",
        ));
    }
    let mut targets: Vec<Box<str>> = Vec::with_capacity(raw.targets.len());
    for target in raw.targets {
        if target.contains('*') || ModelPattern::parse(&target).is_none() {
            return Err(ConfigError::Invalid(
                "upgrade_check.targets: a target is a model name of 1-256 bytes without *",
            ));
        }
        if targets.iter().any(|t| **t == *target) {
            return Err(ConfigError::Invalid(
                "upgrade_check.targets: duplicate model",
            ));
        }
        targets.push(target.into());
    }
    let sample_percent = match raw.sample_percent {
        None => 100,
        Some(p @ 1..=100) => p,
        Some(_) => return Err(ConfigError::Percent("upgrade_check.sample_percent")),
    };
    Ok(UpgradeCheck {
        targets: targets.into(),
        sample_percent,
    })
}

pub(crate) fn validate_aliases(raw: Vec<RawAlias>) -> Result<Option<Aliases>, ConfigError> {
    if raw.is_empty() {
        return Ok(None);
    }
    let aliases = raw
        .into_iter()
        .map(|alias| {
            Alias::new(
                &alias.name,
                &alias.model,
                alias
                    .when
                    .into_iter()
                    .map(|rule| (rule.subjects, rule.model)),
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .and_then(Aliases::new)
        .map_err(|AliasError(reason)| ConfigError::Invalid(reason))?;
    Ok(Some(aliases))
}
