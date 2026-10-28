//! Gateway and mock upstream harness shared by the integration tests.

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
pub(crate) use gateway_core::{config::Config, prompt::HashKey};
pub(crate) use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
pub(crate) use http_body_util::{BodyExt, Either, Full};
pub(crate) use hyper::body::{Body, Frame, Incoming};
pub(crate) use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
pub(crate) use tokio::{
    io::{AsyncBufReadExt, BufReader, DuplexStream, Lines},
    net::TcpListener,
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};
pub(crate) use wye::{Gateway, Resources, UpstreamAuth, UpstreamResources};
pub(crate) use wye_testkit::TestCa;

pub(crate) const API_KEY: &str = "sk-test-provider-key";
pub(crate) const WAIT: Duration = Duration::from_secs(5);

/// Upstream response body that yields chunks as the test sends them.
pub(crate) struct ChannelBody(pub(crate) mpsc::Receiver<Bytes>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|b| Ok(Frame::data(b))))
    }
}

pub(crate) type UpBody = Either<Full<Bytes>, ChannelBody>;

pub(crate) struct Seen {
    pub(crate) version: Version,
    pub(crate) method: Method,
    pub(crate) uri: Uri,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

pub(crate) type Handler =
    Arc<dyn Fn(Seen) -> Pin<Box<dyn Future<Output = Response<UpBody>> + Send>> + Send + Sync>;

pub(crate) struct Harness {
    pub(crate) gateway: SocketAddr,
    pub(crate) upstream_hits: Arc<AtomicUsize>,
    pub(crate) client: Client<HttpConnector, Full<Bytes>>,
    pub(crate) usage: Mutex<Lines<BufReader<DuplexStream>>>,
    /// Sending (or dropping) triggers the gateway's shutdown.
    pub(crate) shutdown: Option<oneshot::Sender<()>>,
    pub(crate) served: JoinHandle<()>,
}

impl Harness {
    /// Gateway in front of a plain-HTTP mock upstream.
    pub(crate) async fn start(handler: Handler, upstream_extra: &str, auth: UpstreamAuth) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
        let resources = with_upstreams(vec![UpstreamResources::new(auth)]);
        Self::launch(
            &format!("http://{upstream}"),
            "",
            upstream_extra,
            resources,
            hits,
        )
        .await
    }

