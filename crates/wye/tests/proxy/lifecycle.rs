//! Concurrency limit and two-phase shutdown.

use super::harness::*;

/// Upstream response that stays open, silent, until the returned senders drop.
fn held_open(open: &Arc<std::sync::Mutex<Vec<mpsc::Sender<Bytes>>>>) -> Handler {
    let open = Arc::clone(open);
    Arc::new(move |_| {
        let (tx, rx) = mpsc::channel(1);
        open.lock().unwrap().push(tx);
        Box::pin(async move {
            let mut res = Response::new(Either::Right(ChannelBody(rx)));
            res.headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            res
        })
    })
}

#[tokio::test]
async fn requests_beyond_the_concurrency_limit_get_overloaded_error() {
    let open = Arc::default();
    let h = Harness::with_server_config(held_open(&open), "max_concurrent_requests = 2").await;
    let first = h.send(messages(), "{}").await;
    let _second = h.send(messages(), "{}").await;

    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status().as_u16(), 529);
    assert!(
        read_body(res)
            .await
            .starts_with(br#"{"type":"error","error":{"type":"overloaded_error""#)
    );
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 2);

    // The permit is released with the response body.
    drop(first);
    let mut status = 0;
    for _ in 0..50 {
        status = h.send(messages(), "{}").await.status().as_u16();
        if status == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, 200);
}

#[tokio::test]
async fn shutdown_drains_then_stops_within_the_deadline() {
    let open = Arc::default();
    let mut h = Harness::with_server_config(
        held_open(&open),
        "drain_delay_secs = 1\nshutdown_grace_secs = 1",
    )
    .await;
    let stream = h.send(messages(), "{}").await;
    assert_eq!(stream.status(), StatusCode::OK);

    let started = std::time::Instant::now();
    h.begin_shutdown();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Draining: not ready, but still alive and serving new requests.
    assert_eq!(
        h.send(Request::get("/readyz"), "").await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        h.send(Request::get("/healthz"), "").await.status(),
        StatusCode::OK
    );
    assert_eq!(h.send(messages(), "{}").await.status(), StatusCode::OK);

    // The never-ending stream cannot hold shutdown past drain delay + grace.
    tokio::time::timeout(Duration::from_secs(5), &mut h.served)
        .await
        .expect("serve did not stop within the deadline")
        .unwrap();
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(1),
        "drain delay skipped: {took:?}"
    );
    assert!(took < Duration::from_secs(4), "deadline exceeded: {took:?}");

    // The open stream was cut, not left hanging.
    let ended = tokio::time::timeout(WAIT, stream.into_body().collect()).await;
    assert!(ended.is_ok(), "stream still open after shutdown");
    drop(open);
}
