#[cfg(feature = "alloc-stats")]
#[global_allocator]
static ALLOCATOR: wye::alloc_stats::Counting = wye::alloc_stats::Counting;

use std::{error::Error, ffi::OsString, path::Path, process::ExitCode, sync::Arc};

use gateway_core::config::Config;
use wye::{Gateway, Resources};

const USAGE: &str = "usage:
  wye serve --config <path>
  wye check-config <path>
  wye probe <url>
  wye doctor --config <path>
  wye init [--subscription] [--anthropic-key-env <name>] [--openai-key-env <name>] [--out <path>]
  wye policy-test --config <path> <dir>
  wye report [<usage.jsonl>]           (default: standard input)
  wye explain --config <path> [--credential <file>] [--session <id>] [--path <path>] [--target-model <model>]... <request.json>
  wye claude [--gateway <url>] [--credential-file <path>] [--] [claude args...]
  wye codex [--gateway <url>] [--credential-file <path>] [--] [codex args...]
  wye token keygen --out <key-file>
  wye token issue --key <key-file> --kid <id> --issuer <iss> --audience <aud> \
--subject <sub> --ttl-secs <n> [--models <name,prefix*,...>] [--max-output-tokens <n>]";

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wye: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<OsString>) -> Result<(), Box<dyn Error>> {
    let args: Vec<&str> = args
        .iter()
        .map(|a| a.to_str().ok_or("arguments must be valid UTF-8"))
        .collect::<Result<_, _>>()?;
    match args.as_slice() {
        ["serve", "--config", path] => serve(load(path)?, config_dir(path)),
        ["token", "keygen", "--out", path] => wye::cli::token::keygen(Path::new(path)),
        ["token", "issue", flags @ ..] => wye::cli::token::issue(flags),
        ["claude", rest @ ..] => {
            let code = wye::claude(rest)?;
            std::process::exit(code);
        }
        ["codex", rest @ ..] => {
            let code = wye::codex(rest)?;
            std::process::exit(code);
        }
        ["explain", rest @ ..] => explain(rest),
        ["policy-test", "--config", path, dir] => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let (report, passed) = wye::policy_test(&load(path)?, Path::new(dir), now)?;
            print!("{report}");
            if !passed {
                std::process::exit(3);
            }
            Ok(())
        }
        ["init", rest @ ..] => {
            print!("{}", wye::init(&wye::InitOptions::from_args(rest)?)?);
            Ok(())
        }
        ["doctor", "--config", path] => {
            let config = load(path)?;
            let (report, ok) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(wye::doctor(&config, config_dir(path)));
            print!("{report}");
            if !ok {
                std::process::exit(3);
            }
            Ok(())
        }
        ["probe", url] => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(wye::probe(url))
            .map_err(Into::into),
        ["report"] => {
            print!("{}", wye::report(std::io::stdin().lock())?);
            Ok(())
        }
        ["report", path] => {
            let file = std::fs::File::open(path).map_err(|e| format!("cannot read {path}: {e}"))?;
            print!("{}", wye::report(std::io::BufReader::new(file))?);
            Ok(())
        }
        ["check-config", path] => {
            // Key sets are files too; secrets in the environment are not checked.
            wye::verifier(&load(path)?, config_dir(path))?;
            println!("{path}: ok");
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

/// Prints the decision; exits 3 when the request would be refused.
fn explain(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let (mut config, mut credential, mut session, mut path) = (None, None, None, "/v1/messages");
    let mut targets = Vec::new();
    let mut rest = args;
    let request = loop {
        match rest {
            ["--config", value, tail @ ..] => (config, rest) = (Some(*value), tail),
            ["--credential", value, tail @ ..] => (credential, rest) = (Some(*value), tail),
            ["--session", value, tail @ ..] => (session, rest) = (Some(*value), tail),
            ["--path", value, tail @ ..] => (path, rest) = (*value, tail),
            ["--target-model", value, tail @ ..] => {
                targets.push(*value);
                rest = tail;
            }
            [request] => break *request,
            _ => return Err(USAGE.into()),
        }
    };
    let config_path = config.ok_or(USAGE)?;
    let config = load(config_path)?;
    let verifier = wye::verifier(&config, config_dir(config_path))?;
    let token = credential
        .map(|file| std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}")))
        .transpose()?;
    let body = std::fs::read(request).map_err(|e| format!("cannot read {request}: {e}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let credential = match (token.as_deref(), &verifier) {
        (Some(token), Some(verifier)) => wye::Credential::Token(token.trim(), verifier),
        _ => wye::Credential::None,
    };
    let explanation = wye::explain(&config, path, credential, session, &body, now, &targets);
    print!("{}", explanation.report());
    if explanation.refused.is_some() {
        std::process::exit(3);
    }
    Ok(())
}

fn load(path: &str) -> Result<Config, Box<dyn Error>> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    Ok(Config::from_toml_str(&source)?)
}

fn config_dir(path: &str) -> &Path {
    Path::new(path).parent().unwrap_or(Path::new("."))
}

fn serve(config: Config, config_dir: &Path) -> Result<(), Box<dyn Error>> {
    let resources = Resources::resolve(&config, config_dir)?;
    let gateway = Arc::new(Gateway::new(&config, resources)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(config.server.listen).await?;
        eprintln!("wye: listening on http://{}", listener.local_addr()?);
        wye::serve(listener, gateway, shutdown_signal(), tokio::io::stderr()).await;
        eprintln!("wye: stopped");
        Ok(())
    })
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
