use super::*;
use crate::config::Config;

/// A route with one upstream per `(id, weight)`.
fn route(upstreams: &[(&str, u32)]) -> Route {
    let mut toml = String::from("[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\n");
    for (id, weight) in upstreams {
        toml.push_str(&format!(
            "[[routes.claude.upstreams]]\nid = \"{id}\"\nweight = {weight}\n\
             base_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n"
        ));
    }
    Config::from_toml_str(&toml).unwrap().routes.claude.unwrap()
}

fn picks(route: &Route, keys: usize) -> Vec<usize> {
    (0..keys)
        .map(|i| route.pick(format!("session-{i}").as_bytes()))
        .collect()
}

#[test]
fn one_upstream_takes_everything() {
    let route = route(&[("a", 5)]);
    assert!(picks(&route, 100).iter().all(|&i| i == 0));
}

#[test]
fn shares_follow_weights() {
    let route = route(&[("a", 1), ("b", 2), ("c", 1)]);
    let keys = 40_000;
    let mut counts = [0usize; 3];
    for i in picks(&route, keys) {
        counts[i] += 1;
    }
    for (count, expected) in counts.into_iter().zip([0.25, 0.5, 0.25]) {
        let share = count as f64 / keys as f64;
        assert!((share - expected).abs() < 0.015, "{counts:?}");
    }
}

#[test]
fn adding_an_upstream_moves_keys_only_to_it() {
    let before = picks(&route(&[("a", 1), ("b", 1)]), 5_000);
    let after = picks(&route(&[("a", 1), ("b", 1), ("c", 1)]), 5_000);
    let mut moved = 0;
    for (old, new) in before.iter().zip(&after) {
        if old != new {
            assert_eq!(*new, 2, "a key moved between existing upstreams");
            moved += 1;
        }
    }
    // About a third moves to the new upstream.
    assert!((1_400..1_950).contains(&moved), "{moved}");
}

#[test]
fn removing_an_upstream_moves_only_its_keys() {
    let before = picks(&route(&[("a", 1), ("b", 1), ("c", 1)]), 5_000);
    let after = picks(&route(&[("a", 1), ("c", 1)]), 5_000);
    for (old, new) in before.iter().zip(&after) {
        match old {
            0 => assert_eq!(*new, 0),
            2 => assert_eq!(*new, 1),
            _ => {}
        }
    }
}

#[test]
fn choice_depends_on_ids_not_order() {
    let ab = picks(&route(&[("a", 1), ("b", 1)]), 1_000);
    let ba = picks(&route(&[("b", 1), ("a", 1)]), 1_000);
    for (x, y) in ab.iter().zip(&ba) {
        assert_eq!(*x, 1 - *y);
    }
}

/// Replicas running different Wye versions must agree during a rolling
/// update: changing the hash or the scoring moves every session.
#[test]
fn choice_is_pinned_across_versions() {
    let route = route(&[("team-a", 1), ("team-b", 1), ("team-c", 2)]);
    let got: Vec<usize> = [
        "",
        "alice",
        "bob",
        "carol",
        "dave",
        "8f2b3c1e-0d6a-4f7e-9b1a-2c3d4e5f6a7b",
        "svc-ci",
    ]
    .iter()
    .map(|key| route.pick(key.as_bytes()))
    .collect();
    assert_eq!(got, PINNED);
}
const PINNED: [usize; 7] = [0, 2, 2, 2, 2, 2, 2];

fn route_with(extra: &str, upstreams: &[&str]) -> Route {
    let mut toml = format!("[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\n{extra}\n");
    for id in upstreams {
        toml.push_str(&format!(
            "[[routes.claude.upstreams]]\nid = \"{id}\"\n\
             base_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n"
        ));
    }
    Config::from_toml_str(&toml).unwrap().routes.claude.unwrap()
}

#[test]
fn the_ranking_starts_with_the_pick_and_holds_every_upstream() {
    let route = route(&[("a", 1), ("b", 2), ("c", 1), ("d", 1)]);
    for i in 0..500 {
        let key = format!("session-{i}");
        let ranking: Vec<usize> = route.rank(key.as_bytes()).iter().collect();
        assert_eq!(ranking[0], route.pick(key.as_bytes()));
        let mut sorted = ranking.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, [0, 1, 2, 3]);
    }
}

#[test]
fn failover_moves_to_the_next_ranked_upstream_not_the_next_in_config() {
    // Without a's share, its keys spread over b and c by their weights.
    let all = route(&[("a", 1), ("b", 1), ("c", 1)]);
    let without_a = route(&[("b", 1), ("c", 1)]);
    for i in 0..500 {
        let key = format!("session-{i}");
        let ranking: Vec<usize> = all.rank(key.as_bytes()).iter().collect();
        if ranking[0] == 0 {
            // The second choice is where the key would go if a left.
            assert_eq!(ranking[1] - 1, without_a.pick(key.as_bytes()), "{key}");
        }
    }
}

#[test]
fn ordered_selection_goes_in_config_order() {
    let route = route_with("select = \"ordered\"", &["first", "second", "third"]);
    assert_eq!(route.select(), Select::Ordered);
    for key in ["", "s-1", "alice"] {
        assert_eq!(
            route.rank(key.as_bytes()).iter().collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }
}

#[test]
fn failover_settings_are_validated() {
    let ok = route_with("failover = [\"429\", \"connect\"]", &["a", "b"]);
    let f = ok.failover().unwrap();
    assert!(f.retries_status(429) && !f.retries_status(529) && !f.retries_status(500));
    assert!(f.on_connect);
    assert_eq!(f.max_body_bytes, DEFAULT_FAILOVER_MAX_BODY_BYTES);
    assert_eq!(f.max_held_bytes, DEFAULT_FAILOVER_MAX_HELD_BYTES);
    let parse = |extra: &str, ids: &[&str]| {
        let mut toml = format!("[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\n{extra}\n");
        for id in ids {
            toml.push_str(&format!(
                "[[routes.claude.upstreams]]\nid = \"{id}\"\nbase_url = \"https://x.example\"\napi_key_env = \"K\"\n"
            ));
        }
        Config::from_toml_str(&toml)
            .map(|_| ())
            .unwrap_err()
            .to_string()
    };
    assert!(parse("failover = [\"500\"]", &["a", "b"]).contains("may already have been processed"));
    assert!(parse("failover = [\"429\"]", &["a"]).contains("at least two upstreams"));
    assert!(parse("failover = []", &["a", "b"]).contains("at least one reason"));
    assert!(parse("failover_max_body_bytes = 10", &["a", "b"]).contains("need failover"));
    assert!(
        parse(
            "failover = [\"429\"]\nfailover_max_held_bytes = 0",
            &["a", "b"]
        )
        .contains("greater than zero")
    );
    assert!(parse("select = \"random\"", &["a", "b"]).contains("ordered"));
}
