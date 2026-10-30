//! `wye service`: the gateway in the background, run by the operating
//! system's own service manager, as the user: a LaunchAgent (launchd) on
//! macOS, a systemd user unit on Linux. No sudo, and nothing outside the
//! home directory changes.
//!
//! Nothing happens unannounced: `install` shows the file it writes, what
//! runs, where the log goes, and how to undo it, and asks first; every
//! command a step runs is printed as it runs.

use std::{
    io::{BufRead, IsTerminal, Write},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use gateway_core::config::{Config, UpstreamCredential};

/// The service's launchd label.
const LABEL: &str = "dev.wye.gateway";
/// The systemd unit.
const UNIT: &str = "wye.service";
/// How long `install` and `restart` wait for the gateway to answer.
const START_WAIT: Duration = Duration::from_secs(5);

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
    /// Where the gateway's stderr, the usage log, is appended.
    pub log: PathBuf,
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
        let (program, config, log) = (text(&self.program)?, text(&self.config)?, text(&self.log)?);
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
                 \x20 <key>StandardErrorPath</key><string>{}</string>\n\
                 \x20 <key>ExitTimeOut</key><integer>{}</integer>\n\
                 </dict>\n</plist>\n",
                xml(&program),
                xml(&config),
                xml(&log),
                self.stop_secs
            ),
            Manager::Systemd => {
                for path in [&program, &config, &log] {
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
                     StandardError=append:{}\nTimeoutStopSec={}\n\n\
                     [Install]\nWantedBy=default.target\n",
                    escape(&program),
                    escape(&config),
                    escape(&log),
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
                let [label, program, serve, flag, config, log] = strings.as_slice() else {
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
                    log: line("StandardError=append:")?.into(),
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
type Commands = Vec<Vec<String>>;

fn launchctl(args: &[&str]) -> Vec<String> {
    std::iter::once("launchctl")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn systemctl(args: &[&str]) -> Vec<String> {
    ["systemctl", "--user"]
        .into_iter()
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect()
}

/// The commands that start the service from `file`; `replacing` a loaded
/// one stops it first.
fn start_commands(manager: Manager, uid: &str, file: &Path, replacing: bool) -> Commands {
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

fn restart_commands(manager: Manager, uid: &str) -> Commands {
    match manager {
        Manager::Launchd => vec![launchctl(&[
            "kickstart",
            "-k",
            &format!("gui/{uid}/{LABEL}"),
        ])],
        Manager::Systemd => vec![systemctl(&["restart", UNIT])],
    }
}

fn stop_commands(manager: Manager, uid: &str) -> Commands {
    match manager {
        Manager::Launchd => vec![launchctl(&["bootout", &format!("gui/{uid}/{LABEL}")])],
        Manager::Systemd => vec![systemctl(&["disable", "--now", UNIT])],
    }
}

/// The plan `install` shows before it asks.
fn plan(
    manager: Manager,
    service: &Service,
    file: &Path,
    replacing: bool,
    commands: &Commands,
    show: &dyn Fn(&Path) -> String,
) -> String {
    let mut out = super::args::wrap(
        &format!(
            "wye service install sets up the gateway as {}: {} No sudo; nothing outside \
             your home directory changes.",
            manager.kind(),
            manager.promise()
        ),
        0,
        super::args::WIDTH,
    );
    let verb = if replacing { "Replace" } else { "Write" };
    out.push_str(&format!(
        "\n  {verb:<8} {}\n  {:<8} {} serve --config {}\n  {:<8} {} (appended)\n",
        show(file),
        "Run",
        show(&service.program),
        show(&service.config),
        "Log",
        show(&service.log),
    ));
    for (i, command) in commands.iter().enumerate() {
        let label = if i == 0 { "Then" } else { "" };
        out.push_str(&format!("  {label:<8} {}\n", shell(command, show)));
    }
    out.push_str("\nUndo with `wye service uninstall`.\n");
    out
}

/// `command` as a shell line, home paths shortened.
fn shell(command: &[String], show: &dyn Fn(&Path) -> String) -> String {
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

/// Environment variables a config's secrets come from: a service does not
/// inherit the shell's environment, so they would be missing.
fn env_secrets(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = config
        .routes
        .upstreams()
        .filter_map(|u| match &u.credential {
            UpstreamCredential::ApiKeyEnv(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    names.extend(
        config
            .cache_diagnostics
            .as_ref()
            .map(|d| d.hash_key_env.clone()),
    );
    names.extend(
        config
            .telemetry
            .as_ref()
            .and_then(|t| t.otlp_headers_env.clone()),
    );
    names.sort();
    names.dedup();
    names
}

/// The config at `path`, checked as the service will start it: parsed,
/// with no secret in the environment, and every file it names readable.
/// Errors name the config, which may not be the one the user expected.
fn check(path: &Path, show: &dyn Fn(&Path) -> String) -> Result<Config, String> {
    let named = |e: String| format!("{}: {e}", show(path));
    let source =
        std::fs::read_to_string(path).map_err(|e| named(format!("cannot read it: {e}")))?;
    let config = Config::from_toml_str(&source).map_err(|e| named(e.to_string()))?;
    let names = env_secrets(&config);
    if !names.is_empty() {
        let list = names
            .iter()
            .map(|n| format!("${n}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(named(format!(
            "a service starts outside your shell, so it would not see {list}. Read \
             provider keys from files instead (api_key_file = \"anthropic.key\", next to \
             the config, readable only by you), as `wye init` writes them; for the cache \
             diagnostics key and OTLP headers, run `wye serve` under a service definition \
             of your own (docs/operations.md#running-as-a-service). Another config: -c <path>"
        )));
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    crate::startup::Resources::resolve(&config, dir).map_err(|e| named(e.to_string()))?;
    Ok(config)
}

/// Where the gateway answers its health check on this machine.
fn health_url(listen: SocketAddr) -> String {
    let ip = match listen.ip() {
        ip if ip.is_unspecified() && ip.is_ipv4() => Ipv4Addr::LOCALHOST.into(),
        ip if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    format!("http://{}/healthz", SocketAddr::new(ip, listen.port()))
}

/// What the service definition file holds.
enum Installed {
    None,
    Ours(Service),
    /// A definition `install` did not write, such as one made by hand from
    /// the docs: `install` offers to replace it; nothing else touches it.
    Foreign,
}

/// The paths and identity a step needs.
pub struct Context {
    pub manager: Manager,
    /// The user's home directory (`$HOME`).
    pub user_home: PathBuf,
    /// Wye's home directory, where the log goes.
    pub wye_home: PathBuf,
    pub show: Box<dyn Fn(&Path) -> String>,
}

impl Context {
    fn file(&self) -> PathBuf {
        self.manager.file(&self.user_home)
    }

    /// What the definition file holds.
    fn installed(&self) -> Result<Installed, String> {
        let file = self.file();
        match std::fs::read_to_string(&file) {
            Ok(text) => {
                Ok(Service::parse(self.manager, &text).map_or(Installed::Foreign, Installed::Ours))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Installed::None),
            Err(e) => Err(format!("cannot read {}: {e}", (self.show)(&file))),
        }
    }

    /// The service `install` wrote, or why there is none to act on.
    fn ours(&self) -> Result<Service, String> {
        match self.installed()? {
            Installed::Ours(service) => Ok(service),
            Installed::None => Err("not installed; `wye service install` sets it up".into()),
            Installed::Foreign => Err(format!(
                "{} was not written by wye service install, so it is left alone; \
                 `wye service install` offers to replace it",
                (self.show)(&self.file())
            )),
        }
    }
}

/// `wye service install`.
pub fn install(
    context: &Context,
    config_path: &Path,
    yes: bool,
    dry_run: bool,
) -> Result<(), String> {
    let show = &context.show;
    let config_path = std::path::absolute(config_path).map_err(|e| e.to_string())?;
    let config = check(&config_path, show)?;
    let program = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("cannot find this wye's path: {e}"))?;
    let service = Service {
        program,
        config: config_path,
        log: context.wye_home.join("usage.jsonl"),
        stop_secs: (config.server.drain_delay + config.server.shutdown_grace).as_secs() + 5,
    };
    let definition = service.definition(context.manager)?;
    let file = context.file();
    let installed = context.installed()?;
    let replacing = !matches!(installed, Installed::None);
    let health = health_url(config.server.listen);
    // A gateway that answers when none is installed is another one, such
    // as `wye serve` in a terminal: the service could not take its port.
    if !replacing && probe(&health).is_ok() {
        return Err(format!(
            "a gateway already answers at {} (a wye serve in a terminal?); stop it first",
            health.trim_end_matches("/healthz")
        ));
    }
    let uid = uid()?;
    let commands = start_commands(context.manager, &uid, &file, replacing);
    print!(
        "{}",
        plan(context.manager, &service, &file, replacing, &commands, show)
    );
    if matches!(installed, Installed::Foreign) {
        println!(
            "{} was not written by wye service install; it is replaced.",
            show(&file)
        );
    }
    if dry_run {
        println!("\n{}:\n\n{definition}", show(&file));
        return Ok(());
    }
    if !yes && !confirm(&file, &definition, show)? {
        println!("Nothing changed.");
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", show(dir)))?;
    }
    std::fs::create_dir_all(&context.wye_home)
        .map_err(|e| format!("cannot create {}: {e}", show(&context.wye_home)))?;
    write_atomically(&file, &definition)?;
    println!("Wrote {}", show(&file));
    run_all(&commands, show)?;
    wait_for(&health, &service.log, show)
}

/// Asks before installing; `[s]how` prints the file and asks again.
fn confirm(file: &Path, definition: &str, show: &dyn Fn(&Path) -> String) -> Result<bool, String> {
    if !std::io::stdin().is_terminal() {
        return Err(
            "not a terminal to ask on: install with --yes, or see the plan with \
                    --dry-run"
                .into(),
        );
    }
    loop {
        print!("Install? [y]es, [n]o, [s]how the file: ");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut answer = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "s" | "show" => println!("\n{}:\n\n{definition}", show(file)),
            _ => return Ok(false),
        }
    }
}

/// `wye service status`: whether it is installed and answering; `false`
/// when installed but not answering.
pub fn status(context: &Context) -> Result<bool, String> {
    let show = &context.show;
    let service = match context.installed()? {
        Installed::Ours(service) => service,
        Installed::None => {
            println!("Not installed. `wye service install` sets it up.");
            return Ok(true);
        }
        Installed::Foreign => {
            println!(
                "{} exists but was not written by wye service install; \
                 `wye service install` offers to replace it.",
                show(&context.file())
            );
            return Ok(true);
        }
    };
    let log_size = std::fs::metadata(&service.log)
        .map(|m| format!(" ({})", super::size(m.len())))
        .unwrap_or_default();
    println!(
        "{:<9} {}\n{:<9} {} serve --config {}\n{:<9} {}{log_size}",
        "Installed",
        show(&context.file()),
        "Runs",
        show(&service.program),
        show(&service.config),
        "Log",
        show(&service.log),
    );
    let answering = std::fs::read_to_string(&service.config)
        .ok()
        .and_then(|source| Config::from_toml_str(&source).ok())
        .map(|config| health_url(config.server.listen))
        .map(|url| (probe(&url).is_ok(), url));
    match answering {
        Some((true, url)) => {
            println!(
                "{:<9} answering at {}",
                "State",
                url.trim_end_matches("/healthz")
            );
            Ok(true)
        }
        Some((false, url)) => {
            println!(
                "{:<9} not answering at {}; the end of the log may say why",
                "State",
                url.trim_end_matches("/healthz")
            );
            Ok(false)
        }
        None => {
            println!(
                "{:<9} its config does not parse; `wye validate -c {}`",
                "State",
                show(&service.config)
            );
            Ok(false)
        }
    }
}

/// `wye service restart`: checks the installed config, then restarts.
pub fn restart(context: &Context) -> Result<(), String> {
    let show = &context.show;
    let service = context.ours()?;
    let config = check(&service.config, show)
        .map_err(|e| format!("{e}\nThe service keeps running as it was."))?;
    run_all(&restart_commands(context.manager, &uid()?), show)?;
    wait_for(&health_url(config.server.listen), &service.log, show)
}

/// `wye service uninstall`: stops the service and removes its definition.
pub fn uninstall(context: &Context) -> Result<(), String> {
    let show = &context.show;
    let file = context.file();
    let service = match context.installed()? {
        Installed::None => {
            println!("Not installed; nothing changed.");
            return Ok(());
        }
        _ => context.ours()?,
    };
    run_all(&stop_commands(context.manager, &uid()?), show)?;
    std::fs::remove_file(&file).map_err(|e| format!("cannot remove {}: {e}", show(&file)))?;
    println!("Removed {}", show(&file));
    if context.manager == Manager::Systemd {
        run_all(&vec![systemctl(&["daemon-reload"])], show)?;
    }
    println!(
        "The config {} and the log {} stay.",
        show(&service.config),
        show(&service.log)
    );
    Ok(())
}

/// Runs each command, printing it first; a `bootout` of a service that is
/// not loaded is not an error.
fn run_all(commands: &Commands, show: &dyn Fn(&Path) -> String) -> Result<(), String> {
    for command in commands {
        println!("  $ {}", shell(command, show));
        let (program, args) = command.split_first().ok_or("an empty command")?;
        // launchd may still be tearing the old service down after `bootout`.
        let attempts = if args.first().map(String::as_str) == Some("bootstrap") {
            10
        } else {
            1
        };
        let mut result = None;
        for _ in 0..attempts {
            let output = std::process::Command::new(program)
                .args(args)
                .output()
                .map_err(|e| format!("cannot run {program}: {e}"))?;
            if output.status.success() {
                result = Some(Ok(()));
                break;
            }
            result = Some(Err(String::from_utf8_lossy(&output.stderr)
                .trim()
                .to_owned()));
            std::thread::sleep(Duration::from_millis(300));
        }
        if let Some(Err(why)) = result
            && args.first().map(String::as_str) != Some("bootout")
        {
            return Err(format!("{} failed: {why}", shell(command, show)));
        }
    }
    Ok(())
}

/// Waits for the gateway to answer, and says so.
fn wait_for(health: &str, log: &Path, show: &dyn Fn(&Path) -> String) -> Result<(), String> {
    let deadline = std::time::Instant::now() + START_WAIT;
    let url = health.trim_end_matches("/healthz");
    while std::time::Instant::now() < deadline {
        if probe(health).is_ok() {
            println!("The gateway answers at {url}.");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "nothing answers at {url} yet; the end of {} may say why",
        show(log)
    ))
}

fn probe(url: &str) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(super::probe(url))
}

/// The user id, for launchd's `gui/<uid>` domain.
fn uid() -> Result<String, String> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| format!("cannot run id -u: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Writes `text` to `path` through a temporary file, so a half-written
/// definition is never loaded.
fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests;
