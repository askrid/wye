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
    Gateway, Resources, UsageFile, UsageOut,
    cli::{
        Agent,
        args::{self, ArgsError, Command, Id},
        current_thread, emit, files, gateway_url, no_gateway,
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
        Err(ArgsError::Missing(why)) => return Err(why.into()),
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
    match command.id {
        Id::Serve => {
            let path = config_path()?;
            serve(
                files::load(&path)?,
                &path,
                parsed.value("usage-log").map(Path::new),
            )?;
            Ok(0)
        }
        Id::ServiceInstall => {
            service::install(
                &Context::new(env)?,
                &config_path()?,
                parsed.flag("yes"),
                parsed.flag("dry-run"),
            )?;
            Ok(0)
        }
        Id::ServiceStatus => Ok(found(service::status(&Context::new(env)?)?)),
        Id::ServiceRestart => {
            service::restart(&Context::new(env)?)?;
            Ok(0)
        }
        Id::ServiceUninstall => {
            service::uninstall(&Context::new(env)?)?;
            Ok(0)
        }
        Id::Validate => {
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
        Id::Doctor => {
            let path = config_path()?;
            let config = files::load(&path)?;
            // The key `wye token issue` signs with by default, if this
            // machine has one.
            let signing_key = files::home(&env)
                .map(|home| home.join(files::SIGNING_KEY_FILE))
                .filter(|key| key.exists());
            let (report, ok) = current_thread()?.block_on(wye::doctor(
                &config,
                files::config_dir(&path),
                signing_key.as_deref(),
            ));
            let shown = files::tilde(&path, &env);
            Ok(verdicts(&format!("config: {shown}\n{report}"), ok))
        }
        Id::Claude => std::process::exit(run_agent(Agent::Claude, &parsed, env)?),
        Id::Codex => std::process::exit(run_agent(Agent::Codex, &parsed, env)?),
        Id::Report => {
            emit(&run_report(&parsed, &env, Paint::stdout())?);
            Ok(0)
        }
        Id::Init => {
            emit(&Paint::stdout().style(&run_init(&parsed, &env)?));
            Ok(0)
        }
        Id::TokenKeygen => {
            emit(&Paint::stdout().style(&token::run_keygen(&parsed, &env)?));
            Ok(0)
        }
        Id::TokenIssue => {
            token::run_issue(&parsed, &env)?;
            Ok(0)
        }
        Id::Explain => {
            let (report, forwarded) = run_explain(&parsed, &env)?;
            Ok(verdicts(&report, forwarded))
        }
        Id::PolicyTest => {
            let path = config_path()?;
            let config = files::load(&path)?;
            let dir = match parsed.positional.first() {
                Some(dir) => PathBuf::from(dir),
                None => files::config_dir(&path).join(files::POLICY_TESTS_DIR),
            };
            let (report, passed) = wye::policy_test(&config, &dir, unix_secs()?)?;
            let shown = files::tilde(&path, &env);
            Ok(verdicts(&format!("config: {shown}\n{report}"), passed))
        }
        Id::PolicyDiff => {
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
        Id::Probe => {
            let given = parsed.positional.first().copied();
            let url = gateway_url(given, &env);
            let checked = probe_blocking(&url).map_err(|why| match given {
                Some(_) => why,
                None => no_gateway(&url, &why, "`wye probe <url>`", &env),
            })?;
            emit(&format!("{checked}: {}\n", Paint::stdout().green("ok")));
            Ok(0)
        }
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
