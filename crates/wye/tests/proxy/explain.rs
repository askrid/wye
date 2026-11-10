//! `wye explain` against the server: for the same config, credential, and
//! body, both give the same answer, word for word.

use wye::{CallerToken, explain};

use super::harness::*;

#[tokio::test]
async fn explain_and_the_server_answer_alike() {
    let key = keypair();
    let auth = auth_section(
        &key,
        "models = [\"claude-sonnet-*\", \"claude-haiku-*\"]\nmax_output_tokens = 1000",
    );
    let aliases = "[[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"";
    let h = Harness::keyed(ok(), &format!("{auth}\n{aliases}")).await;
    let verifier = wye::verifier(&h.config, Path::new("/")).unwrap().unwrap();

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
            &h.config,
            "/v1/messages",
            CallerToken::Token(token, &verifier),
            &HeaderMap::new(),
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

/// The same agreement on the OpenAI route, whose errors carry the kind as
/// `code`, and for an API no route serves.
#[tokio::test]
async fn explain_and_the_server_answer_alike_on_responses() {
    let key = keypair();
    let auth = auth_section(&key, "models = [\"gpt-5*\"]\nmax_output_tokens = 1000");
    let h = Harness::keyed(
        ok(),
        &format!("provider = \"openai\"\nid = \"openai\"\n{auth}"),
    )
    .await;
    let verifier = wye::verifier(&h.config, Path::new("/")).unwrap().unwrap();
    let open = token(&key, "alice", None, None);
    let cases: [(&str, &str, &str); 6] = [
        (
            "allowed",
            "/v1/responses",
            r#"{"model":"gpt-5","max_output_tokens":5}"#,
        ),
        (
            "beyond config",
            "/v1/responses",
            r#"{"model":"o3","max_output_tokens":5}"#,
        ),
        (
            "over the ceiling",
            "/v1/responses",
            r#"{"model":"gpt-5","max_output_tokens":5000}"#,
        ),
        (
            "no ceiling field",
            "/v1/responses",
            r#"{"model":"gpt-5","input":[]}"#,
        ),
        (
            "a second value",
            "/v1/responses",
            r#"{"model":"gpt-5","max_output_tokens":5}{"x":1}"#,
        ),
        (
            "no route",
            "/v1/messages",
            r#"{"model":"claude-haiku-4-5","max_tokens":5}"#,
        ),
    ];
    for (name, path, body) in cases {
        let explained = explain(
            &h.config,
            path,
            CallerToken::Token(&open, &verifier),
            &HeaderMap::new(),
            body.as_bytes(),
            now(),
            &[],
        );
        let req = Request::post(path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {open}"));
        let res = h.send(req, body.to_owned()).await;
        let status = res.status().as_u16();
        let answer = read_body(res).await;
        let served = (status != 200).then(|| {
            let error: serde_json::Value = serde_json::from_slice(&answer).unwrap();
            let error = &error["error"];
            // Anthropic's shape names the kind `type`; OpenAI's, `code`.
            let kind = error["code"].as_str().or(error["type"].as_str()).unwrap();
            (
                status,
                kind.to_owned(),
                error["message"].as_str().unwrap().to_owned(),
            )
        });
        let explained = explained
            .refused
            .map(|(status, kind, message)| (status, kind.to_owned(), message));
        assert_eq!(served, explained, "{name}");
        // A request no route serves is refused before it is recorded.
        if name != "no route" {
            h.usage_record().await;
        }
    }
}
