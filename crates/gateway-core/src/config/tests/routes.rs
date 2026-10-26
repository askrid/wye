//! `[upstream]` and `[routes]`: upstreams, providers, and their validation.

use super::*;

fn parse_route(route: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[routes.claude]\n{route}"
    ))
}

const ACCOUNT: &str = "base_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n";

#[test]
fn route_upstreams_parse_with_ids_and_weights() {
    let config = parse_route(&format!(
        "select = \"cache_affine\"\n\
         [[routes.claude.upstreams]]\nid = \"team-a\"\nweight = 3\n{ACCOUNT}\
         [[routes.claude.upstreams]]\nid = \"team-b\"\nprovider = \"anthropic\"\n\
         base_url = \"https://api.anthropic.com\"\napi_key_file = \"b.key\"\n"
    ))
    .unwrap();
    let upstreams = claude(&config).upstreams();
    assert_eq!(upstreams.len(), 2);
    assert_eq!(upstreams[0].id.as_deref(), Some("team-a"));
    assert_eq!(upstreams[0].weight, 3);
    assert_eq!(upstreams[1].id.as_deref(), Some("team-b"));
    assert_eq!(upstreams[1].weight, 1);
    assert_eq!(
        upstreams[1].credential,
        UpstreamCredential::ApiKeyFile("b.key".into())
    );
    // The single [upstream] has no id.
    let single = parse(ACCOUNT).unwrap();
    assert_eq!(claude(&single).upstreams()[0].id, None);
}

#[test]
fn route_upstreams_are_validated() {
    let entry = |extra: &str| format!("[[routes.claude.upstreams]]\n{extra}\n{ACCOUNT}");
    let rejected = [
        (String::new(), "missing field `upstreams`"),
        ("upstreams = []".into(), "between 1 and 16"),
        (entry(""), "needs an id"),
        (entry("id = \"a b\""), "letters, digits"),
        (entry("id = \"\""), "letters, digits"),
        (entry("id = \"a\"") + &entry("id = \"a\""), "used twice"),
        (entry("id = \"a\"\nweight = 0"), "between 1 and 1000"),
        (entry("id = \"a\"\nweight = 1001"), "between 1 and 1000"),
        (entry("id = \"a\"\nprovider = \"vertex\""), "provider"),
        (
            format!("select = \"random\"\n{}", entry("id = \"a\"")),
            "cache_affine",
        ),
        (
            "[[routes.claude.upstreams]]\nid = \"a\"\nbase_url = \"https://api.anthropic.com\"\n\
             forward_caller_credential = true"
                .into(),
            "single [upstream]",
        ),
        (
            entry("id = \"a\"").replace("api_key_env = \"K\"", "api_key_env = \"1K\""),
            "routes.claude.upstreams \"a\": upstream.api_key_env",
        ),
    ];
    for (route, expected) in rejected {
        let err = parse_route(&route).map(|_| ()).unwrap_err().to_string();
        assert!(err.contains(expected), "{route:?}: {err}");
    }
    let many: String = (0..17).map(|i| entry(&format!("id = \"a{i}\""))).collect();
    assert!(parse_route(&many).is_err());
}

#[test]
fn upstream_and_routes_are_exclusive() {
    let both = Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[upstream]\n{ACCOUNT}\
         [routes.claude]\n[[routes.claude.upstreams]]\nid = \"a\"\n{ACCOUNT}"
    ))
    .unwrap_err();
    assert!(both.to_string().contains("not both"), "{both}");
    let neither = Config::from_toml_str("[server]\nlisten = \"127.0.0.1:8080\"\n").unwrap_err();
    assert!(neither.to_string().contains("set [upstream]"), "{neither}");
    let id_in_single = parse(&format!("id = \"a\"\n{ACCOUNT}")).unwrap_err();
    assert!(
        id_in_single.to_string().contains("belong in"),
        "{id_in_single}"
    );
}

#[test]
fn openai_route_alone_or_beside_claude() {
    let openai = "[routes.openai]\n[[routes.openai.upstreams]]\nid = \"oa\"\n\
                  base_url = \"https://api.openai.com\"\napi_key_env = \"OPENAI_API_KEY\"\n";
    let alone =
        Config::from_toml_str(&format!("[server]\nlisten = \"127.0.0.1:8080\"\n{openai}")).unwrap();
    assert!(alone.routes.claude.is_none());
    let route = alone.routes.openai.as_ref().unwrap();
    assert_eq!(route.upstreams()[0].provider, Provider::OpenAi);
    assert_eq!(
        alone.routes.get(Api::Responses).unwrap().upstreams().len(),
        1
    );
    assert!(alone.routes.get(Api::Messages).is_none());

    let both = Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[upstream]\n{ACCOUNT}{openai}"
    ))
    .unwrap();
    assert_eq!(claude(&both).upstreams()[0].provider, Provider::Anthropic);
    let ids: Vec<_> = both.routes.upstreams().map(|u| u.id.as_deref()).collect();
    assert_eq!(ids, [None, Some("oa")]);

    let wrong_provider = Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n{}",
        openai.replace("id = \"oa\"", "id = \"oa\"\nprovider = \"anthropic\"")
    ))
    .unwrap_err();
    assert!(
        wrong_provider
            .to_string()
            .contains("routes.openai.upstreams \"oa\": provider must be \"openai\""),
        "{wrong_provider}"
    );
}

#[test]
fn base_url_ending_in_v1_is_refused() {
    for url in [
        "https://api.openai.com/v1",
        "https://gw.example.com/anthropic/v1/",
    ] {
        let err = parse(&format!("base_url = \"{url}\"\napi_key_env = \"K\"")).unwrap_err();
        assert!(err.to_string().contains("drop the trailing /v1"), "{err}");
    }
    assert!(parse("base_url = \"https://gw.example.com/v1beta\"\napi_key_env = \"K\"").is_ok());
}

#[test]
fn claude_platform_on_aws_needs_a_workspace() {
    let entry = |extra: &str| {
        format!(
            "[[routes.claude.upstreams]]\nid = \"aws\"\n{extra}\n\
             base_url = \"https://aws-external-anthropic.us-west-2.api.aws\"\napi_key_env = \"K\"\n"
        )
    };
    let ok = parse_route(&entry(
        "provider = \"claude-platform-aws\"\nworkspace_id = \"wrkspc_01AbCdEf23GhIj\"",
    ))
    .unwrap();
    let upstream = &claude(&ok).upstreams()[0];
    assert_eq!(upstream.provider, Provider::ClaudePlatformAws);
    assert_eq!(
        upstream.workspace_id.as_deref(),
        Some("wrkspc_01AbCdEf23GhIj")
    );
    let refused = [
        ("provider = \"claude-platform-aws\"", "needs workspace_id"),
        (
            "provider = \"claude-platform-aws\"\nworkspace_id = \"ws-1\"",
            "wrkspc_ followed by",
        ),
        ("workspace_id = \"wrkspc_1\"", "claude-platform-aws\" only"),
        ("provider = \"bedrock\"", "Bedrock and Vertex are planned"),
    ];
    for (extra, expected) in refused {
        let err = parse_route(&entry(extra))
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{extra}: {err}");
    }
}
