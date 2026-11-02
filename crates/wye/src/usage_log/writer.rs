//! The writer task: formats queued records into the JSON-lines log and,
//! with `[telemetry]`, exports them over OTLP, without letting a blocked log
//! stop the export.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

use super::{
    QUEUE_CAPACITY, Record, Sinks,
    line::{self, line, push_line},
    metrics, otlp, unix_nanos,
};

/// Queued records batched into one write at most: bounds the write buffer,
/// and lets the export ticks run while records keep arriving. While a write
/// waits with OTLP on, up to [`QUEUE_CAPACITY`] records taken for export are
/// kept for the next write.
const WRITE_BATCH: usize = 256;
/// A partly filled OTLP batch is sent after at most this long.
const OTLP_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
/// Cumulative OTLP metrics are exported this often.
const OTLP_METRICS_INTERVAL: Duration = Duration::from_secs(10);

pub(super) async fn write_records<W>(
    mut rx: mpsc::Receiver<Record>,
    mut out: W,
    dropped: Arc<AtomicU64>,
    sinks: Sinks,
) where
    W: AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(4096);
    let context = line::Context {
        prompt_key: sinks.prompt_key.as_deref(),
        pricing: sinks.pricing.as_deref(),
        upgrade_targets: sinks.upgrade_targets.as_deref(),
    };
    // The exporter task is owned here and finishes its queue after the loop.
    let (mut otlp, exporter) = match sinks.otlp {
        Some(target) => {
            let (batch, exporter) = otlp::Batch::start(target);
            let metrics = metrics::Metrics::new(unix_nanos());
            (Some((batch, metrics)), Some(exporter))
        }
        None => (None, None),
    };
    let export_metrics = |batch: &otlp::Batch, metrics: &metrics::Metrics| {
        let lost = batch
            .dropped
            .load(Ordering::Relaxed)
            .saturating_add(dropped.load(Ordering::Relaxed));
        let body = metrics.encode(
            &batch.target.resource,
            unix_nanos(),
            sinks.active.current(),
            lost,
        );
        batch.send_metrics(body);
    };
    // The first ticks come one period after start, not immediately.
    let after = |period| tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    let mut logs_tick = after(OTLP_FLUSH_INTERVAL);
    logs_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut metrics_tick = after(OTLP_METRICS_INTERVAL);
    metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let flush = |otlp: &mut Option<(otlp::Batch, metrics::Metrics)>| {
        if let Some((batch, _)) = otlp.as_mut() {
            batch.flush();
        }
    };
    let to_otlp = |otlp: &mut Option<(otlp::Batch, metrics::Metrics)>, line: &line::Line<'_>| {
        if let Some((batch, metrics)) = otlp.as_mut() {
            metrics.record(line);
            batch.push(line);
            if batch.is_full() {
                batch.flush();
            }
        }
    };
    let export = |otlp: &Option<(otlp::Batch, metrics::Metrics)>| {
        if let Some((batch, metrics)) = otlp.as_ref() {
            export_metrics(batch, metrics);
        }
    };
    // A log that failed a write (its reader is gone) is not written again;
    // OTLP export goes on without it.
    let mut log_open = true;
    // Records taken while the log's write waits, for OTLP: kept for the next
    // write up to the queue's capacity, as many as the queue would have held
    // for the log without OTLP.
    let mut spill = Vec::new();
    let mut spilled: u64 = 0;
    let mut queue_open = true;
    loop {
        // Cancel safety: `recv` and the ticks lose nothing when another branch wins.
        let record = tokio::select! {
            record = rx.recv() => record,
            _ = logs_tick.tick(), if otlp.is_some() => { flush(&mut otlp); continue; }
            _ = metrics_tick.tick(), if otlp.is_some() => { export(&otlp); continue; }
        };
        let Some(record) = record else { break };
        let mut next = Some(record);
        let mut lines: u64 = 0;
        // Batch what else is queued into one write.
        for _ in 0..WRITE_BATCH {
            let Some(r) = next.take().or_else(|| rx.try_recv().ok()) else {
                break;
            };
            let line = line(&r, &context);
            push_line(&mut buf, &line);
            lines += 1;
            to_otlp(&mut otlp, &line);
        }
        while !buf.is_empty() {
            if log_open {
                // A log that blocks (a full pipe nobody reads) must not stop
                // OTLP export: records and ticks are served while the write
                // waits. Cancel safety: the write is never dropped before it
                // completes, and `recv` loses nothing.
                let write = async {
                    out.write_all(&buf).await?;
                    out.flush().await
                };
                tokio::pin!(write);
                log_open = loop {
                    tokio::select! {
                        written = &mut write => break written.is_ok(),
                        record = rx.recv(), if queue_open && otlp.is_some() => {
                            let Some(r) = record else {
                                // Shutdown: export what was counted now,
                                // not after a write that may never end.
                                queue_open = false;
                                flush(&mut otlp);
                                export(&otlp);
                                continue;
                            };
                            let line = line(&r, &context);
                            to_otlp(&mut otlp, &line);
                            if spilled < QUEUE_CAPACITY as u64 {
                                push_line(&mut spill, &line);
                                spilled += 1;
                            } else {
                                dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        _ = logs_tick.tick(), if otlp.is_some() => flush(&mut otlp),
                        _ = metrics_tick.tick(), if otlp.is_some() => export(&otlp),
                    }
                };
            }
            if !log_open {
                // Counted like a full queue's losses, so OTLP reports them.
                dropped.fetch_add(lines, Ordering::Relaxed);
            }
            buf.clear();
            std::mem::swap(&mut buf, &mut spill);
            lines = std::mem::take(&mut spilled);
        }
    }
    // The log's own losses, written before the exporter's last exports,
    // which a slow collector can hold past shutdown's flush.
    let count = dropped.load(Ordering::Relaxed);
    if count > 0 && log_open {
        let line = format!("{{\"event\":\"usage_log_dropped\",\"count\":{count}}}\n");
        let _ = out.write_all(line.as_bytes()).await;
        let _ = out.flush().await;
    }
    if let Some((mut batch, metrics)) = otlp {
        batch.flush();
        export_metrics(&batch, &metrics);
        let otlp_lost = Arc::clone(&batch.dropped);
        drop(batch);
        if let Some(exporter) = exporter {
            let _ = exporter.await;
        }
        // After the exporter: the last exports' failures count too.
        let otlp_dropped = otlp_lost.load(Ordering::Relaxed);
        if otlp_dropped > 0 && log_open {
            let line = format!("{{\"event\":\"otlp_dropped\",\"records\":{otlp_dropped}}}\n");
            let _ = out.write_all(line.as_bytes()).await;
        }
    }
    let _ = out.flush().await;
}
