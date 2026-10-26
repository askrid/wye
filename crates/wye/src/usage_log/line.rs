//! One usage record as borrowed fields: the JSON line and the input to OTLP.

use std::time::Duration;

use gateway_core::{
    agent::CacheMiss,
    pricing::{CostStatus, Pico, Pricing, Usd},
    prompt::HashKey,
    usage::ToolCalls,
};
use serde::{Serialize, Serializer};

use super::{Record, agent::ToolDurations};
use crate::{diagnostics::PromptRecord, upgrade_check::UpgradeIssues};

/// What the writer adds to every record: keys, prices, upgrade targets.
pub(super) struct Context<'a> {
    pub(super) prompt_key: Option<&'a HashKey>,
    pub(super) pricing: Option<&'a Pricing>,
    pub(super) upgrade_targets: Option<&'a [Box<str>]>,
}

/// One JSON line, serialized from borrowed fields: no per-record allocation
/// beyond growing the writer's reused buffer.
#[derive(Serialize)]
pub(super) struct Line<'a> {
    pub(super) event: &'static str,
    pub(super) unix_ms: u64,
    pub(super) path: &'a str,
    /// `gen_ai.system` in OTLP; the path names the API in the JSON line.
    #[serde(skip)]
    pub(super) system: &'static str,
    pub(super) status: u16,
    pub(super) outcome: &'static str,
    pub(super) duration_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) headers_ms: Option<f64>,
    /// The route upstream, when the route names its upstreams.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) upstream: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) subject: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) session_id: Option<&'a str>,
    /// Only for the OTLP span; not part of the JSON line.
    #[serde(skip)]
    pub(super) traceparent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) request_class: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) agent_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) parent_agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prompt_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) compaction: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) context_compacted: Option<&'static str>,
    /// Tool calls that ran since the agent's previous request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_durations: Option<ToolDurations<'a>>,
    /// The alias the client named, when config resolved one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) requested_model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<&'a str>,
    pub(super) usage_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_read_input_tokens: Option<u64>,
    /// The request's cost in USD, an exact decimal string, when `[pricing]`
    /// priced it.
    #[serde(
        rename = "cost_usd",
        skip_serializing_if = "Option::is_none",
        serialize_with = "usd"
    )]
    pub(super) cost: Option<Pico>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cost_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pricing_version: Option<&'a str>,
    /// Tool calls the response requested, by tool name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_calls: Option<ToolCallsField<'a>>,
    /// Why the request wrote the prompt cache without reading it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_miss: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prompt: Option<PromptRecord>,
    /// For a checked request, the target models that would reject it and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) upgrade_issues: Option<UpgradeIssues<'a>>,
}

/// A cost as an exact decimal string, written without an intermediate String.
fn usd<S: Serializer>(cost: &Option<Pico>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&Usd(cost.unwrap_or(0)))
}

/// The record as borrowed fields, written as a JSON line and as OTLP. With
/// `pricing`, every record carries a cost status: a record without usage
/// (an error, a non-streaming response) is `unknown`, never $0.
pub(super) fn line<'a>(r: &'a Record, context: &Context<'a>) -> Line<'a> {
    let pricing = context.pricing;
    let usage = r
        .usage
        .as_ref()
        .map(|(usage, _)| *usage)
        .unwrap_or_default();
    let cost = pricing.map(|pricing| match (&r.model, &r.usage) {
        (Some(model), Some((usage, status))) if *status != "unknown" => {
            pricing.cost(model, usage, *status == "complete")
        }
        _ => gateway_core::pricing::Cost {
            pico: None,
            status: CostStatus::Unknown,
        },
    });
    let agent = r.info.agent.fields();
    Line {
        event: "request",
        unix_ms: r.unix_ms as u64,
        path: r.info.endpoint.path,
        system: r.info.endpoint.api.system(),
        status: r.status.as_u16(),
        outcome: r.outcome.as_str(),
        duration_ms: ms(r.duration),
        headers_ms: r.headers_after.map(ms),
        upstream: r.info.upstream.as_deref(),
        subject: r.info.identity.as_ref().map(|i| &*i.subject),
        // Client-supplied, like the agent ids: kept only as a bounded token.
        session_id: r
            .info
            .session_id
            .as_ref()
            .and_then(|v| gateway_core::agent::token(v.as_bytes())),
        traceparent: r.info.traceparent.as_ref().and_then(|v| v.to_str().ok()),
        request_class: agent.request_class,
        agent_type: agent.agent_type,
        agent_id: agent.agent_id,
        parent_agent_id: agent.parent_agent_id,
        prompt_id: agent.prompt_id,
        compaction: agent.compaction.map(|c| c.as_str()),
        context_compacted: agent.context_compacted.map(|c| c.as_str()),
        tool_durations: agent.tool_durations,
        requested_model: r.info.requested_model.as_deref(),
        model: r.model.as_deref(),
        usage_status: r
            .usage
            .as_ref()
            .map_or("not_observed", |(_, status)| status),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cost: cost.and_then(|c| c.pico),
        cost_status: cost.map(|c| c.status.as_str()),
        pricing_version: pricing.map(|p| &*p.version),
        tool_calls: r.tool_calls.as_deref().map(ToolCallsField),
        cache_miss: CacheMiss::classify(
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
            agent.compaction,
            agent.context_compacted,
        )
        .map(CacheMiss::as_str),
        prompt: r
            .info
            .prompt
            .as_ref()
            .zip(context.prompt_key)
            .map(|(slot, key)| PromptRecord::new(slot, key)),
        upgrade_issues: r
            .info
            .upgrade
            .as_ref()
            .and_then(|slot| slot.get().copied())
            .zip(context.upgrade_targets)
            .map(|(findings, targets)| UpgradeIssues { findings, targets }),
    }
}

/// Name of the count of tool calls not recorded by name; parentheses are
/// outside the tool-name alphabet, so it never collides with a tool.
const OTHER_TOOLS: &str = "(other)";

/// Tool calls as a JSON object of name to count.
#[derive(Clone, Copy)]
pub(super) struct ToolCallsField<'a>(pub(super) &'a ToolCalls);

impl<'a> ToolCallsField<'a> {
    pub(super) fn iter(self) -> impl Iterator<Item = (&'a str, u32)> {
        let other = (self.0.other() > 0).then(|| (OTHER_TOOLS, self.0.other()));
        self.0.iter().chain(other)
    }
}

impl Serialize for ToolCallsField<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

pub(super) fn push_line(buf: &mut Vec<u8>, line: &Line<'_>) {
    // Serializing plain fields into a Vec cannot fail.
    let _ = serde_json::to_writer(&mut *buf, line);
    buf.push(b'\n');
}

/// Milliseconds rounded to two decimals.
fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1000.0 * 100.0).round() / 100.0
}
