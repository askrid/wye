//! Streaming, backpressure, cancellation, and timeouts.

use super::harness::*;

#[tokio::test]
async fn streams_events_before_upstream_completes() {
    let (chunks_tx, chunks_rx) = mpsc::channel::<Bytes>(4);
    let body = std::sync::Mutex::new(Some(chunks_rx));
    let handler: Handler = Arc::new(move |_| {
        let rx = body.lock().unwrap().take().unwrap();
        Box::pin(async move { streaming(rx) })
    });
    let h = Harness::api_key(handler).await;

    chunks_tx
        .send(Bytes::from_static(b"event: ping\ndata: {}\n\n"))
        .await
        .unwrap();
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let mut body = res.into_body();

    let first = tokio::time::timeout(WAIT, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap(), "event: ping\ndata: {}\n\n");

    chunks_tx
        .send(Bytes::from_static(b"event: message_stop\ndata: {}\n\n"))
        .await
        .unwrap();
    drop(chunks_tx);
    let rest = tokio::time::timeout(WAIT, body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(rest, "event: message_stop\ndata: {}\n\n");
}

#[tokio::test]
async fn client_disconnect_releases_upstream_stream() {
    let (chunks_tx, chunks_rx) = mpsc::channel::<Bytes>(1);
    let body = std::sync::Mutex::new(Some(chunks_rx));
    let handler: Handler = Arc::new(move |_| {
        let rx = body.lock().unwrap().take().unwrap();
        Box::pin(async move { Response::new(Either::Right(ChannelBody(rx))) })
    });
    let h = Harness::api_key(handler).await;

    chunks_tx
        .send(Bytes::from_static(b"data: 1\n\n"))
        .await
        .unwrap();
    let res = h.send(messages(), "{}").await;
    let mut body = res.into_body();
    tokio::time::timeout(WAIT, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(body);
    drop(h.client);

    // The upstream body is dropped once Wye drops its upstream connection.
    tokio::time::timeout(WAIT, chunks_tx.closed())
        .await
        .expect("upstream stream was not released");
}

/// An event stream that stalls (or breaks) at an event boundary ends with
/// an Anthropic `error` event instead of a dropped connection; one that
/// stalls mid-event still fails, since appending would corrupt the event.
#[tokio::test]
async fn stalled_event_stream_ends_with_an_error_event() {
    let handler: Handler = Arc::new(|seen: Seen| {
        Box::pin(async move {
            let (tx, rx) = mpsc::channel::<Bytes>(1);
            let first: &'static [u8] = if seen.uri.query() == Some("mid=1") {
                b"event: content_block_delta\ndata: {\"partial"
            } else {
                b"event: ping\ndata: {\"type\":\"ping\"}\n\n"
            };
            tx.send(Bytes::from_static(first)).await.unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(tx);
            });
            streaming(rx)
        })
    });
    let h = Harness::start(
        handler,
        "api_key_env = \"UNUSED\"\nstream_idle_timeout_secs = 1",
        UpstreamAuth::api_key(API_KEY).unwrap(),
    )
    .await;
    let res = h.send(messages(), "{}").await;
    let body = tokio::time::timeout(WAIT, res.into_body().collect())
        .await
        .unwrap()
        .expect("the stream ends cleanly")
        .to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(text.starts_with("event: ping\n"), "{text}");
    assert!(
        text.ends_with("\n\n")
            && text.contains(
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\""
            ),
        "{text}"
    );
    assert_eq!(h.usage_record().await["outcome"], "failed");

    let res = h
        .send(
            Request::post("/v1/messages?mid=1").header("x-claude-code-session-id", "s-1"),
            "{}",
        )
        .await;
    let mut body = res.into_body();
    tokio::time::timeout(WAIT, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let next = tokio::time::timeout(WAIT, body.frame()).await.unwrap();
    assert!(matches!(next, Some(Err(_))), "mid-event stall: {next:?}");
}

#[tokio::test]
async fn idle_upstream_stream_is_terminated() {
    let handler: Handler = Arc::new(|_| {
        Box::pin(async {
            let (tx, rx) = mpsc::channel::<Bytes>(1);
            tx.send(Bytes::from_static(b"data: 1\n\n")).await.unwrap();
            // Keep the sender alive without sending, so the stream goes idle.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(tx);
            });
            Response::new(Either::Right(ChannelBody(rx)))
        })
    });
    let h = Harness::start(
        handler,
        "api_key_env = \"UNUSED\"\nstream_idle_timeout_secs = 1",
        UpstreamAuth::api_key(API_KEY).unwrap(),
    )
    .await;
    let res = h.send(messages(), "{}").await;
    let mut body = res.into_body();
    tokio::time::timeout(WAIT, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let next = tokio::time::timeout(WAIT, body.frame()).await.unwrap();
    assert!(
        matches!(next, Some(Err(_))),
        "expected a terminated stream, got {next:?}"
    );
}

#[tokio::test]
async fn slow_upstream_headers_time_out() {
    let handler: Handler = Arc::new(|_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            full(StatusCode::OK, "{}")
        })
    });
    let h = Harness::start(
        handler,
        "api_key_env = \"UNUSED\"\nresponse_header_timeout_secs = 1",
        UpstreamAuth::api_key(API_KEY).unwrap(),
    )
    .await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(res.headers()["content-type"], "application/json");
    let body = read_body(res).await;
    assert!(body.starts_with(br#"{"type":"error","error":{"type":"api_error""#));
}

#[tokio::test]
async fn slow_reader_backpressures_the_upstream() {
    const CHUNK: usize = 1024 * 1024;
    const CHUNKS: usize = 64;
    let produced = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&produced);
    let handler: Handler = Arc::new(move |_| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(1);
            tokio::spawn(async move {
                let chunk = Bytes::from(vec![b'x'; CHUNK]);
                for _ in 0..CHUNKS {
                    if tx.send(chunk.clone()).await.is_err() {
                        return;
                    }
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            });
            Response::new(Either::Right(ChannelBody(rx)))
        })
    });
    let h = Harness::api_key(handler).await;

    // Receive headers, then read nothing.
    let res = h.send(messages(), "{}").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let buffered = produced.load(Ordering::Relaxed);
    // Socket buffers on both hops hold a few MiB; without backpressure the
    // upstream would have produced all 64 MiB by now.
    assert!(
        buffered < CHUNKS / 4,
        "upstream produced {buffered} MiB for a reader that read none"
    );

    // Reading resumes the flow to the end.
    let body = tokio::time::timeout(Duration::from_secs(20), res.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(body.len(), CHUNK * CHUNKS);
}

#[tokio::test]
async fn client_that_stops_reading_is_disconnected() {
    let (released_tx, released) = oneshot::channel();
    let released_tx = std::sync::Mutex::new(Some(released_tx));
    let handler: Handler = Arc::new(move |_| {
        let released_tx = released_tx.lock().unwrap().take().unwrap();
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(1);
            tokio::spawn(async move {
                let chunk = Bytes::from(vec![b'x'; 64 * 1024]);
                while tx.send(chunk.clone()).await.is_ok() {}
                let _ = released_tx.send(());
            });
            Response::new(Either::Right(ChannelBody(rx)))
        })
    });
    let h = Harness::with_server_config(handler, "client_write_timeout_secs = 1").await;

    // Receive headers, then read nothing: the socket fills, writes block,
    // and after the timeout Wye closes the connection and the upstream stream.
    let res = h.send(messages(), "{}").await;
    tokio::time::timeout(WAIT, released)
        .await
        .expect("upstream stream was not released")
        .unwrap();
    drop(res);
}
