//! Concurrency limit, two-phase shutdown, and the binary: `wye claude`, `--help`, `--version`.

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

#[tokio::test]
async fn probe_succeeds_only_for_a_2xx_answer() {
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") }))).await;
    wye::probe(&format!("http://{}/healthz", h.gateway))
        .await
        .unwrap();
    let not_found = wye::probe(&format!("http://{}/nope", h.gateway))
        .await
        .unwrap_err();
    assert!(not_found.contains("404"), "{not_found}");
    // https is accepted (a remote gateway for `wye claude`); other schemes are not.
    let ftp = wye::probe("ftp://127.0.0.1/healthz").await.unwrap_err();
    assert!(ftp.contains("http:// or https://"), "{ftp}");
    // Nothing listens on this port.
    let refused = wye::probe("http://127.0.0.1:9/healthz").await.unwrap_err();
    assert!(
        refused.starts_with("http://127.0.0.1:9/healthz:"),
        "{refused}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn wye_claude_runs_claude_against_the_gateway() {
    let h = Harness::api_key(Arc::new(|_| Box::pin(async { full(StatusCode::OK, "{}") }))).await;
    let dir = std::env::temp_dir().join(format!("wye-claude-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A stand-in for claude that records what it was given and exits 3.
    let fake = dir.join("claude");
    let seen = dir.join("seen");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$ANTHROPIC_BASE_URL\" \"$ANTHROPIC_AUTH_TOKEN\" \"${{ANTHROPIC_API_KEY-unset}}\" \"$*\" > {}\nexit 3\n",
            seen.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&fake, permissions).unwrap();
    let credential = dir.join("credential");
    std::fs::write(&credential, "gateway-token\n").unwrap();

    let gateway = format!("http://{}", h.gateway);
    // Run the binary off the runtime, which also serves the gateway.
    let run = |args: Vec<std::ffi::OsString>| {
        let fake = fake.clone();
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_wye"))
                .args(args)
                .env("WYE_CLAUDE_BIN", fake)
                .env("ANTHROPIC_API_KEY", "sk-should-be-removed")
                .env_remove("CLAUDE_CODE_GATEWAY_HINT_HEADERS")
                .output()
                .unwrap()
        })
    };
    let output = run(vec![
        "claude".into(),
        "--gateway".into(),
        gateway.clone().into(),
        "--credential-file".into(),
        credential.clone().into(),
        "-p".into(),
        "fix tests".into(),
    ])
    .await
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "claude's exit code passes through"
    );
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        // The URL also goes in --settings, for background sessions; the
        // credential never goes on the command line.
        format!(
            "{gateway}|gateway-token|unset|--settings {{\"env\":{{\"ANTHROPIC_BASE_URL\":\"{gateway}\"}}}} -p fix tests\n"
        )
    );

    // Without a reachable gateway, claude is not started.
    let output = run(vec![
        "claude".into(),
        "--gateway".into(),
        "http://127.0.0.1:9".into(),
    ])
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("wye serve"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The sample policy tests in examples/ pass against their config.
#[test]
fn example_policy_tests_pass() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy-tests");
    let config =
        Config::from_toml_str(&std::fs::read_to_string(dir.join("wye.toml")).unwrap()).unwrap();
    let (report, passed) = wye::policy_test(&config, &dir, 0).unwrap();
    assert!(passed, "{report}");
    assert!(report.ends_with("4 passed, 0 failed\n"), "{report}");
}

#[test]
fn the_cli_answers_help_and_version_and_names_an_unknown_command() {
    let wye = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_wye"))
            .args(args)
            .output()
            .unwrap()
    };
    let help = wye(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("wye serve --config"));
    let version = wye(&["--version"]);
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("wye {}\n", env!("CARGO_PKG_VERSION"))
    );
    let typo = wye(&["srve"]);
    assert_eq!(typo.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&typo.stderr).starts_with("wye: unknown command `srve`"));
    // A known command with wrong arguments gets its own usage.
    let wrong = wye(&["serve"]);
    assert_eq!(
        String::from_utf8_lossy(&wrong.stderr),
        "wye: usage:\n  wye serve --config <path>\nall commands: `wye --help`\n"
    );
}
