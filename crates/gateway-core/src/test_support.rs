//! Helpers the unit tests share.

use proptest::prelude::*;

use crate::{
    config::{Config, ConfigError},
    policy::ModelPattern,
};

/// The smallest valid `[[upstreams]]` entry, with its own provider key.
pub(crate) const KEYED: &str =
    "[[upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"K\" }\n";

/// A config of a [`KEYED`] upstream and `extra`, the sections after it.
pub(crate) fn config(extra: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(&format!("{KEYED}{extra}"))
}

/// Feeds `body` to `feed` in chunks of `sizes`, repeated until the body
/// ends; a size of 0 counts as 1.
pub(crate) fn feed_sized(body: &[u8], sizes: &[usize], mut feed: impl FnMut(&[u8])) {
    let mut rest = body;
    for &size in sizes.iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let (chunk, tail) = rest.split_at(size.clamp(1, rest.len()));
        feed(chunk);
        rest = tail;
    }
}

/// `list` parsed as model patterns.
pub(crate) fn patterns(list: &[&str]) -> Vec<ModelPattern> {
    list.iter()
        .map(|p| ModelPattern::parse(p).unwrap())
        .collect()
}

/// Feeds `bytes` to `feed` in pieces, cut wherever `cuts` say (any values).
pub(crate) fn feed_cut(bytes: &[u8], cuts: Vec<usize>, mut feed: impl FnMut(&[u8])) {
    let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c % (bytes.len() + 1)).collect();
    cuts.sort_unstable();
    let mut at = 0;
    for cut in cuts {
        feed(&bytes[at..cut]);
        at = cut;
    }
    feed(&bytes[at..]);
}

/// Chunks of arbitrary bytes, half of them from `vocabulary`: the bytes the
/// scanner under test turns on.
pub(crate) fn arbitrary_chunks(vocabulary: &'static [u8]) -> impl Strategy<Value = Vec<Vec<u8>>> {
    let byte = prop_oneof![any::<u8>(), prop::sample::select(vocabulary.to_vec())];
    prop::collection::vec(prop::collection::vec(byte, 0..256), 0..32)
}
