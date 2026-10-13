//! Wye benchmark and regression gate.
//!
//! Runs the same scenarios directly against a mock Anthropic upstream and
//! through a real `wye` binary, then reports Wye's added cost:
//!
//! - `unary_*`: added latency for small non-streaming requests.
//! - `sse_event_*`: added delay per streamed SSE event, measured from the
//!   timestamp the mock writes into each event.
//! - `sse_ttfb_*`: added time to the first streamed byte.
//! - `throughput_*`: Wye CPU per streamed MB and achieved MB/s.
//! - `idle_rss_kb_per_stream`: Wye memory per open, idle stream.
//! - `cancel_upstream_release_*`: time from a client disconnect to Wye
//!   releasing the upstream stream, while the upstream is silent.
//!
//! `compare` fails when any gated metric is worse than the baseline beyond its
//! tolerance. Mock, load client, and Wye share one machine, so absolute numbers
//! are only comparable between runs on the same machine.
//!
//! Usage:
//!   wye-bench run --wye <path-to-wye> --out <results.json>
//!   wye-bench compare <baseline.json> <results.json>
//!   wye-bench adopt-new <baseline.json> <results.json>

// Development tool, not on the request path: panics abort a benchmark run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::{
    collections::BTreeMap,
    convert::Infallible,
    net::SocketAddr,
    pin::Pin,
    process::Stdio,
    sync::OnceLock,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body_util::{BodyExt, Either, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, process::Child, task::JoinSet, time::Sleep};

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Receives the time each mock SSE body is dropped, so the cancel scenario can
/// see when Wye released the upstream stream.
static BODY_DROPPED: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<u64>>> =
    std::sync::Mutex::new(None);

fn now_nanos() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

// ---------------------------------------------------------------- mock upstream

/// SSE body that emits `remaining` events of `size` payload bytes every `interval`.
struct SseBody {
    remaining: u32,
    payload: Bytes,
    interval: Duration,
    sleep: Option<Pin<Box<Sleep>>>,
    /// Report the drop time (cancel scenario only).
    report_drop: bool,
}

impl Drop for SseBody {
    fn drop(&mut self) {
        if self.report_drop
            && let Some(tx) = BODY_DROPPED.lock().unwrap().as_ref()
        {
            let _ = tx.send(now_nanos());
        }
    }
}

impl Body for SseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        if let Some(sleep) = self.sleep.as_mut()
            && sleep.as_mut().poll(cx).is_pending()
        {
            return Poll::Pending;
        }
        self.remaining -= 1;
        let interval = self.interval;
        if !interval.is_zero() && self.remaining > 0 {
            self.sleep = Some(Box::pin(tokio::time::sleep(interval)));
        }
        let mut event = format!(
            "event: content_block_delta\ndata: {{\"t\":{},\"p\":\"",
            now_nanos()
        )
        .into_bytes();
        event.extend_from_slice(&self.payload);
        event.extend_from_slice(b"\"}\n\n");
        Poll::Ready(Some(Ok(Frame::data(Bytes::from(event)))))
    }
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

async fn mock_handle(
    req: Request<Incoming>,
) -> Result<Response<Either<Full<Bytes>, SseBody>>, Infallible> {
    let headers = req.headers().clone();
    // Drain the request body like a real upstream would.
    let _ = req.into_body().collect().await;
    if let Some(size) = header_u64(&headers, "x-bench-unary") {
        let body = Bytes::from(vec![b'x'; size as usize]);
        return Ok(Response::new(Either::Left(Full::new(body))));
    }
    let size = header_u64(&headers, "x-bench-size").unwrap_or(64) as usize;
    let mut res = Response::new(Either::Right(SseBody {
        remaining: header_u64(&headers, "x-bench-events").unwrap_or(1) as u32,
        payload: Bytes::from(vec![b'x'; size]),
        interval: Duration::from_micros(header_u64(&headers, "x-bench-interval-us").unwrap_or(0)),
        sleep: None,
        report_drop: headers.contains_key("x-bench-report-drop"),
    }));
    res.headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    Ok(res)
}

