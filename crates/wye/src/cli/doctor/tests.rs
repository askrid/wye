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
        report.contains("FAIL  upstream down: $WYE_DOCTOR_SURELY_UNSET is not set"),
        "{report}"
    );
    assert!(
        report.contains("FAIL  upstream down: http://127.0.0.1:1/ unreachable"),
        "{report}"
    );
    assert!(report.ends_with("2 problems\n"), "{report}");
}
