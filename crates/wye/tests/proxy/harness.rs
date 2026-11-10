//! Gateway and mock upstream harness shared by the integration tests: the
//! gateway and its client here, the mock in [`upstreams`], gateway tokens in
//! [`tokens`].

mod tokens;
mod upstreams;

pub(crate) use tokens::*;
pub(crate) use upstreams::*;

pub(crate) use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    path::Path,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

pub(crate) use bytes::Bytes;
pub(crate) use gateway_core::token::{self, NewToken};
pub(crate) use gateway_core::{config::Config, prompt::HashKey};
pub(crate) use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
pub(crate) use http_body_util::{BodyExt, Either, Full};
pub(crate) use hyper::body::{Body, Frame, Incoming};
pub(crate) use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
pub(crate) use ring::signature::{Ed25519KeyPair, KeyPair};
pub(crate) use tokio::{
    io::{AsyncBufReadExt, BufReader, DuplexStream, Lines},
    net::TcpListener,
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};
pub(crate) use wye::{Gateway, Resources, UpstreamAuth, UpstreamResources, UsageOut};
pub(crate) use wye_testkit::{TestCa, ed25519_keypair as keypair};

pub(crate) const PROVIDER_KEY: &str = "sk-test-provider-key";
pub(crate) const WAIT: Duration = Duration::from_secs(5);
/// How long a check that something never arrives waits for it: far longer
/// than a request over loopback takes, so a bug shows within it.
pub(crate) const QUIET: Duration = Duration::from_millis(200);

/// Asserts that `rx` receives nothing within [`QUIET`].
pub(crate) async fn assert_quiet<T: std::fmt::Debug>(rx: &mut mpsc::Receiver<T>, why: &str) {
    if let Ok(Some(got)) = tokio::time::timeout(QUIET, rx.recv()).await {
        panic!("{why}: {got:?}");
    }
}

pub(crate) struct Harness {
    pub(crate) gateway: SocketAddr,
    /// The config the gateway serves, for `explain` to answer from.
    pub(crate) config: Config,
    pub(crate) upstream_hits: Arc<AtomicUsize>,
    pub(crate) client: Client<HttpConnector, Full<Bytes>>,
    pub(crate) usage: Mutex<Lines<BufReader<DuplexStream>>>,
    /// Sending (or dropping) triggers the gateway's shutdown.
    pub(crate) shutdown: Option<oneshot::Sender<()>>,
    /// Whether the usage log was written out at shutdown.
    pub(crate) served: JoinHandle<bool>,
}

impl Harness {
    /// Passthrough gateway in front of a plain-HTTP mock upstream: the
    /// caller's own credential goes upstream.
    pub(crate) async fn passthrough(handler: Handler) -> Self {
        let resources = with_upstreams(vec![UpstreamResources::new(UpstreamAuth::ForwardCaller)]);
        let write = |addr: SocketAddr| {
            let entry = "forward_caller_credential = true";
            config(
                "",
                &format!("[[upstreams]]\nbase_url = \"http://{addr}\"\n{entry}\n"),
            )
        };
        Self::configured(handler, write, resources).await
    }

    /// Provider-key gateway in front of a plain mock upstream, with extra
    /// `[server]` lines, extra lines after the upstream's key (further
    /// sections included), and `resources`.
    async fn keyed_with(
        handler: Handler,
        server_extra: &str,
        upstream_extra: &str,
        resources: Resources,
    ) -> Self {
        let write = |addr| config(server_extra, &upstream(addr, upstream_extra));
        Self::configured(handler, write, resources).await
    }

    /// Provider-key gateway in front of a plain mock upstream.
    pub(crate) async fn provider_key(handler: Handler) -> Self {
        Self::keyed(handler, "").await
    }

    /// [`Self::provider_key`], with extra lines in its upstream entry.
    pub(crate) async fn keyed(handler: Handler, upstream_extra: &str) -> Self {
        Self::keyed_with(handler, "", upstream_extra, keyed_resources()).await
    }

