//! `wye probe <url>`: a one-shot health check for container images without
//! `curl` (for example a Docker `HEALTHCHECK` against `/readyz`).

use std::time::Duration;

use bytes::Bytes;
use http::{Request, Uri, uri::Scheme};
use http_body_util::{BodyExt, Empty};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};

/// The whole probe, including the connection, must finish within this.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// `GET url` over plain HTTP; `Ok` for a 2xx response within the timeout.
pub async fn probe(url: &str) -> Result<(), String> {
    let uri: Uri = url.parse().map_err(|_| format!("{url}: not a valid URL"))?;
    if uri.scheme() != Some(&Scheme::HTTP) {
        return Err(format!(
            "{url}: probes use plain http (Wye serves http inside its container)"
        ));
    }
    let client = Client::builder(TokioExecutor::new()).build_http::<Empty<Bytes>>();
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
