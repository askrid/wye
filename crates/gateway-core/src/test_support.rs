//! Helpers the unit tests share.

use crate::policy::ModelPattern;

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
