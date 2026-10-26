//! `wye report`: a summary of a usage log (the JSON lines `wye serve` writes
//! to stderr), offline and with no telemetry backend.
//!
//! It answers what the gateway cannot per request: totals per Claude Code
//! session and prompt, spend by request class, and which cache misses are
//! regressions. A miss on a session's first request is expected; one request
//! cannot know it is first, but the whole log can.

use std::{collections::HashMap, fmt::Write as _, io::BufRead};

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
    outcome: Option<String>,
    subject: Option<String>,
    session_id: Option<String>,
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
    #[serde(default)]
    tool_durations: Vec<Tool>,
    #[serde(default)]
    tool_calls: HashMap<String, u64>,
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
        self.input += r.input_tokens.unwrap_or(0);
        self.output += r.output_tokens.unwrap_or(0);
        self.cache_read += r.cache_read_input_tokens.unwrap_or(0);
        self.cache_write += r.cache_creation_input_tokens.unwrap_or(0);
    }

    fn total(self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
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
    /// Some summed record was partial or unknown: the sum is a lower bound.
    lower_bound: bool,
}

impl Cost {
    fn add(&mut self, r: &Record) {
        let Some(status) = &r.cost_status else { return };
        self.priced += 1;
        self.lower_bound |= status != "complete";
        if let Some(pico) = r.cost_usd.as_deref().and_then(parse_usd) {
            self.pico = self.pico.saturating_add(pico);
        }
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
    /// Time and cache-miss label of the earliest request seen.
    first: Option<(u64, Option<String>)>,
}

#[derive(Default)]
struct Summary {
    total: Group,
    skipped: u64,
    outcomes: HashMap<String, u64>,
    classes: HashMap<String, Group>,
    sessions: HashMap<String, Session>,
    prompts: HashMap<String, Group>,
    /// Per tool: calls with a reported duration, their total ms, and calls
    /// responses requested.
    tools: HashMap<String, (u64, u64, u64)>,
    misses: HashMap<String, u64>,
}

impl Summary {
    fn add(&mut self, r: Record) {
        self.total.add(&r);
        *self
            .outcomes
            .entry(r.outcome.clone().unwrap_or_default())
            .or_default() += 1;
        let class = r.request_class.as_deref().unwrap_or("(no hint)");
        self.classes.entry(class.to_owned()).or_default().add(&r);
        if let Some(miss) = &r.cache_miss {
            *self.misses.entry(miss.clone()).or_default() += 1;
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
            if session.first.as_ref().is_none_or(|(at, _)| r.unix_ms < *at) {
                session.first = Some((r.unix_ms, r.cache_miss.clone()));
            }
        }
    }

    /// Unexplained misses that were a session's first request: expected.
    fn session_starts(&self) -> u64 {
        self.sessions
            .values()
            .filter(|s| matches!(&s.first, Some((_, Some(m))) if m == "unexplained"))
            .count() as u64
    }
}

/// Summarizes a usage log. Lines that are not request records (shutdown
/// notices, other output) are skipped and counted.
pub fn report(input: impl BufRead) -> std::io::Result<String> {
    let mut summary = Summary::default();
    for line in input.lines() {
        match serde_json::from_str::<Record>(&line?) {
            Ok(r) if r.event == "request" => summary.add(r),
            _ => summary.skipped += 1,
        }
    }
    Ok(render(&summary))
}

fn render(s: &Summary) -> String {
    let mut out = String::new();
    let mut outcomes: Vec<_> = s.outcomes.iter().collect();
    outcomes.sort();
    let _ = writeln!(
        out,
        "{} requests in {} sessions ({} other lines skipped); outcomes: {}",
        s.total.requests,
        s.sessions.len(),
        s.skipped,
        outcomes
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let _ = writeln!(out, "\nBy request class");
    table_header(&mut out, "class");
    table_row(&mut out, "all", &s.total);
    let mut classes: Vec<_> = s.classes.iter().collect();
    classes.sort_by(|a, b| {
        b.1.tokens
            .total()
            .cmp(&a.1.tokens.total())
            .then(a.0.cmp(b.0))
    });
    for (name, group) in classes {
        table_row(&mut out, name, group);
    }

    let starts = s.session_starts();
    let count = |k: &str| s.misses.get(k).copied().unwrap_or(0);
    let _ = writeln!(
        out,
        "\nCache misses (cache written, nothing read): {} unexplained, {} session starts, \
         {} compactions, {} after compaction",
        count("unexplained").saturating_sub(starts),
        starts,
        count("compaction"),
        count("after_compaction"),
    );

    if !s.tools.is_empty() {
        let _ = writeln!(
            out,
            "\nTools: calls the model requested; calls timed by Claude Code, and their time"
        );
        let _ = writeln!(
            out,
            "  {:<28} {:>9} {:>7} {:>10}",
            "tool", "requested", "timed", "time"
        );
        let mut tools: Vec<_> = s.tools.iter().collect();
        tools.sort_by(|a, b| {
            ((b.1).1, (b.1).2)
                .cmp(&((a.1).1, (a.1).2))
                .then(a.0.cmp(b.0))
        });
        for (name, (timed, ms, requested)) in tools.iter().take(TOP) {
            let _ = writeln!(
                out,
                "  {name:<28} {requested:>9} {timed:>7} {:>10}",
                seconds(*ms as f64)
            );
        }
    }

    let _ = writeln!(out, "\nTop sessions by tokens");
    table_header(&mut out, "session (subject)");
    let mut sessions: Vec<_> = s.sessions.iter().collect();
    sessions.sort_by(|a, b| {
        b.1.group
            .tokens
            .total()
            .cmp(&a.1.group.tokens.total())
            .then(a.0.cmp(b.0))
    });
    for (id, session) in sessions.iter().take(TOP) {
        let label = match &session.subject {
            Some(subject) => format!("{} ({subject})", short(id)),
            None => short(id).to_owned(),
        };
        table_row(&mut out, &label, &session.group);
        if session.compactions > 0 {
            let _ = writeln!(out, "    {} compactions", session.compactions);
        }
    }

    if !s.prompts.is_empty() {
        let _ = writeln!(out, "\nTop prompts by tokens");
        table_header(&mut out, "prompt");
        let mut prompts: Vec<_> = s.prompts.iter().collect();
        prompts.sort_by(|a, b| {
            b.1.tokens
                .total()
                .cmp(&a.1.tokens.total())
                .then(a.0.cmp(b.0))
        });
        for (id, group) in prompts.iter().take(TOP) {
            table_row(&mut out, short(id), group);
        }
    }
    out
}

fn table_header(out: &mut String, first: &str) {
    let _ = writeln!(
        out,
        "  {first:<28} {:>7} {:>12} {:>10} {:>13} {:>13} {:>7} {:>11} {:>10} {:>10}",
        "reqs", "input", "output", "cache read", "cache write", "hit", "cost", "time", "tools"
    );
}

fn table_row(out: &mut String, name: &str, g: &Group) {
    let t = g.tokens;
    let _ = writeln!(
        out,
        "  {name:<28} {:>7} {:>12} {:>10} {:>13} {:>13} {:>7} {:>11} {:>10} {:>10}",
        g.requests,
        t.input,
        t.output,
        t.cache_read,
        t.cache_write,
        t.hit_rate(),
        g.cost.show(),
        seconds(g.model_ms),
        seconds(g.tool_ms as f64),
    );
}

fn seconds(ms: f64) -> String {
    format!("{:.1}s", ms / 1000.0)
}

/// The first 8 characters of an id: enough to tell rows apart.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests;
