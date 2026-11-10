//! Token usage from a non-streaming JSON response: the top-level `model`,
//! `usage`, and tool calls of a Messages, Responses, or Chat Completions
//! body (Chat's tool calls sit in `choices` and are not counted), read as the
//! body streams with constant memory. A long completion makes the body
//! large; only the small `usage` object is kept.

use super::{
    Scanned, Usage, UsageStatus,
    anthropic::RawUsage,
    fields::{Items, ResponseFields},
    openai::{ChatUsage, OpenAiUsage},
};
use crate::api::Api;

/// Incremental usage scanner for one JSON response body.
#[derive(Debug)]
pub struct JsonUsageScanner {
    api: Api,
    fields: ResponseFields,
}

impl JsonUsageScanner {
    /// For a JSON response body of `api`.
    pub fn new(api: Api) -> Self {
        let items = match api {
            Api::Messages => Items::Content,
            Api::Responses => Items::Output,
            Api::ChatCompletions => Items::None,
        };
        Self {
            api,
            fields: ResponseFields::new(None, items),
        }
    }

    /// Reads the next chunk of the body, split anywhere.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.fields.feed(chunk);
    }

    /// The usage the body reported; `None` when absent, malformed, or cut
    /// off by a body that ended early.
    fn read(&self) -> Option<Usage> {
        if self.fields.failed() {
            return None;
        }
        let bytes = self.fields.usage_bytes().ok()??;
        let mut usage = Usage::default();
        match self.api {
            Api::Messages => serde_json::from_slice::<RawUsage<'_>>(bytes)
                .ok()?
                .merge_into(&mut usage),
            Api::Responses => serde_json::from_slice::<OpenAiUsage>(bytes)
                .ok()?
                .merge_into(&mut usage),
            Api::ChatCompletions => serde_json::from_slice::<ChatUsage>(bytes)
                .ok()?
                .merge_into(&mut usage),
        }
        if self.api != Api::Messages {
            usage.unpriced |= super::unpriced_tier(self.fields.service_tier());
        }
        usage.unpriced |= self.fields.hosted_tool;
        Some(usage)
    }

    /// The body's usage; zero counts when it has none.
    pub fn usage(&self) -> Usage {
        self.read().unwrap_or_default()
    }

    /// The top-level `model`.
    pub fn model(&self) -> Option<&str> {
        self.fields.model()
    }

    /// Complete when the body's `usage` was read: a JSON response reports
    /// its final usage once.
    pub fn status(&self) -> UsageStatus {
        match self.read() {
            Some(_) => UsageStatus::Complete,
            None => UsageStatus::Unknown,
        }
    }

    /// What the scan saw, once the body has ended. Reads the usage once.
    pub fn finish(self) -> Scanned {
        let usage = self.read();
        Scanned {
            status: match usage {
                Some(_) => UsageStatus::Complete,
                None => UsageStatus::Unknown,
            },
            usage: usage.unwrap_or_default(),
            model: self.fields.model().map(Box::from),
            tool_calls: self.fields.tools,
        }
    }
}

#[cfg(test)]
mod tests;
