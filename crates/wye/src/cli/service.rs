//! `wye service`: the gateway in the background, run by the operating
//! system's own service manager, as the user: a LaunchAgent (launchd) on
//! macOS, a systemd user unit on Linux. No sudo, and nothing outside the
//! home directory changes.
//!
//! Nothing happens unannounced: `install` shows the file it writes, what
//! runs, where the log goes, and how to undo it, and `uninstall` what it
//! stops, removes, and keeps; both ask first. That plan is a command's
//! result, on stdout; every command a step runs is printed to stderr as it
//! runs.

use std::{
    io::{BufRead, IsTerminal},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use gateway_core::config::Config;

use super::{
    Env, Error,
    files::{self, MESSAGE_LOG_FILE, USAGE_LOG_FILE},
    paint::Paint,
    probe_blocking as probe, say,
};
use crate::startup::env_secrets;

mod run;
mod unit;

#[cfg(test)]
use run::retry;
use run::{run_all, uid, wait_for, write_atomically};
#[cfg(test)]
use unit::UNIT;
use unit::{
    Commands, LABEL_WIDTH, plan, restart_commands, shell, start_commands, stop_commands, systemctl,
    uninstall_plan,
};
pub use unit::{Manager, Service};

/// How long the gateway may take to answer once started.
const START_WAIT: Duration = Duration::from_secs(5);

/// The config at `path`, checked as the service will start it: parsed,
/// with no secret in the environment, and every file it names readable.
/// Errors name the config, which may not be the one the user expected.
fn check(path: &Path, show: &dyn Fn(&Path) -> String) -> Result<Config, String> {
    let named = |e: String| format!("{}: {e}", show(path));
    let config = files::load(path)?;
    let names = env_secrets(&config);
    if !names.is_empty() {
        let list = names
            .iter()
            .map(|n| format!("${n}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(named(format!(
            "a service starts outside your shell, so it would not see {list}. Read \
             each secret from a file instead, next to the config and readable only by \
             you, as `wye init` writes them: provider_key = {{ file = \"anthropic.key\" }}, \
             and the same {{ file = \"...\" }} for hash_key and otlp_headers. Another \
             config: -c <path>"
        )));
    }
    crate::startup::Resources::resolve(&config, files::config_dir(path))
        .map_err(|e| named(e.to_string()))?;
    Ok(config)
}

/// The URL of a gateway listening on `listen`, from this machine.
fn local_url(listen: SocketAddr) -> String {
    let ip = match listen.ip() {
        ip if ip.is_unspecified() && ip.is_ipv4() => Ipv4Addr::LOCALHOST.into(),
        ip if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    format!("http://{}", SocketAddr::new(ip, listen.port()))
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
    /// The context of whoever runs this command.
    pub fn new(env: impl Env + 'static) -> Result<Self, String> {
        let user_home = env.var("HOME").ok_or("no home directory ($HOME)")?;
        Ok(Self {
            manager: Manager::here()?,
            user_home: user_home.into(),
            wye_home: files::home(&env).ok_or("no home directory ($HOME)")?,
            show: Box::new(move |path: &Path| files::tilde(path, &env)),
        })
    }

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
) -> Result<(), Error> {
    let show = &context.show;
    let config_path = std::path::absolute(config_path).map_err(|e| e.to_string())?;
    let config = check(&config_path, show)?;
    let program = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("cannot find this wye's path: {e}"))?;
    let service = Service {
        program,
        config: config_path,
        usage_log: context.wye_home.join(USAGE_LOG_FILE),
        messages: context.wye_home.join(MESSAGE_LOG_FILE),
        stop_secs: stop_secs(&config),
    };
    let definition = service.definition(context.manager)?;
    let file = context.file();
    let installed = context.installed()?;
    let replacing = !matches!(installed, Installed::None);
    let url = local_url(config.server.listen);
    // A gateway that answers when none is installed is another one, such
    // as `wye serve` in a terminal: the service could not take its port.
    if !replacing && probe(&url).is_ok() {
        return Err(format!(
            "a gateway already answers at {url} (a wye serve in a terminal?); stop it first"
        )
        .into());
    }
    let uid = uid()?;
    let commands = start_commands(context.manager, &uid, &file, replacing);
    super::emit(&plan(
        context.manager,
        &service,
        &file,
        replacing,
        &commands,
        show,
    ));
    if matches!(installed, Installed::Foreign) {
        say!(
            "{} was not written by wye service install; it is replaced.",
            show(&file)
        );
    }
    if dry_run {
        say!("\n{}:\n\n{definition}", show(&file));
        return Ok(());
    }
    if !yes && !confirm("Install", Some((&file, &definition)), show)? {
        eprintln!("Nothing changed.");
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", show(dir)))?;
    }
    // It may later hold a signing key.
    super::create_private_dir(&context.wye_home)?;
    write_atomically(&file, &definition)?;
    eprintln!("Wrote {}", show(&file));
    if replacing {
        eprintln!(
            "The running gateway finishes its requests first, for up to {} s.",
            service.stop_secs
        );
    }
    run_all(&commands, show, stopping(&service))?;
    Ok(wait_for(&url, &service.messages, show, START_WAIT)?)
}

