//! `wye report`: a summary of usage logs (the JSON lines `wye serve` writes
//! to stdout or its `--usage-log`), offline and with no telemetry backend,
//! optionally from a time on.
//!
//! It answers what the gateway cannot per request: totals per Claude Code
//! session and prompt, spend by request class, and which cache misses are
//! regressions. A miss on the first cache write of a session or a subagent
//! (a conversation of its own) is expected; one request cannot know it is
//! first, but the whole log can.

use std::{
    borrow::Cow,
    collections::HashMap,
    io::BufRead,
    path::{Path, PathBuf},
};

use gateway_core::{
    agent::CacheMiss,
    pricing::{CostStatus, PICO_PER_USD, parse_usd},
};
use serde::Deserialize;

use super::{
    Env, Error, Exit,
    args::Parsed,
    duration_secs, emit,
    files::{self, USAGE_LOG_FILE},
    paint::Paint,
};
use crate::log_files;

mod render;

use render::render;

/// Rows shown in the session and prompt rankings.
const TOP: usize = 10;

/// The usage-record fields the report reads; everything else is ignored, so
/// every format version reads alike: v2 renamed only `path`, which the
/// report does not read. Its strings borrow from the line.
#[derive(Deserialize)]
struct Record<'a> {
    #[serde(borrow)]
    event: Text<'a>,
    #[serde(default)]
    unix_ms: u64,
    #[serde(default)]
    duration_ms: f64,
    status: Option<u16>,
    #[serde(borrow)]
    outcome: Option<Text<'a>>,
    /// Why an upstream gave no usable response: `connect`, `timeout`, or
    /// `reset`.
    #[serde(borrow)]
    upstream_error: Option<Text<'a>>,
    #[serde(borrow)]
    upstream: Option<Text<'a>>,
    #[serde(borrow)]
    subject: Option<Text<'a>>,
    #[serde(borrow)]
    session_id: Option<Text<'a>>,
    #[serde(borrow)]
    agent_id: Option<Text<'a>>,
    #[serde(borrow)]
    request_class: Option<Text<'a>>,
    #[serde(borrow)]
    prompt_id: Option<Text<'a>>,
    #[serde(borrow)]
    compaction: Option<Text<'a>>,
    #[serde(borrow)]
    cache_miss: Option<Text<'a>>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    #[serde(borrow)]
    cost_usd: Option<Text<'a>>,
    #[serde(borrow)]
    cost_status: Option<Text<'a>>,
    /// Target model to the rules it would break.
    #[serde(borrow, default)]
    upgrade_issues: HashMap<Text<'a>, Vec<Text<'a>>>,
    #[serde(borrow, default)]
    tool_durations: Vec<Tool<'a>>,
    #[serde(borrow, default)]
    tool_calls: HashMap<Text<'a>, u64>,
    #[serde(borrow, default)]
    failovers: Vec<FailoverRecord<'a>>,
}

/// A string in a record, borrowed from the line unless it holds an escape,
/// which serde unescapes into a string of its own. A `&str` would fail on
/// such a string, and drop its record into the skipped lines.
#[derive(Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
struct Text<'a>(#[serde(borrow)] Cow<'a, str>);

impl std::ops::Deref for Text<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

/// An upstream that rejected the request before another answered.
#[derive(Deserialize)]
struct FailoverRecord<'a> {
    #[serde(borrow)]
    upstream: Option<Text<'a>>,
    /// `429`, `529`, or `connect`.
    #[serde(borrow)]
    reason: Text<'a>,
}

