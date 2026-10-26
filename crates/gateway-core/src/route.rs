//! Choosing the upstream for a request.
//!
//! The choice is a pure function of the request's affinity key and the
//! config, so every replica and `wye explain` make the same one with no
//! shared state. Prompt caches are per provider account, so a session stays
//! on one upstream: weighted rendezvous hashing keyed by the session id (or
//! the credential subject) sends a key to the upstream with the highest
//! score, and a key moves only when its upstream leaves the route.

use xxhash_rust::xxh3::{xxh3_64, xxh3_64_with_seed};

use crate::config::Upstream;

/// Most upstreams in a route.
pub const MAX_UPSTREAMS: usize = 16;
/// Largest `weight`.
pub const MAX_WEIGHT: u32 = 1000;

/// The upstreams that serve one API, and how a request picks one.
#[derive(Debug, Clone)]
pub struct Route {
    /// Non-empty, ids unique when there are several.
    upstreams: Box<[Upstream]>,
    /// Per upstream, the hash seed derived from its id.
    seeds: Box<[u64]>,
}

impl Route {
    /// `upstreams` is validated: non-empty, at most [`MAX_UPSTREAMS`], ids unique.
    pub(crate) fn new(upstreams: Vec<Upstream>) -> Self {
        let seeds = upstreams
            .iter()
            .map(|u| xxh3_64(u.id.as_deref().unwrap_or_default().as_bytes()))
            .collect();
        Self {
            upstreams: upstreams.into(),
            seeds,
        }
    }

    /// In config order.
    pub fn upstreams(&self) -> &[Upstream] {
        &self.upstreams
    }

    /// The index of the upstream for a request with affinity `key` (the
    /// session id, else the credential subject, else empty).
    ///
    /// Each upstream scores `weight / -ln(u)`, with `u` uniform in (0, 1)
    /// from a hash of the key seeded by the upstream's id; the highest score
    /// wins. A key then lands on an upstream with probability proportional
    /// to its weight. Floating point only ranks; an exact tie, which needs
    /// equal 53-bit hashes, goes to the earlier upstream.
    pub fn pick(&self, key: &[u8]) -> usize {
        if self.upstreams.len() == 1 {
            return 0;
        }
        let mut best = (0, f64::NEG_INFINITY);
        for (i, (upstream, seed)) in self.upstreams.iter().zip(&self.seeds).enumerate() {
            let score = f64::from(upstream.weight) / -unit(xxh3_64_with_seed(key, *seed)).ln();
            if score > best.1 {
                best = (i, score);
            }
        }
        best.0
    }
}

/// The top 53 bits of `hash` as a float in (0, 1), never 0 or 1.
fn unit(hash: u64) -> f64 {
    const STEPS: f64 = (1u64 << 53) as f64;
    ((hash >> 11) as f64 + 0.5) / STEPS
}

#[cfg(test)]
mod tests;
