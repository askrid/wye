//! `wye report`: a summary of a usage log (the JSON lines `wye serve` writes
//! to stderr), offline and with no telemetry backend.
//!
//! It answers what the gateway cannot per request: totals per Claude Code
//! session and prompt, spend by request class, and which cache misses are
//! regressions. A miss on the first cache write of a session or a subagent
//! (a conversation of its own) is expected; one request cannot know it is
//! first, but the whole log can.

use std::{collections::HashMap, io::BufRead};

use gateway_core::pricing::parse_usd;
use serde::Deserialize;

/// Rows shown in the session and prompt rankings.
const TOP: usize = 10;

/// The usage-record fields the report reads; everything else is ignored.
#[derive(Deserialize)]
struct Record {
    event: String,
    #[serde(default)]
    unix_ms: u64,
    #[serde(default)]
    duration_ms: f64,
    status: Option<u16>,
    outcome: Option<String>,
    upstream: Option<String>,
    subject: Option<String>,
    session_id: Option<String>,
    agent_id: Option<String>,
    request_class: Option<String>,
    prompt_id: Option<String>,
    compaction: Option<String>,
    cache_miss: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cost_usd: Option<String>,
    cost_status: Option<String>,
    /// Target model to the rules it would break.
    #[serde(default)]
    upgrade_issues: HashMap<String, Vec<String>>,
    #[serde(default)]
    tool_durations: Vec<Tool>,
    #[serde(default)]
    tool_calls: HashMap<String, u64>,
    #[serde(default)]
    failovers: Vec<FailoverRecord>,
}

/// An upstream that refused the request before another answered.
#[derive(Deserialize)]
struct FailoverRecord {
    upstream: Option<String>,
    status: Option<u16>,
}

#[derive(Deserialize)]
struct Tool {
    name: String,
    ms: u64,
}

#[derive(Default, Clone, Copy)]
struct Tokens {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Tokens {
    fn add(&mut self, r: &Record) {
        // Saturating: a corrupt line must not wrap the totals.
        let add = |sum: &mut u64, n: Option<u64>| *sum = sum.saturating_add(n.unwrap_or(0));
        add(&mut self.input, r.input_tokens);
        add(&mut self.output, r.output_tokens);
        add(&mut self.cache_read, r.cache_read_input_tokens);
        add(&mut self.cache_write, r.cache_creation_input_tokens);
    }

    fn total(self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    /// Share of prompt tokens read from the cache.
    fn hit_rate(self) -> String {
        let prompt = self.input + self.cache_read + self.cache_write;
        if prompt == 0 {
            "-".into()
        } else {
            format!("{:.1}%", self.cache_read as f64 * 100.0 / prompt as f64)
        }
    }
}

/// Summed cost, exact in pico-USD.
#[derive(Default, Clone, Copy)]
struct Cost {
    pico: u64,
    /// Records with a cost status.
    priced: u64,
    /// Some record was partial, unknown, or not priced at all while others
    /// were: the sum is a lower bound.
    lower_bound: bool,
}

impl Cost {
    fn add(&mut self, r: &Record) {
        let mut tokens = Tokens::default();
        tokens.add(r);
        let Some(status) = &r.cost_status else {
            // Written without `[pricing]` (before it was added, or by a
            // replica without it): its tokens are not in the sum.
            self.lower_bound |= tokens.total() > 0;
            return;
        };
        self.priced += 1;
        let pico = r.cost_usd.as_deref().and_then(parse_usd);
        self.lower_bound |= status != "complete" || pico.is_none();
        self.pico = self.pico.saturating_add(pico.unwrap_or(0));
    }

