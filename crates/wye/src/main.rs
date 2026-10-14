mod token;

#[cfg(feature = "alloc-stats")]
#[global_allocator]
static ALLOCATOR: wye::alloc_stats::Counting = wye::alloc_stats::Counting;

use std::{error::Error, ffi::OsString, path::Path, process::ExitCode, sync::Arc};

use gateway_core::config::Config;
use wye::{Gateway, Resources};

const USAGE: &str = "usage:
  wye serve --config <path>
  wye check-config <path>
  wye probe <http-url>
  wye explain --config <path> [--credential <file>] [--session <id>] [--path <path>] <request.json>
  wye claude [--gateway <url>] [--credential-file <path>] [--] [claude args...]
  wye token keygen --out <key-file>
  wye token issue --key <key-file> --kid <id> --issuer <iss> --audience <aud> \
--subject <sub> --ttl-secs <n> [--models <name,prefix*,...>]";

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
        ["token", "keygen", "--out", path] => token::keygen(Path::new(path)),
        ["token", "issue", flags @ ..] => token::issue(flags),
        ["claude", rest @ ..] => {
            let code = wye::claude(rest)?;
            std::process::exit(code);
        }
        ["explain", rest @ ..] => explain(rest),
        ["probe", url] => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(wye::probe(url))
            .map_err(Into::into),
        ["check-config", path] => {
            load(path)?;
            println!("{path}: ok");
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

/// Prints the decision; exits 3 when the request would be refused.
fn explain(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let (mut config, mut credential, mut session, mut path) = (None, None, None, "/v1/messages");
    let mut rest = args;
    let request = loop {
        match rest {
            ["--config", value, tail @ ..] => (config, rest) = (Some(*value), tail),
            ["--credential", value, tail @ ..] => (credential, rest) = (Some(*value), tail),
            ["--session", value, tail @ ..] => (session, rest) = (Some(*value), tail),
            ["--path", value, tail @ ..] => (path, rest) = (*value, tail),
            [request] => break *request,
            _ => return Err(USAGE.into()),
        }
    };
    let config = load(config.ok_or(USAGE)?)?;
    let token = credential
        .map(|file| std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}")))
        .transpose()?;
    let body = std::fs::read(request).map_err(|e| format!("cannot read {request}: {e}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let explanation = wye::explain(
        &config,
        path,
        token.as_deref().map(str::trim),
        session,
        &body,
        now,
    );
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
