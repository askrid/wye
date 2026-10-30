//! The commands: every command's options, summary, and notes, and how help
//! groups them. The parser and the help both read these tables.

use super::{Command, Opt};

/// `-c, --config <path>`, on every command that reads a config.
const CONFIG: Opt = Opt {
    long: "config",
    short: Some('c'),
    value: Some("path"),
    help: "The config (default: $WYE_CONFIG, else ~/.wye/wye.toml)",
    repeats: false,
};

/// The commands, in the order help lists them.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "serve",
        summary: "Run the gateway in the foreground",
        args: "",
        arity: (0, 0),
        options: &[CONFIG],
        passthrough: false,
        notes: "One JSON usage record per request on stdout; messages on stderr. Keep them \
                with `wye serve >> ~/.wye/usage.jsonl`.",
    },
    Command {
        name: "service install",
        summary: "Run the gateway in the background, now and at every login",
        args: "",
        arity: (0, 0),
        options: &[
            CONFIG,
            Opt {
                short: Some('y'),
                ..Opt::flag("yes", "Install without asking")
            },
            Opt::flag("dry-run", "Show the plan and the file; change nothing"),
        ],
        passthrough: false,
        notes: "On macOS a LaunchAgent (launchd), on Linux a systemd user unit: both run \
                as you, without sudo. Shows what it writes and runs, and asks first.",
    },
    Command {
        name: "service status",
        summary: "Show whether the background gateway is installed and answering",
        args: "",
        arity: (0, 0),
        options: &[],
        passthrough: false,
        notes: "",
    },
    Command {
        name: "service restart",
        summary: "Restart the background gateway, after checking its config",
        args: "",
        arity: (0, 0),
        options: &[],
        passthrough: false,
        notes: "Run it after editing the config or installing a new wye.",
    },
    Command {
        name: "service uninstall",
        summary: "Stop the background gateway and remove it",
        args: "",
        arity: (0, 0),
        options: &[],
        passthrough: false,
        notes: "The config and the usage log stay.",
    },
    Command {
        name: "validate",
        summary: "Check a config without starting the gateway",
        args: "",
        arity: (0, 0),
        options: &[CONFIG],
        passthrough: false,
        notes: "Reads the identity providers' key sets too; secrets in the environment are \
                left to `wye doctor`.",
    },
    Command {
        name: "doctor",
        summary: "Check a config's keys, files, and endpoints",
        args: "",
        arity: (0, 0),
        options: &[CONFIG],
        passthrough: false,
        notes: "",
    },
    Command {
        name: "claude",
        summary: "Start Claude Code through the gateway",
        args: "[<claude args>...]",
        arity: (0, usize::MAX),
        options: &WRAPPER,
        passthrough: true,
        notes: WRAPPER_NOTES,
    },
    Command {
        name: "codex",
        summary: "Start Codex through the gateway",
        args: "[<codex args>...]",
        arity: (0, usize::MAX),
        options: &WRAPPER,
        passthrough: true,
        notes: WRAPPER_NOTES,
    },
    Command {
        name: "report",
        summary: "Summarize a usage log",
        args: "[<log>]",
        arity: (0, 1),
        options: &[],
        passthrough: false,
        notes: "Reads ~/.wye/usage.jsonl, the log `wye service install` keeps, unless \
                given a file; `-` reads standard input.",
    },
    Command {
        name: "init",
        summary: "Write a starter config",
        args: "",
        arity: (0, 0),
        options: &[
            Opt::flag(
                "api-key",
                "The gateway holds an Anthropic API key, in a file",
            ),
            Opt::flag(
                "team",
                "A shared gateway: tokens, a signing key, listens on all interfaces",
            ),
            Opt::flag("openai", "Also serve the OpenAI APIs (for Codex)"),
            Opt::value(
                "out",
                "path",
                "Where to write it (default: ~/.wye/wye.toml; with --team, ./wye.toml)",
            ),
        ],
        passthrough: false,
        notes: "By default, for you on this machine: Claude Code's login, or any API key a \
                caller sends, goes through, and the gateway holds no secret. Never \
                overwrites a file.",
    },
    Command {
        name: "token keygen",
        summary: "Make a signing key for gateway tokens",
        args: "",
        arity: (0, 0),
        options: &[Opt::value(
            "out",
            "path",
            "Where to write it (default: ~/.wye/issuer.key)",
        )],
        passthrough: false,
        notes: "Prints the public half for [[auth.keys]]. `wye init --team` makes the first \
                key; this one is for rotation.",
    },
    Command {
        name: "token issue",
        summary: "Issue a gateway token",
        args: "<subject>",
        arity: (1, 1),
        options: &[
            CONFIG,
            Opt::value("key", "path", "Signing key (default: ~/.wye/issuer.key)"),
            Opt::value(
                "ttl",
                "duration",
                "Lifetime: 30m, 8h, 7d (default: the config's longest)",
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
                "Write it to a new file only you can read (default: stdout)",
            ),
        ],
        passthrough: false,
        notes: "The issuer, audience, and key id come from the config's [auth], the key id \
                found by the signing key's public half.",
    },
    Command {
        name: "explain",
        summary: "Show what the gateway would do with one request",
        args: "<request.json>",
        arity: (1, 1),
        options: &[
            CONFIG,
            Opt::value("token-file", "path", "The caller's gateway token"),
            Opt::value("session", "id", "The Claude Code session id"),
            Opt::value("path", "path", "The endpoint (default: /v1/messages)"),
            Opt {
                repeats: true,
                ..Opt::value(
                    "target-model",
                    "model",
                    "Check against this model's breaking changes too",
                )
            },
        ],
        passthrough: false,
        notes: "Offline: calls what the server calls. `-` reads the request from standard \
                input.",
    },
    Command {
        name: "policy-test",
        summary: "Check sample requests against expected decisions",
        args: "[<dir>]",
        arity: (0, 1),
        options: &[CONFIG],
        passthrough: false,
        notes: "Each <name>.json is a request and <name>.expect.toml its decision. The \
                default directory is policy-tests, next to the config.",
    },
    Command {
        name: "policy-diff",
        summary: "Show what a config change widens or narrows",
        args: "<old> <new>",
        arity: (2, 2),
        options: &[],
        passthrough: false,
        notes: "",
    },
    Command {
        name: "probe",
        summary: "Check that a gateway or URL answers 2xx",
        args: "[<url>]",
        arity: (0, 1),
        options: &[],
        passthrough: false,
        notes: "The default is the gateway `wye claude` would use: $WYE_URL, else this \
                machine's. A URL without a path names a gateway, and asks its /healthz; \
                give /readyz for a load balancer's view. For images without curl.",
    },
];

const WRAPPER: [Opt; 2] = [
    Opt::value(
        "url",
        "url",
        "The gateway (default: $WYE_URL, else http://127.0.0.1:8080)",
    ),
    Opt::value(
        "token-file",
        "path",
        "A file holding a gateway token (default: $WYE_TOKEN)",
    ),
];

const WRAPPER_NOTES: &str = "Without a token the agent keeps its own login. Arguments \
    after `--`, or from the first one wye does not know, go to the agent unchanged. A \
    gateway beyond this machine must be https.";

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
        &[
            "validate",
            "doctor",
            "explain",
            "policy-test",
            "policy-diff",
            "probe",
        ],
    ),
];
