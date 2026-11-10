//! Upload scenarios: agent-sized request bodies, as Claude Code resends its
//! whole context every turn.

use bytes::Bytes;

use super::{Direct, successes};
use crate::{
    client::Target,
    gateway_process::GatewayProcess,
    mock, scenarios,
    stats::{Out, phase},
};

pub(crate) const UPLOAD_BYTES: usize = 1024 * 1024;
pub(crate) const UPLOAD_CONCURRENCY: usize = 16;
pub(super) const UPLOAD_REQUESTS: usize = 256;
/// The upload scenario's body is one long string; the agent upload sends a
/// Claude Code-shaped body of the same size, which costs the cache diagnostics
/// scanner more (`mock::agent_json`).
pub(super) const AGENT_UPLOAD_REQUESTS: usize = 128;

/// Added latency and CPU per MB of the upload scenario, and of the agent
/// upload where the path measures it.
pub(super) async fn measure_uploads(direct: &Direct, process: &GatewayProcess, out: &mut Out<'_>) {
    let upload_scenario = ("upload", upload_body(), UPLOAD_REQUESTS, &direct.upload);
    let agent_scenario = direct
        .agent_upload
        .as_ref()
        .map(|samples| ("agent_upload", agent_body(), AGENT_UPLOAD_REQUESTS, samples));
    for (name, body, requests, direct_samples) in
        std::iter::once(upload_scenario).chain(agent_scenario)
    {
        let uploaded_mb = (body.len() * requests) as f64 / 1e6;
        let ((via, failed), cpu) = process
            .cpu_during(phase(
                format!("{}{name}", out.prefix),
                upload(process.target(), body, requests),
            ))
            .await;
        out.put(&format!("{name}_failed_requests"), failed as f64);
        out.put_added(name, &via, direct_samples);
        out.put(
            &format!("{name}_cpu_ms_per_mb"),
            cpu * 1_000.0 / uploaded_mb,
        );
    }
}

/// Agent-sized request body: one long message string.
pub(super) fn upload_body() -> Bytes {
    mock::message_json(UPLOAD_BYTES)
}

/// Agent-sized request body shaped like Claude Code's.
pub(super) fn agent_body() -> Bytes {
    mock::agent_json(UPLOAD_BYTES)
}

/// Sorted latencies of `requests` uploads of `body`, and the number that failed.
pub(super) async fn upload(target: &Target, body: Bytes, requests: usize) -> (Vec<u64>, usize) {
    successes(scenarios::upload(target, UPLOAD_CONCURRENCY, requests, body).await)
}
