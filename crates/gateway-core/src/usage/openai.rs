//! The usage-bearing shapes as OpenAI writes them, for the Responses and
//! Chat Completions scanners, and the tool calls a Responses item stands for.

use serde::Deserialize;

use super::Usage;

/// `usage`, as OpenAI's Responses API reports it.
#[derive(Deserialize)]
pub(super) struct OpenAiUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    input_tokens_details: Option<InputDetails>,
}

#[derive(Deserialize)]
struct InputDetails {
    cached_tokens: Option<u64>,
}

impl OpenAiUsage {
    pub(super) fn merge_into(self, usage: &mut Usage) {
        merge_openai(
            usage,
            self.input_tokens,
            self.output_tokens,
            self.input_tokens_details,
        );
    }
}

/// Into Wye's meaning: OpenAI's input count includes cached tokens, ours
/// does not, so the cached share becomes `cache_read_input_tokens`.
fn merge_openai(
    usage: &mut Usage,
    input: Option<u64>,
    output: Option<u64>,
    details: Option<InputDetails>,
) {
    let cached = details.and_then(|d| d.cached_tokens).unwrap_or(0);
    usage.input_tokens = input.map(|i| i.saturating_sub(cached));
    usage.cache_read_input_tokens = input.map(|_| cached);
    usage.output_tokens = output;
}

/// `usage`, as Chat Completions reports it.
#[derive(Deserialize)]
pub(super) struct ChatUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    prompt_tokens_details: Option<InputDetails>,
}

impl ChatUsage {
    /// Reasoning tokens are part of `completion_tokens`, as billed.
    pub(super) fn merge_into(self, usage: &mut Usage) {
        merge_openai(
            usage,
            self.prompt_tokens,
            self.completion_tokens,
            self.prompt_tokens_details,
        );
    }
}

/// The start of a Responses output item (`response.output_item.added`): an
/// item starts without its content.
#[derive(Deserialize)]
pub(super) struct OutputItem<'a> {
    #[serde(rename = "type")]
    pub(super) kind: &'a str,
    pub(super) name: Option<&'a str>,
}

/// Hosted tools OpenAI bills per call, outside token prices.
const HOSTED_TOOLS: [&str; 4] = [
    "web_search",
    "file_search",
    "code_interpreter",
    "image_generation",
];

/// The tool call a Responses output item of type `kind` stands for: the
/// name counted, and whether it is a hosted tool billed per call. `None`
/// for messages, reasoning, and anything else that is not a call.
pub(super) fn openai_tool<'a>(kind: &'a str, name: Option<&'a str>) -> Option<(&'a str, bool)> {
    match kind {
        "function_call" | "custom_tool_call" => Some((name.unwrap_or_default(), false)),
        kind => {
            let tool = kind.strip_suffix("_call")?;
            Some((tool, HOSTED_TOOLS.contains(&tool)))
        }
    }
}
