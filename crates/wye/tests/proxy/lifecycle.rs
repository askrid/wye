//! Concurrency limit, two-phase shutdown, and the binary: `wye claude`, `--help`, `--version`.

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
    let hits = Arc::new(AtomicUsize::new(0));
    let (handler, _bodies) = recording(|seen| seen.body);
    let upstream = spawn_upstream(handler, Arc::clone(&hits), None, None).await;
    let resources = with_upstreams(vec![UpstreamResources::new(
        UpstreamAuth::api_key(API_KEY).unwrap(),
    )]);
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n\
         max_held_request_bytes = 1024\n\
         [upstream]\nbase_url = \"http://{upstream}\"\napi_key_env = \"UNUSED\"\n\
         [[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n"
    );
    let h = Harness::launch_config(&config, resources, hits).await;
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
    tokio::time::sleep(Duration::from_millis(100)).await;

    let res = h
        .send(messages(), r#"{"messages":[],"pad":"0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789","model":"coding"}"#)
        .await;
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

#[tokio::test]
async fn probe_succeeds_only_for_a_2xx_answer() {
    let h = Harness::api_key(ok()).await;
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
    let h = Harness::api_key(ok()).await;
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
        // The URL and the hints also go in --settings, for background sessions; the
        // credential never goes on the command line.
        format!(
            "{gateway}|gateway-token|unset|--settings {{\"env\":{{\"ANTHROPIC_BASE_URL\":\"{gateway}\",\"CLAUDE_CODE_GATEWAY_HINT_HEADERS\":\"1\"}}}} -p fix tests\n"
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

#[test]
fn review_commands_exit_3_on_what_they_report() {
    let dir = std::env::temp_dir().join(format!("wye-exit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    // Nothing listens on port 1: `doctor` stays on this machine.
    let loopback = write(
        "loopback.toml",
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"http://127.0.0.1:1\"\nforward_caller_credential = true\n",
    );
    let keyed = write(
        "keyed.toml",
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"http://127.0.0.1:1\"\napi_key_env = \"WYE_TEST_UNSET\"\n",
    );
    let request = write("request.json", r#"{"messages":[]}"#);
    let status = |args: &[&std::ffi::OsStr]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_wye"))
            .args(args)
            .env_remove("WYE_TEST_UNSET")
            .output()
            .unwrap()
            .status
            .code()
    };
    // The operator's key where callers brought their own widens access.
    assert_eq!(
        status(&[
            "policy-diff".as_ref(),
            loopback.as_os_str(),
            keyed.as_os_str()
        ]),
        Some(3)
    );
    assert_eq!(
        status(&[
            "policy-diff".as_ref(),
            keyed.as_os_str(),
            loopback.as_os_str()
        ]),
        Some(0)
    );
    // explain: a path no route serves is a refusal; the key is not read.
    let explain = |path: &str| {
        status(&[
            "explain".as_ref(),
            "--config".as_ref(),
            keyed.as_os_str(),
            "--path".as_ref(),
            path.as_ref(),
            request.as_os_str(),
        ])
    };
    assert_eq!(explain("/v1/messages"), Some(0));
    assert_eq!(explain("/v1/unknown"), Some(3));
    // policy-test: a case whose expectation is wrong fails.
    let cases = dir.join("cases");
    std::fs::create_dir_all(&cases).unwrap();
    std::fs::write(cases.join("wrong.json"), r#"{"model":"m"}"#).unwrap();
    std::fs::write(cases.join("wrong.expect.toml"), "decision = \"refuse\"\n").unwrap();
    assert_eq!(
        status(&[
            "policy-test".as_ref(),
            "--config".as_ref(),
            keyed.as_os_str(),
            cases.as_os_str()
        ]),
        Some(3)
    );
    // The provider key is not set.
    assert_eq!(
        status(&["doctor".as_ref(), "--config".as_ref(), keyed.as_os_str()]),
        Some(3)
    );
    let examples =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy-tests");
    assert_eq!(
        status(&[
            "policy-test".as_ref(),
            "--config".as_ref(),
            examples.join("wye.toml").as_os_str(),
            examples.as_os_str()
        ]),
        Some(0)
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
