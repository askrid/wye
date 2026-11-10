//! `wye doctor --config <path>`: checks what a configuration needs before
//! `wye serve` runs it, and says what to change.
//!
//! Each referenced secret is checked to be present (never printed), each
//! secret file, and the signing key if this machine has one, to be closed
//! to other users, each file to parse, and each upstream and the OTLP
//! endpoint to answer over the network: one unauthenticated `HEAD` to the
//! base URL, so DNS, TCP, TLS (with the configured extra roots), and HTTP
//! are proven while no model request is made and nothing is billed. Every
//! check runs, so one pass lists every problem.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use bytes::Bytes;
use gateway_core::config::{Config, Provider, ProviderKey, Secret, Upstream};
use http::{Request, Uri};
use http_body_util::Empty;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};

use super::{
    Env, Error, Exit,
    args::Parsed,
    files::{self, SIGNING_KEY_FILE},
    paint::verdict,
};
use crate::startup::{
    open_to_others, otlp_headers, prompt_key, upstream_auth, upstream_roots, verifier,
};

/// Each network check, including the connection, must finish within this.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// One finding: a line of the report, for a check that passed or failed.
type Check = Result<String, String>;

/// `wye doctor`: every check of the config, printed under it; `Failed`
/// when one fails.
pub fn run(parsed: &Parsed<'_>, env: &impl Env) -> Result<Exit, Error> {
    let (path, config) = files::load_config(parsed, env)?;
    // The key `wye token issue` signs with by default, if this machine has
    // one.
    let signing_key = files::home(env)
        .map(|home| home.join(SIGNING_KEY_FILE))
        .filter(|key| key.exists());
    let (report, ok) = super::current_thread()
        .map_err(|e| e.to_string())?
        .block_on(doctor(
            &config,
            files::config_dir(&path),
            signing_key.as_deref(),
        ));
    Ok(super::checked(&super::about(&path, env, &report), ok))
}

/// Runs every check of `config` (read from `config_dir`), and of the
/// `signing_key` file this machine holds, if any; the report and whether
/// all passed.
pub async fn doctor(
    config: &Config,
    config_dir: &Path,
    signing_key: Option<&Path>,
) -> (String, bool) {
    let mut checks = Vec::new();
    for upstream in config.routes.upstreams() {
        let name = format!("upstream {}", upstream.id);
        checks.push(provider_key(&name, upstream, config_dir));
        checks.extend(endpoint_path(&name, upstream));
        checks.push(reachable(&name, upstream, config_dir).await);
    }
    if config.auth.is_some() {
        checks.push(match verifier(config, config_dir) {
            Ok(_) => Ok("auth: identity providers' key sets read".to_owned()),
            Err(e) => Err(format!("auth: {e}")),
        });
    }
    if let Some(diagnostics) = &config.cache_diagnostics {
        let source = &diagnostics.hash_key;
        checks.push(
            prompt_key(source, config_dir)
                .map_err(|e| e.to_string())
                .and_then(|_| found(source, config_dir).map_err(|why| format!("hash_key {why}")))
                .map(|found| format!("cache_diagnostics: hash key {found}"))
                .map_err(|e| format!("cache_diagnostics: {e}")),
        );
    }
    // It signs gateway tokens: anyone who can read it can mint one.
    checks.extend(
        signing_key
            .and_then(open_to_others)
            .map(|why| Err(format!("signing key {why}"))),
    );
    if let Some(telemetry) = &config.telemetry {
        if let Some(source) = &telemetry.otlp_headers {
            checks.push(
                otlp_headers(source, config_dir)
                    .map_err(|e| e.to_string())
                    .and_then(|_| {
                        found(source, config_dir).map_err(|why| format!("otlp_headers {why}"))
                    })
                    .map(|found| format!("telemetry: OTLP headers {found}"))
                    .map_err(|e| format!("telemetry: {e}")),
            );
        }
        checks.push(head("telemetry", &telemetry.otlp_logs_uri, &[]).await);
    }
    let problems = checks.iter().filter(|c| c.is_err()).count();
    let mut report = String::new();
    for check in &checks {
        report.push_str(&match check {
            Ok(line) => verdict(true, line),
            Err(line) => verdict(false, line),
        });
    }
    report.push_str(&match problems {
        0 => "no problems found\n".to_owned(),
        n => format!("{}\n", super::text::plural(n as u64, "problem", "problems")),
    });
    (report, problems == 0)
}

