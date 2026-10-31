//! Token usage from a non-streaming JSON response: the top-level `model`,
//! `usage`, and tool calls of a Messages, Responses, or Chat Completions
//! body (Chat's tool calls sit in `choices` and are not counted), read as the
//! body streams with constant memory. A long completion makes the body
//! large; only the small `usage` object is kept.

use super::{
    ToolCalls, Usage, UsageStatus,
    chat::ChatUsage,
    fields::{Items, ResponseFields},
    raw::RawUsage,
};
use crate::api::Api;

/// Incremental usage scanner for one JSON response body.
#[derive(Debug)]
pub struct JsonUsageScanner {
    api: Api,
    fields: ResponseFields,
}

impl JsonUsageScanner {
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

    pub fn feed(&mut self, chunk: &[u8]) {
        self.fields.feed(chunk);
    }

    /// The usage the body reported; `None` when absent, malformed, or cut
    /// off by a body that ended early.
    fn read(&self) -> Option<Usage> {
        if self.fields.failed {
            return None;
        }
        let bytes = self.fields.usage_bytes().ok()??;
        let mut usage = Usage::default();
        match self.api {
            Api::Messages => serde_json::from_slice::<RawUsage<'_>>(bytes)
                .ok()?
                .merge_into(&mut usage),
            Api::Responses => self.fields.openai_usage().ok()??.merge_into(&mut usage),
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

    pub fn usage(&self) -> Usage {
        self.read().unwrap_or_default()
    }

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

    pub fn take_tool_calls(&mut self) -> Option<Box<ToolCalls>> {
        self.fields.tools.take()
    }
}

#[cfg(test)]
mod tests;
