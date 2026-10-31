//! `wye explain` against the server: for the same config, credential, and
//! body, both give the same answer, word for word.

use wye::{Credential, explain};

use super::harness::*;

#[tokio::test]
async fn explain_and_the_server_answer_alike() {
    let key = keypair();
    let (upstream, hits) = spawn_counted(ok()).await;
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nmax_request_body_bytes = 1024\n\
         [upstream]\nbase_url = \"http://{upstream}\"\napi_key_env = \"UNUSED\"\n{}\n\
         [[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n",
        auth_section(
            &key,
            "models = [\"claude-sonnet-*\", \"claude-haiku-*\"]\nmax_output_tokens = 1000"
        )
    );
    let parsed = Config::from_toml_str(&config).unwrap();
    let verifier = wye::verifier(&parsed, Path::new("/")).unwrap().unwrap();
    let h = Harness::launch_config(&config, keyed_resources(), hits).await;

    let open = token(&key, "alice", None, None);
    let haiku_only = token(&key, "alice", Some(&["claude-haiku-*"]), None);
    let capped = token(&key, "alice", None, Some(10));
    let cases: [(&str, &str, &str); 9] = [
        (
            "allowed",
            &open,
            r#"{"model":"claude-sonnet-4-5","max_tokens":5}"#,
        ),
        (
            "beyond config",
            &open,
            r#"{"model":"claude-opus-5-5","max_tokens":5}"#,
        ),
        (
            "alias to a forbidden model",
            &open,
            r#"{"model":"coding","max_tokens":5}"#,
        ),
        (
            "beyond the token",
            &haiku_only,
            r#"{"model":"claude-sonnet-4-5","max_tokens":5}"#,
        ),
        (
            "over config's ceiling",
            &open,
            r#"{"model":"claude-haiku-4-5","max_tokens":5000}"#,
        ),
        (
            "over the token's ceiling",
            &capped,
            r#"{"max_tokens":50,"model":"claude-haiku-4-5"}"#,
        ),
        ("no model", &open, r#"{"max_tokens":5,"messages":[]}"#),
        (
            "a second model",
            &open,
            r#"{"model":"claude-haiku-4-5","max_tokens":5,"model":"x"}"#,
        ),
        ("not JSON", &open, "not json"),
    ];
    for (name, token, body) in cases {
        let explained = explain(
            &parsed,
            "/v1/messages",
            Credential::Token(token, &verifier),
            None,
            body.as_bytes(),
            now(),
            &[],
        );
        let body = body.to_owned();
        let res = h.send(bearer(token), body).await;
        let status = res.status().as_u16();
        let answer = read_body(res).await;
        let served = (status != 200).then(|| {
            let error: serde_json::Value = serde_json::from_slice(&answer).unwrap();
            (
                status,
                error["error"]["type"].as_str().unwrap().to_owned(),
                error["error"]["message"].as_str().unwrap().to_owned(),
            )
        });
        let explained = explained
            .refused
            .map(|(status, kind, message)| (status, kind.to_owned(), message));
        assert_eq!(served, explained, "{name}");
        h.usage_record().await;
    }
}
