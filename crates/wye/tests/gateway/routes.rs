//! A route with several upstreams: the choice per session, each upstream's
//! own provider key, and the upstream in usage records.

use super::harness::*;

/// An upstream that answers with its name and the `x-api-key` it received.
async fn named_upstream(name: &'static str, hits: &Arc<AtomicUsize>) -> SocketAddr {
    let handler: Handler = Arc::new(move |seen: Seen| {
        let key = seen.headers["x-api-key"].to_str().unwrap().to_owned();
        Box::pin(async move {
            let body = format!("{{\"upstream\":\"{name}\",\"key\":\"{key}\"}}");
            full(StatusCode::OK, body)
        })
    });
    spawn_upstream(handler, Arc::clone(hits), None, None).await
}

#[tokio::test]
async fn sessions_stick_to_one_upstream_with_its_own_key() {
    let hits = Arc::new(AtomicUsize::new(0));
    let a = named_upstream("team-a", &hits).await;
    let b = named_upstream("team-b", &hits).await;
    let upstreams = upstream(a, "id = \"team-a\"") + &upstream(b, "id = \"team-b\"");
    let resources = with_upstreams(vec![
        UpstreamResources::new(UpstreamAuth::api_key("key-team-a").unwrap()),
        UpstreamResources::new(UpstreamAuth::api_key("key-team-b").unwrap()),
    ]);
    let h = Harness::launch_config(&config("", &upstreams), resources, hits).await;
    let route = h.config.routes.claude.as_ref().unwrap();

    let mut used = [false; 2];
    for i in 0..16 {
        let session = format!("session-{i}");
        let expected = &*route.upstreams()[route.pick(session.as_bytes())].id;
        // Twice per session: the same upstream both times.
        for _ in 0..2 {
            let mut req = messages();
            req.headers_mut()
                .unwrap()
                .insert("x-claude-code-session-id", session.parse().unwrap());
            let res = h.send(req, r#"{"model":"m","max_tokens":1}"#).await;
            assert_eq!(res.status(), StatusCode::OK);
            let body: serde_json::Value = serde_json::from_slice(&read_body(res).await).unwrap();
            assert_eq!(body["upstream"], expected, "{session}");
            assert_eq!(body["key"], format!("key-{expected}"));
            assert_eq!(h.usage_record().await["upstream"], expected);
        }
        used[usize::from(expected == "team-b")] = true;
    }
    assert_eq!(used, [true, true], "16 sessions all landed on one upstream");
}

#[tokio::test]
async fn claude_platform_on_aws_gets_a_bearer_key_and_the_workspace() {
    let (handler, mut rx) = recording(|seen| seen.headers);
    let h = Harness::single_route(
        handler,
        "",
        "id = \"aws\"\nprovider = \"claude-platform-aws\"\nworkspace_id = \"wrkspc_01AbCd\"",
        UpstreamAuth::bearer("aws-short-term-key").unwrap(),
    )
    .await;
    let res = h
        .send(messages().header("x-api-key", "client-key"), "{}")
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    read_body(res).await;
    let headers = rx.recv().await.unwrap();
    assert_eq!(headers["authorization"], "Bearer aws-short-term-key");
    assert_eq!(headers["anthropic-workspace-id"], "wrkspc_01AbCd");
    assert!(headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn bedrock_gets_an_api_key_and_its_own_model_ids() {
    let (handler, mut rx) = recording(|seen| (seen.headers, seen.body));
    let h = Harness::single_route(
        handler,
        "",
        "id = \"bedrock\"\nprovider = \"bedrock\"",
        UpstreamAuth::api_key("bedrock-key").unwrap(),
    )
    .await;
    for (sent, received) in [
        (
            r#"{"model":"claude-opus-5-5","max_tokens":1}"#,
            r#"{"model":"anthropic.claude-opus-5-5","max_tokens":1}"#,
        ),
        // Already a Bedrock id: kept.
        (
            r#"{"model":"anthropic.claude-opus-5-5","max_tokens":1}"#,
            r#"{"model":"anthropic.claude-opus-5-5","max_tokens":1}"#,
        ),
    ] {
        let res = h.send(messages(), sent).await;
        assert_eq!(res.status(), StatusCode::OK);
        read_body(res).await;
        let (headers, body) = rx.recv().await.unwrap();
        assert_eq!(body, received);
        assert_eq!(
            headers["content-length"],
            received.len().to_string().as_str()
        );
        assert_eq!(headers["x-api-key"], "bedrock-key");
        assert!(headers.get("authorization").is_none());
    }
}
