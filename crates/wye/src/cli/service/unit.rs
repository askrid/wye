//! The service definition: what `install` writes for each service manager,
//! how it reads one back, the commands that start, restart, and stop it,
//! and the plan `install` shows. Pure: no file is read and nothing runs.

use std::path::{Path, PathBuf};

/// The service's launchd label.
const LABEL: &str = "dev.wye.gateway";
/// The systemd unit.
pub(super) const UNIT: &str = "wye.service";
/// The service's open-file limit, soft and hard. Each client connection
/// takes a file descriptor, and so does its upstream connection while a
/// request is in flight: about twice `server.max_connections` (20,000 by
/// default). Without it, launchd starts the service with 256 and systemd
/// with 1024. launchd sets it even above `kern.maxfilesperproc` (122,880 on
/// a 2026 Mac), which still caps what the process opens; systemd's default
/// hard limit is 524,288.
pub(super) const OPEN_FILES: u32 = 65_536;
/// Width of the labels in the plan `install` shows and in `status`:
/// "Installed", "Usage log".
pub(super) const LABEL_WIDTH: usize = 9;
/// The service manager of this operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Launchd,
    Systemd,
}

impl Manager {
    /// This system's manager.
    pub fn here() -> Result<Self, String> {
        if cfg!(target_os = "macos") {
            Ok(Self::Launchd)
        } else if cfg!(target_os = "linux") {
            Ok(Self::Systemd)
        } else {
            Err(
                "wye service knows macOS (launchd) and Linux (systemd); elsewhere, run \
                 `wye serve` under your own service manager"
                    .into(),
            )
        }
    }

    /// What the definition is, in the manager's words.
    fn kind(self) -> &'static str {
        match self {
            Self::Launchd => "a macOS LaunchAgent",
            Self::Systemd => "a systemd user unit",
        }
    }

    /// Where the definition lives.
    pub fn file(self, user_home: &Path) -> PathBuf {
        match self {
            Self::Launchd => user_home
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
            Self::Systemd => user_home.join(".config/systemd/user").join(UNIT),
        }
    }

    /// What the manager does with the service, in one sentence.
    fn promise(self) -> &'static str {
        match self {
            Self::Launchd => {
                "launchd starts it now and at every login, as you, and restarts it if it stops."
            }
            Self::Systemd => {
                "systemd starts it now and when you log in, as you, and restarts it if it \
                 fails. It stops when you log out, unless lingering is on (loginctl \
                 enable-linger)."
            }
        }
    }
}

/// What the service runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub program: PathBuf,
    pub config: PathBuf,
    /// The usage log: the gateway's `--usage-log`, which it rotates. A
    /// definition an older wye wrote appends stdout to it instead.
    pub usage_log: PathBuf,
    /// The message log: where its stderr is appended, such as why it failed
    /// to start or stopped. Its stdout goes there too: with `--usage-log` it
    /// writes nothing there, and anything it ever did would be a message
    /// worth keeping beside the others, where `/dev/null` would lose it.
    pub messages: PathBuf,
    /// How long the manager waits for it to stop: the config's drain delay
    /// and shutdown grace, and a margin.
    pub stop_secs: u64,
}

