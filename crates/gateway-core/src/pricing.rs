//! Request cost from the token usage a provider reports and the prices in
//! `[pricing]`.
//!
//! Money is never floating point: a price is parsed once into integer
//! pico-USD per token (a price per million tokens with at most six decimals
//! is an exact integer), and a cost is a checked `u64` sum that fails closed.

use std::fmt;

use crate::{policy::ModelPattern, usage::Usage};

/// Pico-USD (10^-12 USD) per token, or a cost in pico-USD.
pub type Pico = u64;

/// Most `[[pricing.models]]` entries.
pub const MAX_PRICED_MODELS: usize = 256;
/// Most tiers per entry.
pub const MAX_TIERS: usize = 8;

/// Prices for one set of models, per token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prices {
    pub input: Pico,
    pub output: Pico,
    pub cache_read: Option<Pico>,
    pub cache_write_5m: Option<Pico>,
    pub cache_write_1h: Option<Pico>,
}

/// Prices that change above a prompt length; unset fields inherit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    /// Applies when the prompt (input plus cache reads and writes) is longer.
    pub above_prompt_tokens: u64,
    pub input: Option<Pico>,
    pub output: Option<Pico>,
    pub cache_read: Option<Pico>,
    pub cache_write_5m: Option<Pico>,
    pub cache_write_1h: Option<Pico>,
}

/// One `[[pricing.models]]` entry.
#[derive(Debug, Clone)]
pub struct PricedModels {
    pub models: Box<[ModelPattern]>,
    /// Route upstream ids the entry is limited to; empty: every upstream.
    /// Providers and regions can price the same model differently.
    pub upstreams: Box<[Box<str>]>,
    pub prices: Prices,
    /// Sorted by threshold, ascending.
    pub tiers: Box<[Tier]>,
}

/// `[pricing]`: a versioned price table.
#[derive(Debug, Clone)]
pub struct Pricing {
    pub version: Box<str>,
    pub entries: Box<[PricedModels]>,
}

/// How much of a request's cost the table covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostStatus {
    /// Every billed token was priced.
    Complete,
    /// A lower bound: tokens without a price, unreported cache TTLs, per-use
    /// tools or service tiers outside the table, or usage that ended early.
    Partial,
    /// No price for the model, no usage, or an overflow.
    Unknown,
}

impl CostStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
        }
    }
}

/// A request's cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    /// `None` when unknown.
    pub pico: Option<Pico>,
    pub status: CostStatus,
}

impl Cost {
    const UNKNOWN: Self = Self {
        pico: None,
        status: CostStatus::Unknown,
    };
}

impl Pricing {
    /// The entry that prices `model` served by `upstream` (a route
    /// upstream's id): one scoped to the upstream before any unscoped one;
    /// within each, the first match.
    pub fn entry(&self, model: &str, upstream: Option<&str>) -> Option<&PricedModels> {
        let for_model = |e: &&PricedModels| e.models.iter().any(|p| p.matches(model));
        let scoped = self
            .entries
            .iter()
            .filter(for_model)
            .find(|e| upstream.is_some_and(|u| e.upstreams.iter().any(|id| &**id == u)));
        scoped.or_else(|| {
            self.entries
                .iter()
                .filter(for_model)
                .find(|e| e.upstreams.is_empty())
        })
    }

