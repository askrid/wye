//! The commands: every command's options, summary, and notes, and how help
//! groups them. The parser and the help both read these tables.
//!
//! A flag one command shares with another is defined here once, so it reads
//! the same everywhere (docs/development.md, "Command-line conventions").

use super::{Command, Id, Opt};

/// `-c, --config <path>`: the config a command reads, or `init` creates.
const CONFIG: Opt = Opt {
    short: Some('c'),
    ..Opt::value(
        "config",
        "path",
        "The config (default: $WYE_CONFIG, else $WYE_HOME/wye.toml)",
    )
};

/// `--url <url>`: the gateway a client uses.
const URL: Opt = Opt::value(
    "url",
    "url",
    concat!(
        "The gateway (default: $WYE_URL, else http://",
        gateway_core::default_listen!(),
        ")"
    ),
);

/// `--token-file <path>`: the gateway token a client presents.
const TOKEN_FILE: Opt = Opt::value(
    "token-file",
    "path",
    "Read the gateway token from this file (default: the token in $WYE_TOKEN)",
);

/// `-y, --yes`, on a command that asks before it changes something.
const YES: Opt = Opt {
    short: Some('y'),
    ..Opt::flag("yes", "Do it without asking")
};

/// `--dry-run`, on a command that asks before it changes something.
const DRY_RUN: Opt = Opt::flag("dry-run", "Show what it would change; change nothing");

/// The options of `wye claude` and `wye codex`.
const WRAPPER: [Opt; 2] = [URL, TOKEN_FILE];

/// The notes of `wye claude` and `wye codex`.
const WRAPPER_NOTES: &str = "Without a token the agent keeps its own login. Arguments \
    after `--`, or from the first one wye does not know, go to the agent unchanged. A \
    gateway beyond this machine must be https.";

/// A command with no arguments, options, or notes.
const BASE: Command = Command {
    id: Id::Serve,
    name: "",
    summary: "",
    args: "",
    arity: (0, 0),
    options: &[],
    passthrough: false,
    notes: "",
    missing: "",
};