impl Service {
    /// The definition file's text.
    pub fn definition(&self, manager: Manager) -> Result<String, String> {
        let text = |path: &Path| {
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))
        };
        let (program, config) = (text(&self.program)?, text(&self.config)?);
        let (log, messages) = (text(&self.usage_log)?, text(&self.messages)?);
        Ok(match manager {
            Manager::Launchd => format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
                 \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <!-- Written by wye service install; removed by wye service uninstall. -->\n\
                 <plist version=\"1.0\">\n<dict>\n\
                 \x20 <key>Label</key><string>{LABEL}</string>\n\
                 \x20 <key>ProgramArguments</key>\n\x20 <array>\n\
                 \x20   <string>{}</string>\n\x20   <string>serve</string>\n\
                 \x20   <string>--config</string>\n\x20   <string>{}</string>\n\
                 \x20   <string>--usage-log</string>\n\x20   <string>{}</string>\n\x20 </array>\n\
                 \x20 <key>RunAtLoad</key><true/>\n\x20 <key>KeepAlive</key><true/>\n\
                 \x20 <key>StandardOutPath</key><string>{messages}</string>\n\
                 \x20 <key>StandardErrorPath</key><string>{messages}</string>\n\
                 \x20 <key>ExitTimeOut</key><integer>{}</integer>\n\
                 \x20 <key>SoftResourceLimits</key>\n\x20 <dict>\n\
                 \x20   <key>NumberOfFiles</key><integer>{OPEN_FILES}</integer>\n\x20 </dict>\n\
                 \x20 <key>HardResourceLimits</key>\n\x20 <dict>\n\
                 \x20   <key>NumberOfFiles</key><integer>{OPEN_FILES}</integer>\n\x20 </dict>\n\
                 </dict>\n</plist>\n",
                xml(&program),
                xml(&config),
                xml(&log),
                self.stop_secs,
                messages = xml(&messages),
            ),
            Manager::Systemd => {
                for path in [&program, &config, &log, &messages] {
                    if path.contains(|c: char| c.is_whitespace() || "\"'\\$".contains(c)) {
                        return Err(format!(
                            "{path}: a path with spaces, quotes, or $; systemd needs one without"
                        ));
                    }
                }
                // `%` starts a systemd specifier.
                let escape = |s: &str| s.replace('%', "%%");
                format!(
                    "# Written by wye service install; removed by wye service uninstall.\n\
                     [Unit]\nDescription=Wye gateway\n\n\
                     [Service]\nExecStart={} serve --config {} --usage-log {}\n\
                     Restart=on-failure\n\
                     StandardOutput=append:{messages}\nStandardError=append:{messages}\n\
                     TimeoutStopSec={}\nLimitNOFILE={OPEN_FILES}\n\n\
                     [Install]\nWantedBy=default.target\n",
                    escape(&program),
                    escape(&config),
                    escape(&log),
                    self.stop_secs,
                    messages = escape(&messages),
                )
            }
        })
    }

    /// What a definition `install` wrote runs; `None` for one it did not
    /// write. One an older wye wrote is still ours, and `restart` rewrites a
    /// definition whose text differs: without `--usage-log`, the gateway's
    /// stdout was the usage log; the open-file limit is not read back, as
    /// every definition this wye writes has the same one.
    pub fn parse(manager: Manager, text: &str) -> Option<Self> {
        match manager {
            Manager::Launchd => {
                let strings: Vec<String> = text
                    .split("<string>")
                    .skip(1)
                    .filter_map(|s| s.split_once("</string>").map(|(v, _)| unxml(v)))
                    .collect();
                let (label, program, serve, flag, config, log, messages) = match strings.as_slice()
                {
                    [
                        label,
                        program,
                        serve,
                        flag,
                        config,
                        log_flag,
                        log,
                        _,
                        messages,
                    ] if log_flag == "--usage-log" => {
                        (label, program, serve, flag, config, log, messages)
                    }
                    [label, program, serve, flag, config, log, messages] => {
                        (label, program, serve, flag, config, log, messages)
                    }
                    _ => return None,
                };
                if label != LABEL || serve != "serve" || flag != "--config" {
                    return None;
                }
                let stop_secs = text
                    .split_once("<key>ExitTimeOut</key>")?
                    .1
                    .split_once("<integer>")?
                    .1
                    .split_once("</integer>")?
                    .0
                    .parse()
                    .ok()?;
                Some(Self {
                    program: program.into(),
                    config: config.into(),
                    usage_log: log.into(),
                    messages: messages.into(),
                    stop_secs,
                })
            }
            Manager::Systemd => {
                let line = |key: &str| {
                    text.lines()
                        .find_map(|l| l.strip_prefix(key))
                        .map(|v| v.replace("%%", "%"))
                };
                let exec = line("ExecStart=")?;
                let words: Vec<&str> = exec.split(' ').collect();
                let (program, config, log) = match words.as_slice() {
                    [program, "serve", "--config", config, "--usage-log", log] => {
                        (program, config, (*log).to_owned())
                    }
                    [program, "serve", "--config", config] => {
                        (program, config, line("StandardOutput=append:")?)
                    }
                    _ => return None,
                };
                Some(Self {
                    program: program.into(),
                    config: config.into(),
                    usage_log: log.into(),
                    messages: line("StandardError=append:")?.into(),
                    stop_secs: line("TimeoutStopSec=")?.parse().ok()?,
                })
            }
        }
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unxml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The commands a step runs, each as program and arguments.
pub(super) type Commands = Vec<Vec<String>>;

fn launchctl(args: &[&str]) -> Vec<String> {
    std::iter::once("launchctl")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect()
}

pub(super) fn systemctl(args: &[&str]) -> Vec<String> {
    ["systemctl", "--user"]
        .into_iter()
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect()
}

/// The commands that start the service from `file`; `replacing` a loaded
/// one stops it first.
pub(super) fn start_commands(
    manager: Manager,
    uid: &str,
    file: &Path,
    replacing: bool,
) -> Commands {
    let target = format!("gui/{uid}/{LABEL}");
    let domain = format!("gui/{uid}");
    let file = file.display().to_string();
    match manager {
        Manager::Launchd if replacing => vec![
            launchctl(&["bootout", &target]),
            launchctl(&["bootstrap", &domain, &file]),
        ],
        Manager::Launchd => vec![launchctl(&["bootstrap", &domain, &file])],
        Manager::Systemd => vec![
            systemctl(&["daemon-reload"]),
            systemctl(&["enable", UNIT]),
            systemctl(&["restart", UNIT]),
        ],
    }
}

pub(super) fn restart_commands(manager: Manager, uid: &str) -> Commands {
    match manager {
        Manager::Launchd => vec![launchctl(&[
            "kickstart",
            "-k",
            &format!("gui/{uid}/{LABEL}"),
        ])],
        Manager::Systemd => vec![systemctl(&["restart", UNIT])],
    }
}

pub(super) fn stop_commands(manager: Manager, uid: &str) -> Commands {
    match manager {
        Manager::Launchd => vec![launchctl(&["bootout", &format!("gui/{uid}/{LABEL}")])],
        Manager::Systemd => vec![systemctl(&["disable", "--now", UNIT])],
    }
}

/// The plan `install` shows before it asks.
pub(super) fn plan(
    manager: Manager,
    service: &Service,
    file: &Path,
    replacing: bool,
    commands: &Commands,
    show: &dyn Fn(&Path) -> String,
) -> String {
    let mut out = crate::cli::text::wrap(
        &format!(
            "wye service install sets up the gateway as {}: {} No sudo; nothing outside \
             your home directory changes.",
            manager.kind(),
            manager.promise()
        ),
        0,
    );
    let verb = if replacing { "Replace" } else { "Write" };
    out.push_str(&format!(
        "\n  {verb:<LABEL_WIDTH$} {}\n  {:<LABEL_WIDTH$} {} serve --config {}\n  \
         {:<LABEL_WIDTH$} {} (appended)\n  {:<LABEL_WIDTH$} {} (appended)\n",
        show(file),
        "Run",
        show(&service.program),
        show(&service.config),
        "Usage log",
        show(&service.usage_log),
        "Messages",
        show(&service.messages),
    ));
    for (i, command) in commands.iter().enumerate() {
        let label = if i == 0 { "Then" } else { "" };
        out.push_str(&format!(
            "  {label:<LABEL_WIDTH$} {}\n",
            shell(command, show)
        ));
    }
    out.push_str("\nUndo with `wye service uninstall`.\n");
    out
}

/// The plan `uninstall` shows before it asks: the commands that `stop` the
/// service, the file it removes, the commands that `reload` the manager
/// after, and the files that stay.
pub(super) fn uninstall_plan(
    service: &Service,
    file: &Path,
    stop: &Commands,
    reload: &Commands,
    show: &dyn Fn(&Path) -> String,
) -> String {
    let mut out = crate::cli::text::wrap(
        "wye service uninstall stops the background gateway and removes its service \
         definition; your files stay.",
        0,
    );
    out.push('\n');
    let mut row = |label: &str, text: String| {
        out.push_str(&format!("  {label:<LABEL_WIDTH$} {text}\n"));
    };
    for (i, command) in stop.iter().enumerate() {
        row(if i == 0 { "Stop" } else { "" }, shell(command, show));
    }
    row("Remove", show(file));
    for (i, command) in reload.iter().enumerate() {
        row(if i == 0 { "Then" } else { "" }, shell(command, show));
    }
    row("Keep", format!("{} (the config)", show(&service.config)));
    row(
        "",
        format!(
            "{} (the usage log and its rotated files)",
            show(&service.usage_log)
        ),
    );
    row("", format!("{} (the message log)", show(&service.messages)));
    out
}

/// `command` as a shell line, home paths shortened.
pub(super) fn shell(command: &[String], show: &dyn Fn(&Path) -> String) -> String {
    command
        .iter()
        .map(|arg| {
            if arg.starts_with('/') {
                show(Path::new(arg))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