#[derive(Deserialize)]
struct Tool<'a> {
    #[serde(borrow)]
    name: Text<'a>,
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
    fn add(&mut self, r: &Record<'_>) {
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
    fn add(&mut self, r: &Record<'_>) {
        let mut tokens = Tokens::default();
        tokens.add(r);
        let Some(status) = r.cost_status.as_deref() else {
            // Written without `[pricing]` (before it was added, or by a
            // replica without it): its tokens are not in the sum.
            self.lower_bound |= tokens.total() > 0;
            return;
        };
        self.priced += 1;
        let pico = r.cost_usd.as_deref().and_then(parse_usd);
        self.lower_bound |= status != CostStatus::Complete.as_str() || pico.is_none();
        self.pico = self.pico.saturating_add(pico.unwrap_or(0));
    }

    /// Dollars to four decimals (display only); `+` marks a lower bound.
    fn show(self) -> String {
        if self.priced == 0 {
            return "-".into();
        }
        let usd = self.pico as f64 / PICO_PER_USD as f64;
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
    fn add(&mut self, r: &Record<'_>) {
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
    /// The first cache write of the session's main thread. Earlier requests
    /// that use no cache (`count_tokens`, refusals) do not count.
    first_write: Option<FirstWrite>,
    /// The first cache write of each subagent, by agent id: a conversation
    /// of its own, with a prompt cache of its own.
    subagents: HashMap<String, Option<FirstWrite>>,
}

/// When a conversation first wrote to its cache, and the cache-miss label
/// of that request.
type FirstWrite = (u64, Option<String>);

/// Keeps the earlier of `first` and a cache write at `unix_ms` labeled
/// `miss`.
fn earliest(first: &mut Option<FirstWrite>, unix_ms: u64, miss: Option<&str>) {
    if first.as_ref().is_none_or(|(at, _)| unix_ms < *at) {
        *first = Some((unix_ms, miss.map(str::to_owned)));
    }
}

/// Whether a conversation's first cache write was an unexplained miss: its
/// start, which is expected.
fn unexplained(first: &Option<FirstWrite>) -> bool {
    first
        .as_ref()
        .is_some_and(|(_, miss)| miss.as_deref() == Some(CacheMiss::Unexplained.as_str()))
}

/// Applies `f` to `map`'s value for `key`, a default one if there is none:
/// an owned key is made only for a key not seen before.
fn at<V: Default>(map: &mut HashMap<String, V>, key: &str, f: impl FnOnce(&mut V)) {
    match map.get_mut(key) {
        Some(value) => f(value),
        None => f(map.entry(key.to_owned()).or_default()),
    }
}

/// The records a report covers, by `unix_ms`: from `since` on. An end is
/// a filter for a pipe (`jq 'select(.unix_ms < ...)'`), not a flag.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Window {
    since: Option<u64>,
}

impl Window {
    /// The window `--since` names, `now` in Unix seconds.
    fn parse(since: Option<&str>, now: u64) -> Result<Self, String> {
        let since = since
            .map(|when| {
                log_files::midnight(when)
                    .or_else(|| duration_secs(when).map(|ago| now.saturating_sub(ago)))
                    .map(|secs| secs.saturating_mul(1000))
                    .ok_or_else(|| {
                        format!(
                            concat!(
                                "--since {}: write it as a time ago (",
                                gateway_core::duration_syntax!(),
                                ") or a UTC date (2026-11-01)"
                            ),
                            when
                        )
                    })
            })
            .transpose()?;
        Ok(Self { since })
    }

    fn contains(self, unix_ms: u64) -> bool {
        self.since.is_none_or(|since| unix_ms >= since)
    }

    /// The window in words, `since 2026-11-01 00:00:00 UTC`; `None` for all
    /// records.
    fn show(self) -> Option<String> {
        self.since
            .map(|since| format!("since {}", log_files::readable(since / 1000)))
    }
}

/// Session and subagent starts among the unexplained cache misses.
#[derive(Default)]
struct Starts {
    sessions: u64,
    subagents: u64,
}

#[derive(Default)]
struct Summary {
    window: Window,
    /// Records outside the window.
    outside: u64,
    total: Group,
    /// Lines `wye serve` prints about itself (`wye: listening on ...`).
    notices: u64,
    skipped: u64,
    outcomes: HashMap<String, u64>,
    /// Requests answered with a 4xx or 5xx status, by status.
    errors: HashMap<u16, u64>,
    /// `upstream_error` outcomes by why.
    upstream_errors: HashMap<String, u64>,
    classes: HashMap<String, Group>,
    /// Route upstreams, by the id records name them by.
    upstreams: HashMap<String, Group>,
    /// Rejections that moved requests on, by (upstream, reason).
    failovers: HashMap<(String, String), u64>,
    sessions: HashMap<String, Session>,
    prompts: HashMap<String, Group>,
    /// Per tool: calls with a reported duration, their total ms, and calls
    /// responses requested.
    tools: HashMap<String, (u64, u64, u64)>,
    misses: HashMap<String, u64>,
    /// Per (target, rule): requests and the clients (subjects) that sent them.
    upgrades: HashMap<(String, String), (u64, Vec<String>)>,
}

impl Summary {
    fn add(&mut self, r: &Record<'_>) {
        self.total.add(r);
        at(
            &mut self.outcomes,
            r.outcome.as_deref().unwrap_or_default(),
            |n| *n += 1,
        );
        if let Some(status) = r.status.filter(|s| *s >= 400) {
            *self.errors.entry(status).or_default() += 1;
        }
        if let Some(error) = r.upstream_error.as_deref() {
            at(&mut self.upstream_errors, error, |n| *n += 1);
        }
        // No request-class hint: an SDK caller, or an agent without them.
        let class = r.request_class.as_deref().unwrap_or("(unclassified)");
        at(&mut self.classes, class, |group| group.add(r));
        if let Some(upstream) = r.upstream.as_deref() {
            at(&mut self.upstreams, upstream, |group| group.add(r));
        }
        // Rare: a failover, an upgrade issue.
        for failover in &r.failovers {
            let upstream = failover.upstream.as_deref().unwrap_or_default();
            *self
                .failovers
                .entry((upstream.to_owned(), failover.reason.to_string()))
                .or_default() += 1;
        }
        if let Some(miss) = r.cache_miss.as_deref() {
            at(&mut self.misses, miss, |n| *n += 1);
        }
        for (target, rules) in &r.upgrade_issues {
            for rule in rules {
                let entry = self
                    .upgrades
                    .entry((target.to_string(), rule.to_string()))
                    .or_default();
                entry.0 += 1;
                let client = r.subject.as_deref().unwrap_or("(no token)");
                if !entry.1.iter().any(|c| c == client) {
                    entry.1.push(client.to_owned());
                }
            }
        }
        for tool in &r.tool_durations {
            at(&mut self.tools, &tool.name, |entry| {
                entry.0 += 1;
                entry.1 += tool.ms;
            });
        }
        for (name, count) in &r.tool_calls {
            at(&mut self.tools, name, |entry| entry.2 += count);
        }
        if let Some(prompt) = r.prompt_id.as_deref() {
            at(&mut self.prompts, prompt, |group| group.add(r));
        }
        if let Some(id) = r.session_id.as_deref() {
            at(&mut self.sessions, id, |session| {
                session.group.add(r);
                session.compactions += u64::from(r.compaction.is_some());
                if session.subject.is_none() {
                    session.subject = r.subject.as_deref().map(str::to_owned);
                }
                if r.cache_creation_input_tokens.is_some_and(|n| n > 0) {
                    let miss = r.cache_miss.as_deref();
                    match r.agent_id.as_deref() {
                        None => earliest(&mut session.first_write, r.unix_ms, miss),
                        Some(agent) => at(&mut session.subagents, agent, |first| {
                            earliest(first, r.unix_ms, miss);
                        }),
                    }
                }
            });
        }
    }

    /// Unexplained misses that were the first cache write of a session's
    /// main thread and of its subagents: expected.
    fn starts(&self) -> Starts {
        let mut starts = Starts::default();
        for session in self.sessions.values() {
            starts.sessions += u64::from(unexplained(&session.first_write));
            starts.subagents += session
                .subagents
                .values()
                .filter(|w| unexplained(w))
                .count() as u64;
        }
        starts
    }

    /// Adds a usage log's records in the window. Lines that are not request
    /// records (shutdown notices, other output) are skipped and counted.
    /// Records of every format version are read: those without `v` too.
    fn read(&mut self, mut input: impl BufRead) -> std::io::Result<()> {
        // One buffer for every line, which each record borrows from.
        let mut buffer = String::new();
        loop {
            buffer.clear();
            if input.read_line(&mut buffer)? == 0 {
                return Ok(());
            }
            let line = match buffer.strip_suffix('\n') {
                Some(line) => line.strip_suffix('\r').unwrap_or(line),
                None => &buffer,
            };
            match serde_json::from_str::<Record<'_>>(line) {
                Ok(r) if &*r.event == "request" && self.window.contains(r.unix_ms) => self.add(&r),
                Ok(r) if &*r.event == "request" => self.outside += 1,
                _ if line.starts_with("wye: ") => self.notices += 1,
                _ => self.skipped += 1,
            }
        }
    }
}

