//! Upgrade checks: which fields of a request a target model would reject.
//!
//! Moving an alias or a client to a newer model fails in production when the
//! newer model refuses what the older one accepted. [`UpgradeScanner`] reads
//! the few request fields the rules name, with constant memory, and
//! [`issues`] lists the rules a target model breaks. Report only: the
//! request is never changed.
//!
//! Each rule cites the provider's migration notes; one test per rule.

mod scanner;

pub use scanner::UpgradeScanner;

/// A section of Anthropic's model migration guide (read 2026-10-26).
macro_rules! guide {
    ($anchor:literal) => {
        concat!(
            "https://platform.claude.com/docs/en/models/opus-5-5/migration-guide",
            $anchor
        )
    };
}

/// What a request uses that some models reject.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Findings {
    /// `thinking.type`.
    pub thinking: Option<Thinking>,
    /// `tool_choice.type` is `any` or `tool`.
    pub forced_tool_choice: bool,
    /// A `tools[].type` of an earlier computer-use tool.
    pub computer_tool: bool,
    /// The last `messages[]` entry is an assistant turn (prefill).
    pub prefill: bool,
    /// A `messages[]` entry with role `system`.
    pub system_role: bool,
    /// `temperature`, `top_p`, or `top_k` is set.
    pub sampling: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    Disabled,
    /// Manual extended thinking with `budget_tokens`.
    Enabled,
    Other,
}

/// A request change a newer model requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    ThinkingDisabled,
    BudgetTokens,
    ForcedToolChoice,
    ComputerTool,
    Prefill,
    Sampling,
    SystemRole,
}

impl Rule {
    pub const ALL: [Rule; 7] = [
        Rule::ThinkingDisabled,
        Rule::BudgetTokens,
        Rule::ForcedToolChoice,
        Rule::ComputerTool,
        Rule::Prefill,
        Rule::Sampling,
        Rule::SystemRole,
    ];

    /// The name in usage records and telemetry.
    pub fn name(self) -> &'static str {
        match self {
            Self::ThinkingDisabled => "thinking_disabled",
            Self::BudgetTokens => "budget_tokens",
            Self::ForcedToolChoice => "forced_tool_choice",
            Self::ComputerTool => "computer_tool",
            Self::Prefill => "prefill",
            Self::Sampling => "sampling",
            Self::SystemRole => "system_role",
        }
    }

    /// What the client changes, for `wye explain`.
    pub fn fix(self) -> &'static str {
        match self {
            Self::ThinkingDisabled => "thinking cannot be disabled; omit thinking or use effort",
            Self::BudgetTokens => {
                "manual thinking budgets are rejected; use adaptive thinking and effort"
            }
            Self::ForcedToolChoice => {
                "tool_choice any or tool is rejected; use auto with strict tools"
            }
            Self::ComputerTool => {
                "earlier computer-use tool types are rejected; use the current toolset"
            }
            Self::Prefill => {
                "a prefilled assistant turn is rejected; end messages with a user turn"
            }
            Self::Sampling => "temperature, top_p, and top_k are rejected; omit them",
            Self::SystemRole => "role system in messages is rejected; use the system field",
        }
    }

    /// Where the provider says so.
    pub fn source(self) -> &'static str {
        match self {
            // Fable 5 and 5.1 and Sonnet 5.5 also reject it, per Anthropic's
            // API reference ("API drift": `budget_tokens`).
            Self::BudgetTokens => guide!("#migrating-from-claude-opus-46"),
            Self::Prefill => guide!("#migrating-from-claude-opus-45"),
            Self::SystemRole => guide!("#migrating-from-claude-opus-47"),
            Self::ComputerTool => guide!("#computer-use-toolset"),
            Self::Sampling => guide!("#migrating-from-claude-sonnet-5"),
            Self::ThinkingDisabled => guide!("#thinking-cant-be-disabled"),
            Self::ForcedToolChoice => guide!("#forced-tool-use-is-not-supported"),
        }
    }

    /// Model families that reject the request (dated ids included).
    fn rejected_by(self) -> &'static [&'static str] {
        match self {
            Self::ThinkingDisabled | Self::ForcedToolChoice | Self::ComputerTool => {
                &["claude-opus-5-5"]
            }
            Self::BudgetTokens => &[
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-opus-5",
                "claude-opus-5-5",
                "claude-sonnet-5",
                "claude-sonnet-5-5",
                "claude-fable-5",
                "claude-fable-5-1",
            ],
            Self::Prefill => &[
                "claude-opus-4-6",
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-opus-5",
                "claude-opus-5-5",
                "claude-sonnet-5",
            ],
            Self::Sampling => &["claude-sonnet-5", "claude-opus-5-5"],
            Self::SystemRole => &["claude-opus-4-7"],
        }
    }

    fn applies(self, f: &Findings) -> bool {
        match self {
            Self::ThinkingDisabled => f.thinking == Some(Thinking::Disabled),
            Self::BudgetTokens => f.thinking == Some(Thinking::Enabled),
            Self::ForcedToolChoice => f.forced_tool_choice,
            Self::ComputerTool => f.computer_tool,
            Self::Prefill => f.prefill,
            Self::Sampling => f.sampling,
            Self::SystemRole => f.system_role,
        }
    }
}

/// Whether `model` is `family` or a dated id of it (`family-20261001`), so
/// `claude-opus-5` does not match `claude-opus-5-5`.
fn in_family(model: &str, family: &str) -> bool {
    match model.strip_prefix(family) {
        Some("") => true,
        Some(rest) => rest
            .strip_prefix('-')
            .is_some_and(|date| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit())),
        None => false,
    }
}

/// The rules `target` breaks for a request with `findings`.
pub fn issues(findings: &Findings, target: &str) -> impl Iterator<Item = Rule> {
    Rule::ALL.into_iter().filter(move |rule| {
        rule.applies(findings) && rule.rejected_by().iter().any(|f| in_family(target, f))
    })
}

#[cfg(test)]
mod tests;
