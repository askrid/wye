//! TLS upstreams, trusted roots, and upstream connection spreading.

use super::harness::*;

#[tokio::test]
async fn forwards_over_tls_with_http2_to_a_trusted_upstream() {
    let ca = TestCa::generate();
    let (tx, mut rx) = mpsc::channel(1);
    let handler: Handler = Arc::new(move |seen: Seen| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send((seen.version, seen.headers, seen.body))
                .await
                .unwrap();
            full(StatusCode::OK, r#"{"id":"msg_tls"}"#)
        })
    });
    let h = Harness::start_tls(handler, &ca, true).await;
    let res = h.send(messages(), r#"{"model":"claude-x"}"#).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(read_body(res).await, r#"{"id":"msg_tls"}"#);
    let (version, headers, body) = rx.recv().await.unwrap();
    assert_eq!(version, Version::HTTP_2);
    assert_eq!(headers["x-api-key"], API_KEY);
    assert_eq!(body, r#"{"model":"claude-x"}"#);
}

#[tokio::test]
async fn untrusted_upstream_certificate_is_rejected() {
    let ca = TestCa::generate();
    let h = Harness::start_tls(
        Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") })),
        &ca,
        false,
    )
    .await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), 0);
}

#[test]
fn ca_file_resolves_relative_to_the_config_directory() {
    let ca = TestCa::generate();
    let dir = std::env::temp_dir().join(format!("wye-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("certs")).unwrap();
    std::fs::write(dir.join("certs/ca.pem"), ca.ca_pem()).unwrap();
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:0\"\n[upstream]\nbase_url = \"https://api.anthropic.com\"\n\
         forward_caller_credential = true\nca_file = \"certs/ca.pem\"",
    )
    .unwrap();

    let resources = Resources::resolve(&config, &dir).unwrap();
    assert_eq!(resources.extra_roots, vec![ca.ca_cert()]);

    std::fs::write(dir.join("certs/ca.pem"), "not a certificate").unwrap();
    let err = Resources::resolve(&config, &dir).unwrap_err().to_string();
    assert!(err.contains("no PEM certificates found"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn concurrent_streams_beyond_one_connection_limit_are_not_queued() {
    const PEER_LIMIT: u32 = 4;
    const STREAMS: usize = 10;
    let ca = TestCa::generate();
    // Streams stay open until the test ends, holding their HTTP/2 stream slots.
    let open: Arc<std::sync::Mutex<Vec<mpsc::Sender<Bytes>>>> = Arc::default();
    let held = Arc::clone(&open);
    let handler: Handler = Arc::new(move |_| {
        let (tx, rx) = mpsc::channel(1);
        held.lock().unwrap().push(tx);
        Box::pin(async move { Response::new(Either::Right(ChannelBody(rx))) })
    });
    let h = Harness::start_tls_with(handler, &ca, true, Some(PEER_LIMIT), "").await;

    // Each response keeps its stream open, so stream n needs a free slot while n-1 are held.
    let mut responses = Vec::new();
    for _ in 0..STREAMS {
        let res = h.send(messages(), "{}").await;
        assert_eq!(res.status(), StatusCode::OK);
        responses.push(res);
    }
    assert_eq!(h.upstream_hits.load(Ordering::Relaxed), STREAMS);
    drop(open);
}

#[tokio::test]
async fn http_version_pins_the_upstream_protocol() {
    for (extra, expected) in [
        ("", Version::HTTP_2),
        ("http_version = \"1.1\"", Version::HTTP_11),
    ] {
        let ca = TestCa::generate();
        let (tx, mut rx) = mpsc::channel::<Version>(1);
        let handler: Handler = Arc::new(move |seen: Seen| {
            let tx = tx.clone();
            Box::pin(async move {
                tx.send(seen.version).await.unwrap();
                full(StatusCode::OK, "{}")
            })
        });
        let h = Harness::start_tls_with(handler, &ca, true, None, extra).await;
        let res = h.send(messages(), "{}").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(rx.recv().await.unwrap(), expected, "{extra:?}");
    }
}
