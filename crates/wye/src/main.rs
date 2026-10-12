use std::{error::Error, ffi::OsString, process::ExitCode, sync::Arc};

use gateway_core::config::{Config, UpstreamCredential};
use wye::{Gateway, StartupError, UpstreamAuth};

const USAGE: &str = "usage:\n  wye serve --config <path>\n  wye check-config <path>";

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
        ["serve", "--config", path] => serve(load(path)?),
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

fn serve(config: Config) -> Result<(), Box<dyn Error>> {
    let auth = match &config.upstream.credential {
        UpstreamCredential::ApiKeyEnv(name) => {
            let key = std::env::var(name).map_err(|_| StartupError::MissingEnv(name.clone()))?;
            UpstreamAuth::api_key(&key)?
        }
        UpstreamCredential::ForwardCaller => UpstreamAuth::ForwardCaller,
    };
    let gateway = Arc::new(Gateway::new(&config, auth)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(config.server.listen).await?;
        eprintln!("wye: listening on http://{}", listener.local_addr()?);
        wye::serve(
            listener,
            gateway,
            shutdown_signal(),
            config.server.shutdown_grace,
        )
        .await;
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
