//! `[[upstreams]]`: providers, the routes they make, and their validation.

use super::*;

fn parse_upstreams(upstreams: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(&format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n{upstreams}"
    ))
}

const ACCOUNT: &str = "base_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"K\" }\n";
const OPENAI: &str = "provider = \"openai\"\nprovider_key = { env = \"O\" }\n";

#[test]
fn upstreams_parse_with_ids_and_weights() {
    let config = parse_upstreams(&format!(
        "[[upstreams]]\nid = \"team-a\"\nweight = 3\n{ACCOUNT}\
         [[upstreams]]\nid = \"team-b\"\nprovider = \"anthropic\"\n\
         base_url = \"https://api.anthropic.com\"\nprovider_key = {{ file = \"b.key\" }}\n"
    ))
    .unwrap();
    let upstreams = claude(&config).upstreams();
    assert_eq!(upstreams.len(), 2);
    assert_eq!(&*upstreams[0].id, "team-a");
    assert_eq!(upstreams[0].weight, 3);
    assert_eq!(&*upstreams[1].id, "team-b");
    assert_eq!(upstreams[1].weight, 1);
    assert!(!upstreams[1].standby);
    assert_eq!(
        upstreams[1].provider_key,
        ProviderKey::Secret(Secret::File("b.key".into()))
    );
    // Without an id, an upstream is named by its provider; it may still
    // have one.
    let alone = parse(ACCOUNT).unwrap();
    assert_eq!(&*claude(&alone).upstreams()[0].id, "anthropic");
    let named = parse(&format!("id = \"a\"\n{ACCOUNT}")).unwrap();
    assert_eq!(&*claude(&named).upstreams()[0].id, "a");
}

#[test]
fn the_provider_decides_the_route() {
    let bedrock = "provider = \"bedrock\"\n\
                   base_url = \"https://bedrock-mantle.us-east-1.api.aws/anthropic\"\n\
                   provider_key = { env = \"B\" }\n";
    // Entries of both routes, interleaved: each route keeps config order.
    let config = parse_upstreams(&format!(
        "[[upstreams]]\nid = \"oa-1\"\n{OPENAI}[[upstreams]]\n{ACCOUNT}\
         [[upstreams]]\nid = \"oa-2\"\n{OPENAI}[[upstreams]]\n{bedrock}"
    ))
    .unwrap();
    fn ids(route: &Route) -> Vec<&str> {
        route.upstreams().iter().map(|u| &*u.id).collect()
    }
    assert_eq!(ids(claude(&config)), ["anthropic", "bedrock"]);
    assert_eq!(
        ids(config.routes.openai.as_ref().unwrap()),
        ["oa-1", "oa-2"]
    );
    for (provider, route) in [
        (Provider::Anthropic, RouteId::Claude),
        (Provider::ClaudePlatformAws, RouteId::Claude),
        (Provider::Bedrock, RouteId::Claude),
        (Provider::OpenAi, RouteId::OpenAi),
    ] {
        assert_eq!(provider.route(), route);
        assert_eq!(Provider::parse(provider.as_str()), Some(provider));
    }
    assert_eq!(Provider::parse("vertex"), None);
}

