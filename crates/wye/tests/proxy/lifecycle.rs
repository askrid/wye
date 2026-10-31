//! Concurrency limit and two-phase shutdown.

use super::harness::*;

/// Upstream response that stays open, silent, until the returned senders drop.
fn held_open(open: &Arc<std::sync::Mutex<Vec<mpsc::Sender<Bytes>>>>) -> Handler {
    let open = Arc::clone(open);
    Arc::new(move |_| {
        let (tx, rx) = mpsc::channel(1);
        open.lock().unwrap().push(tx);
        Box::pin(async move { streaming(rx) })
    })
}

#[tokio::test]
async fn requests_beyond_the_concurrency_limit_get_an_overload_error() {
    let open = Arc::default();
    let h = Harness::with_server_config(held_open(&open), "max_concurrent_requests = 2").await;
    let first = h.send(messages(), "{}").await;
    let _second = h.send(messages(), "{}").await;

    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status().as_u16(), 529);
    // Claude Code waits `retry-after` seconds and retries; above 60 it gives up.
    assert_eq!(res.headers()["retry-after"], "1");
    assert_eq!(res.headers()["x-should-retry"], "true");
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
async fn requests_beyond_the_hold_budget_get_an_overload_error() {
    // An alias makes the gateway hold each body until its model is read.
    let (handler, _bodies) = recording(|seen| seen.body);
    let (upstream, hits) = spawn_counted(handler).await;
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n\
         max_held_request_bytes = 1024\n\
         [upstream]\nbase_url = \"http://{upstream}\"\napi_key_env = \"UNUSED\"\n\
         [[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n"
    );
    let h = Harness::launch_config(&config, keyed_resources(), hits).await;
    // A first request holds 900 bytes, its model still to come.
    let (tx, rx) = mpsc::channel::<Bytes>(2);
    let client: Client<HttpConnector, ChannelBody> =
        Client::builder(TokioExecutor::new()).build_http();
    let held = Request::post(format!("http://{}/v1/messages", h.gateway))
        .body(ChannelBody(rx))
        .unwrap();
    let _first = tokio::spawn(async move { client.request(held).await });
    let padding = format!(r#"{{"messages":[],"pad":"{}","#, "x".repeat(880));
    tx.send(Bytes::from(padding)).await.unwrap();
    // Until the gateway has read those bytes the budget has room. Without a
    // model, this body reaches no upstream either way: 400 with room, 529 without.
    const PAD: &str = "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789";
    let mut status = 0;
    for _ in 0..250 {
        let body = format!(r#"{{"messages":[],"pad":"{PAD}"}}"#);
        status = h.send(messages(), body).await.status().as_u16();
        if status == 529 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, 529);

    let body = format!(r#"{{"messages":[],"pad":"{PAD}","model":"coding"}}"#);
    let res = h.send(messages(), body).await;
    assert_eq!(res.status().as_u16(), 529);
    assert_eq!(res.headers()["retry-after"], "1");
    assert_eq!(res.headers()["x-should-retry"], "true");
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 0);
    drop(tx);
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

    // The open stream was cut, not left hanging, and so were the upstream
    // requests behind it: the provider stops generating.
    let ended = tokio::time::timeout(WAIT, stream.into_body().collect()).await;
    assert!(
        matches!(ended, Ok(Err(_))),
        "the stream must end cut, not complete or hanging"
    );
    let senders: Vec<_> = open.lock().unwrap().drain(..).collect();
    assert_eq!(senders.len(), 2);
    for sender in senders {
        tokio::time::timeout(WAIT, sender.closed())
            .await
            .expect("an upstream request outlived shutdown");
    }
}

#[tokio::test]
async fn shutdown_does_not_wait_on_a_usage_log_nobody_reads() {
    let mut h = Harness::api_key(ok()).await;
    // Nothing reads the usage stream, so its 64 KiB buffer fills and the
    // writer waits on it.
    for _ in 0..400 {
        h.send(messages(), "{}").await;
    }
    h.begin_shutdown();
    let flushed = tokio::time::timeout(WAIT, &mut h.served)
        .await
        .expect("serve waited on the usage log")
        .unwrap();
    assert!(!flushed);
}