/// Asks whether to `verb` (`Install`, `Uninstall`), on stderr; with `file`,
/// `[s]how` prints it and asks again. Without a terminal to ask on, it is
/// for `--yes` or `--dry-run` to say.
fn confirm(
    verb: &str,
    file: Option<(&Path, &str)>,
    show: &dyn Fn(&Path) -> String,
) -> Result<bool, Error> {
    if !std::io::stdin().is_terminal() {
        return Err(Error::Usage(format!(
            "not a terminal to ask on: {} with --yes, or see the plan with --dry-run",
            verb.to_lowercase()
        )));
    }
    let choices = if file.is_some() {
        "[y]es, [n]o, [s]how the file"
    } else {
        "[y]es, [n]o"
    };
    loop {
        eprint!("{verb}? {choices}: ");
        let mut answer = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        match (answer.trim().to_ascii_lowercase().as_str(), file) {
            ("y" | "yes", _) => return Ok(true),
            ("s" | "show", Some((file, definition))) => {
                say!("\n{}:\n\n{definition}", show(file));
            }
            _ => return Ok(false),
        }
    }
}

/// What `wye service status` found; the exit status follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Installed, and the gateway answers.
    Answering,
    /// Nothing `install` wrote, nor a definition of another's.
    NotInstalled,
    /// Installed but not answering, its config broken, or a definition
    /// `install` did not write.
    Failing,
}

/// `wye service status`: whether it is installed and answering.
pub fn status(context: &Context) -> Result<Status, String> {
    let show = &context.show;
    let service = match context.installed()? {
        Installed::Ours(service) => service,
        Installed::None => {
            say!("Not installed. `wye service install` sets it up.");
            return Ok(Status::NotInstalled);
        }
        Installed::Foreign => {
            say!(
                "{} exists but was not written by wye service install; \
                 `wye service install` offers to replace it.",
                show(&context.file())
            );
            return Ok(Status::Failing);
        }
    };
    let log_size = std::fs::metadata(&service.usage_log)
        .map(|m| format!(" ({})", file_size(m.len())))
        .unwrap_or_default();
    say!(
        "{:<LABEL_WIDTH$} {}\n{:<LABEL_WIDTH$} {} serve --config {}\n\
         {:<LABEL_WIDTH$} {}{log_size}\n{:<LABEL_WIDTH$} {}",
        "Installed",
        show(&context.file()),
        "Runs",
        show(&service.program),
        show(&service.config),
        "Usage log",
        show(&service.usage_log),
        "Messages",
        show(&service.messages),
    );
    let answering = files::load(&service.config)
        .ok()
        .map(|config| local_url(config.server.listen))
        .map(|url| (probe(&url).is_ok(), url));
    match answering {
        Some((true, url)) => {
            say!(
                "{:<LABEL_WIDTH$} {} at {url}",
                "State",
                Paint::stdout().green("answering")
            );
            Ok(Status::Answering)
        }
        Some((false, url)) => {
            say!(
                "{:<LABEL_WIDTH$} {} at {url}; the end of the message log may say why",
                "State",
                Paint::stdout().red("not answering"),
            );
            Ok(Status::Failing)
        }
        None => {
            say!(
                "{:<LABEL_WIDTH$} its config does not parse; `wye doctor -c {}`",
                "State",
                show(&service.config)
            );
            Ok(Status::Failing)
        }
    }
}

