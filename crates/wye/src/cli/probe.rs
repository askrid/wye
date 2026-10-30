//! `wye probe <url>`: a one-shot health check for container images without
//! `curl` (for example a Docker `HEALTHCHECK` against `/readyz`), and the
//! check `wye claude` makes before launching Claude Code against a local or
//! remote gateway.

use std::time::Duration;

use bytes::Bytes;
use http::{Request, Uri};
use http_body_util::{BodyExt, Empty};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};

/// The whole probe, including the connection, must finish within this.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The health endpoint of the gateway at base URL `gateway`.
pub fn health_url(gateway: &str) -> String {
    format!("{}{}", gateway.trim_end_matches('/'), crate::HEALTH_PATH)
}

/// `GET url` over HTTP or HTTPS (built-in web PKI roots); `Ok` with the URL
/// asked for a 2xx response within the timeout. A URL without a path names a
/// gateway, whose health endpoint is asked: its `/` serves nothing.
pub async fn probe(url: &str) -> Result<String, String> {
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
    let https = crate::tls::https_connector(&[]).map_err(|e| e.to_string())?;
    let client = Client::builder(TokioExecutor::new()).build::<_, Empty<Bytes>>(https);
    let request = Request::get(uri)
        .body(Empty::new())
        .map_err(|e| format!("{url}: {e}"))?;
    let status = tokio::time::timeout(PROBE_TIMEOUT, async {
        let res = client
            .request(request)
            .await
            .map_err(|e| super::error_chain(&e))?;
        let status = res.status();
        res.into_body().collect().await.map_err(|e| e.to_string())?;
        Ok::<_, String>(status)
    })
    .await
    .map_err(|_| format!("{url}: no response within {}s", PROBE_TIMEOUT.as_secs()))?
    .map_err(|e| format!("{url}: {e}"))?;
    if status.is_success() {
        Ok(url)
    } else {
        Err(format!("{url}: {status}"))
    }
}
