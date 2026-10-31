//! The usage-bearing events as Anthropic writes them, deserialized with
//! borrowed strings: no allocation unless a value has escapes.

use std::{borrow::Cow, fmt};

use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
};

use super::Usage;

#[derive(Deserialize)]
pub(super) struct StartData<'a> {
    #[serde(borrow)]
    pub(super) message: StartMessage<'a>,
}

#[derive(Deserialize)]
pub(super) struct StartMessage<'a> {
    pub(super) model: Option<String>,
    #[serde(borrow)]
    pub(super) usage: Option<RawUsage<'a>>,
}

#[derive(Deserialize)]
pub(super) struct BlockStartData<'a> {
    #[serde(borrow)]
    pub(super) content_block: ContentBlock<'a>,
}

#[derive(Deserialize)]
pub(super) struct ContentBlock<'a> {
    #[serde(rename = "type")]
    pub(super) kind: &'a str,
    pub(super) name: Option<&'a str>,
}

#[derive(Deserialize)]
pub(super) struct DeltaData<'a> {
    #[serde(borrow)]
    pub(super) usage: Option<RawUsage<'a>>,
}

#[derive(Deserialize)]
pub(super) struct RawUsage<'a> {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation: Option<RawCacheCreation>,
    server_tool_use: Option<AnyCount>,
    #[serde(borrow)]
    service_tier: Option<Cow<'a, str>>,
    #[serde(borrow)]
    speed: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct RawCacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

/// Whether any value in an object is a positive count; other values are
/// skipped, so a new field cannot fail the usage read. Allocates nothing.
struct AnyCount(bool);

impl<'de> Deserialize<'de> for AnyCount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(AnyCountVisitor)
    }
}

struct AnyCountVisitor;

impl<'de> Visitor<'de> for AnyCountVisitor {
    type Value = AnyCount;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a count or an object of counts")
    }

    fn visit_u64<E>(self, n: u64) -> Result<AnyCount, E> {
        Ok(AnyCount(n > 0))
    }

    fn visit_i64<E>(self, n: i64) -> Result<AnyCount, E> {
        Ok(AnyCount(n > 0))
    }

    fn visit_f64<E>(self, _: f64) -> Result<AnyCount, E> {
        Ok(AnyCount(false))
    }

    fn visit_str<E>(self, _: &str) -> Result<AnyCount, E> {
        Ok(AnyCount(false))
    }

    fn visit_bool<E>(self, _: bool) -> Result<AnyCount, E> {
        Ok(AnyCount(false))
    }

    fn visit_unit<E>(self) -> Result<AnyCount, E> {
        Ok(AnyCount(false))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<AnyCount, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(AnyCount(false))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<AnyCount, A::Error> {
        let mut any = false;
        while let Some((_, AnyCount(n))) = map.next_entry::<IgnoredAny, AnyCount>()? {
            any |= n;
        }
        Ok(AnyCount(any))
    }
}

impl RawUsage<'_> {
    /// Later events replace earlier counts; `message_delta` output tokens are cumulative.
    pub(super) fn merge_into(self, usage: &mut Usage) {
        let take = |field: &mut Option<u64>, value: Option<u64>| *field = value.or(*field);
        take(&mut usage.input_tokens, self.input_tokens);
        take(&mut usage.output_tokens, self.output_tokens);
        take(
            &mut usage.cache_creation_input_tokens,
            self.cache_creation_input_tokens,
        );
        take(
            &mut usage.cache_read_input_tokens,
            self.cache_read_input_tokens,
        );
        if let Some(split) = self.cache_creation {
            take(
                &mut usage.cache_creation_5m_tokens,
                split.ephemeral_5m_input_tokens,
            );
            take(
                &mut usage.cache_creation_1h_tokens,
                split.ephemeral_1h_input_tokens,
            );
        }
        let non_standard = |v: &Option<Cow<'_, str>>| v.as_deref().is_some_and(|v| v != "standard");
        usage.unpriced |= self.server_tool_use.is_some_and(|tools| tools.0)
            || non_standard(&self.service_tier)
            || non_standard(&self.speed);
    }
}