    /// [`Self::keyed`] in front of an upstream that answers `200 {}` and
    /// passes `pick` of each request it receives to the returned channel.
    pub(crate) async fn recorded<T: Send + 'static>(
        pick: fn(Seen) -> T,
        upstream_extra: &str,
    ) -> (Self, mpsc::Receiver<T>) {
        let (handler, rx) = recording(pick);
        (Self::keyed(handler, upstream_extra).await, rx)
    }

    /// [`Self::provider_key`], with extra `[server]` lines.
    pub(crate) async fn with_server_config(handler: Handler, server_extra: &str) -> Self {
        Self::keyed_with(handler, server_extra, "", keyed_resources()).await
    }

    /// Provider-key gateway with cache diagnostics on, keyed with `key`, with extra
    /// `[cache_diagnostics]` lines.
    pub(crate) async fn with_cache_diagnostics(
        handler: Handler,
        key: &[u8],
        diagnostics_extra: &str,
    ) -> Self {
        let resources = Resources {
            prompt_key: Some(HashKey::new(key).unwrap()),
            ..keyed_resources()
        };
        let extra =
            format!("[cache_diagnostics]\nhash_key = {{ env = \"UNUSED\" }}\n{diagnostics_extra}");
        Self::keyed_with(handler, "", &extra, resources).await
    }

    /// Gateway in front of a TLS mock upstream; `trusted` adds the mock's CA to Wye's roots.
    pub(crate) async fn start_tls(handler: Handler, ca: &TestCa, trusted: bool) -> Self {
        Self::start_tls_with(handler, ca, trusted, None, "").await
    }

    /// Like `start_tls`, with the mock's HTTP/2 stream limit and extra lines in
    /// the upstream entry.
    pub(crate) async fn start_tls_with(
        handler: Handler,
        ca: &TestCa,
        trusted: bool,
        mock_max_streams: Option<u32>,
        upstream_extra: &str,
    ) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let upstream = spawn_upstream(handler, Arc::clone(&hits), Some(ca), mock_max_streams).await;
        let resources = with_upstreams(vec![UpstreamResources {
            auth: UpstreamAuth::api_key(PROVIDER_KEY).unwrap(),
            extra_roots: if trusted {
                vec![ca.ca_cert()]
            } else {
                Vec::new()
            },
        }]);
        // The entry `upstream` writes, over TLS.
        let entry = format!(
            "[[upstreams]]\nbase_url = \"https://{upstream}\"\n\
             provider_key = {{ env = \"UNUSED\" }}\n{upstream_extra}\n"
        );
        Self::launch_config(&config("", &entry), resources, hits).await
    }

    /// Gateway with one upstream, whose provider (in `upstream_lines`)
    /// makes its only route: the mock serving `handler`, with `auth`.
    pub(crate) async fn single_route(
        handler: Handler,
        server_extra: &str,
        upstream_lines: &str,
        auth: UpstreamAuth,
    ) -> Self {
        let write = |addr| config(server_extra, &upstream(addr, upstream_lines));
        let resources = with_upstreams(vec![UpstreamResources::new(auth)]);
        Self::configured(handler, write, resources).await
    }

    /// Gateway in front of the mock serving `handler`, from the config
    /// `config` writes for the mock's address.
    pub(crate) async fn configured(
        handler: Handler,
        config: impl FnOnce(SocketAddr) -> String,
        resources: Resources,
    ) -> Self {
        let (addr, hits) = spawn_counted(handler).await;
        Self::launch_config(&config(addr), resources, hits).await
    }

    /// Gateway serving the whole TOML document `config`.
    pub(crate) async fn launch_config(
        config: &str,
        resources: Resources,
        upstream_hits: Arc<AtomicUsize>,
    ) -> Self {
        Self::launch_adjusted(config, resources, upstream_hits, |gateway| gateway).await
    }

    /// [`Self::launch_config`], with the gateway changed by `adjust` before
    /// it serves: for a fixed bound a test cannot wait out.
    pub(crate) async fn launch_adjusted(
        config: &str,
        resources: Resources,
        upstream_hits: Arc<AtomicUsize>,
        adjust: impl FnOnce(Gateway) -> Gateway,
    ) -> Self {
        let config = Config::from_toml_str(config).unwrap();
        // Key files in test configs are absolute paths.
        let resources = Resources {
            verifier: wye::verifier(&config, Path::new("/")).unwrap(),
            ..resources
        };
        let gateway = Arc::new(adjust(Gateway::new(&config, resources).unwrap()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (usage_out, usage_in) = tokio::io::duplex(64 * 1024);
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let served = tokio::spawn(wye::serve(
            listener,
            gateway,
            async move {
                let _ = shutdown_rx.await;
            },
            UsageOut::Stream(usage_out),
        ));
        Self {
            gateway: addr,
            config,
            upstream_hits,
            client: Client::builder(TokioExecutor::new()).build_http(),
            usage: Mutex::new(BufReader::new(usage_in).lines()),
            shutdown: Some(shutdown),
            served,
        }
    }

    /// Gateway in front of `handler`, exporting OTLP to `collector` with
    /// `otlp_headers` and `telemetry_extra` lines in `[telemetry]`.
    pub(crate) async fn with_telemetry(
        handler: Handler,
        collector: Handler,
        telemetry_extra: &str,
        otlp_headers: HeaderMap,
    ) -> Self {
        let (collector, _) = spawn_counted(collector).await;
        let resources = Resources {
            otlp_headers,
            ..keyed_resources()
        };
        let telemetry =
            format!("[telemetry]\notlp_endpoint = \"http://{collector}\"\n{telemetry_extra}");
        Self::keyed_with(handler, "", &telemetry, resources).await
    }

    /// Closes the usage log's reading end, as when the process reading
    /// stdout exits.
    pub(crate) async fn close_usage_log(&self) {
        let (_, closed) = tokio::io::duplex(1);
        *self.usage.lock().await = BufReader::new(closed).lines();
    }

    /// Triggers shutdown, as a SIGTERM would.
    pub(crate) fn begin_shutdown(&mut self) {
        let _ = self.shutdown.take().unwrap().send(());
    }

    /// Triggers shutdown and waits for serve to end; returns whether the usage
    /// log was written out.
    pub(crate) async fn shut_down(&mut self) -> bool {
        self.begin_shutdown();
        tokio::time::timeout(WAIT, &mut self.served)
            .await
            .unwrap()
            .unwrap()
    }

    pub(crate) async fn send(
        &self,
        req: http::request::Builder,
        body: impl Into<Bytes>,
    ) -> Response<Incoming> {
        let path = req.uri_ref().unwrap().to_string();
        let req = req
            .uri(format!("http://{}{path}", self.gateway))
            .body(Full::new(body.into()))
            .unwrap();
        tokio::time::timeout(WAIT, self.client.request(req))
            .await
            .unwrap()
            .unwrap()
    }

    /// Sends `chunks` as a body without a declared length, pausing between
    /// them, so the gateway forwards the first before the rest arrive.
    pub(crate) async fn send_chunked(
        &self,
        req: http::request::Builder,
        chunks: &[&[u8]],
    ) -> Response<Incoming> {
        let (tx, rx) = mpsc::channel::<Bytes>(2);
        let client: Client<HttpConnector, ChannelBody> =
            Client::builder(TokioExecutor::new()).build_http();
        let path = req.uri_ref().unwrap().to_string();
        let req = req
            .uri(format!("http://{}{path}", self.gateway))
            .body(ChannelBody(rx))
            .unwrap();
        let response = tokio::spawn(async move { client.request(req).await });
        for (i, chunk) in chunks.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // A refusal may close the request before the last chunk.
            if tx.send(Bytes::copy_from_slice(chunk)).await.is_err() {
                break;
            }
        }
        drop(tx);
        tokio::time::timeout(WAIT, response)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    /// A connection to the gateway that has sent `head`, for requests the
    /// client cannot send.
    pub(crate) async fn raw(&self, head: &[u8]) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;
        let mut stream = tokio::net::TcpStream::connect(self.gateway).await.unwrap();
        stream.write_all(head).await.unwrap();
        stream
    }

    /// Waits, within [`WAIT`], until the upstream has received `n` requests.
    pub(crate) async fn reached(&self, n: usize) {
        let reached = async {
            while self.upstream_hits.load(Ordering::Relaxed) < n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, reached).await.unwrap();
    }

    /// Next usage record written by the gateway.
    pub(crate) async fn usage_record(&self) -> serde_json::Value {
        let line = tokio::time::timeout(WAIT, self.usage.lock().await.next_line())
            .await
            .expect("no usage record")
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

/// What `wye serve` resolves at startup, for tests: the upstreams'
/// credentials in config order, nothing else.
pub(crate) fn with_upstreams(upstreams: Vec<UpstreamResources>) -> Resources {
    Resources {
        upstreams,
        verifier: None,
        prompt_key: None,
        otlp_headers: HeaderMap::new(),
    }
}

/// [`with_upstreams`] of one upstream with the provider key [`PROVIDER_KEY`].
pub(crate) fn keyed_resources() -> Resources {
    with_upstreams(vec![UpstreamResources::new(
        UpstreamAuth::api_key(PROVIDER_KEY).unwrap(),
    )])
}

/// A config on a free loopback port with a 1 KiB body limit, `server_extra`
/// lines in `[server]`, then `rest`: upstream entries and other sections.
pub(crate) fn config(server_extra: &str, rest: &str) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n{server_extra}\n{rest}"
    )
}