/// The commands, in the order help lists them.
pub const COMMANDS: &[Command] = &[
    Command {
        id: Id::Serve,
        name: "serve",
        summary: "Run the gateway in the foreground",
        options: &[
            CONFIG,
            Opt::value(
                "usage-log",
                "path",
                "Append usage records to this file, not stdout (default: stdout)",
            ),
        ],
        notes: "One JSON usage record per request on stdout, or in the --usage-log file, \
                which at 64 MiB is renamed beside itself with the UTC time and never \
                deleted. Messages go to stderr. `wye service install` passes \
                --usage-log $WYE_HOME/usage.jsonl.",
        ..BASE
    },
    Command {
        id: Id::ServiceInstall,
        name: "service install",
        summary: "Run the gateway in the background, now and at every login",
        options: &[CONFIG, YES, DRY_RUN],
        notes: "On macOS a LaunchAgent (launchd), on Linux a systemd user unit: both run \
                as you, without sudo. Shows what it writes and runs, and asks first; \
                --dry-run also shows the file.",
        ..BASE
    },
    Command {
        id: Id::ServiceStatus,
        name: "service status",
        summary: "Show whether the background gateway is installed and answering",
        ..BASE
    },
    Command {
        id: Id::ServiceRestart,
        name: "service restart",
        summary: "Restart the background gateway, after checking its config",
        notes: "Run it after editing the config or installing a new wye. It rewrites the \
                service definition first when this wye or the config's drain and grace call \
                for a new one.",
        ..BASE
    },
    Command {
        id: Id::ServiceUninstall,
        name: "service uninstall",
        summary: "Stop the background gateway and remove it",
        options: &[YES, DRY_RUN],
        notes: "Shows what it stops and removes, and asks first. The config, the usage \
                log, and the message log stay.",
        ..BASE
    },
    Command {
        id: Id::Doctor,
        name: "doctor",
        summary: "Check a config's secrets, files, and upstreams",
        options: &[CONFIG],
        ..BASE
    },
    Command {
        id: Id::Claude,
        name: "claude",
        summary: "Start Claude Code through the gateway",
        args: "[<claude args>...]",
        arity: (0, usize::MAX),
        options: &WRAPPER,
        passthrough: true,
        notes: WRAPPER_NOTES,
        ..BASE
    },
    Command {
        id: Id::Codex,
        name: "codex",
        summary: "Start Codex through the gateway",
        args: "[<codex args>...]",
        arity: (0, usize::MAX),
        options: &WRAPPER,
        passthrough: true,
        notes: WRAPPER_NOTES,
        ..BASE
    },
    Command {
        id: Id::Report,
        name: "report",
        summary: "Summarize a usage log",
        args: "[<usage-log>...]",
        arity: (0, usize::MAX),
        options: &[Opt::value(
            "since",
            "when",
            concat!(
                "Records from then on: a time ago (",
                gateway_core::duration_syntax!(),
                ") or a UTC date (2026-11-01)"
            ),
        )],
        notes: "Reads $WYE_HOME/usage.jsonl, the usage log `wye service install` keeps, and \
                its rotated files (usage-*.jsonl), unless given others; `-` reads \
                standard input, as in `cat replica-*.jsonl | wye report -`.",
        ..BASE
    },
    Command {
        id: Id::Init,
        name: "init",
        summary: "Write a starter config and policy tests",
        options: &[
            CONFIG,
            Opt::flag(
                "provider-key",
                "The gateway holds an Anthropic API key, in a file (still this machine only)",
            ),
            Opt::flag(
                "shared",
                "Reachable from other machines: gateway tokens, a signing key, the provider \
                 key from the environment",
            ),
            Opt::flag("openai", "Also serve the OpenAI APIs (for Codex)"),
        ],
        notes: "Two choices: who can reach the gateway (this machine, or --shared) and \
                whose credential pays (the caller's, or --provider-key). By default, \
                for you on this machine: Claude Code's login, or any API key a caller \
                sends, goes through, and the gateway holds no secret. A shared \
                gateway's config belongs in the repository it is reviewed in: -c wye.toml \
                there. Sample policy tests go in policy-tests, next to the config, for \
                `wye policy-test`. Never overwrites a file.",
        ..BASE
    },
    Command {
        id: Id::TokenKeygen,
        name: "token keygen",
        summary: "Make a signing key for gateway tokens",
        options: &[Opt::value(
            "out",
            "path",
            "Write the key to this new file, readable only by you (default: \
             $WYE_HOME/signing.key)",
        )],
        notes: "Prints the line to put under [auth]: signing_keys with its public half. \
                `wye init --shared` makes the first key; this one is for rotation.",
        ..BASE
    },
    Command {
        id: Id::TokenIssue,
        name: "token issue",
        summary: "Issue a gateway token",
        args: "<subject>",
        arity: (1, 1),
        options: &[
            CONFIG,
            Opt::value(
                "signing-key",
                "path",
                "The signing key (default: $WYE_HOME/signing.key)",
            ),
            Opt::value(
                "ttl",
                "duration",
                concat!(
                    "Lifetime: ",
                    gateway_core::duration_syntax!(),
                    " (default: the config's longest)"
                ),
            ),
            Opt::value(
                "models",
                "list",
                "Models it may use: names, or prefixes ending in *",
            ),
            Opt::value("max-output-tokens", "n", "A lower output-token ceiling"),
            Opt::value(
                "out",
                "path",
                "Write the token to this new file, readable only by you (default: stdout)",
            ),
        ],
        notes: "The issuer, audience, and key id come from the config's [auth], the key id \
                found by the signing key's public half.",
        missing: "who the token is for, as in `wye token issue alice --out alice.token`",
        ..BASE
    },
    Command {
        id: Id::Explain,
        name: "explain",
        summary: "Show what the gateway would do with one request",
        args: "<request.json>",
        // A missing request is named by `explain` itself, which can point at a
        // sample next to the config.
        arity: (0, 1),
        options: &[
            CONFIG,
            TOKEN_FILE,
            Opt::value("session", "id", "The client's session id"),
            Opt::value("endpoint", "path", "The endpoint (default: /v1/messages)"),
            Opt {
                short: Some('H'),
                repeats: true,
                ..Opt::value(
                    "header",
                    "'name: value'",
                    "A request header (default: a local client's Host, the listen address)",
                )
            },
            Opt {
                repeats: true,
                ..Opt::value(
                    "target-model",
                    "model",
                    "Check against this model instead of [upgrade_check]",
                )
            },
        ],
        notes: "Offline: calls what the server calls. `-` reads the request from standard \
                input.",
        ..BASE
    },
    Command {
        id: Id::PolicyTest,
        name: "policy-test",
        summary: "Check sample requests against expected decisions",
        args: "[<dir>]",
        arity: (0, 1),
        options: &[CONFIG],
        notes: "Each <name>.json is a request and <name>.expect.toml its decision. The \
                default directory is policy-tests, next to the config, where `wye init` \
                writes samples. It reads the identity providers' key sets too, as the \
                gateway does, and no secret, so CI needs no credentials.",
        ..BASE
    },
    Command {
        id: Id::PolicyDiff,
        name: "policy-diff",
        summary: "Show what a config change widens or narrows",
        args: "<old> <new>",
        arity: (2, 2),
        missing: "the configs before and after a change, as in \
                  `wye policy-diff ../base/wye.toml wye.toml` after \
                  `git worktree add ../base main`",
        ..BASE
    },
    Command {
        id: Id::Probe,
        name: "probe",
        summary: "Check that a gateway or URL answers 2xx",
        args: "[<url>]",
        arity: (0, 1),
        notes: "The default is the gateway `wye claude` would use: $WYE_URL, else this \
                machine's. A URL without a path names a gateway, and asks its /healthz; \
                give /readyz for a load balancer's view. For images without curl.",
        ..BASE
    },
];

/// Commands with subcommands, as the command list shows them.
pub(super) const FAMILIES: &[(&str, &str)] = &[
    (
        "service",
        "Run the gateway in the background (launchd or systemd, as you)",
    ),
    ("token", "Make signing keys and gateway tokens"),
];

/// The command list, grouped.
pub(super) const GROUPS: &[(&str, &[&str])] = &[
    ("Run", &["serve", "service"]),
    ("Use", &["claude", "codex", "report"]),
    ("Set up", &["init", "token"]),
    (
        "Check",
        &["doctor", "explain", "policy-test", "policy-diff", "probe"],
    ),
];