    /// Dollars to four decimals (display only); `+` marks a lower bound.
    fn show(self) -> String {
        if self.priced == 0 {
            return "-".into();
        }
        let usd = self.pico as f64 / 1e12;
        format!("${usd:.4}{}", if self.lower_bound { "+" } else { "" })
    }
}

#[derive(Default)]
struct Group {
    requests: u64,
    tokens: Tokens,
    cost: Cost,
    model_ms: f64,
    tool_ms: u64,
}

impl Group {
    fn add(&mut self, r: &Record) {
        self.requests += 1;
        self.tokens.add(r);
        self.cost.add(r);
        self.model_ms += r.duration_ms;
        self.tool_ms += r.tool_durations.iter().map(|t| t.ms).sum::<u64>();
    }
}

#[derive(Default)]
struct Session {
    group: Group,
    subject: Option<String>,
    compactions: u64,
}

/// One conversation with its own prompt cache: a session's main thread
/// (no agent id) or one of its subagents.
type Conversation = (String, Option<String>);

#[derive(Default)]
struct Summary {
    total: Group,
    /// Lines `wye serve` prints about itself (`wye: listening on ...`), which
    /// share stderr with the records.
    notices: u64,
    skipped: u64,
    outcomes: HashMap<String, u64>,
    /// Requests answered with a 4xx or 5xx status, by status.
    errors: HashMap<u16, u64>,
    classes: HashMap<String, Group>,
    /// Route upstreams, when the log names them.
    upstreams: HashMap<String, Group>,
    /// Refusals that moved requests on, by (upstream, reason).
    failovers: HashMap<(String, String), u64>,
    sessions: HashMap<String, Session>,
    /// Time and cache-miss label of each conversation's first cache write.
    /// Earlier requests that use no cache (`count_tokens`, refusals) do not
    /// count.
    first_writes: HashMap<Conversation, (u64, Option<String>)>,
    prompts: HashMap<String, Group>,
    /// Per tool: calls with a reported duration, their total ms, and calls
    /// responses requested.
    tools: HashMap<String, (u64, u64, u64)>,
    misses: HashMap<String, u64>,
    /// Per (target, rule): requests and the clients (subjects) that sent them.
    upgrades: HashMap<(String, String), (u64, Vec<String>)>,
}

impl Summary {
    fn add(&mut self, r: Record) {
        self.total.add(&r);
        *self
            .outcomes
            .entry(r.outcome.clone().unwrap_or_default())
            .or_default() += 1;
        if let Some(status) = r.status.filter(|s| *s >= 400) {
            *self.errors.entry(status).or_default() += 1;
        }
        let class = r.request_class.as_deref().unwrap_or("(no hint)");
        self.classes.entry(class.to_owned()).or_default().add(&r);
        if let Some(upstream) = &r.upstream {
            self.upstreams.entry(upstream.clone()).or_default().add(&r);
        }
        for failover in &r.failovers {
            let reason = failover
                .status
                .map_or_else(|| "connect".to_owned(), |s| s.to_string());
            let upstream = failover.upstream.clone().unwrap_or_default();
            *self.failovers.entry((upstream, reason)).or_default() += 1;
        }
        if let Some(miss) = &r.cache_miss {
            *self.misses.entry(miss.clone()).or_default() += 1;
        }
        for (target, rules) in &r.upgrade_issues {
            for rule in rules {
                let entry = self
                    .upgrades
                    .entry((target.clone(), rule.clone()))
                    .or_default();
                entry.0 += 1;
                let client = r.subject.as_deref().unwrap_or("(no credential)");
                if !entry.1.iter().any(|c| c == client) {
                    entry.1.push(client.to_owned());
                }
            }
        }
        for tool in &r.tool_durations {
            let entry = self.tools.entry(tool.name.clone()).or_default();
            entry.0 += 1;
            entry.1 += tool.ms;
        }
        for (name, count) in &r.tool_calls {
            self.tools.entry(name.clone()).or_default().2 += count;
        }
        if let Some(prompt) = &r.prompt_id {
            self.prompts.entry(prompt.clone()).or_default().add(&r);
        }
        if let Some(id) = &r.session_id {
            let session = self.sessions.entry(id.clone()).or_default();
            session.group.add(&r);
            session.compactions += u64::from(r.compaction.is_some());
            if session.subject.is_none() {
                session.subject.clone_from(&r.subject);
            }
            if r.cache_creation_input_tokens.is_some_and(|n| n > 0) {
                let first = self
                    .first_writes
                    .entry((id.clone(), r.agent_id.clone()))
                    .or_insert_with(|| (r.unix_ms, r.cache_miss.clone()));
                if r.unix_ms < first.0 {
                    *first = (r.unix_ms, r.cache_miss.clone());
                }
            }
        }
    }

    /// Unexplained misses that were the first cache write of a session's
    /// main thread and of its subagents: expected.
    fn starts(&self) -> Starts {
        let mut starts = Starts::default();
        for ((_, agent), (_, miss)) in &self.first_writes {
            if miss.as_deref() == Some("unexplained") {
                match agent {
                    None => starts.sessions += 1,
                    Some(_) => starts.subagents += 1,
                }
            }
        }
        starts
    }
}

#[derive(Default)]
struct Starts {
    sessions: u64,
    subagents: u64,
}

/// Summarizes a usage log. Lines that are not request records (shutdown
/// notices, other output) are skipped and counted.
pub fn report(input: impl BufRead) -> std::io::Result<String> {
    let mut summary = Summary::default();
    for line in input.lines() {
        let line = line?;
        match serde_json::from_str::<Record>(&line) {
            Ok(r) if r.event == "request" => summary.add(r),
            _ if line.starts_with("wye: ") => summary.notices += 1,
            _ => summary.skipped += 1,
        }
    }
    Ok(render(&summary))
}

mod render;

use render::render;

#[cfg(test)]
mod tests;