#[test]
fn an_upstream_is_named_by_its_provider_unless_another_shares_it() {
    let bedrock = "provider = \"bedrock\"\n\
                   base_url = \"https://bedrock-mantle.us-east-1.api.aws/anthropic\"\n\
                   provider_key = { env = \"B\" }\n";
    let mixed = parse_upstreams(&format!(
        "[[upstreams]]\n{ACCOUNT}[[upstreams]]\n{bedrock}[[upstreams]]\n{OPENAI}"
    ))
    .unwrap();
    let ids: Vec<&str> = mixed.routes.upstreams().map(|u| &*u.id).collect();
    assert_eq!(ids, ["anthropic", "bedrock", "openai"]);
    // Two of one provider each need an id, the one that has one too.
    for second in ["", "id = \"b\"\n"] {
        let err = parse_upstreams(&format!(
            "[[upstreams]]\n{ACCOUNT}[[upstreams]]\n{second}{ACCOUNT}"
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "upstreams: entries that share a provider each need an id"
        );
    }
    // A written id may not take another's default, in either route.
    for other in [bedrock, OPENAI] {
        let id = if other == OPENAI { "openai" } else { "bedrock" };
        let taken = parse_upstreams(&format!(
            "[[upstreams]]\nid = \"{id}\"\n{ACCOUNT}[[upstreams]]\n{other}"
        ))
        .unwrap_err();
        assert_eq!(
            taken.to_string(),
            format!("upstreams \"{id}\": id is used twice")
        );
    }
    // An entry whose provider is wrong is named by what it wrote.
    let wrong = parse_upstreams("[[upstreams]]\nprovider = \"vertex\"\n").unwrap_err();
    assert_eq!(
        wrong.to_string(),
        "upstreams \"vertex\": provider must be \"anthropic\", \"claude-platform-aws\", \
         \"bedrock\", or \"openai\" (Vertex is planned)"
    );
}

#[test]
fn anthropic_and_openai_need_no_base_url_and_regional_providers_do() {
    let config = Config::from_toml_str(
        "[[upstreams]]\nforward_caller_credential = true\n\
         [[upstreams]]\nprovider = \"openai\"\nprovider_key = { file = \"openai.key\" }\n",
    )
    .unwrap();
    let base: Vec<String> = config.routes.upstreams().map(|u| u.base_url()).collect();
    assert_eq!(
        base,
        ["https://api.anthropic.com", "https://api.openai.com"]
    );
    // Bedrock and Claude Platform on AWS URLs name a region.
    for (provider, extra, region) in [
        ("bedrock", "", "a Bedrock URL"),
        (
            "claude-platform-aws",
            "workspace_id = \"wrkspc_1\"\n",
            "a Claude Platform on AWS URL",
        ),
    ] {
        let err = parse_upstreams(&format!(
            "[[upstreams]]\nprovider = \"{provider}\"\n{extra}\
             provider_key = {{ env = \"K\" }}\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.starts_with(&format!(
                "upstreams \"{provider}\": set base_url: {region} names its region"
            )),
            "{err}"
        );
    }
}

#[test]
fn upstreams_are_validated() {
    let entry = |extra: &str| format!("[[upstreams]]\n{extra}\n{ACCOUNT}");
    let rejected = [
        (
            String::new(),
            "add an [[upstreams]] entry: its provider (by default \"anthropic\") decides \
             which API it serves",
        ),
        (
            entry("") + &entry("id = \"a\""),
            "upstreams: entries that share a provider each need an id",
        ),
        (entry("id = \"a b\""), "letters, digits"),
        (entry("id = \"\""), "letters, digits"),
        (entry("id = \"a\"") + &entry("id = \"a\""), "used twice"),
        (
            entry("id = \"a\"") + &format!("[[upstreams]]\nid = \"a\"\n{OPENAI}"),
            "upstreams \"a\": id is used twice",
        ),
        (entry("id = \"a\"\nweight = 0"), "between 1 and 1000"),
        (entry("id = \"a\"\nweight = 1001"), "between 1 and 1000"),
        (entry("id = \"a\"\nprovider = \"vertex\""), "provider"),
        (
            entry("id = \"a\"\nselect = \"ordered\""),
            "unknown field `select`",
        ),
        (
            "[[upstreams]]\nid = \"a\"\nbase_url = \"https://api.anthropic.com\"\n\
             forward_caller_credential = true\n"
                .to_owned()
                + &entry("id = \"b\""),
            "upstreams \"a\": forward_caller_credential needs a route with one upstream: \
             several upstreams use their own provider keys",
        ),
        (
            entry("id = \"a\"\nweight = 0").replace("id = \"a\"\n", ""),
            "upstreams \"anthropic\": weight must be between 1 and 1000",
        ),
        (
            entry("id = \"a\"").replace(
                "provider_key = { env = \"K\" }",
                "provider_key = { env = \"1K\" }",
            ),
            "upstreams \"a\": provider_key: \"1K\" is not a valid",
        ),
        (
            entry("id = \"a\"").replace("https://api.anthropic.com", "http://x.example"),
            "upstreams \"a\": base_url \"http://x.example\": plain http",
        ),
        (
            // One key per upstream: a second is a duplicate.
            entry("id = \"a\"\nprovider_key = { file = \"k\" }") + &entry("id = \"b\""),
            "duplicate key",
        ),
        (
            "[[upstreams]]\nid = \"a\"\nbase_url = \"https://x.example\"\n".to_owned()
                + &entry("id = \"b\""),
            "upstreams \"a\": set provider_key = { env = \"NAME\" } or \
             { file = \"path\" }",
        ),
        (
            "[[upstreams]]\nbase_url = \"https://x.example\"\n".into(),
            "upstreams \"anthropic\": set provider_key = { env = \"NAME\" } or \
             { file = \"path\" }, or forward_caller_credential = true",
        ),
        (
            entry("id = \"a\"\nconnect_timeout = \"0s\""),
            "upstreams \"a\": connect_timeout must be greater than zero",
        ),
    ];
    for (upstreams, expected) in rejected {
        let err = parse_upstreams(&upstreams)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{upstreams:?}: {err}");
    }
    let empty = Config::from_toml_str("upstreams = []").unwrap_err();
    assert!(
        empty.to_string().starts_with("add an [[upstreams]] entry"),
        "{empty}"
    );
}

