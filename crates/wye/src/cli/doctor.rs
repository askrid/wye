//! `wye doctor --config <path>`: checks what a configuration needs before
//! `wye serve` runs it, and says what to change.
//!
//! Each referenced secret is checked to be present (never printed), each
//! file to parse, and each upstream and the OTLP endpoint to answer over
//! the network: one unauthenticated `HEAD` to the base URL, so DNS, TCP,
//! TLS (with the configured extra roots), and HTTP are proven while no
//! model request is made and nothing is billed. Every check runs, so one
//! pass lists every problem.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use bytes::Bytes;
use gateway_core::{
    config::{Config, Provider, Telemetry, Upstream, UpstreamCredential, is_loopback_host},
    prompt::HashKey,
};
use http::{Request, Uri};
use http_body_util::Empty;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};

use crate::startup::{env, load_certificates, read_secret, verifier};

/// Each network check, including the connection, must finish within this.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// One finding: a line of the report.
struct Check {
    ok: bool,
    line: String,
}

/// Runs every check of `config` (read from `config_dir`); the report and
/// whether all passed.
pub async fn doctor(config: &Config, config_dir: &Path) -> (String, bool) {
    let mut checks = Vec::new();
    for (_, route) in config.routes.iter() {
        for upstream in route.upstreams() {
            let name = upstream
                .id
                .as_deref()
                .map_or_else(|| "[upstream]".to_owned(), |id| format!("upstream {id}"));
            checks.push(credential(&name, upstream, config_dir));
            checks.extend(endpoint_path(&name, upstream));
            checks.push(reachable(&name, upstream, config_dir).await);
        }
    }
    if config.auth.is_some() {
        checks.push(match verifier(config, config_dir) {
            Ok(_) => pass("auth: issuers' key sets read"),
            Err(e) => fail(format!("auth: {e}")),
        });
    }
    if let Some(diagnostics) = &config.cache_diagnostics {
        let name = &diagnostics.hash_key_env;
        checks.push(match env(name) {
            Ok(key) if HashKey::new(key.as_bytes()).is_some() => {
                pass(format!("cache_diagnostics: ${name} set"))
            }
            Ok(_) => fail(format!(
                "cache_diagnostics: ${name} must hold at least {} bytes (openssl rand -hex 32)",
                HashKey::MIN_LEN
            )),
            Err(_) => fail(format!("cache_diagnostics: ${name} is not set")),
        });
    }
    if let Some(telemetry) = &config.telemetry {
        checks.extend(headers_in_clear(telemetry));
        if let Some(name) = &telemetry.otlp_headers_env {
            checks.push(
                match env(name).ok().map(|v| crate::usage_log::parse_headers(&v)) {
                    Some(Some(_)) => pass(format!("telemetry: ${name} set")),
                    Some(None) => fail(format!(
                        "telemetry: ${name} must hold key=value pairs separated by commas"
                    )),
                    None => fail(format!("telemetry: ${name} is not set")),
                },
            );
        }
        checks.push(head("telemetry", &telemetry.otlp_logs_uri, &[]).await);
    }
    let problems = checks.iter().filter(|c| !c.ok).count();
    let mut report = String::new();
    for check in &checks {
        report.push_str(if check.ok { "ok    " } else { "FAIL  " });
        report.push_str(&check.line);
        report.push('\n');
    }
    report.push_str(&match problems {
        0 => "no problems found\n".to_owned(),
        1 => "1 problem\n".to_owned(),
        n => format!("{n} problems\n"),
    });
    (report, problems == 0)
}

/// OTLP headers, usually a backend's API key, sent over plain http to
/// another machine.
fn headers_in_clear(telemetry: &Telemetry) -> Option<Check> {
    let name = telemetry.otlp_headers_env.as_ref()?;
    let uri = &telemetry.otlp_logs_uri;
    (uri.scheme_str() == Some("http") && !uri.host().is_some_and(is_loopback_host)).then(|| {
        fail(format!(
            "telemetry: ${name} would cross the network in the clear to {}; \
             use https, or a collector on this machine",
            uri.authority().map_or("the collector", |a| a.as_str())
        ))
    })
}

