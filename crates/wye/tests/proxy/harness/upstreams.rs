//! The mock upstream: what it receives, and the answers tests give it.

use super::*;

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

/// A plain-HTTP mock upstream, and the count of requests it receives.
pub(crate) async fn spawn_counted(handler: Handler) -> (SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
    (upstream, hits)
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

pub(crate) fn full(status: StatusCode, body: impl Into<Bytes>) -> Response<UpBody> {
    let mut res = Response::new(Either::Left(Full::new(body.into())));
    *res.status_mut() = status;
    res
}

/// `res` as an event stream.
pub(crate) fn sse(mut res: Response<UpBody>) -> Response<UpBody> {
    res.headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    res
}

/// A response whose body is the chunks the test sends on `rx`'s channel.
pub(crate) fn channel(rx: mpsc::Receiver<Bytes>) -> Response<UpBody> {
    Response::new(Either::Right(ChannelBody(rx)))
}

/// A response that sends `first`, then nothing more until the gateway drops
/// it.
pub(crate) fn stalls_after(first: &'static [u8]) -> Response<UpBody> {
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(Bytes::from_static(first)).unwrap();
    tokio::spawn(async move { tx.closed().await });
    channel(rx)
}

/// The senders of the responses a [`held_open`] upstream keeps open.
pub(crate) type Held = Arc<std::sync::Mutex<Vec<mpsc::Sender<Bytes>>>>;

/// An upstream whose responses are event streams that stay open, silent,
/// until their senders, kept in the returned list, drop.
pub(crate) fn held_open() -> (Handler, Held) {
    let held = Held::default();
    let open = Arc::clone(&held);
    let handler: Handler = Arc::new(move |_| {
        let (tx, rx) = mpsc::channel(1);
        open.lock().unwrap().push(tx);
        Box::pin(async move { sse(channel(rx)) })
    });
    (handler, held)
}

/// `res` as JSON.
pub(crate) fn json(mut res: Response<UpBody>) -> Response<UpBody> {
    res.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    res
}

/// An upstream that answers every request with `reply()`.
pub(crate) fn answering(reply: impl Fn() -> Response<UpBody> + Send + Sync + 'static) -> Handler {
    let reply = Arc::new(reply);
    Arc::new(move |_| {
        let reply = Arc::clone(&reply);
        Box::pin(async move { reply() })
    })
}

/// An upstream that answers its one request with `respond(value)`; another
/// request is a panic the gateway sees as a broken connection.
pub(crate) fn once<T, Fut>(value: T, respond: impl Fn(T) -> Fut + Send + Sync + 'static) -> Handler
where
    T: Send + 'static,
    Fut: Future<Output = Response<UpBody>> + Send + 'static,
{
    let value = std::sync::Mutex::new(Some(value));
    Arc::new(move |_| {
        let value = value.lock().unwrap().take().unwrap();
        Box::pin(respond(value))
    })
}

/// An upstream that answers `200 {}`.
pub(crate) fn ok() -> Handler {
    answering(|| full(StatusCode::OK, "{}"))
}

/// An upstream that answers `200 {}` and passes `pick` of each request it
/// receives to the returned channel.
pub(crate) fn recording<T: Send + 'static>(pick: fn(Seen) -> T) -> (Handler, mpsc::Receiver<T>) {
    recording_reply(pick, || full(StatusCode::OK, "{}"))
}

/// [`recording`], answering `reply()`. A receiver the test dropped is
/// tolerated.
pub(crate) fn recording_reply<T: Send + 'static>(
    pick: fn(Seen) -> T,
    reply: impl Fn() -> Response<UpBody> + Send + Sync + 'static,
) -> (Handler, mpsc::Receiver<T>) {
    let (tx, rx) = mpsc::channel(8);
    let reply = Arc::new(reply);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        let reply = Arc::clone(&reply);
        Box::pin(async move {
            let _ = tx.send(pick(seen)).await;
            reply()
        })
    });
    (handler, rx)
}
