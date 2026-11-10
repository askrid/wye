//! One usage record as borrowed fields: the JSON line and the input to OTLP.

use std::time::Duration;

use gateway_core::{
    agent::{self, CacheMiss},
    pricing::{CostStatus, Pico, Pricing, Usd},
    prompt::HashKey,
    usage::{Scanned, ToolCalls, UsageStatus},
};
use http::HeaderValue;
use serde::{Serialize, Serializer};

use super::{
    Outcome, Record, RejectedAttempt, agent::ToolDurations, prompt_hashes::PromptRecord,
    upgrade_issues::UpgradeIssues,
};

/// The `usage_status` of a response that carries no usage: an error,
/// `count_tokens`, a content type that is neither SSE nor JSON.
const NO_USAGE: &str = "none";

/// The record format's version, every line's `v`: raised by any change that
/// renames a field or changes its meaning, so a reader can tell formats
/// apart. Added fields keep it. 2: `path` became `endpoint`.
pub(super) const RECORD_VERSION: u8 = 2;

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
    /// What the line records: `request` for a usage record.
    pub(super) event: &'static str,
    /// [`RECORD_VERSION`]; the JSON line only, not OTLP.
    pub(super) v: u8,
    pub(super) unix_ms: u64,
    /// The endpoint, such as `/v1/messages`; `http.route` in OTLP.
    pub(super) endpoint: &'a str,
    /// `gen_ai.provider.name` in OTLP; the JSON line has the `upstream`
    /// id, which config maps to its provider.
    #[serde(skip)]
    pub(super) provider_name: &'static str,
    /// The status the client got.
    pub(super) status: u16,
    #[serde(serialize_with = "outcome")]
    pub(super) outcome: Outcome,
    /// Why the upstream gave no usable response, for an `upstream_error`
    /// outcome only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) upstream_error: Option<&'static str>,
    /// From the request's arrival to its record: the response's end, or
    /// Wye's own answer.
    pub(super) duration_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) response_headers_ms: Option<f64>,
    /// The route upstream; absent when refused before one was chosen.
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
    pub(super) after_compaction: Option<&'static str>,
    /// Tool calls that ran since the agent's previous request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_durations: Option<ToolDurations<'a>>,
    /// The alias the client named, when config resolved one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) requested_model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<&'a str>,
    /// How much of the usage the scan saw; `none` for a response that
    /// carries none.
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
    /// How complete `cost_usd` is, when `[pricing]` is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cost_status: Option<&'static str>,
    /// The price table's version, when `[pricing]` is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pricing_version: Option<&'a str>,
    /// Tool calls the response requested, by tool name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_calls: Option<ToolCallsField<'a>>,
    /// Why the request wrote the prompt cache without reading it.
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "cache_miss")]
    pub(super) cache_miss: Option<CacheMiss>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prompt_hashes: Option<PromptRecord>,
    /// For a checked request, the target models that would reject it and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) upgrade_issues: Option<UpgradeIssues<'a>>,
    /// Upstreams that rejected the request before `upstream` answered.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub(super) failovers: &'a [RejectedAttempt],
}

#[cfg(test)]
impl Line<'_> {
    /// A completed `/v1/messages` request with nothing observed, for tests
    /// to set the fields they check.
    pub(super) fn blank() -> Self {
        Line {
            event: "request",
            v: RECORD_VERSION,
            unix_ms: 0,
            endpoint: "/v1/messages",
            provider_name: "anthropic",
            status: 200,
            outcome: Outcome::Completed,
            upstream_error: None,
            duration_ms: 0.0,
            response_headers_ms: None,
            upstream: None,
            subject: None,
            session_id: None,
            traceparent: None,
            request_class: None,
            agent_type: None,
            agent_id: None,
            parent_agent_id: None,
            prompt_id: None,
            compaction: None,
            after_compaction: None,
            tool_durations: None,
            requested_model: None,
            model: None,
            usage_status: NO_USAGE,
            input_tokens: None,
            output_tokens: None,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            cost: None,
            cost_status: None,
            pricing_version: None,
            tool_calls: None,
            cache_miss: None,
            prompt_hashes: None,
            upgrade_issues: None,
            failovers: &[],
        }
    }
}

