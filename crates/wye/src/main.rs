#[cfg(feature = "alloc-stats")]
#[global_allocator]
static ALLOCATOR: wye::alloc_stats::Counting = wye::alloc_stats::Counting;

use std::{
    error::Error,
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use gateway_core::config::Config;
use wye::{
    Gateway, Resources,
    cli::{
        Agent,
        args::{self, ArgsError, Command},
        current_thread, emit, files, gateway_url,
        paint::Paint,
        probe_blocking, run_agent, run_explain, run_init, run_report,
        service::{self, Context},
        token, unix_secs,
    },
};

/// How long `serve` waits, after the server stopped, for blocking work such
/// as a usage-log write to finish before the process exits anyway.
const STOP_WAIT: Duration = Duration::from_secs(1);

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("{} {e}", Paint::stderr().red("wye:"));
            ExitCode::FAILURE
        }
    }
}

/// Exit status of a command that found what it checks for: a problem, a
/// refusal, a failed case, or widened access.
const FOUND: u8 = 3;

fn run(args: Vec<OsString>) -> Result<u8, Box<dyn Error>> {
    let args: Vec<&str> = args
        .iter()
        .map(|a| a.to_str().ok_or("arguments must be valid UTF-8"))
        .collect::<Result<_, _>>()?;
    let help = match args.as_slice() {
        [] | ["--help" | "-h" | "help"] => Some(args::overview()),
        ["--version" | "-V" | "version"] => Some(format!("wye {}\n", env!("CARGO_PKG_VERSION"))),
        ["help" | "--help" | "-h", topic @ ..] => Some(help_for(topic)?),
        [family] | [family, "--help" | "-h" | "help"] => args::family_help(family),
        [family, "help", sub] => Some(help_for(&[family, sub])?),
        _ => None,
    };
    if let Some(text) = help {
        emit(&Paint::stdout().style(&text));
        return Ok(0);
    }
    let Some((command, rest)) = Command::find(&args) else {
        return Err(unknown(&args).into());
    };
    let parsed = match command.parse(rest) {
        Ok(parsed) => parsed,
        Err(ArgsError::Help) => {
            emit(&Paint::stdout().style(&command.help()));
            return Ok(0);
        }
        Err(ArgsError::Usage(why)) => {
            return Err(format!(
                "{why}\nusage: {}\nmore: wye {} --help",
                command.usage(),
                command.name
            )
            .into());
        }
    };
    let env = |name: &str| std::env::var(name).ok();
    let config_path = || files::config_path(&parsed, &env);
    match command.name {
        "serve" => {
            let path = config_path()?;
            serve(files::load(&path)?, &path)?;
            Ok(0)
        }
        "service install" => {
            service::install(
                &Context::new(env)?,
                &config_path()?,
                parsed.flag("yes"),
                parsed.flag("dry-run"),
            )?;
            Ok(0)
        }
        "service status" => Ok(found(service::status(&Context::new(env)?)?)),
        "service restart" => {
            service::restart(&Context::new(env)?)?;
            Ok(0)
        }
        "service uninstall" => {
            service::uninstall(&Context::new(env)?)?;
            Ok(0)
        }
        "validate" => {
            let path = config_path()?;
            // Key sets are files too; secrets in the environment are not checked.
            let verified = wye::verifier(&files::load(&path)?, files::config_dir(&path));
            files::named(&path, verified)?;
            emit(&format!(
                "{}: {}\n",
                files::tilde(&path, &env),
                Paint::stdout().green("ok")
            ));
            Ok(0)
        }
        "doctor" => {
            let path = config_path()?;
            let config = files::load(&path)?;
            let (report, ok) =
                current_thread()?.block_on(wye::doctor(&config, files::config_dir(&path)));
            let shown = files::tilde(&path, &env);
            Ok(verdicts(&format!("config: {shown}\n{report}"), ok))
        }
        "claude" => std::process::exit(run_agent(Agent::Claude, &parsed, env)?),
        "codex" => std::process::exit(run_agent(Agent::Codex, &parsed, env)?),
        "report" => {
            emit(&run_report(&parsed, &env, Paint::stdout())?);
            Ok(0)
        }
        "init" => {
            emit(&Paint::stdout().style(&run_init(&parsed, &env)?));
            Ok(0)
        }
        "token keygen" => {
            emit(&Paint::stdout().style(&token::run_keygen(&parsed, &env)?));
            Ok(0)
        }
        "token issue" => {
            token::run_issue(&parsed, &env)?;
            Ok(0)
        }
        "explain" => {
            let (report, forwarded) = run_explain(&parsed, &env)?;
            Ok(verdicts(&report, forwarded))
        }
        "policy-test" => {
            let path = config_path()?;
            let config = files::load(&path)?;
            let dir = match parsed.positional.first() {
                Some(dir) => PathBuf::from(dir),
                None => files::config_dir(&path).join("policy-tests"),
            };
            let (report, passed) = wye::policy_test(&config, &dir, unix_secs()?)?;
            let shown = files::tilde(&path, &env);
            Ok(verdicts(&format!("config: {shown}\n{report}"), passed))
        }
        "policy-diff" => {
            let [old, new] = parsed.positional.as_slice() else {
                return Err("policy-diff takes two configs".into());
            };
            let (old, new) = (Path::new(old), Path::new(new));
            let (before, after) = (files::load(old)?, files::load(new)?);
            let mut diff = wye::policy_diff(&before, &after);
            diff.key_sets(
                &before,
                files::config_dir(old),
                &after,
                files::config_dir(new),
            )?;
            Ok(verdicts(&diff.report(), !diff.widens()))
        }
        "probe" => {
            let url = gateway_url(parsed.positional.first().copied(), &env);
            let checked = probe_blocking(&url)?;
            emit(&format!("{checked}: {}\n", Paint::stdout().green("ok")));
            Ok(0)
        }
        other => Err(format!("`{other}` is listed but not handled").into()),
    }
}

/// Prints a check's report; the exit status of its result.
fn verdicts(report: &str, ok: bool) -> u8 {
    emit(&Paint::stdout().style(report));
    found(ok)
}

fn found(ok: bool) -> u8 {
    if ok { 0 } else { FOUND }
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

fn serve(config: Config, path: &Path) -> Result<(), Box<dyn Error>> {
    let resources = files::named(path, Resources::resolve(&config, files::config_dir(path)))?;
    let gateway = Arc::new(files::named(path, Gateway::new(&config, resources))?);
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
            "wye: listening on http://{} (config {})",
            listener.local_addr()?,
            path.display()
        );
        let flushed = wye::serve(listener, gateway, shutdown_signal(), tokio::io::stdout()).await;
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
