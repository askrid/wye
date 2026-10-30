#![allow(clippy::unwrap_used)]

use super::*;

#[tokio::test]
async fn reports_each_problem_and_what_works() {
    let upstream = wye_testkit::spawn_upstream(
        |_req: http::Request<hyper::body::Incoming>| async {
            http::Response::new(http_body_util::Full::new(Bytes::new()))
        },
        None,
        None,
    )
    .await;
    // Setting variables is unsafe in edition 2024: PATH stands in for a set
    // key, a surely unset name for a missing one.
    let config = Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\n\
         [[routes.claude.upstreams]]\nid = \"up\"\nbase_url = \"http://{upstream}\"\napi_key_env = \"PATH\"\n\
         [[routes.claude.upstreams]]\nid = \"down\"\nbase_url = \"http://127.0.0.1:1\"\n\
         api_key_env = \"WYE_DOCTOR_SURELY_UNSET\"\n"
    ))
    .unwrap();
    let (report, ok) = doctor(&config, Path::new(".")).await;
    assert!(!ok, "{report}");
    assert!(
        report.contains("ok    upstream up: $PATH set (sent as x-api-key)"),
        "{report}"
    );
    assert!(
        report.contains(&format!("ok    upstream up: http://{upstream}/ reachable")),
        "{report}"
    );
    assert!(
        report.contains(
            "FAIL  upstream down: environment variable WYE_DOCTOR_SURELY_UNSET is not set"
        ),
        "{report}"
    );
    assert!(
        report.contains("FAIL  upstream down: http://127.0.0.1:1/ unreachable"),
        "{report}"
    );
    assert!(report.ends_with("2 problems\n"), "{report}");
}

#[tokio::test]
async fn a_bedrock_upstream_needs_the_anthropic_path() {
    // Nothing listens on port 1: only the path check is asserted.
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\n\
         [[routes.claude.upstreams]]\nid = \"right\"\nprovider = \"bedrock\"\n\
         base_url = \"http://127.0.0.1:1/anthropic\"\napi_key_env = \"PATH\"\n\
         [[routes.claude.upstreams]]\nid = \"wrong\"\nprovider = \"bedrock\"\n\
         base_url = \"http://127.0.0.1:1\"\napi_key_env = \"PATH\"\n",
    )
    .unwrap();
    let (report, _) = doctor(&config, Path::new(".")).await;
    assert!(
        report.contains("ok    upstream right: Bedrock Messages endpoint path"),
        "{report}"
    );
    assert!(
        report.contains(
            "FAIL  upstream wrong: Bedrock's Messages endpoint is under /anthropic: \
             base_url = \"http://127.0.0.1:1/anthropic\""
        ),
        "{report}"
    );
}

/// Checked without a network: the rule reads only the config.
#[test]
fn otlp_headers_over_plain_http_beyond_this_machine_are_a_problem() {
    let config = |endpoint: &str| {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [upstream]\nbase_url = \"http://127.0.0.1:1\"\nforward_caller_credential = true\n\
             [telemetry]\notlp_endpoint = \"{endpoint}\"\notlp_headers_env = \"PATH\"\n"
        ))
        .unwrap()
    };
    let in_clear = |endpoint: &str| {
        let config = config(endpoint);
        super::headers_in_clear(config.telemetry.as_ref().unwrap()).map(|check| check.line)
    };
    assert_eq!(
        in_clear("http://collector.example:4318").as_deref(),
        Some(
            "telemetry: $PATH would cross the network in the clear to collector.example:4318; \
             use https, or a collector on this machine"
        )
    );
    assert_eq!(in_clear("https://collector.example:4318"), None);
    assert_eq!(in_clear("http://127.0.0.1:4318"), None);
}

/// doctor resolves a key as `serve` does: a key file whose contents cannot
/// be an HTTP header fails here, not first at startup.
#[tokio::test]
async fn a_key_serve_would_refuse_fails_the_check() {
    let dir = std::env::temp_dir().join(format!("wye-doctor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("key"), "sk-\u{1}broken\n").unwrap();
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"http://127.0.0.1:1\"\napi_key_file = \"key\"\n",
    )
    .unwrap();
    let (report, ok) = doctor(&config, &dir).await;
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!ok, "{report}");
    assert!(
        report.contains(
            "FAIL  [upstream]: upstream API key contains characters not allowed in an HTTP header"
        ),
        "{report}"
    );
}
