//! The service definition: what `install` writes for each service manager,
//! how it reads one back, the commands that start, restart, and stop it,
//! and the plan `install` shows. Pure: no file is read and nothing runs.

use std::path::{Path, PathBuf};

/// The service's launchd label.
const LABEL: &str = "dev.wye.gateway";
/// The systemd unit.
pub(super) const UNIT: &str = "wye.service";
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
    /// Where the gateway's stdout, the usage log, is appended.
    pub log: PathBuf,
    /// Where its stderr is appended: why it failed to start or stopped.
    pub errors: PathBuf,
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
        let (log, errors) = (text(&self.log)?, text(&self.errors)?);
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
                 \x20   <string>--config</string>\n\x20   <string>{}</string>\n\x20 </array>\n\
                 \x20 <key>RunAtLoad</key><true/>\n\x20 <key>KeepAlive</key><true/>\n\
                 \x20 <key>StandardOutPath</key><string>{}</string>\n\
                 \x20 <key>StandardErrorPath</key><string>{}</string>\n\
                 \x20 <key>ExitTimeOut</key><integer>{}</integer>\n\
                 </dict>\n</plist>\n",
                xml(&program),
                xml(&config),
                xml(&log),
                xml(&errors),
                self.stop_secs
            ),
            Manager::Systemd => {
                for path in [&program, &config, &log, &errors] {
                    if path.contains(|c: char| c.is_whitespace() || c == '"' || c == '\\') {
                        return Err(format!(
                            "{path}: a path with spaces or quotes; systemd needs one without"
                        ));
                    }
                }
                // `%` starts a systemd specifier.
                let escape = |s: &str| s.replace('%', "%%");
                format!(
                    "# Written by wye service install; removed by wye service uninstall.\n\
                     [Unit]\nDescription=Wye gateway\n\n\
                     [Service]\nExecStart={} serve --config {}\nRestart=on-failure\n\
                     StandardOutput=append:{}\nStandardError=append:{}\nTimeoutStopSec={}\n\n\
                     [Install]\nWantedBy=default.target\n",
                    escape(&program),
                    escape(&config),
                    escape(&log),
                    escape(&errors),
                    self.stop_secs
                )
            }
        })
    }

    /// What a definition `install` wrote runs; `None` for one it did not
    /// write.
    pub fn parse(manager: Manager, text: &str) -> Option<Self> {
        match manager {
            Manager::Launchd => {
                let strings: Vec<String> = text
                    .split("<string>")
                    .skip(1)
                    .filter_map(|s| s.split_once("</string>").map(|(v, _)| unxml(v)))
                    .collect();
                let [label, program, serve, flag, config, log, errors] = strings.as_slice() else {
                    return None;
                };
                if label != LABEL || serve != "serve" || flag != "--config" {
                    return None;
                }
                let stop_secs = text
                    .split_once("<integer>")?
                    .1
                    .split_once("</integer>")?
                    .0
                    .parse()
                    .ok()?;
                Some(Self {
                    program: program.into(),
                    config: config.into(),
                    log: log.into(),
                    errors: errors.into(),
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
                let mut words = exec.split(' ');
                let (program, serve, flag, config) =
                    (words.next()?, words.next()?, words.next()?, words.next()?);
                if serve != "serve" || flag != "--config" || words.next().is_some() {
                    return None;
                }
                Some(Self {
                    program: program.into(),
                    config: config.into(),
                    log: line("StandardOutput=append:")?.into(),
                    errors: line("StandardError=append:")?.into(),
                    stop_secs: line("TimeoutStopSec=")?.parse().ok()?,
                })
            }
        }
    }
}

pub(super) fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(super) fn unxml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The commands a step runs, each as program and arguments.
pub(super) type Commands = Vec<Vec<String>>;

pub(super) fn launchctl(args: &[&str]) -> Vec<String> {
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
    let mut out = crate::cli::args::wrap(
        &format!(
            "wye service install sets up the gateway as {}: {} No sudo; nothing outside \
             your home directory changes.",
            manager.kind(),
            manager.promise()
        ),
        0,
        crate::cli::args::WIDTH,
    );
    let verb = if replacing { "Replace" } else { "Write" };
    out.push_str(&format!(
        "\n  {verb:<8} {}\n  {:<8} {} serve --config {}\n  {:<8} {} (appended)\n  {:<8} {} (appended)\n",
        show(file),
        "Run",
        show(&service.program),
        show(&service.config),
        "Log",
        show(&service.log),
        "Errors",
        show(&service.errors),
    ));
    for (i, command) in commands.iter().enumerate() {
        let label = if i == 0 { "Then" } else { "" };
        out.push_str(&format!("  {label:<8} {}\n", shell(command, show)));
    }
    out.push_str("\nUndo with `wye service uninstall`.\n");
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
