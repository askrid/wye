//! `[routes]`: upstreams, providers, and their validation.

use super::*;

fn parse_route(route: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[routes.claude]\n{route}"
    ))
}

const ACCOUNT: &str = "base_url = \"https://api.anthropic.com\"\nprovider_key_env = \"K\"\n";

#[test]
fn route_upstreams_parse_with_ids_and_weights() {
    let config = parse_route(&format!(
        "select = \"cache_affine\"\n\
         [[routes.claude.upstreams]]\nid = \"team-a\"\nweight = 3\n{ACCOUNT}\
         [[routes.claude.upstreams]]\nid = \"team-b\"\nprovider = \"anthropic\"\n\
         base_url = \"https://api.anthropic.com\"\nprovider_key_file = \"b.key\"\n"
    ))
    .unwrap();
    let upstreams = claude(&config).upstreams();
    assert_eq!(upstreams.len(), 2);
    assert_eq!(upstreams[0].id.as_deref(), Some("team-a"));
    assert_eq!(upstreams[0].weight, 3);
    assert_eq!(upstreams[1].id.as_deref(), Some("team-b"));
    assert_eq!(upstreams[1].weight, 1);
    assert_eq!(upstreams[1].provider_key, ProviderKey::File("b.key".into()));
    // A route's only upstream needs no id; it may still have one.
    let alone = parse(ACCOUNT).unwrap();
    assert_eq!(claude(&alone).upstreams()[0].id, None);
    let named = parse(&format!("id = \"a\"\n{ACCOUNT}")).unwrap();
    assert_eq!(claude(&named).upstreams()[0].id.as_deref(), Some("a"));
}

#[test]
fn route_upstreams_are_validated() {
    let entry = |extra: &str| format!("[[routes.claude.upstreams]]\n{extra}\n{ACCOUNT}");
    let rejected = [
        (String::new(), "missing field `upstreams`"),
        (
            "upstreams = []".into(),
            "routes.claude.upstreams must have between 1 and 16 entries",
        ),
        (
            entry("") + &entry("id = \"a\""),
            "routes.claude.upstreams: every entry needs an id when the route has two or more",
        ),
        (entry("id = \"a b\""), "letters, digits"),
        (entry("id = \"\""), "letters, digits"),
        (entry("id = \"a\"") + &entry("id = \"a\""), "used twice"),
        (
            entry("id = \"a\"")
                + "[routes.openai]\n[[routes.openai.upstreams]]\nid = \"a\"\n\
                   base_url = \"https://api.openai.com\"\nprovider_key_env = \"O\"\n",
            "routes.openai.upstreams \"a\": id is used by routes.claude too",
        ),
        (entry("id = \"a\"\nweight = 0"), "between 1 and 1000"),
        (entry("id = \"a\"\nweight = 1001"), "between 1 and 1000"),
        (entry("id = \"a\"\nprovider = \"vertex\""), "provider"),
        (
            format!("select = \"random\"\n{}", entry("id = \"a\"")),
            "routes.claude.select must be \"cache_affine\" or \"ordered\"",
        ),
        (
            "[[routes.claude.upstreams]]\nid = \"a\"\nbase_url = \"https://api.anthropic.com\"\n\
             forward_caller_credential = true\n"
                .to_owned()
                + &entry("id = \"b\""),
            "routes.claude.upstreams: forward_caller_credential needs a route with one upstream",
        ),
        (
            entry("id = \"a\"\nweight = 0").replace("id = \"a\"\n", ""),
            "routes.claude.upstreams[0]: weight must be between 1 and 1000",
        ),
        (
            entry("id = \"a\"").replace("provider_key_env = \"K\"", "provider_key_env = \"1K\""),
            "routes.claude.upstreams \"a\": provider_key_env: \"1K\" is not a valid",
        ),
        (
            entry("id = \"a\"").replace("https://api.anthropic.com", "http://x.example"),
            "routes.claude.upstreams \"a\": base_url \"http://x.example\": plain http",
        ),
        (
            entry("id = \"a\"\nprovider_key_file = \"k\"") + &entry("id = \"b\""),
            "routes.claude.upstreams \"a\": set only one of provider_key_env and provider_key_file",
        ),
        (
            "[[routes.claude.upstreams]]\nid = \"a\"\nbase_url = \"https://x.example\"\n"
                .to_owned()
                + &entry("id = \"b\""),
            "routes.claude.upstreams \"a\": set provider_key_env or provider_key_file",
        ),
        (
            "[[routes.claude.upstreams]]\nbase_url = \"https://x.example\"\n".into(),
            "routes.claude.upstreams[0]: set provider_key_env, provider_key_file, or \
             forward_caller_credential = true",
        ),
        (
            entry("id = \"a\"\nconnect_timeout_secs = 0"),
            "routes.claude.upstreams \"a\": connect_timeout_secs must be greater than zero",
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
fn a_route_is_required_and_the_upstream_section_is_gone() {
    let neither = Config::from_toml_str("[server]\nlisten = \"127.0.0.1:8080\"\n").unwrap_err();
    assert!(
        neither
            .to_string()
            .contains("set [[routes.claude.upstreams]]"),
        "{neither}"
    );
    let short = Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[upstream]\n{ACCOUNT}"
    ))
    .unwrap_err();
    assert!(
        short.to_string().contains("unknown field `upstream`"),
        "{short}"
    );
}

#[test]
fn only_a_claude_route_with_one_upstream_forwards_the_caller_credential() {
    let forwarding = parse(FORWARDING).unwrap();
    assert_eq!(
        claude(&forwarding).upstreams()[0].provider_key,
        ProviderKey::ForwardCaller
    );
    let openai = Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n[[routes.openai.upstreams]]\n{FORWARDING}"
    ))
    .unwrap_err();
    assert_eq!(
        openai.to_string(),
        "routes.openai.upstreams[0]: forward_caller_credential belongs to routes.claude only"
    );
}