async fn spawn_mock() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let _ = stream.set_nodelay(true);
            tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(
                TokioIo::new(stream),
                hyper::service::service_fn(mock_handle),
            ));
        }
    });
    addr
}

// ---------------------------------------------------------------- wye process

struct WyeProcess {
    /// Held so the process lives as long as this value; `kill_on_drop` stops it.
    _child: Child,
    addr: SocketAddr,
    pid: u32,
}

impl WyeProcess {
    async fn start(binary: &str, upstream: SocketAddr, dir: &std::path::Path) -> Self {
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let config = dir.join("bench-wye.toml");
        std::fs::write(
            &config,
            format!(
                "[server]\nlisten = \"127.0.0.1:{port}\"\nshutdown_grace_secs = 0\n\n\
                 [upstream]\nbase_url = \"http://{upstream}\"\nforward_caller_credential = true\n"
            ),
        )
        .unwrap();
        let child = tokio::process::Command::new(binary)
            .args(["serve", "--config"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {binary}: {e}"));
        let pid = child.id().unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let client = client();
        for _ in 0..200 {
            let req = Request::get(format!("http://{addr}/healthz"))
                .body(Full::new(Bytes::new()))
                .unwrap();
            if client.request(req).await.is_ok() {
                return Self {
                    _child: child,
                    addr,
                    pid,
                };
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("wye did not become healthy");
    }

    fn ps(&self, field: &str) -> String {
        let out = std::process::Command::new("ps")
            .args(["-o", &format!("{field}="), "-p", &self.pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn rss_kb(&self) -> f64 {
        self.ps("rss").parse().unwrap()
    }

    /// Total CPU time (user + system) in seconds, from `ps -o time` (`[[dd-]hh:]mm:ss.cc`).
    fn cpu_secs(&self) -> f64 {
        let raw = self.ps("time");
        let (days, rest) = raw
            .split_once('-')
            .map_or((0.0, raw.as_str()), |(d, r)| (d.parse().unwrap(), r));
        rest.split(':')
            .fold(0.0, |acc, part| acc * 60.0 + part.parse::<f64>().unwrap())
            + days * 86_400.0
    }
}

// ---------------------------------------------------------------- load client

type BenchClient = Client<HttpConnector, Full<Bytes>>;

fn client() -> BenchClient {
    let mut http = HttpConnector::new();
    http.set_nodelay(true);
    Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(4096)
        .build(http)
}

fn request(target: SocketAddr, headers: &[(&'static str, String)]) -> Request<Full<Bytes>> {
    let mut req = Request::post(format!("http://{target}/v1/messages?beta=true"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01");
    for (name, value) in headers {
        req = req.header(*name, value.as_str());
    }
    req.body(Full::new(Bytes::from_static(
        br#"{"model":"bench","max_tokens":1,"messages":[]}"#,
    )))
    .unwrap()
}

/// Runs `total` requests with `concurrency` workers; returns per-request results.
async fn run_workers<T: Send + 'static, F, Fut>(concurrency: usize, total: usize, f: F) -> Vec<T>
where
    F: Fn() -> Fut + Clone + Send + 'static,
    Fut: Future<Output = T> + Send,
{
    let mut set = JoinSet::new();
    for worker in 0..concurrency {
        let f = f.clone();
        let count = total / concurrency + usize::from(worker < total % concurrency);
        set.spawn(async move {
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                out.push(f().await);
            }
            out
        });
    }
    let mut all = Vec::with_capacity(total);
    while let Some(part) = set.join_next().await {
        all.extend(part.unwrap());
    }
    all
}

async fn unary(target: SocketAddr, concurrency: usize, total: usize) -> Vec<u64> {
    let client = client();
    run_workers(concurrency, total, move || {
        let client = client.clone();
        async move {
            let start = Instant::now();
            let res = client
                .request(request(target, &[("x-bench-unary", "512".into())]))
                .await
                .unwrap();
            res.into_body().collect().await.unwrap();
            start.elapsed().as_micros() as u64
        }
    })
    .await
}

/// Per stream: (time to first byte in µs, per-event delays in µs, bytes received).
async fn sse(
    target: SocketAddr,
    concurrency: usize,
    total: usize,
    events: u32,
    size: usize,
    interval_us: u64,
) -> Vec<(u64, Vec<u64>, u64)> {
    let client = client();
    run_workers(concurrency, total, move || {
        let client = client.clone();
        async move {
            let start = Instant::now();
            let res = client
                .request(request(
                    target,
                    &[
                        ("x-bench-events", events.to_string()),
                        ("x-bench-size", size.to_string()),
                        ("x-bench-interval-us", interval_us.to_string()),
                    ],
                ))
                .await
                .unwrap();
            let mut body = res.into_body();
            let mut ttfb = None;
            let mut delays = Vec::with_capacity(events as usize);
            let mut pending: Vec<u8> = Vec::new();
            let mut bytes = 0u64;
            while let Some(frame) = body.frame().await {
                let Ok(data) = frame.unwrap().into_data() else {
                    continue;
                };
                let received = now_nanos();
                ttfb.get_or_insert_with(|| start.elapsed().as_micros() as u64);
                bytes += data.len() as u64;
                pending.extend_from_slice(&data);
                while let Some(end) = find(&pending, b"\n\n") {
                    let event: Vec<u8> = pending.drain(..end + 2).collect();
                    if let Some(sent) = event_timestamp(&event) {
                        delays.push(received.saturating_sub(sent) / 1_000);
                    }
                }
            }
            (ttfb.unwrap_or(0), delays, bytes)
        }
    })
    .await
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn event_timestamp(event: &[u8]) -> Option<u64> {
    let start = find(event, b"\"t\":")? + 4;
    let digits = event[start..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    std::str::from_utf8(&event[start..start + digits])
        .ok()?
        .parse()
        .ok()
}

/// Opens `count` streams that stay open and idle, then returns the held bodies.
async fn open_idle_streams(target: SocketAddr, count: usize) -> Vec<Incoming> {
    let client = client();
    let mut set = JoinSet::new();
    for _ in 0..count {
        let client = client.clone();
        set.spawn(async move {
            let res = client
                .request(request(
                    target,
                    &[
                        ("x-bench-events", "2".into()),
                        ("x-bench-size", "16".into()),
                        ("x-bench-interval-us", "60000000".into()),
                    ],
                ))
                .await
                .unwrap();
            let mut body = res.into_body();
            body.frame().await.unwrap().unwrap();
            body
        });
    }
    let mut bodies = Vec::with_capacity(count);
    while let Some(body) = set.join_next().await {
        bodies.push(body.unwrap());
    }
    bodies
}

/// Time from the client dropping a stream to the upstream body being dropped, in µs.
///
/// The upstream sends one event and then pauses for 60 s, like a model thinking,
/// so the gateway has no write on which to notice the disconnect. Runs
/// sequentially so each drop report belongs to the current stream.
async fn cancel_latency(target: SocketAddr, rounds: usize) -> Vec<u64> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // Each call installs its own channel; the mock lives for the whole process.
    *BODY_DROPPED.lock().unwrap() = Some(tx);
    let mut out = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        // A fresh client per round: dropping a pooled connection's body would otherwise
        // only return the connection to the pool.
        let res = client()
            .request(request(
                target,
                &[
                    ("x-bench-events", "2".into()),
                    ("x-bench-size", "16".into()),
                    ("x-bench-interval-us", "60000000".into()),
                    ("x-bench-report-drop", "1".into()),
                ],
            ))
            .await
            .unwrap();
        let mut body = res.into_body();
        body.frame().await.unwrap().unwrap();
        let cancelled = now_nanos();
        drop(body);
        let latency = match tokio::time::timeout(CANCEL_TIMEOUT, rx.recv()).await {
            Ok(Some(dropped)) => dropped.saturating_sub(cancelled) / 1_000,
            _ => CANCEL_TIMEOUT.as_micros() as u64,
        };
        out.push(latency);
        // Drain reports from a late drop so the next round starts clean.
        while rx.try_recv().is_ok() {}
    }
    out
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    sorted[((p / 100.0) * (sorted.len() - 1) as f64).round() as usize] as f64
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

// ---------------------------------------------------------------- scenarios

const UNARY_CONCURRENCY: usize = 32;
const UNARY_TOTAL: usize = 20_000;
const SSE_CONCURRENCY: usize = 64;
const SSE_STREAMS: usize = 640;
const SSE_EVENTS: u32 = 50;
const SSE_INTERVAL_US: u64 = 2_000;
const THROUGHPUT_CONCURRENCY: usize = 8;
const THROUGHPUT_STREAMS: usize = 32;
const THROUGHPUT_EVENTS: u32 = 2_000;
const THROUGHPUT_EVENT_BYTES: usize = 4_096;
const IDLE_STREAMS: usize = 1_000;
const CANCEL_ROUNDS: usize = 20;
/// Reported when the upstream is not released at all within this time.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

struct Measured {
    unary: Vec<u64>,
    ttfb: Vec<u64>,
    event_delay: Vec<u64>,
}

async fn measure(target: SocketAddr) -> Measured {
    // Warm up connection pools and allocators.
    unary(target, UNARY_CONCURRENCY, 2_000).await;
    sse(target, SSE_CONCURRENCY, 128, 5, 64, 0).await;

    let unary = sorted(unary(target, UNARY_CONCURRENCY, UNARY_TOTAL).await);
    let streams = sse(
        target,
        SSE_CONCURRENCY,
        SSE_STREAMS,
        SSE_EVENTS,
        128,
        SSE_INTERVAL_US,
    )
    .await;
    let ttfb = sorted(streams.iter().map(|s| s.0).collect());
    let event_delay = sorted(streams.into_iter().flat_map(|s| s.1).collect());
    Measured {
        unary,
        ttfb,
        event_delay,
    }
}

/// Returns (MB/s, MB streamed).
async fn throughput(target: SocketAddr) -> (f64, f64) {
    let start = Instant::now();
    let big = sse(
        target,
        THROUGHPUT_CONCURRENCY,
        THROUGHPUT_STREAMS,
        THROUGHPUT_EVENTS,
        THROUGHPUT_EVENT_BYTES,
        0,
    )
    .await;
    let secs = start.elapsed().as_secs_f64();
    let mb = big.iter().map(|s| s.2).sum::<u64>() as f64 / 1e6;
    (mb / secs, mb)
}

/// Runs every scenario `repeat` times and reports the per-metric median.
async fn run(binary: &str, out: &str, repeat: usize) {
    let mock = spawn_mock().await;
    let dir = std::path::Path::new(out)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .to_owned();
    std::fs::create_dir_all(&dir).unwrap();
    let mut runs = Vec::with_capacity(repeat);
    for i in 1..=repeat {
        eprintln!("run {i}/{repeat}");
        // A fresh process per run, so memory retained from earlier runs cannot hide new allocations.
        let wye = WyeProcess::start(binary, mock, &dir).await;
        runs.push(once(mock, &wye).await);
    }
    let metrics: BTreeMap<String, f64> = runs[0]
        .keys()
        .map(|name| {
            let mut values: Vec<f64> = runs.iter().map(|r| r[name]).collect();
            values.sort_by(f64::total_cmp);
            (name.clone(), values[values.len() / 2])
        })
        .collect();

    let report = json!({
        "machine": machine(),
        "repeat": repeat,
        "scenarios": {
            "unary": {"concurrency": UNARY_CONCURRENCY, "requests": UNARY_TOTAL, "response_bytes": 512},
            "sse": {"concurrency": SSE_CONCURRENCY, "streams": SSE_STREAMS, "events": SSE_EVENTS, "interval_us": SSE_INTERVAL_US},
            "throughput": {"concurrency": THROUGHPUT_CONCURRENCY, "streams": THROUGHPUT_STREAMS, "events": THROUGHPUT_EVENTS, "event_bytes": THROUGHPUT_EVENT_BYTES},
            "idle": {"streams": IDLE_STREAMS},
        },
        "metrics": metrics,
    });
    std::fs::write(out, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
    print_metrics(&report["metrics"]);
}

/// One measurement of every scenario, direct and through Wye back to back.
async fn once(mock: SocketAddr, wye: &WyeProcess) -> BTreeMap<String, f64> {
    let direct = measure(mock).await;
    let (direct_mb_s, _) = throughput(mock).await;

    let via = measure(wye.addr).await;
    let cpu_before = wye.cpu_secs();
    let (wye_mb_s, wye_mb) = throughput(wye.addr).await;
    let cpu_after = wye.cpu_secs();

    tokio::time::sleep(Duration::from_millis(500)).await;
    let rss_before = wye.rss_kb();
    let held = open_idle_streams(wye.addr, IDLE_STREAMS).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rss_open = wye.rss_kb();
    drop(held);

    let mut metrics = BTreeMap::new();
    let mut put = |name: &str, value: f64| {
        metrics.insert(name.to_owned(), (value * 100.0).round() / 100.0);
    };
    for (label, p) in [("p50", 50.0), ("p99", 99.0)] {
        put(
            &format!("direct_unary_{label}_us"),
            percentile(&direct.unary, p),
        );
        put(&format!("wye_unary_{label}_us"), percentile(&via.unary, p));
        put(
            &format!("unary_added_{label}_us"),
            percentile(&via.unary, p) - percentile(&direct.unary, p),
        );
        put(
            &format!("sse_ttfb_added_{label}_us"),
            percentile(&via.ttfb, p) - percentile(&direct.ttfb, p),
        );
        put(
            &format!("sse_event_added_{label}_us"),
            percentile(&via.event_delay, p) - percentile(&direct.event_delay, p),
        );
    }
    put("direct_throughput_mb_s", direct_mb_s);
    put("wye_throughput_mb_s", wye_mb_s);
    // `ps` reports CPU time in 10 ms steps; the scenario streams enough data to make that negligible.
    put(
        "throughput_cpu_ms_per_mb",
        (cpu_after - cpu_before) * 1_000.0 / wye_mb,
    );
    put(
        "idle_rss_kb_per_stream",
        (rss_open - rss_before) / IDLE_STREAMS as f64,
    );
    put("wye_rss_idle_kb", rss_before);
    let cancel = sorted(cancel_latency(wye.addr, CANCEL_ROUNDS).await);
    put("cancel_upstream_release_p50_us", percentile(&cancel, 50.0));
    put("cancel_upstream_release_max_us", percentile(&cancel, 100.0));
    metrics
}

fn machine() -> Value {
    let sysctl = |key: &str| {
        std::process::Command::new("sysctl")
            .args(["-n", key])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    };
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu": sysctl("machdep.cpu.brand_string"),
        "cpus": std::thread::available_parallelism().map_or(0, |n| n.get()),
    })
}

fn print_metrics(metrics: &Value) {
    for (name, value) in metrics.as_object().unwrap() {
        println!("{name:32} {value}");
    }
}

// ---------------------------------------------------------------- regression gate

/// (metric, relative tolerance, absolute tolerance). Lower is better for all of them.
///
/// Tolerances come from the spread of repeated runs on the baseline machine.
/// `sse_ttfb_added_p99_us` is reported but not gated: it swung from -521 to
/// 772 µs between identical runs.
const GATES: &[(&str, f64, f64)] = &[
    ("unary_added_p50_us", 0.20, 40.0),
    ("unary_added_p99_us", 0.50, 400.0),
    ("sse_ttfb_added_p50_us", 0.40, 100.0),
    ("sse_event_added_p50_us", 0.20, 40.0),
    ("sse_event_added_p99_us", 0.50, 150.0),
    ("throughput_cpu_ms_per_mb", 0.12, 0.05),
    ("idle_rss_kb_per_stream", 0.10, 2.0),
    // Guards the property, not the microseconds: a release that waits for the
    // next upstream write would take seconds.
    ("cancel_upstream_release_max_us", 1.0, 20_000.0),
];

fn compare(baseline: &str, current: &str) -> bool {
    let load = |path: &str| -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}")),
        )
        .unwrap()
    };
    let (base, cur) = (load(baseline), load(current));
    if base["machine"] != cur["machine"] {
        eprintln!(
            "warning: baseline was recorded on a different machine; comparison is unreliable"
        );
    }
    let mut ok = true;
    println!(
        "{:32} {:>10} {:>10} {:>10}  result",
        "metric", "baseline", "current", "limit"
    );
    for &(name, rel, abs) in GATES {
        let (Some(b), Some(c)) = (
            base["metrics"][name].as_f64(),
            cur["metrics"][name].as_f64(),
        ) else {
            println!("{name:32} missing");
            ok = false;
            continue;
        };
        // Added-latency metrics can be near zero or negative (noise); the absolute floor absorbs that.
        let limit = b.max(0.0) * (1.0 + rel) + abs;
        let pass = c <= limit;
        ok &= pass;
        println!(
            "{name:32} {b:>10.2} {c:>10.2} {limit:>10.2}  {}",
            if pass { "ok" } else { "REGRESSED" }
        );
    }
    ok
}

/// Copies metrics that exist in `current` but not in `baseline` into the baseline.
///
/// Used when a new metric is added: existing baseline values stay untouched, so
/// adding a metric never loosens an existing gate.
fn adopt_new(baseline: &str, current: &str) {
    let mut base: Value =
        serde_json::from_str(&std::fs::read_to_string(baseline).unwrap()).unwrap();
    let cur: Value = serde_json::from_str(&std::fs::read_to_string(current).unwrap()).unwrap();
    let metrics = base["metrics"].as_object_mut().unwrap();
    for (name, value) in cur["metrics"].as_object().unwrap() {
        if !metrics.contains_key(name) {
            println!("adopted {name} = {value}");
            metrics.insert(name.clone(), value.clone());
        }
    }
    std::fs::write(
        baseline,
        serde_json::to_string_pretty(&base).unwrap() + "\n",
    )
    .unwrap();
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["run", "--wye", binary, "--out", out, rest @ ..] => {
            let repeat = match rest {
                [] => 3,
                ["--repeat", n] => n.parse().expect("--repeat takes a positive integer"),
                _ => return usage(),
            };
            assert!(repeat > 0, "--repeat must be positive");
            EPOCH.get_or_init(Instant::now);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run(binary, out, repeat));
            std::process::ExitCode::SUCCESS
        }
        ["adopt-new", baseline, current] => {
            adopt_new(baseline, current);
            std::process::ExitCode::SUCCESS
        }
        ["compare", baseline, current] => {
            if compare(baseline, current) {
                std::process::ExitCode::SUCCESS
            } else {
                eprintln!("benchmark regression: do not commit this change");
                std::process::ExitCode::FAILURE
            }
        }
        _ => usage(),
    }
}

fn usage() -> std::process::ExitCode {
    eprintln!(
        "usage:\n  wye-bench run --wye <path> --out <results.json> [--repeat N]\n  wye-bench compare <baseline.json> <results.json>\n  wye-bench adopt-new <baseline.json> <results.json>"
    );
    std::process::ExitCode::from(2)
}