/// The limit is per route: 16 Claude upstreams beside an OpenAI one are
/// fine, 17 are not.
#[test]
fn a_route_takes_at_most_16_upstreams() {
    let entry = |i: usize| format!("[[upstreams]]\nid = \"a{i}\"\n{ACCOUNT}");
    let sixteen: String = (0..16).map(entry).collect();
    let config = parse_upstreams(&format!("{sixteen}[[upstreams]]\n{OPENAI}")).unwrap();
    assert_eq!(claude(&config).upstreams().len(), 16);
    let err = parse_upstreams(&format!("{sixteen}{}", entry(16))).unwrap_err();
    assert_eq!(
        err.to_string(),
        "upstreams: the Claude route takes at most 16 upstreams"
    );
}

#[test]
fn a_standby_takes_no_weight_and_a_route_needs_one_that_is_not() {
    let config = parse_upstreams(&format!(
        "[[upstreams]]\nid = \"a\"\n{ACCOUNT}[[upstreams]]\nid = \"b\"\nstandby = true\n{ACCOUNT}"
    ))
    .unwrap();
    let route = claude(&config);
    assert!(!route.upstreams()[0].standby && route.upstreams()[1].standby);
    assert_eq!(route.sharing(), 1);
    let weighted = parse_upstreams(&format!(
        "[[upstreams]]\nid = \"a\"\n{ACCOUNT}\
         [[upstreams]]\nid = \"b\"\nstandby = true\nweight = 2\n{ACCOUNT}"
    ))
    .unwrap_err();
    assert_eq!(
        weighted.to_string(),
        "upstreams \"b\": weight does not apply to a standby: standbys take failover in \
         config order"
    );
    // Every route is checked on its own: the OpenAI one is all standby.
    let all_standby = parse_upstreams(&format!(
        "[[upstreams]]\n{ACCOUNT}[[upstreams]]\nstandby = true\n{OPENAI}"
    ))
    .unwrap_err();
    assert_eq!(
        all_standby.to_string(),
        "upstreams: the OpenAI route needs an upstream that is not standby: standbys take \
         only failover"
    );
}

#[test]
fn a_route_is_required_and_the_old_sections_are_gone() {
    let neither = Config::from_toml_str("[server]\nlisten = \"127.0.0.1:8080\"\n").unwrap_err();
    assert!(
        neither
            .to_string()
            .starts_with("add an [[upstreams]] entry"),
        "{neither}"
    );
    for old in [
        format!("[upstream]\n{ACCOUNT}"),
        format!("[[routes.claude.upstreams]]\n{ACCOUNT}"),
    ] {
        let err = parse_upstreams(&old).unwrap_err().to_string();
        assert!(err.contains("unknown field"), "{err}");
    }
}

/// Passthrough on either route: its only upstream, loopback only.
#[test]
fn only_a_routes_only_upstream_forwards_the_caller_credential() {
    let forwarding = parse(FORWARDING).unwrap();
    assert_eq!(
        claude(&forwarding).upstreams()[0].provider_key,
        ProviderKey::ForwardCaller
    );
    let openai = "[[upstreams]]\nprovider = \"openai\"\nforward_caller_credential = true\n";
    let both = parse_upstreams(&format!("[[upstreams]]\n{FORWARDING}{openai}")).unwrap();
    for route in [claude(&both), both.routes.openai.as_ref().unwrap()] {
        assert_eq!(
            route.upstreams()[0].provider_key,
            ProviderKey::ForwardCaller
        );
    }
    // A Claude upstream beside it does not make it share its route.
    let beside = parse_upstreams(&format!(
        "[[upstreams]]\nid = \"a\"\n{ACCOUNT}[[upstreams]]\nid = \"b\"\n{ACCOUNT}{openai}"
    ));
    assert!(beside.is_ok(), "{beside:?}");
    let two = parse_upstreams(&format!(
        "{}[[upstreams]]\nid = \"oa\"\n{OPENAI}",
        openai.replace("[[upstreams]]\n", "[[upstreams]]\nid = \"mine\"\n")
    ))
    .unwrap_err();
    assert!(
        two.to_string().starts_with(
            "upstreams \"mine\": forward_caller_credential needs a route with one upstream"
        ),
        "{two}"
    );
    let shared = Config::from_toml_str(&format!(
        "[server]\nlisten = \"0.0.0.0:8080\"\n[auth]\naudience = \"wye\"\n\
         signing_keys = [\"{PUBLIC_KEY}\"]\n{openai}"
    ))
    .unwrap_err();
    assert!(
        shared
            .to_string()
            .contains("cannot be combined with forward_caller_credential"),
        "{shared}"
    );
}

