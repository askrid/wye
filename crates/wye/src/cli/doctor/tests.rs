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
         [[routes.claude.upstreams]]\nid = \"up\"\nbase_url = \"http://{upstream}\"\nprovider_key = {{ env = \"PATH\" }}\n\
         [[routes.claude.upstreams]]\nid = \"down\"\nbase_url = \"http://127.0.0.1:1\"\n\
         provider_key = {{ env = \"WYE_DOCTOR_SURELY_UNSET\" }}\n"
    ))
    .unwrap();
    let (report, ok) = doctor(&config, Path::new("."), None).await;
    assert!(!ok, "{report}");
    assert!(
        report.contains("ok    upstream up: provider key $PATH set (sent as x-api-key)"),
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
         base_url = \"http://127.0.0.1:1/anthropic\"\nprovider_key = { env = \"PATH\" }\n\
         [[routes.claude.upstreams]]\nid = \"wrong\"\nprovider = \"bedrock\"\n\
         base_url = \"http://127.0.0.1:1\"\nprovider_key = { env = \"PATH\" }\n",
    )
    .unwrap();
    let (report, _) = doctor(&config, Path::new("."), None).await;
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

/// A key file other users can read lends them the key; `serve` warns of
/// it, doctor fails.
#[cfg(unix)]
#[tokio::test]
async fn a_key_file_open_to_others_is_a_problem() {
    use std::os::unix::fs::PermissionsExt;
    let dir = wye_testkit::TempDir::new("doctor-modes");
    let key = dir.join("key");
    std::fs::write(&key, "sk-test\n").unwrap();
    let hash_key = dir.join("hash.key");
    std::fs::write(&hash_key, format!("{}\n", "k".repeat(32))).unwrap();
    let signing = dir.join("signing.key");
    std::fs::write(&signing, "not checked for its contents\n").unwrap();
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [[routes.claude.upstreams]]\nbase_url = \"http://127.0.0.1:1\"\nprovider_key = { file = \"key\" }\n\
         [cache_diagnostics]\nhash_key = { file = \"hash.key\" }\n",
    )
    .unwrap();
    let chmod = |path: &Path, mode| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    for mode in [0o644, 0o640, 0o604, 0o620] {
        chmod(&key, mode);
        chmod(&hash_key, mode);
        chmod(&signing, mode);
        let (report, ok) = doctor(&config, &dir, Some(&signing)).await;
        assert!(!ok, "{report}");
        let key = key.display();
        let hash_key = hash_key.display();
        assert!(
            report.contains(&format!(
                "FAIL  upstream anthropic: provider_key {key} is open to group \
                 or others (mode {mode:o}): chmod 600 {key}"
            )),
            "{report}"
        );
        assert!(
            report.contains(&format!(
                "FAIL  cache_diagnostics: hash_key {hash_key} is open to group or others \
                 (mode {mode:o}): chmod 600 {hash_key}"
            )),
            "{report}"
        );
        let signing = signing.display();
        assert!(
            report.contains(&format!(
                "FAIL  signing key {signing} is open to group or others (mode {mode:o}): \
                 chmod 600 {signing}"
            )),
            "{report}"
        );
        assert_eq!(
            crate::key_file_warnings(&config, &dir),
            [
                format!(
                    "provider_key {key} is open to group or others (mode {mode:o}): \
                     chmod 600 {key}"
                ),
                format!(
                    "cache_diagnostics.hash_key {hash_key} is open to group or others \
                     (mode {mode:o}): chmod 600 {hash_key}"
                ),
            ]
        );
    }
    for mode in [0o600, 0o400] {
        chmod(&key, mode);
        chmod(&hash_key, mode);
        chmod(&signing, mode);
        let (report, _) = doctor(&config, &dir, Some(&signing)).await;
        assert!(
            report.contains("ok    upstream anthropic: provider key key read (sent as x-api-key)"),
            "{report}"
        );
        assert!(
            report.contains("ok    cache_diagnostics: hash key hash.key read"),
            "{report}"
        );
        assert!(!report.contains("signing key"), "{report}");
        assert!(crate::key_file_warnings(&config, &dir).is_empty());
    }
}

/// doctor resolves a key as `serve` does: a key file whose contents cannot
/// be an HTTP header fails here, not first at startup.
#[tokio::test]
async fn a_key_serve_would_refuse_fails_the_check() {
    let dir = wye_testkit::TempDir::new("doctor");
    std::fs::write(dir.join("key"), "sk-\u{1}broken\n").unwrap();
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [[routes.claude.upstreams]]\nbase_url = \"http://127.0.0.1:1\"\nprovider_key = { file = \"key\" }\n",
    )
    .unwrap();
    let (report, ok) = doctor(&config, &dir, None).await;
    assert!(!ok, "{report}");
    assert!(
        report.contains(
            "FAIL  upstream anthropic: upstream API key contains characters not allowed in an HTTP header"
        ),
        "{report}"
    );
}