/// An `[[upstreams]]` entry for the plain mock at `addr`, with `lines`, its
/// provider key from an unset variable.
pub(crate) fn upstream(addr: SocketAddr, lines: &str) -> String {
    format!(
        "[[upstreams]]\nbase_url = \"http://{addr}\"\nprovider_key = {{ env = \"UNUSED\" }}\n{lines}\n"
    )
}

pub(crate) fn messages() -> http::request::Builder {
    Request::post("/v1/messages?beta=true")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "future-capability-2099,another")
        .header("x-claude-code-session-id", "s-1")
}

/// [`messages`], carrying `token` as a bearer token.
pub(crate) fn bearer(token: &str) -> http::request::Builder {
    messages().header("authorization", format!("Bearer {token}"))
}

/// The next frame of `body`, which must come within [`WAIT`].
pub(crate) async fn next_frame(body: &mut Incoming) -> Frame<Bytes> {
    tokio::time::timeout(WAIT, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

pub(crate) async fn read_body(res: Response<Incoming>) -> Bytes {
    tokio::time::timeout(WAIT, res.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes()
}

/// A refusal as (status, kind, message), `None` for a `200`; the kind is
/// OpenAI's `code`, else Anthropic's `type`.
pub(crate) async fn refusal(res: Response<Incoming>) -> Option<(u16, String, String)> {
    let status = res.status().as_u16();
    let body = read_body(res).await;
    (status != 200).then(|| {
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let error = &error["error"];
        let kind = error["code"].as_str().or(error["type"].as_str()).unwrap();
        let message = error["message"].as_str().unwrap();
        (status, kind.to_owned(), message.to_owned())
    })
}

/// `explain`'s refusal with its kind owned, to compare with [`refusal`]'s.
pub(crate) fn owned(refused: Option<(u16, &str, String)>) -> Option<(u16, String, String)> {
    refused.map(|(status, kind, message)| (status, kind.to_owned(), message))
}
