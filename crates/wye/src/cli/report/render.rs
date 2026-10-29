//! The report as text: totals, tables by request class and upstream, cache
//! misses, upgrade issues, failovers, tools, and the top sessions and prompts.

use std::fmt::Write as _;

use super::{Group, Summary, TOP};
use crate::cli::plural;

pub(super) fn render(s: &Summary) -> String {
    let mut out = String::new();
    if s.total.requests == 0 {
        let _ = writeln!(
            out,
            "no request records ({} skipped); the gateway writes one per request to \
             stderr: `wye serve --config wye.toml 2>>usage.jsonl`",
            plural(s.skipped, "other line", "other lines"),
        );
        return out;
    }
    // Without `[pricing]` nothing carries a cost; the column would read "-".
    let columns = Columns {
        cost: s.total.cost.priced > 0,
    };
    let mut outcomes: Vec<_> = s.outcomes.iter().collect();
    outcomes.sort();
    let _ = writeln!(
        out,
        "{} in {} ({} skipped); outcomes: {}",
        plural(s.total.requests, "request", "requests"),
        plural(s.sessions.len() as u64, "session", "sessions"),
        plural(s.skipped, "other line", "other lines"),
        outcomes
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let _ = writeln!(out, "\nBy request class");
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
        let _ = writeln!(out, "\nBy upstream");
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
            "\nFailovers: refusals that moved a request to the next upstream"
        );
        let _ = writeln!(
            out,
            "  {:24} {:>8} {:>10}",
            "upstream", "reason", "requests"
        );
        let mut failovers: Vec<_> = s.failovers.iter().collect();
        failovers.sort();
        for ((upstream, reason), count) in failovers {
            let _ = writeln!(out, "  {upstream:24} {reason:>8} {count:>10}");
        }
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

    if !s.upgrades.is_empty() {
        let _ = writeln!(
            out,
            "\nUpgrade issues: requests a target model would reject, and the clients that sent them"
        );
        let mut upgrades: Vec<_> = s.upgrades.iter().collect();
        upgrades.sort_by(|a, b| (b.1).0.cmp(&(a.1).0).then(a.0.cmp(b.0)));
        for ((target, rule), (requests, clients)) in upgrades {
            let mut clients = clients.clone();
            clients.sort();
            let _ = writeln!(
                out,
                "  {target:<24} {rule:<20} {requests:>7}  {}",
                clients.join(", ")
            );
        }
    }

    if !s.tools.is_empty() {
        let _ = writeln!(
            out,
            "\nTools: calls the model requested; calls timed by Claude Code, and their time"
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
            .map(|(name, _)| name.len())
            .fold(28, usize::max);
        let _ = writeln!(
            out,
            "  {:<width$} {:>9} {:>7} {:>10}",
            "tool", "requested", "timed", "time"
        );
        for (name, (timed, ms, requested)) in tools {
            let _ = writeln!(
                out,
                "  {name:<width$} {requested:>9} {timed:>7} {:>10}",
                seconds(*ms as f64)
            );
        }
    }

    let _ = writeln!(out, "\nTop sessions by tokens");
    columns.header(&mut out, "session (subject)");
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
        let label = match &session.subject {
            Some(subject) => format!("{} ({subject})", short(id)),
            None => short(id).to_owned(),
        };
        columns.row(&mut out, &label, &session.group);
        if session.compactions > 0 {
            let _ = writeln!(
                out,
                "    {}",
                plural(session.compactions, "compaction", "compactions")
            );
        }
    }

    if !s.prompts.is_empty() {
        let _ = writeln!(out, "\nTop prompts by tokens");
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

/// The columns of the token tables.
struct Columns {
    cost: bool,
}

impl Columns {
    fn header(&self, out: &mut String, first: &str) {
        let _ = write!(
            out,
            "  {first:<28} {:>7} {:>12} {:>10} {:>13} {:>13} {:>7}",
            "reqs", "input", "output", "cache read", "cache write", "hit"
        );
        if self.cost {
            let _ = write!(out, " {:>11}", "cost");
        }
        let _ = writeln!(out, " {:>10} {:>10}", "time", "tools");
    }

    fn row(&self, out: &mut String, name: &str, g: &Group) {
        let t = g.tokens;
        let _ = write!(
            out,
            "  {name:<28} {:>7} {:>12} {:>10} {:>13} {:>13} {:>7}",
            g.requests,
            t.input,
            t.output,
            t.cache_read,
            t.cache_write,
            t.hit_rate(),
        );
        if self.cost {
            let _ = write!(out, " {:>11}", g.cost.show());
        }
        let _ = writeln!(
            out,
            " {:>10} {:>10}",
            seconds(g.model_ms),
            seconds(g.tool_ms as f64)
        );
    }
}

fn seconds(ms: f64) -> String {
    format!("{:.1}s", ms / 1000.0)
}

/// The first 8 characters of an id: enough to tell rows apart.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}