#[test]
fn openai_route_alone_or_beside_claude() {
    let openai = "[routes.openai]\n[[routes.openai.upstreams]]\nid = \"oa\"\n\
                  base_url = \"https://api.openai.com\"\nprovider_key_env = \"OPENAI_API_KEY\"\n";
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
        "[server]\nlisten = \"127.0.0.1:8080\"\n[[routes.claude.upstreams]]\n{ACCOUNT}{openai}"
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
        let err = parse(&format!("base_url = \"{url}\"\nprovider_key_env = \"K\"")).unwrap_err();
        assert!(
            err.to_string().starts_with(&format!(
                "routes.claude.upstreams[0]: base_url \"{url}\": drop the trailing /v1"
            )),
            "{err}"
        );
    }
    assert!(
        parse("base_url = \"https://gw.example.com/v1beta\"\nprovider_key_env = \"K\"").is_ok()
    );
}

#[test]
fn claude_platform_on_aws_needs_a_workspace() {
    let entry = |extra: &str| {
        format!(
            "[[routes.claude.upstreams]]\nid = \"aws\"\n{extra}\n\
             base_url = \"https://aws-external-anthropic.us-west-2.api.aws\"\nprovider_key_env = \"K\"\n"
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
        ("provider = \"vertex\"", "Vertex is planned"),
        (
            "provider = \"bedrock\"\nworkspace_id = \"wrkspc_1\"",
            "claude-platform-aws\" only",
        ),
    ];
    for (extra, expected) in refused {
        let err = parse_route(&entry(extra))
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{extra}: {err}");
    }
}

#[test]
fn bedrock_takes_an_api_key_and_prefixes_model_ids() {
    let route = parse_route(
        "[[routes.claude.upstreams]]\nid = \"bedrock\"\nprovider = \"bedrock\"\n\
         base_url = \"https://bedrock-mantle.us-east-1.api.aws/anthropic\"\nprovider_key_env = \"K\"\n",
    )
    .unwrap();
    let upstream = &claude(&route).upstreams()[0];
    assert_eq!(upstream.provider, Provider::Bedrock);
    assert_eq!(upstream.path_prefix, "/anthropic");
    assert!(!upstream.provider.bearer_key());
    assert_eq!(upstream.provider.model_prefix(), Some("anthropic."));
}

#[test]
fn pricing_scoped_to_upstreams_names_route_upstreams() {
    let route = format!(
        "[[routes.claude.upstreams]]\nid = \"aws-east\"\n{ACCOUNT}\
         [pricing]\nversion = \"v\"\n[[pricing.prices]]\nmodels = [\"claude-*\"]\n\
         input = \"1\"\noutput = \"2\"\nupstreams = [\"UPSTREAM\"]\n"
    );
    let ok = parse_route(&route.replace("UPSTREAM", "aws-east")).unwrap();
    assert_eq!(&*ok.pricing.unwrap().entries[0].upstreams[0], "aws-east");
    let err = parse_route(&route.replace("UPSTREAM", "aws-wset"))
        .map(|_| ())
        .unwrap_err();
    assert!(
        err.to_string().contains("must name a route upstream"),
        "{err}"
    );
}