    /// API-key gateway in front of a plain mock upstream, with extra `[server]` lines.
    pub(crate) async fn with_server_config(handler: Handler, server_extra: &str) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
        let resources = with_upstreams(vec![UpstreamResources::new(
            UpstreamAuth::api_key(API_KEY).unwrap(),
        )]);
        let base_url = format!("http://{upstream}");
        Self::launch(
            &base_url,
            server_extra,
            "api_key_env = \"UNUSED\"",
            resources,
            hits,
        )
        .await
    }

    /// API-key gateway with cache diagnostics on, keyed with `key`, with extra
    /// `[cache_diagnostics]` lines.
    pub(crate) async fn with_cache_diagnostics(
        handler: Handler,
        key: &[u8],
        diagnostics_extra: &str,
    ) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
        let resources = Resources {
            upstreams: vec![UpstreamResources::new(
                UpstreamAuth::api_key(API_KEY).unwrap(),
            )],
            verifier: None,
            prompt_key: Some(HashKey::new(key).unwrap()),
            otlp_headers: HeaderMap::new(),
        };
        let base_url = format!("http://{upstream}");
        let extra = format!(
            "api_key_env = \"UNUSED\"\n[cache_diagnostics]\nhash_key_env = \"UNUSED\"\n{diagnostics_extra}"
        );
        Self::launch(&base_url, "", &extra, resources, hits).await
    }

    /// Gateway in front of a TLS mock upstream; `trusted` adds the mock's CA to Wye's roots.
    pub(crate) async fn start_tls(handler: Handler, ca: &TestCa, trusted: bool) -> Self {
        Self::start_tls_with(handler, ca, trusted, None, "").await
    }

    /// Like `start_tls`, with the mock's HTTP/2 stream limit and extra `[upstream]` lines.
    pub(crate) async fn start_tls_with(
        handler: Handler,
        ca: &TestCa,
        trusted: bool,
        mock_max_streams: Option<u32>,
        upstream_extra: &str,
    ) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let upstream = spawn_upstream(handler, Arc::clone(&hits), Some(ca), mock_max_streams).await;
        let resources = Resources {
            upstreams: vec![UpstreamResources {
                auth: UpstreamAuth::api_key(API_KEY).unwrap(),
                extra_roots: if trusted {
                    vec![ca.ca_cert()]
                } else {
                    Vec::new()
                },
            }],
            verifier: None,
            prompt_key: None,
            otlp_headers: HeaderMap::new(),
        };
        let base_url = format!("https://{upstream}");
        let extra = format!("api_key_env = \"UNUSED\"\n{upstream_extra}");
        Self::launch(&base_url, "", &extra, resources, hits).await
    }

    pub(crate) async fn launch(
        base_url: &str,
        server_extra: &str,
        upstream_extra: &str,
        resources: Resources,
        upstream_hits: Arc<AtomicUsize>,
    ) -> Self {
        let config = format!(
            "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n{server_extra}\n\
             [upstream]\nbase_url = \"{base_url}\"\n{upstream_extra}"
        );
        Self::launch_config(&config, resources, upstream_hits).await
    }

    /// Gateway serving the whole TOML document `config`.
    pub(crate) async fn launch_config(
        config: &str,
        resources: Resources,
        upstream_hits: Arc<AtomicUsize>,
    ) -> Self {
        let config = Config::from_toml_str(config).unwrap();
        // Key files in test configs are absolute paths.
        let resources = Resources {
            verifier: wye::verifier(&config, Path::new("/")).unwrap(),
            ..resources
        };
        let gateway = Arc::new(Gateway::new(&config, resources).unwrap());
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
            usage_out,
        ));
        Self {
            gateway: addr,
            upstream_hits,
            client: Client::builder(TokioExecutor::new()).build_http(),
            usage: Mutex::new(BufReader::new(usage_in).lines()),
            shutdown: Some(shutdown),
            served,
        }
    }

    /// Triggers shutdown, as a SIGTERM would.
    pub(crate) fn begin_shutdown(&mut self) {
        let _ = self.shutdown.take().unwrap().send(());
    }

    pub(crate) async fn api_key(handler: Handler) -> Self {
        Self::start(
            handler,
            "api_key_env = \"UNUSED\"",
            UpstreamAuth::api_key(API_KEY).unwrap(),
        )
        .await
    }

    pub(crate) async fn send(
        &self,
        req: http::request::Builder,
        body: &'static str,
    ) -> Response<Incoming> {
        let path = req.uri_ref().unwrap().to_string();
        let req = req
            .uri(format!("http://{}{path}", self.gateway))
            .body(Full::new(Bytes::from_static(body.as_bytes())))
            .unwrap();
        tokio::time::timeout(WAIT, self.client.request(req))
            .await
            .unwrap()
            .unwrap()
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

pub(crate) async fn spawn_upstream(
    handler: Handler,
    hits: Arc<AtomicUsize>,
    tls: Option<&TestCa>,
    max_streams: Option<u32>,
) -> SocketAddr {
    let serve = move |req: Request<Incoming>| {
        let handler = Arc::clone(&handler);
        hits.fetch_add(1, Ordering::Relaxed);
        async move {
            let (parts, body) = req.into_parts();
            // An aborted request body never reaches the handler.
            let Ok(body) = body.collect().await.map(|b| b.to_bytes()) else {
                return full(StatusCode::BAD_REQUEST, "incomplete request body");
            };
            handler(Seen {
                version: parts.version,
                method: parts.method,
                uri: parts.uri,
                headers: parts.headers,
                body,
            })
            .await
        }
    };
    wye_testkit::spawn_upstream(serve, tls, max_streams).await
}

pub(crate) fn full(status: StatusCode, body: &'static str) -> Response<UpBody> {
    let mut res = Response::new(Either::Left(Full::new(Bytes::from_static(body.as_bytes()))));
    *res.status_mut() = status;
    res
}

pub(crate) fn messages() -> http::request::Builder {
    Request::post("/v1/messages?beta=true")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "future-capability-2099,another")
        .header("x-claude-code-session-id", "s-1")
}

pub(crate) async fn read_body(res: Response<Incoming>) -> Bytes {
    tokio::time::timeout(WAIT, res.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes()
}

/// Unix time in seconds, for credential claims.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
