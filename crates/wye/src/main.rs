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
        Agent, InitMode, InitOptions, Launch,
        args::{self, ArgsError, Command},
        paint::Paint,
        service, token,
    },
};

/// How long `serve` waits, after the server stopped, for blocking work such
/// as a stderr write to finish before the process exits anyway.
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
        ["--version" | "-V"] => Some(format!("wye {}\n", env!("CARGO_PKG_VERSION"))),
        ["help", topic @ ..] => Some(help_for(topic)?),
        [family] | [family, "--help" | "-h"] => args::family_help(family),
        _ => None,
    };
    if let Some(text) = help {
        print!("{text}");
        return Ok(0);
    }
    let Some((command, rest)) = Command::find(&args) else {
        return Err(unknown(&args).into());
    };
    let parsed = match command.parse(rest) {
        Ok(parsed) => parsed,
        Err(ArgsError::Help) => {
            print!("{}", command.help());
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
    let config_path = || -> Result<PathBuf, String> {
        args::find_config(parsed.value("config"), &env, Path::exists)
    };
    let found = |ok: bool| if ok { 0 } else { FOUND };
    match command.name {
        "serve" => {
            let path = config_path()?;
            serve(load(&path)?, &path)?;
            Ok(0)
        }
        "service install" | "service status" | "service restart" | "service uninstall" => {
            let user_home = env("HOME")
                .filter(|h| !h.is_empty())
                .ok_or("no home directory ($HOME)")?;
            let context = service::Context {
                manager: service::Manager::here()?,
                user_home: user_home.into(),
                wye_home: args::home(&env).ok_or("no home directory ($HOME)")?,
                show: Box::new(move |path: &Path| args::tilde(path, &env)),
            };
            match command.name {
                "service install" => service::install(
                    &context,
                    &config_path()?,
                    parsed.flag("yes"),
                    parsed.flag("dry-run"),
                )?,
                "service status" => return Ok(found(service::status(&context)?)),
                "service restart" => service::restart(&context)?,
                _ => service::uninstall(&context)?,
            }
            Ok(0)
        }
        "validate" => {
            let path = config_path()?;
            // Key sets are files too; secrets in the environment are not checked.
            wye::verifier(&load(&path)?, config_dir(&path))?;
            println!(
                "{}: {}",
                args::tilde(&path, &env),
                Paint::stdout().green("ok")
            );
            Ok(0)
        }
        "doctor" => {
            let path = config_path()?;
            let config = load(&path)?;
            let (report, ok) = current_thread()?.block_on(wye::doctor(&config, config_dir(&path)));
            print!("{}", Paint::stdout().verdicts(&report));
            Ok(found(ok))
        }
        "claude" | "codex" => {
            let agent = if command.name == "claude" {
                Agent::Claude
            } else {
                Agent::Codex
            };
            let launch = Launch::resolve(
                parsed.value("url"),
                parsed.value("token-file"),
                &parsed.positional,
                env,
            )?;
            let code = wye::cli::run_agent(agent, &launch)?;
            std::process::exit(code);
        }
        "report" => {
            let paint = Paint::stdout();
            let report = match parsed.positional.first() {
                Some(&"-") => wye::report(std::io::stdin().lock(), paint)?,
                Some(path) => wye::report(open(Path::new(path))?, paint)?,
                None => {
                    let home = args::home(&env).ok_or("no home directory: pass a log file")?;
                    let path = home.join("usage.jsonl");
                    if !path.exists() {
                        return Err(format!(
                            "no usage log at {}: pass a file, or - for standard input",
                            args::tilde(&path, &env)
                        )
                        .into());
                    }
                    wye::report(open(&path)?, paint)?
                }
            };
            print!("{report}");
            Ok(0)
        }
        "init" => {
            let mode = match (parsed.flag("subscription"), parsed.flag("auth")) {
                (true, true) => {
                    return Err("--subscription and --auth do not mix: a subscription \
                                login is forwarded on loopback only"
                        .into());
                }
                (true, false) => InitMode::Subscription,
                (false, true) => InitMode::Team,
                (false, false) => InitMode::ApiKey,
            };
            let home = args::home(&env);
            let out = match (parsed.value("out"), &home) {
                (Some(out), _) => PathBuf::from(out),
                (None, Some(home)) => home.join("wye.toml"),
                (None, None) => return Err("no home directory: pass --out".into()),
            };
            let options = InitOptions {
                mode,
                openai: parsed.flag("openai"),
                out,
                home,
            };
            print!("{}", wye::init(&options, &env)?);
            Ok(0)
        }
        "token keygen" => {
            let out = match parsed.value("out") {
                Some(out) => PathBuf::from(out),
                None => args::home(&env)
                    .ok_or("no home directory: pass --out")?
                    .join("issuer.key"),
            };
            print!("{}", token::keygen(&out)?);
            Ok(0)
        }
        "token issue" => {
            let key = match parsed.value("key") {
                Some(key) => PathBuf::from(key),
                None => args::home(&env)
                    .ok_or("no home directory: pass --key")?
                    .join("issuer.key"),
            };
            let ttl_secs = parsed
                .value("ttl")
                .map(|ttl| {
                    args::duration_secs(ttl)
                        .ok_or_else(|| format!("--ttl {ttl}: write it as 30m, 8h, or 7d"))
                })
                .transpose()?;
            let request = token::Issue {
                subject: parsed.positional.first().copied().unwrap_or_default(),
                ttl_secs,
                models: parsed.value("models"),
                max_output_tokens: parsed.value("max-output-tokens"),
                issuer: parsed.value("issuer"),
                audience: parsed.value("audience"),
                kid: parsed.value("kid"),
            };
            // Without a config, the claims come from flags alone.
            let flags_only =
                request.issuer.is_some() && request.audience.is_some() && request.kid.is_some();
            let config = match config_path() {
                Ok(path) => Some(load(&path)?),
                Err(_) if flags_only => None,
                Err(e) => return Err(e.into()),
            };
            println!(
                "{}",
                token::issue(&request, &key, config.as_ref(), unix_secs()?)?
            );
            Ok(0)
        }
        "explain" => {
            let path = config_path()?;
            let config = load(&path)?;
            let verifier = wye::verifier(&config, config_dir(&path))?;
            let token = parsed
                .value("token-file")
                .map(|file| {
                    read(Path::new(file)).map(|t| String::from_utf8_lossy(&t).trim().to_owned())
                })
                .transpose()?;
            let body = match parsed.positional.first() {
                Some(&"-") => {
                    let mut body = Vec::new();
                    std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut body)?;
                    body
                }
                Some(file) => read(Path::new(file))?,
                None => Vec::new(),
            };
            let credential = match (token.as_deref(), &verifier) {
                (Some(token), Some(verifier)) => wye::Credential::Token(token, verifier),
                _ => wye::Credential::None,
            };
            let explanation = wye::explain(
                &config,
                parsed.value("path").unwrap_or("/v1/messages"),
                credential,
                parsed.value("session"),
                &body,
                unix_secs()?,
                &parsed.values("target-model"),
            );
            print!("{}", Paint::stdout().verdicts(&explanation.report()));
            Ok(found(explanation.refused.is_none()))
        }
        "policy-test" => {
            let config = load(&config_path()?)?;
            let dir = parsed.positional.first().copied().unwrap_or_default();
            let (report, passed) = wye::policy_test(&config, Path::new(dir), unix_secs()?)?;
            print!("{}", Paint::stdout().verdicts(&report));
            Ok(found(passed))
        }
        "policy-diff" => {
            let [old, new] = parsed.positional.as_slice() else {
                return Err("policy-diff takes two configs".into());
            };
            let (old, new) = (Path::new(old), Path::new(new));
            let (before, after) = (load(old)?, load(new)?);
            let mut diff = wye::policy_diff(&before, &after);
            diff.key_sets(&before, config_dir(old), &after, config_dir(new))?;
            print!("{}", Paint::stdout().verdicts(&diff.report()));
            Ok(found(!diff.widens()))
        }
        "probe" => {
            let url = parsed.positional.first().copied().unwrap_or_default();
            current_thread()?.block_on(wye::probe(url))?;
            Ok(0)
        }
        other => Err(format!("`{other}` is listed but not handled").into()),
    }
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

fn current_thread() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn open(path: &Path) -> Result<std::io::BufReader<std::fs::File>, String> {
    std::fs::File::open(path)
        .map(std::io::BufReader::new)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// The time offline checks verify credentials at.
fn unix_secs() -> Result<u64, std::time::SystemTimeError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

fn load(path: &Path) -> Result<Config, Box<dyn Error>> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(Config::from_toml_str(&source).map_err(|e| format!("{}: {e}", path.display()))?)
}

fn config_dir(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}

fn serve(config: Config, path: &Path) -> Result<(), Box<dyn Error>> {
    let resources = Resources::resolve(&config, config_dir(path))?;
    let gateway = Arc::new(Gateway::new(&config, resources)?);
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
        let flushed = wye::serve(listener, gateway, shutdown_signal(), tokio::io::stderr()).await;
        Ok::<_, Box<dyn Error>>(flushed)
    })?;
    // A write to a stderr nobody reads blocks its thread, and the stderr lock,
    // for good: dropping the runtime would wait on it, and so would a last line.
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