/// `wye report`: the summary of the usage logs it was given, or of the one
/// in the home directory with its rotated files, printed.
pub fn run(parsed: &Parsed<'_>, env: &impl Env) -> Result<Exit, Error> {
    let mut summary = Summary {
        window: Window::parse(parsed.value("since"), super::unix_secs()?).map_err(Error::Usage)?,
        ..Summary::default()
    };
    let paths = match parsed.positional.as_slice() {
        [] => home_logs(env)?,
        given => given.iter().map(PathBuf::from).collect(),
    };
    for path in paths {
        let read = if path == Path::new("-") {
            summary.read(std::io::stdin().lock())
        } else {
            std::fs::File::open(&path).and_then(|file| summary.read(std::io::BufReader::new(file)))
        };
        read.map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    }
    emit(&render(&summary, Paint::stdout()));
    Ok(Exit::Done)
}

/// The usage log in the home directory and its rotated files, oldest first:
/// rotated ones by name (their UTC times), then the log itself.
fn home_logs(env: &impl Env) -> Result<Vec<PathBuf>, String> {
    let log = files::in_home(USAGE_LOG_FILE, "pass a usage log", env)?;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(log.parent().unwrap_or(Path::new(".")))
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            log_files::is_rotated(&log, name.to_str()?).then(|| entry.path())
        })
        .collect();
    paths.sort();
    if log.exists() {
        paths.push(log);
    } else if paths.is_empty() {
        let log = files::tilde(&log, env);
        return Err(format!(
            "no usage log at {log}: `wye service install` keeps one, as does \
             `wye serve --usage-log {log}`; or pass a usage log, or - for standard input"
        ));
    }
    Ok(paths)
}

#[cfg(test)]
mod tests;