/// `wye service restart`: checks the installed config, then restarts.
pub fn restart(context: &Context) -> Result<(), String> {
    let show = &context.show;
    let installed = context.ours()?;
    let config = check(&installed.config, show)
        .map_err(|e| format!("{e}\nThe service keeps running as it was."))?;
    let service = refreshed(&installed, &config);
    let definition = service.definition(context.manager)?;
    let file = context.file();
    let uid = uid()?;
    // The running gateway stops under the definition its manager loaded.
    let wait = stopping(&installed).max(stopping(&service));
    let commands = if std::fs::read_to_string(&file).ok().as_deref() == Some(&*definition) {
        restart_commands(context.manager, &uid)
    } else {
        // This wye is at another path, or the config's drain or grace
        // changed: the manager loads the new one, as `install` does when
        // replacing.
        write_atomically(&file, &definition)?;
        eprintln!(
            "Updated {} for this wye and config (stop timeout {} s).",
            show(&file),
            service.stop_secs
        );
        start_commands(context.manager, &uid, &file, true)
    };
    run_all(&commands, show, wait)?;
    wait_for(
        &local_url(config.server.listen),
        &service.messages,
        show,
        wait + START_WAIT,
    )
}

/// The service this wye would install for `installed`'s program and
/// config: its stop timeout follows the config as it is now.
fn refreshed(installed: &Service, config: &Config) -> Service {
    Service {
        stop_secs: stop_secs(config),
        ..installed.clone()
    }
}

/// How long the manager waits for the gateway to stop: the config's drain
/// delay and shutdown grace, and a margin.
fn stop_secs(config: &Config) -> u64 {
    (config.server.drain_delay + config.server.shutdown_grace).as_secs() + 5
}

/// `wye service uninstall`: shows what it stops, removes, and keeps, asks,
/// then stops the service and removes its definition.
pub fn uninstall(context: &Context, yes: bool, dry_run: bool) -> Result<(), Error> {
    let show = &context.show;
    let file = context.file();
    let service = match context.installed()? {
        Installed::None => {
            eprintln!("Not installed; nothing changed.");
            return Ok(());
        }
        _ => context.ours()?,
    };
    let stop = stop_commands(context.manager, &uid()?);
    let reload = match context.manager {
        Manager::Systemd => vec![systemctl(&["daemon-reload"])],
        Manager::Launchd => vec![],
    };
    super::emit(&uninstall_plan(&service, &file, &stop, &reload, show));
    if dry_run {
        return Ok(());
    }
    if !yes && !confirm("Uninstall", None, show)? {
        eprintln!("Nothing changed.");
        return Ok(());
    }
    run_all(&stop, show, stopping(&service))?;
    std::fs::remove_file(&file).map_err(|e| format!("cannot remove {}: {e}", show(&file)))?;
    eprintln!("Removed {}", show(&file));
    run_all(&reload, show, stopping(&service))?;
    Ok(())
}

/// How long the running gateway may take to stop.
fn stopping(service: &Service) -> Duration {
    Duration::from_secs(service.stop_secs)
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

#[cfg(test)]
mod tests;