    /// The cost of a response from `model`, served by `upstream`, that
    /// reported `usage`, priced by [`Self::entry`]; `usage_complete` is false
    /// when the stream ended before its final usage.
    pub fn cost(
        &self,
        model: &str,
        upstream: Option<&str>,
        usage: &Usage,
        usage_complete: bool,
    ) -> Cost {
        let Some(entry) = self.entry(model, upstream) else {
            return Cost::UNKNOWN;
        };
        let (Some(input), Some(output)) = (usage.input_tokens, usage.output_tokens) else {
            return Cost::UNKNOWN;
        };
        let read = usage.cache_read_input_tokens.unwrap_or(0);
        let written = usage.cache_creation_input_tokens.unwrap_or(0);
        let prompt = input.saturating_add(read).saturating_add(written);
        let prices = entry.prices_for(prompt);

        let split = (
            usage.cache_creation_5m_tokens,
            usage.cache_creation_1h_tokens,
        );
        // Only the total of cache writes known: priced at the higher TTL below,
        // so the cost does not undercount, and not exact.
        let ttl_unknown = split == (None, None) && written > 0;
        let mut complete = usage_complete && !usage.unpriced && !ttl_unknown;
        let mut total: Option<Pico> = Some(0);
        let mut add = |tokens: u64, price: Option<Pico>| {
            if tokens == 0 {
                return;
            }
            match price {
                Some(price) => {
                    total = total
                        .and_then(|t| tokens.checked_mul(price).and_then(|c| t.checked_add(c)));
                }
                None => complete = false,
            }
        };
        add(input, Some(prices.input));
        add(output, Some(prices.output));
        add(read, prices.cache_read);
        match split {
            (None, None) => add(written, prices.cache_write_1h.or(prices.cache_write_5m)),
            (five, one) => {
                add(five.unwrap_or(0), prices.cache_write_5m);
                add(one.unwrap_or(0), prices.cache_write_1h);
            }
        }
        match total {
            None => Cost::UNKNOWN,
            Some(pico) => Cost {
                pico: Some(pico),
                status: if complete {
                    CostStatus::Complete
                } else {
                    CostStatus::Partial
                },
            },
        }
    }
}

impl PricedModels {
    /// The base prices with the highest tier the prompt reaches applied.
    fn prices_for(&self, prompt: u64) -> Prices {
        let mut p = self.prices;
        if let Some(tier) = self
            .tiers
            .iter()
            .rev()
            .find(|t| prompt > t.above_prompt_tokens)
        {
            p.input = tier.input.unwrap_or(p.input);
            p.output = tier.output.unwrap_or(p.output);
            p.cache_read = tier.cache_read.or(p.cache_read);
            p.cache_write_5m = tier.cache_write_5m.or(p.cache_write_5m);
            p.cache_write_1h = tier.cache_write_1h.or(p.cache_write_1h);
        }
        p
    }
}

/// Why a price string was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceError {
    /// Not a non-negative decimal with at most six decimals.
    Format,
    /// Too large to represent.
    Overflow,
}

/// Parses USD per million tokens (`"4.00"`) into pico-USD per token: the
/// value times 10^6, exact for at most six decimals.
pub fn parse_price(text: &str) -> Result<Pico, PriceError> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || (text.contains('.') && !digits(fraction)) || fraction.len() > 6 {
        return Err(PriceError::Format);
    }
    let whole: u64 = whole.parse().map_err(|_| PriceError::Overflow)?;
    // At most six digits, padded to six: below 10^6, no overflow.
    let scaled = fraction
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(6)
        .fold(0u64, |acc, b| acc * 10 + u64::from(b - b'0'));
    whole
        .checked_mul(1_000_000)
        .and_then(|w| w.checked_add(scaled))
        .ok_or(PriceError::Overflow)
}

/// Parses a cost written by [`Usd`] (a decimal with at most 12 decimals)
/// back into pico-USD; `None` for anything else or an overflow.
pub fn parse_usd(text: &str) -> Option<Pico> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || (text.contains('.') && !digits(fraction)) || fraction.len() > 12 {
        return None;
    }
    let fraction = fraction
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(12)
        .fold(0u64, |acc, b| acc * 10 + u64::from(b - b'0'));
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(1_000_000_000_000)?
        .checked_add(fraction)
}

/// A cost in USD as a decimal string, exact: pico-USD with trailing zeros
/// trimmed (`1_234_500_000` → `"0.0012345"`).
pub struct Usd(pub Pico);

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const PER_USD: u64 = 1_000_000_000_000;
        let (whole, mut fraction) = (self.0 / PER_USD, self.0 % PER_USD);
        write!(f, "{whole}")?;
        if fraction > 0 {
            // Trim trailing zeros arithmetically: no intermediate string.
            let mut width = 12;
            while fraction % 10 == 0 {
                fraction /= 10;
                width -= 1;
            }
            write!(f, ".{fraction:0width$}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