fn pass(line: impl Into<String>) -> Check {
    Check {
        ok: true,
        line: line.into(),
    }
}

fn fail(line: impl Into<String>) -> Check {
    Check {
        ok: false,
        line: line.into(),
    }
}

/// A Bedrock upstream's base URL ends in `/anthropic`, where its Messages
/// endpoint is (VPC endpoints too); `None` for other providers.
fn endpoint_path(name: &str, upstream: &Upstream) -> Option<Check> {
    (upstream.provider == Provider::Bedrock).then(|| {
        if upstream.path_prefix.ends_with("/anthropic") {
            pass(format!("{name}: Bedrock Messages endpoint path"))
        } else {
            fail(format!(
                "{name}: Bedrock's Messages endpoint is under /anthropic: \
                 base_url = \"{}://{}/anthropic\"",
                upstream.scheme, upstream.authority
            ))
        }
    })
}

/// The provider credential is present, without printing it.
fn credential(name: &str, upstream: &Upstream, config_dir: &Path) -> Check {
    let header = upstream.provider.key_header();
    match &upstream.credential {
        UpstreamCredential::ApiKeyEnv(var) => match env(var) {
            Ok(key) if !key.trim().is_empty() => {
                pass(format!("{name}: ${var} set (sent as {header})"))
            }
            _ => fail(format!(
                "{name}: ${var} is not set; export the provider key"
            )),
        },
        UpstreamCredential::ApiKeyFile(path) => match read_secret(&config_dir.join(path)) {
            Ok(_) => pass(format!(
                "{name}: {} read (sent as {header})",
                path.display()
            )),
            Err(e) => fail(format!("{name}: {e}")),
        },
        UpstreamCredential::ForwardCaller => pass(format!(
            "{name}: forwards the caller's own credential (loopback only)"
        )),
    }
}

/// The upstream answers HTTP at its base URL, trusting its extra roots.
async fn reachable(name: &str, upstream: &Upstream, config_dir: &Path) -> Check {
    let roots = match &upstream.ca_file {
        Some(path) => match load_certificates(&config_dir.join(path)) {
            Ok(roots) => roots,
            Err(e) => return fail(format!("{name}: {e}")),
        },
        None => Vec::new(),
    };
    let uri = Uri::builder()
        .scheme(upstream.scheme.clone())
        .authority(upstream.authority.clone())
        .path_and_query(if upstream.path_prefix.is_empty() {
            "/"
        } else {
            upstream.path_prefix.as_str()
        })
        .build();
    match uri {
        Ok(uri) => head(name, &uri, &roots).await,
        Err(e) => fail(format!("{name}: {e}")),
    }
}

/// `HEAD uri`: any HTTP status means the endpoint is reachable.
async fn head(
    name: &str,
    uri: &Uri,
    roots: &[rustls::pki_types::CertificateDer<'static>],
) -> Check {
    let https = match crate::upstream::https_connector(roots) {
        Ok(https) => https,
        Err(e) => return fail(format!("{name}: {e}")),
    };
    let client = Client::builder(TokioExecutor::new()).build::<_, Empty<Bytes>>(https);
    let Ok(request) = Request::head(uri.clone()).body(Empty::new()) else {
        return fail(format!("{name}: {uri} is not a valid request URL"));
    };
    let started = Instant::now();
    match tokio::time::timeout(CHECK_TIMEOUT, client.request(request)).await {
        Ok(Ok(res)) => pass(format!(
            "{name}: {uri} reachable in {} ms (HEAD answered {}; any status will do)",
            started.elapsed().as_millis(),
            res.status().as_u16()
        )),
        Ok(Err(e)) => fail(format!("{name}: {uri} unreachable: {}", error_chain(&e))),
        Err(_) => fail(format!(
            "{name}: {uri} did not answer within {}s",
            CHECK_TIMEOUT.as_secs()
        )),
    }
}

/// An error with its causes, which carry the useful part (refused,
/// certificate, DNS).
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(e) = cause {
        text.push_str(": ");
        text.push_str(&e.to_string());
        cause = e.source();
    }
    text
}

#[cfg(test)]
mod tests;
