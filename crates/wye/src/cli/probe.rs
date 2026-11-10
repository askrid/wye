//! `wye probe <url>`: a one-shot health check for container images without
//! `curl` (for example a Docker `HEALTHCHECK` against `/readyz`), and the
//! check `wye claude` and `wye codex` make before launching their agent
//! against a local or remote gateway.

use std::{path::Path, time::Duration};

use bytes::Bytes;
use gateway_core::config::is_loopback_host;
use http::{Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Empty};
use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use rustls::pki_types::CertificateDer;

use super::{Env, Error, Exit, args::Parsed, emit, files, paint::Paint};

/// Each check, the connection included, must finish within this.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Gateway URL when neither `--url` nor `WYE_URL` is set: a gateway on its
/// default listen address.
pub(crate) const DEFAULT_GATEWAY: &str = concat!("http://", gateway_core::default_listen!());

/// `wye probe`: the URL that answered 2xx, printed; else why none did, with
/// what to run when the default gateway did not answer.
pub fn run(parsed: &Parsed<'_>, env: &impl Env) -> Result<Exit, Error> {
    let given = parsed.positional.first().copied();
    let url = gateway_url(given, env);
    let checked = blocking(&url).map_err(|why| match given {
        Some(_) => why,
        None => no_gateway(&url, &why, "`wye probe <url>`", env),
    })?;
    emit(&format!("{checked}: {}\n", Paint::stdout().green("ok")));
    Ok(Exit::Done)
}

/// The gateway to use: `url` (`--url`), else `$WYE_URL`, else this machine's.
pub fn gateway_url(url: Option<&str>, env: &impl Env) -> String {
    url.map(str::to_owned)
        .or_else(|| env.var("WYE_URL"))
        .unwrap_or_else(|| DEFAULT_GATEWAY.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

/// Why nothing answered at `gateway`, and what to run: a gateway on this
/// machine is started, after `wye init` when there is no config to start it
/// with; another is named with `another`.
pub fn no_gateway(gateway: &str, why: &str, another: &str, env: &impl Env) -> String {
    // No config at all: not one in the home, nor a ./wye.toml to name.
    let unconfigured = files::find_config(None, env, Path::exists).is_err()
        && !Path::new(files::CONFIG_FILE).exists();
    let next = if !on_this_machine(gateway) {
        format!("name another with {another}")
    } else if unconfigured {
        format!(
            "set one up with `wye init`, then `wye service install`; or name another \
             with {another}"
        )
    } else {
        format!(
            "start one with `wye service install` or `wye serve`, or name another with \
             {another}"
        )
    };
    format!("no gateway at {gateway} ({why}); {next}")
}

/// Whether the gateway at base URL `gateway` is on this machine: its host
/// is a loopback one.
pub(super) fn on_this_machine(gateway: &str) -> bool {
    gateway
        .parse::<Uri>()
        .is_ok_and(|uri| uri.host().is_some_and(is_loopback_host))
}

/// [`probe`], for a command with no runtime of its own.
pub fn blocking(url: &str) -> Result<String, String> {
    super::current_thread()
        .map_err(|e| e.to_string())?
        .block_on(probe(url))
}

/// The health endpoint of the gateway at base URL `gateway`.
pub fn health_url(gateway: &str) -> String {
    format!("{}{}", gateway.trim_end_matches('/'), crate::HEALTH_PATH)
}

/// `GET url` over HTTP or HTTPS (built-in web PKI roots); `Ok` with the URL
/// asked for a 2xx response within the timeout. A URL without a path names a
/// gateway, whose health endpoint is asked: its `/` serves nothing.
pub async fn probe(url: &str) -> Result<String, String> {
    Checker::new(&[])?.probe(url).await
}

/// An HTTP and HTTPS client for checks, built once for any number of them.
pub(super) struct Checker(Client<HttpsConnector<HttpConnector>, Empty<Bytes>>);

impl Checker {
    /// A checker that trusts the built-in web PKI roots and `roots`.
    pub(super) fn new(roots: &[CertificateDer<'static>]) -> Result<Self, String> {
        let https = crate::tls::https_connector(roots).map_err(|e| e.to_string())?;
        Ok(Self(Client::builder(TokioExecutor::new()).build(https)))
    }

    /// The status `method uri` answers with, its body read to the end,
    /// within [`TIMEOUT`]; else why there is none.
    pub(super) async fn status(&self, method: Method, uri: Uri) -> Result<StatusCode, String> {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Empty::new())
            .map_err(|e| e.to_string())?;
        tokio::time::timeout(TIMEOUT, async {
            let res = self
                .0
                .request(request)
                .await
                .map_err(|e| super::error_chain(&e))?;
            let status = res.status();
            res.into_body().collect().await.map_err(|e| e.to_string())?;
            Ok(status)
        })
        .await
        .map_err(|_| format!("no response within {}s", TIMEOUT.as_secs()))?
    }

    /// [`probe`], with this checker.
    pub(super) async fn probe(&self, url: &str) -> Result<String, String> {
        let uri: Uri = url.parse().map_err(|_| format!("{url}: not a valid URL"))?;
        if !matches!(uri.scheme_str(), Some("http" | "https")) {
            return Err(format!(
                "{url}: the URL must start with http:// or https://"
            ));
        }
        let (uri, url) = if uri.path() == "/" && uri.query().is_none() {
            let health = health_url(url);
            let uri = health
                .parse()
                .map_err(|_| format!("{health}: not a valid URL"))?;
            (uri, health)
        } else {
            (uri, url.to_owned())
        };
        let status = self
            .status(Method::GET, uri)
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        if status.is_success() {
            Ok(url)
        } else {
            Err(format!("{url}: {status}"))
        }
    }
}
