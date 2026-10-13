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
  wye token keygen --out <key-file>
  wye token issue --key <key-file> --kid <id> --issuer <iss> --audience <aud> \
--subject <sub> --ttl-secs <n>";

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
        ["check-config", path] => {
            load(path)?;
            println!("{path}: ok");
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
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