/// A Bedrock upstream's base URL ends in `/anthropic`, where its Messages
/// endpoint is (VPC endpoints too); `None` for other providers.
fn endpoint_path(name: &str, upstream: &Upstream) -> Option<Check> {
    (upstream.provider == Provider::Bedrock).then(|| {
        if upstream.path_prefix.ends_with("/anthropic") {
            Ok(format!("{name}: Bedrock Messages endpoint path"))
        } else {
            Err(format!(
                "{name}: Bedrock's Messages endpoint is under /anthropic: \
                 base_url = \"{}://{}/anthropic\"",
                upstream.scheme, upstream.authority
            ))
        }
    })
}

/// The provider key is present, without printing it, and its file, if
/// any, closed to other users.
fn provider_key(name: &str, upstream: &Upstream, config_dir: &Path) -> Check {
    let header = upstream.provider.key_header();
    upstream_auth(upstream, config_dir).map_err(|e| format!("{name}: {e}"))?;
    match &upstream.provider_key {
        ProviderKey::Secret(secret) => found(secret, config_dir)
            .map(|found| format!("{name}: provider key {found} (sent as {header})"))
            .map_err(|e| format!("{name}: provider_key {e}")),
        ProviderKey::ForwardCaller => Ok(format!(
            "{name}: forwards the caller's own credential (loopback only)"
        )),
    }
}

/// Where a secret that resolved was found, without its value; an error
/// when its file is open to other users.
fn found(secret: &Secret, config_dir: &Path) -> Result<String, String> {
    match secret {
        Secret::Env(name) => Ok(format!("${name} set")),
        Secret::File(path) => match open_to_others(&config_dir.join(path)) {
            Some(why) => Err(why),
            None => Ok(format!("{} read", path.display())),
        },
    }
}

/// The upstream answers HTTP at its base URL, trusting its extra roots.
async fn reachable(name: &str, upstream: &Upstream, config_dir: &Path) -> Check {
    let roots = upstream_roots(upstream, config_dir).map_err(|e| format!("{name}: {e}"))?;
    let uri: Uri = upstream
        .base_url()
        .parse()
        .map_err(|e| format!("{name}: {e}"))?;
    head(name, &uri, &roots).await
}

/// `HEAD uri`: any HTTP status means the endpoint is reachable.
async fn head(
    name: &str,
    uri: &Uri,
    roots: &[rustls::pki_types::CertificateDer<'static>],
) -> Check {
    let https = crate::tls::https_connector(roots).map_err(|e| format!("{name}: {e}"))?;
    let client = Client::builder(TokioExecutor::new()).build::<_, Empty<Bytes>>(https);
    let request = Request::head(uri.clone())
        .body(Empty::new())
        .map_err(|_| format!("{name}: {uri} is not a valid request URL"))?;
    let started = Instant::now();
    match tokio::time::timeout(CHECK_TIMEOUT, client.request(request)).await {
        Ok(Ok(res)) => Ok(format!(
            "{name}: {uri} reachable in {} ms (HEAD answered {}; any status will do)",
            started.elapsed().as_millis(),
            res.status().as_u16()
        )),
        Ok(Err(e)) => Err(format!(
            "{name}: {uri} unreachable: {}",
            super::error_chain(&e)
        )),
        Err(_) => Err(format!(
            "{name}: {uri} did not answer within {}s",
            CHECK_TIMEOUT.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests;
