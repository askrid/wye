//! The report as text: totals, tables by request class and upstream, cache
//! misses, upgrade issues, failovers, tools, and the top sessions and prompts.
//! Every line fits 80 columns: counts of tokens are compact (`635M`), times
//! read as `4h01m`, and long names are cut.

use std::fmt::Write as _;

use gateway_core::agent::CacheMiss;

use super::{Group, Summary, TOP};
use crate::cli::{
    args::{WIDTH, wrap},
    paint::Paint,
    plural,
};

/// Width of a table's first column.
const NAME: usize = 14;
/// Width of a tool name.
const TOOL: usize = 36;

pub(super) fn render(s: &Summary, paint: Paint) -> String {
    let mut out = String::new();
    if s.total.requests == 0 {
        out.push_str(&wrap(
            &format!(
                "No request records ({}). The gateway writes one per request to stdout; \
                 `wye service install` keeps them.",
                skipped(s),
            ),
            0,
            WIDTH,
        ));
        return out;
    }
    // Without `[pricing]` nothing carries a cost; the column would read "-".
    let columns = Columns {
        cost: s.total.cost.priced > 0,
        paint,
    };
    let mut outcomes: Vec<_> = s.outcomes.iter().collect();
    outcomes.sort();
    out.push_str(&wrap(
        &format!(
            "{} in {} ({}); outcomes: {}",
            plural(s.total.requests, "request", "requests"),
            plural(s.sessions.len() as u64, "session", "sessions"),
            skipped(s),
            pairs(outcomes)
        ),
        0,
        WIDTH,
    ));
    // A refusal (429, 529) or a client error is an answer like any other,
    // so its outcome reads completed.
    if !s.errors.is_empty() {
        let mut errors: Vec<_> = s.errors.iter().collect();
        errors.sort();
        let _ = writeln!(out, "error statuses: {}", pairs(errors));
    }

    columns.title(&mut out, "By request class");
    columns.header(&mut out, "class");
    columns.row(&mut out, "all", &s.total);
    let mut classes: Vec<_> = s.classes.iter().collect();
    classes.sort_by(|a, b| {
        b.1.tokens
            .total()
            .cmp(&a.1.tokens.total())
            .then(a.0.cmp(b.0))
    });
    for (name, group) in classes {
        columns.row(&mut out, name, group);
    }

    if !s.upstreams.is_empty() {
        columns.title(&mut out, "By upstream");
        columns.header(&mut out, "upstream");
        let mut upstreams: Vec<_> = s.upstreams.iter().collect();
        upstreams.sort_by(|a, b| a.0.cmp(b.0));
        for (name, group) in upstreams {
            columns.row(&mut out, name, group);
        }
    }
    if !s.failovers.is_empty() {
        let _ = writeln!(
            out,
            "\n{}",
            paint.bold("Failovers: refusals that moved a request to the next upstream")
        );
        let _ = writeln!(
            out,
            "  {:24} {:>8} {:>10}",
            "upstream", "reason", "requests"
        );
        let mut failovers: Vec<_> = s.failovers.iter().collect();
        failovers.sort();
        for ((upstream, reason), count) in failovers {
            let _ = writeln!(out, "  {:24} {reason:>8} {count:>10}", cut(upstream, 24));
        }
    }

    let starts = s.starts();
    let count = |k: &str| s.misses.get(k).copied().unwrap_or(0);
    columns.title(&mut out, "Cache misses: cache written, nothing read");
    out.push_str(&wrap(
        &format!(
            "{} unexplained; expected: {}, {}, {}, {} after compaction",
            count(CacheMiss::Unexplained.as_str())
                .saturating_sub(starts.sessions)
                .saturating_sub(starts.subagents),
            plural(starts.sessions, "session start", "session starts"),
            plural(starts.subagents, "subagent start", "subagent starts"),
            plural(
                count(CacheMiss::Compaction.as_str()),
                "compaction",
                "compactions"
            ),
            count(CacheMiss::AfterCompaction.as_str()),
        ),
        2,
        WIDTH,
    ));

    if !s.upgrades.is_empty() {
        columns.title(
            &mut out,
            "Upgrade issues: requests a target model would reject",
        );
        let mut upgrades: Vec<_> = s.upgrades.iter().collect();
        upgrades.sort_by(|a, b| (b.1).0.cmp(&(a.1).0).then(a.0.cmp(b.0)));
        for ((target, rule), (requests, clients)) in upgrades {
            let mut clients = clients.clone();
            clients.sort();
            let _ = writeln!(
                out,
                "  {:<24} {:<20} {requests:>7}",
                cut(target, 24),
                cut(rule, 20)
            );
            out.push_str(&wrap(&format!("from {}", clients.join(", ")), 4, WIDTH));
        }
    }

    if !s.tools.is_empty() {
        columns.title(
            &mut out,
            "Tools: calls the model requested; calls Claude Code timed, and their time",
        );
        let mut tools: Vec<_> = s.tools.iter().collect();
        tools.sort_by(|a, b| {
            ((b.1).1, (b.1).2)
                .cmp(&((a.1).1, (a.1).2))
                .then(a.0.cmp(b.0))
        });
        tools.truncate(TOP);
        // MCP tool names (`mcp__server__tool`) run long.
        let width = tools
            .iter()
            .map(|(name, _)| name.chars().count())
            .fold(20, usize::max)
            .min(TOOL);
        let _ = writeln!(
            out,
            "  {:<width$} {:>9} {:>7} {:>6}",
            "tool", "requested", "timed", "time"
        );
        for (name, (timed, ms, requested)) in tools {
            let _ = writeln!(
                out,
                "  {:<width$} {requested:>9} {timed:>7} {:>6}",
                cut(name, width),
                duration(*ms as f64)
            );
        }
    }

    columns.title(&mut out, "Top sessions by tokens");
    columns.header(&mut out, "session");
    let mut sessions: Vec<_> = s.sessions.iter().collect();
    sessions.sort_by(|a, b| {
        b.1.group
            .tokens
            .total()
            .cmp(&a.1.group.tokens.total())
            .then(a.0.cmp(b.0))
    });
    // A session without tokens (a refused probe, a count) has no place in a
    // ranking by tokens.
    sessions.retain(|(_, session)| session.group.tokens.total() > 0);
    for (id, session) in sessions.iter().take(TOP) {
        columns.row(&mut out, short(id), &session.group);
        // The subject and compactions on a line of their own: a subject
        // runs longer than the first column.
        let mut details: Vec<String> = session.subject.iter().cloned().collect();
        if session.compactions > 0 {
            details.push(plural(session.compactions, "compaction", "compactions"));
        }
        if !details.is_empty() {
            out.push_str(&wrap(&details.join(", "), 4, WIDTH));
        }
    }

    if !s.prompts.is_empty() {
        columns.title(&mut out, "Top prompts by tokens");
        columns.header(&mut out, "prompt");
        let mut prompts: Vec<_> = s.prompts.iter().collect();
        prompts.sort_by(|a, b| {
            b.1.tokens
                .total()
                .cmp(&a.1.tokens.total())
                .then(a.0.cmp(b.0))
        });
        for (id, group) in prompts.iter().take(TOP) {
            columns.row(&mut out, short(id), group);
        }
    }
    out
}

