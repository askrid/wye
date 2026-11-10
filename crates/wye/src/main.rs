#[cfg(feature = "alloc-stats")]
#[global_allocator]
static ALLOCATOR: wye::alloc_stats::Counting = wye::alloc_stats::Counting;

use std::{error::Error, ffi::OsString, path::Path, process::ExitCode, sync::Arc, time::Duration};

use gateway_core::config::Config;
use wye::{
    Gateway, Resources, UsageFile, UsageOut,
    cli::{
        self, Exit,
        args::{self, ArgsError, Command, Id},
        emit, files,
        paint::Paint,
        service, token,
        wrapper::Agent,
    },
};

/// How long `serve` waits, after the server stopped, for blocking work such
/// as a usage-log write to finish before the process exits anyway.
const STOP_WAIT: Duration = Duration::from_secs(1);

/// Exit status: wrong usage.
const USAGE: u8 = 2;

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(exit) => ExitCode::from(exit as u8),
        Err(e) => {
            eprintln!("{} {e}", Paint::stderr().red("wye:"));
            let usage = matches!(e.downcast_ref(), Some(cli::Error::Usage(_)));
            ExitCode::from(if usage { USAGE } else { Exit::Failed as u8 })
        }
    }
}

fn run(args: Vec<OsString>) -> Result<Exit, Box<dyn Error>> {
    let usage = |why: String| cli::Error::Usage(why);
    let args: Vec<&str> = args
        .iter()
        .map(|a| {
            a.to_str()
                .ok_or_else(|| usage("arguments must be valid UTF-8".into()))
        })
        .collect::<Result<_, _>>()?;
    let help = match args.as_slice() {
        [] | ["--help" | "-h" | "help"] => Some(args::overview()),
        ["--version" | "-V" | "version"] => Some(format!("wye {}\n", env!("CARGO_PKG_VERSION"))),
        ["help" | "--help" | "-h", topic @ ..] => Some(help_for(topic).map_err(usage)?),
        [family] | [family, "--help" | "-h" | "help"] => args::family_help(family),
        [family, "help", sub] => Some(help_for(&[family, sub]).map_err(usage)?),
        _ => None,
    };
    if let Some(text) = help {
        emit(&Paint::stdout().style(&text));
        return Ok(Exit::Done);
    }
    let Some((command, rest)) = Command::find(&args) else {
        return Err(usage(unknown(&args)).into());
    };
    let parsed = match command.parse(rest) {
        Ok(parsed) => parsed,
        Err(ArgsError::Help) => {
            emit(&Paint::stdout().style(&command.help()));
            return Ok(Exit::Done);
        }
        Err(ArgsError::Missing(why)) => return Err(usage(why).into()),
        Err(ArgsError::Usage(why)) => {
            return Err(usage(format!(
                "{why}\nusage: {}\nmore: wye {} --help",
                command.usage(),
                command.name
            ))
            .into());
        }
    };
    let env = |name: &str| std::env::var(name).ok();
    let exit = match command.id {
        Id::Serve => {
            let (path, config) = files::load_config(&parsed, &env)?;
            serve(config, &path, parsed.value("usage-log").map(Path::new))?;
            Exit::Done
        }
        Id::ServiceInstall => service::install(&parsed, &env)?,
        Id::ServiceStatus => service::status(&env)?,
        Id::ServiceRestart => service::restart(&env)?,
        Id::ServiceUninstall => service::uninstall(&parsed, &env)?,
        Id::Doctor => cli::doctor::run(&parsed, &env)?,
        Id::Claude => cli::wrapper::run(Agent::Claude, &parsed, &env)?,
        Id::Codex => cli::wrapper::run(Agent::Codex, &parsed, &env)?,
        Id::Report => cli::report::run(&parsed, &env)?,
        Id::Init => cli::init::run(&parsed, &env)?,
        Id::TokenKeygen => token::run_keygen(&parsed, &env)?,
        Id::TokenIssue => token::run_issue(&parsed, &env)?,
        Id::Explain => cli::explain::run(&parsed, &env)?,
        Id::PolicyTest => cli::policy_test::run(&parsed, &env)?,
        Id::PolicyDiff => cli::policy_diff::run(&parsed)?,
        Id::Probe => cli::probe::run(&parsed, &env)?,
    };
    Ok(exit)
}

/// `wye help <command>`: that command's help.
fn help_for(topic: &[&str]) -> Result<String, String> {
    if let [family] = topic
        && let Some(help) = args::family_help(family)
    {
        return Ok(help);
    }
    match Command::find(topic) {
        Some((command, [])) => Ok(command.help()),
        _ => Err(unknown(topic)),
    }
}

/// The error for a command `args` does not name.
fn unknown(args: &[&str]) -> String {
    match args {
        [family, sub, ..] if args::family_help(family).is_some() => {
            format!("unknown command `{family} {sub}`; see `wye {family} --help`")
        }
        [first, ..] => args::unknown_command(first),
        [] => args::overview(),
    }
}

fn serve(config: Config, path: &Path, usage_log: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let config_dir = files::config_dir(path);
    let resources = files::named(path, Resources::resolve(&config, config_dir))?;
    for warning in wye::key_file_warnings(&config, config_dir) {
        eprintln!("wye: warning: {warning}");
    }
    let gateway = Arc::new(files::named(path, Gateway::new(&config, resources))?);
    let (usage_out, usage_shown) = match usage_log {
        Some(log) => {
            let file = UsageFile::open(log)
                .map_err(|e| format!("cannot open the usage log {}: {e}", log.display()))?;
            (
                UsageOut::File(file),
                format!(", usage log {}", log.display()),
            )
        }
        None => (UsageOut::Stream(tokio::io::stdout()), String::new()),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let flushed = runtime.block_on(async {
        let listen = config.server.listen;
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AddrInUse => {
                    format!("cannot listen on {listen}: the port is in use (a running wye?)")
                }
                _ => format!("cannot listen on {listen}: {e}"),
            })?;
        eprintln!(
            "wye: listening on http://{} (config {}{usage_shown})",
            listener.local_addr()?,
            path.display()
        );
        let flushed = wye::serve(listener, gateway, shutdown_signal(), usage_out).await;
        Ok::<_, Box<dyn Error>>(flushed)
    })?;
    // A write to a stdout nobody reads blocks its thread for good: dropping the
    // runtime would wait on it.
    runtime.shutdown_timeout(STOP_WAIT);
    if flushed {
        eprintln!("wye: stopped");
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
