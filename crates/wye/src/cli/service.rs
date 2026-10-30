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

mod unit;

#[cfg(test)]
use unit::UNIT;
use unit::{Commands, plan, restart_commands, shell, start_commands, stop_commands, systemctl};
pub use unit::{Manager, Service};

/// How long `install` and `restart` wait for the gateway to answer.
const START_WAIT: Duration = Duration::from_secs(5);

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
        .map(|m| format!(" ({})", file_size(m.len())))
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

/// A file's size as people read it: `913 KB`, `4.2 MB`.
fn file_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / f64::from(1u32 << 30)),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / f64::from(1u32 << 20)),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} bytes"),
    }
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