#[test]
fn openai_route_alone_or_beside_claude() {
    let openai = "[[upstreams]]\nprovider = \"openai\"\nid = \"oa\"\n\
                  base_url = \"https://api.openai.com\"\nprovider_key = { env = \"OPENAI_API_KEY\" }\n";
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
        "[server]\nlisten = \"127.0.0.1:8080\"\n[[upstreams]]\n{ACCOUNT}{openai}"
    ))
    .unwrap();
    assert_eq!(claude(&both).upstreams()[0].provider, Provider::Anthropic);
    let ids: Vec<_> = both.routes.upstreams().map(|u| &*u.id).collect();
    assert_eq!(ids, ["anthropic", "oa"]);
}

#[test]
fn base_url_ending_in_v1_is_refused() {
    for url in [
        "https://api.openai.com/v1",
        "https://gw.example.com/anthropic/v1/",
    ] {
        let err = parse(&format!(
            "base_url = \"{url}\"\nprovider_key = {{ env = \"K\" }}"
        ))
        .unwrap_err();
        assert!(
            err.to_string().starts_with(&format!(
                "upstreams \"anthropic\": base_url \"{url}\": drop the trailing /v1"
            )),
            "{err}"
        );
    }
    assert!(
        parse("base_url = \"https://gw.example.com/v1beta\"\nprovider_key = { env = \"K\" }")
            .is_ok()
    );
}

#[test]
fn claude_platform_on_aws_needs_a_workspace() {
    let entry = |extra: &str| {
        format!(
            "[[upstreams]]\nid = \"aws\"\n{extra}\n\
             base_url = \"https://aws-external-anthropic.us-west-2.api.aws\"\nprovider_key = {{ env = \"K\" }}\n"
        )
    };
    let ok = parse_upstreams(&entry(
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
        let err = parse_upstreams(&entry(extra))
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{extra}: {err}");
    }
}

#[test]
fn bedrock_takes_an_api_key_and_prefixes_model_ids() {
    let config = parse_upstreams(
        "[[upstreams]]\nid = \"bedrock\"\nprovider = \"bedrock\"\n\
         base_url = \"https://bedrock-mantle.us-east-1.api.aws/anthropic\"\nprovider_key = { env = \"K\" }\n",
    )
    .unwrap();
    let upstream = &claude(&config).upstreams()[0];
    assert_eq!(upstream.provider, Provider::Bedrock);
    assert_eq!(upstream.path_prefix, "/anthropic");
    assert!(!upstream.provider.bearer_key());
    assert_eq!(upstream.provider.model_prefix(), Some("anthropic."));
}

#[test]
fn pricing_scoped_to_upstreams_names_upstreams() {
    let upstreams = format!(
        "[[upstreams]]\nid = \"aws-east\"\n{ACCOUNT}\
         [pricing]\nversion = \"v\"\n[[pricing.prices]]\nmodels = [\"claude-*\"]\n\
         input = \"1\"\noutput = \"2\"\nupstreams = [\"UPSTREAM\"]\n"
    );
    let ok = parse_upstreams(&upstreams.replace("UPSTREAM", "aws-east")).unwrap();
    assert_eq!(&*ok.pricing.unwrap().entries[0].upstreams[0], "aws-east");
    let err = parse_upstreams(&upstreams.replace("UPSTREAM", "aws-wset"))
        .map(|_| ())
        .unwrap_err();
    assert!(
        err.to_string().contains("must name one of [[upstreams]]"),
        "{err}"
    );
}
