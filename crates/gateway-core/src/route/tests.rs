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
    Config::from_toml_str(&toml).unwrap().route
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
