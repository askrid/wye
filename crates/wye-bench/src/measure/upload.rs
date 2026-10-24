//! Upload scenarios: agent-sized request bodies, as Claude Code resends its
//! whole context every turn.

use std::collections::BTreeMap;

use bytes::Bytes;

use super::{Direct, successes};
use crate::{
    client::Target,
    gateway_process::GatewayProcess,
    mock, scenarios,
    stats::{PERCENTILES, percentile, phase, put},
};

pub(super) const UPLOAD_BYTES: usize = 1024 * 1024;
pub(super) const UPLOAD_CONCURRENCY: usize = 16;
pub(super) const UPLOAD_REQUESTS: usize = 256;
/// The upload scenario's body is one long string; the agent upload sends a
/// Claude Code-shaped body of the same size, which costs the cache diagnostics
/// scanner more (`mock::agent_json`).
pub(super) const AGENT_UPLOAD_REQUESTS: usize = 128;

/// Added latency and CPU per MB of the upload scenario, and of the agent
/// upload where the path measures it.
pub(super) async fn measure_uploads(
    direct: &Direct,
    process: &GatewayProcess,
    prefix: &str,
    metrics: &mut BTreeMap<String, f64>,
) {
    let upload_scenario = ("upload", upload_body(), UPLOAD_REQUESTS, &direct.upload.0);
    let agent_scenario = direct
        .agent_upload
        .as_ref()
        .map(|(samples, _)| ("agent_upload", agent_body(), AGENT_UPLOAD_REQUESTS, samples));
    for (name, body, requests, direct_samples) in
        std::iter::once(upload_scenario).chain(agent_scenario)
    {
        let uploaded_mb = (body.len() * requests) as f64 / 1e6;
        let cpu_before = process.cpu_secs();
        let (via, failed) = phase(
            format!("{prefix}{name}"),
            upload(process.target(), body, requests),
        )
        .await;
        let cpu_after = process.cpu_secs();
        put(
            metrics,
            prefix,
            &format!("{name}_failed_requests"),
            failed as f64,
        );
        for (label, p) in PERCENTILES {
            let added = percentile(&via, p) - percentile(direct_samples, p);
            put(metrics, prefix, &format!("{name}_added_{label}_us"), added);
        }
        put(
            metrics,
            prefix,
            &format!("{name}_cpu_ms_per_mb"),
            (cpu_after - cpu_before) * 1_000.0 / uploaded_mb,
        );
    }
}

/// Agent-sized request body: one long message string.
pub(super) fn upload_body() -> Bytes {
    mock::padded_json(
        r#"{"model":"anthropic/bench","max_tokens":1,"messages":[{"role":"user","content":""#,
        r#""}]}"#,
        UPLOAD_BYTES,
    )
}

/// Agent-sized request body shaped like Claude Code's.
pub(super) fn agent_body() -> Bytes {
    mock::agent_json(UPLOAD_BYTES)
}

/// The agent upload against `target`.
pub(super) async fn upload_agent(target: &Target) -> (Vec<u64>, usize) {
    upload(target, agent_body(), AGENT_UPLOAD_REQUESTS).await
}

/// Sorted latencies of `requests` uploads of `body`, and the number that failed.
pub(super) async fn upload(target: &Target, body: Bytes, requests: usize) -> (Vec<u64>, usize) {
    successes(scenarios::upload(target, UPLOAD_CONCURRENCY, requests, body).await)
}