/// The columns of the token tables: 79 columns with a cost, 69 without.
struct Columns {
    cost: bool,
    paint: Paint,
}

impl Columns {
    /// A section title, after a blank line.
    fn title(&self, out: &mut String, title: &str) {
        let _ = writeln!(out, "\n{}", self.paint.bold(title));
    }

    /// Two header lines: "cache" over the prompt-cache columns, then the
    /// column names.
    fn header(&self, out: &mut String, first: &str) {
        let _ = writeln!(
            out,
            "  {:NAME$} {:5} {:6} {:6} {:>6} {:>6}",
            "", "", "", "", "cache", "cache"
        );
        let _ = write!(
            out,
            "  {first:<NAME$} {:>5} {:>6} {:>6} {:>6} {:>6} {:>6}",
            "reqs", "input", "output", "read", "write", "hit"
        );
        if self.cost {
            let _ = write!(out, " {:>9}", "cost");
        }
        let _ = writeln!(out, " {:>5} {:>5}", "time", "tools");
    }

    fn row(&self, out: &mut String, name: &str, g: &Group) {
        let t = g.tokens;
        let _ = write!(
            out,
            "  {:<NAME$} {:>5} {:>6} {:>6} {:>6} {:>6} {:>6}",
            cut(name, NAME),
            g.requests,
            compact(t.input),
            compact(t.output),
            compact(t.cache_read),
            compact(t.cache_write),
            t.hit_rate(),
        );
        if self.cost {
            let _ = write!(out, " {:>9}", g.cost.show());
        }
        let _ = writeln!(
            out,
            " {:>5} {:>5}",
            duration(g.model_ms),
            duration(g.tool_ms as f64)
        );
    }
}

/// What the report skipped: the gateway's own notices and anything else.
fn skipped(s: &Summary) -> String {
    let others = plural(s.skipped, "other line", "other lines");
    match s.notices {
        0 => format!("{others} skipped"),
        n if s.skipped == 0 => {
            format!("{} skipped", plural(n, "gateway notice", "gateway notices"))
        }
        n => format!(
            "{} and {others} skipped",
            plural(n, "gateway notice", "gateway notices")
        ),
    }
}

/// `name count` pairs, comma-separated.
fn pairs(pairs: Vec<(impl std::fmt::Display, &u64)>) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k} {v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A count in at most four characters: `950`, `9.5K`, `445K`, `1.2M`.
fn compact(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "G"), (1_000_000, "M"), (1_000, "K")];
    for (scale, unit) in UNITS {
        if n >= scale {
            let value = n as f64 / scale as f64;
            // 9.95 would print as 10.0: five characters.
            return if value < 9.95 {
                format!("{value:.1}{unit}")
            } else {
                format!("{value:.0}{unit}")
            };
        }
    }
    n.to_string()
}

/// A duration in at most five characters: `0.7s`, `14s`, `4m40s`, `37m`,
/// `4h01m`, `123h`; `-` for none.
fn duration(ms: f64) -> String {
    let secs = ms / 1000.0;
    match secs {
        s if s < 0.05 => "-".to_owned(),
        s if s < 9.95 => format!("{s:.1}s"),
        s if s < 59.5 => format!("{s:.0}s"),
        _ => {
            let s = secs.round() as u64;
            match s {
                s if s < 600 => format!("{}m{:02}s", s / 60, s % 60),
                s if s < 3600 => format!("{}m", s / 60),
                s if s < 360_000 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
                s => format!("{}h", s / 3600),
            }
        }
    }
}

/// `text` in at most `width` characters, cut with `…`.
fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_owned()
    } else {
        let kept: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

/// The first 8 characters of an id: enough to tell rows apart.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}