fn outcome<S: Serializer>(outcome: &Outcome, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(outcome.as_str())
}

fn cache_miss<S: Serializer>(miss: &Option<CacheMiss>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(miss.map_or("", CacheMiss::as_str))
}

/// A cost as an exact decimal string, written without an intermediate String.
fn usd<S: Serializer>(cost: &Option<Pico>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&Usd(cost.unwrap_or(0)))
}

/// The record as borrowed fields, written as a JSON line and as OTLP. With
/// `pricing`, every record carries a cost status: a record without usage
/// (an error, `count_tokens`, a response neither SSE nor JSON) is `unknown`,
/// never $0.
pub(super) fn line<'a>(r: &'a Record, context: &Context<'a>) -> Line<'a> {
    let pricing = context.pricing;
    let scanned = r.seen.as_ref().and_then(|seen| seen.scanned.as_ref());
    let usage = scanned.map(|s| s.usage).unwrap_or_default();
    let cost = pricing.map(|pricing| match scanned {
        Some(Scanned {
            model: Some(model),
            usage,
            status,
            ..
        }) if *status != UsageStatus::Unknown => pricing.cost(
            model,
            r.info.upstream.as_deref(),
            usage,
            *status == UsageStatus::Complete,
        ),
        _ => gateway_core::pricing::Cost {
            pico: None,
            status: CostStatus::Unknown,
        },
    });
    let headers = &r.info.agent;
    Line {
        event: "request",
        v: RECORD_VERSION,
        unix_ms: r.unix_ms,
        endpoint: r.info.endpoint.path,
        provider_name: r.info.provider.otel_name(),
        status: r.status.as_u16(),
        outcome: r.outcome,
        upstream_error: match r.outcome {
            Outcome::UpstreamError(error) => Some(error.as_str()),
            _ => None,
        },
        duration_ms: ms(r.duration),
        response_headers_ms: r.seen.as_ref().map(|seen| ms(seen.headers_after)),
        upstream: r.info.upstream.as_deref(),
        subject: r.info.identity.as_ref().map(|i| &*i.subject),
        session_id: label(r.info.session_id.as_ref()),
        traceparent: r.info.traceparent.as_ref().and_then(|v| v.to_str().ok()),
        request_class: headers.request_class.map(|c| c.as_str()),
        agent_type: label(headers.agent_type.as_ref()),
        agent_id: label(headers.agent_id.as_ref()),
        parent_agent_id: label(headers.parent_agent_id.as_ref()),
        prompt_id: label(headers.prompt_id.as_ref()),
        compaction: headers.compaction.map(|c| c.as_str()),
        after_compaction: headers.after_compaction.map(|c| c.as_str()),
        tool_durations: headers
            .prev_tool_durations
            .as_ref()
            .map(|v| ToolDurations(v.as_bytes())),
        requested_model: r.info.requested_model.as_deref(),
        model: scanned.and_then(|s| s.model.as_deref()),
        usage_status: scanned.map_or(NO_USAGE, |s| s.status.as_str()),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cost: cost.and_then(|c| c.pico),
        cost_status: cost.map(|c| c.status.as_str()),
        pricing_version: pricing.map(|p| &*p.version),
        tool_calls: scanned
            .and_then(|s| s.tool_calls.as_deref())
            .map(ToolCallsField),
        cache_miss: CacheMiss::classify(
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
            headers.compaction,
            headers.after_compaction,
        ),
        prompt_hashes: r
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
        failovers: &r.info.failovers,
    }
}

/// A client-supplied value, kept only as a bounded label.
fn label(value: Option<&HeaderValue>) -> Option<&str> {
    value.and_then(|v| agent::label(v.as_bytes()))
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

impl Serialize for RejectedAttempt {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("upstream", &*self.upstream)?;
        map.serialize_entry("reason", self.reason.as_str())?;
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Readers find the format's version right after the event.
    #[test]
    fn the_version_follows_the_event() {
        let mut buf = Vec::new();
        push_line(&mut buf, &Line::blank());
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.starts_with(r#"{"event":"request","v":2,"unix_ms":0,"endpoint":"#),
            "{text}"
        );
    }
}
