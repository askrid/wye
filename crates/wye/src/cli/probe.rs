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

/// `GET url` over HTTP or HTTPS (built-in web PKI roots); `Ok` for a 2xx
/// response within the timeout.
pub async fn probe(url: &str) -> Result<(), String> {
    let uri: Uri = url.parse().map_err(|_| format!("{url}: not a valid URL"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err(format!(
            "{url}: the URL must start with http:// or https://"
        ));
    }
    let https = crate::tls::https_connector(&[]).map_err(|e| e.to_string())?;
    let client = Client::builder(TokioExecutor::new()).build::<_, Empty<Bytes>>(https);
    let request = Request::get(uri)
        .body(Empty::new())
        .map_err(|e| format!("{url}: {e}"))?;
    let status = tokio::time::timeout(PROBE_TIMEOUT, async {
        let res = client.request(request).await.map_err(|e| e.to_string())?;
        let status = res.status();
        res.into_body().collect().await.map_err(|e| e.to_string())?;
        Ok::<_, String>(status)
    })
    .await
    .map_err(|_| format!("{url}: no response within {}s", PROBE_TIMEOUT.as_secs()))?
    .map_err(|e| format!("{url}: {e}"))?;
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("{url}: {status}"))
    }
}
