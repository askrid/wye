//! Choosing the upstream for a request.
//!
//! The choice is a pure function of the request's affinity key and the
//! config, so every replica and `wye explain` make the same one with no
//! shared state. Prompt caches are per provider account, so a session stays
//! on one upstream: weighted rendezvous hashing keyed by the session id (or
//! the credential subject) sends a key to the upstream with the highest
//! score, and a key moves only when its upstream leaves the route. With
//! failover, the rest of the ranking is the order of the next attempts, the
//! same on every replica too.

use xxhash_rust::xxh3::{xxh3_64, xxh3_64_with_seed};

use crate::config::Upstream;

/// Most upstreams in a route.
pub const MAX_UPSTREAMS: usize = 16;
/// Largest `weight`.
pub const MAX_WEIGHT: u32 = 1000;

/// How a request picks its upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Select {
    /// Weighted rendezvous hashing by affinity key: a session stays on one
    /// upstream.
    CacheAffine,
    /// The first upstream in config order; the others only on failover.
    Ordered,
}

/// A refusal that proves a request was not processed, so it may go to the
/// next upstream: an ambiguous `5xx` may already be billed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverReason {
    /// `429`: rate limited.
    RateLimited,
    /// `529`: Anthropic overloaded.
    Overloaded,
    /// The connection failed, or was cut before the request body was
    /// delivered whole.
    Connect,
}

impl FailoverReason {
    pub const ALL: [Self; 3] = [Self::RateLimited, Self::Overloaded, Self::Connect];

    /// Its name in config, usage records, and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "429",
            Self::Overloaded => "529",
            Self::Connect => "connect",
        }
    }

    /// The refusing status; `None` for a connection.
    pub fn status(self) -> Option<u16> {
        match self {
            Self::RateLimited => Some(429),
            Self::Overloaded => Some(529),
            Self::Connect => None,
        }
    }

    /// The reason a response with `status` gives, if any.
    pub fn of_status(status: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.status() == Some(status))
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == name)
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Retrying another upstream when one refused a request before it was
/// processed ([`FailoverReason`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failover {
    /// The reasons config enabled, one bit per [`FailoverReason`].
    pub(crate) on: u8,
    /// Larger request bodies are forwarded without failover: resending needs
    /// the bytes already sent.
    pub max_body_bytes: u64,
    /// Request bytes kept for resending across the process; beyond it,
    /// requests are forwarded without failover, never refused.
    pub max_held_bytes: u64,
}

impl Failover {
    /// Whether `reason` moves a request on.
    pub fn retries(&self, reason: FailoverReason) -> bool {
        self.on & reason.bit() != 0
    }

    /// The reasons that move a request on, in [`FailoverReason::ALL`] order.
    pub fn reasons(&self) -> impl Iterator<Item = FailoverReason> + '_ {
        FailoverReason::ALL.into_iter().filter(|&r| self.retries(r))
    }

    pub(crate) fn enable(&mut self, reason: FailoverReason) {
        self.on |= reason.bit();
    }
}

/// Upstream indexes in the order a request tries them: the pick first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ranking {
    order: [u8; MAX_UPSTREAMS],
    len: usize,
}

impl Ranking {
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.order
            .get(..self.len)
            .unwrap_or_default()
            .iter()
            .map(|&i| usize::from(i))
    }

    pub fn first(&self) -> usize {
        self.order.first().map_or(0, |&i| usize::from(i))
    }
}

/// The key a request is routed and sampled by: the client's session id,
/// else the credential subject. Every replica computes the same key, so a
/// session keeps its upstream across them. Without either (no `[auth]`: one
/// loopback user), [`Route::rank`] takes the empty key and sampling none.
pub fn affinity_key<'k>(session: Option<&'k [u8]>, subject: Option<&'k str>) -> Option<&'k [u8]> {
    session.or(subject.map(str::as_bytes))
}

/// The upstreams that serve one API, and how a request picks one.
#[derive(Debug, Clone)]
pub struct Route {
    /// Non-empty, ids unique when there are several.
    upstreams: Box<[Upstream]>,
    /// Per upstream, the hash seed derived from its id.
    seeds: Box<[u64]>,
    select: Select,
    failover: Option<Failover>,
}

impl Route {
    /// `upstreams` is validated: non-empty, at most [`MAX_UPSTREAMS`], ids
    /// unique; failover only with several upstreams.
    pub(crate) fn new(
        upstreams: Vec<Upstream>,
        select: Select,
        failover: Option<Failover>,
    ) -> Self {
        let seeds = upstreams
            .iter()
            .map(|u| xxh3_64(u.id.as_deref().unwrap_or_default().as_bytes()))
            .collect();
        Self {
            upstreams: upstreams.into(),
            seeds,
            select,
            failover,
        }
    }

    /// In config order.
    pub fn upstreams(&self) -> &[Upstream] {
        &self.upstreams
    }

    /// Whether an upstream's provider has its own model ids (Bedrock's
    /// `anthropic.` prefix), so the model's place in the body is needed.
    pub fn prefixes_model_ids(&self) -> bool {
        self.upstreams()
            .iter()
            .any(|u| u.provider.model_prefix().is_some())
    }

    pub fn select(&self) -> Select {
        self.select
    }

    /// Failover between this route's upstreams, when configured.
    pub fn failover(&self) -> Option<&Failover> {
        self.failover.as_ref()
    }

    /// Every upstream in the order a request with affinity `key` tries
    /// them: [`Self::pick`] first, then (for failover) by falling score, or
    /// in config order with [`Select::Ordered`].
    pub fn rank(&self, key: &[u8]) -> Ranking {
        let len = self.upstreams.len().min(MAX_UPSTREAMS);
        let mut order = [0u8; MAX_UPSTREAMS];
        for (i, slot) in order.iter_mut().enumerate().take(len) {
            // At most MAX_UPSTREAMS (16): fits a byte.
            *slot = i as u8;
        }
        if self.select == Select::CacheAffine && len > 1 {
            let mut scores = [0f64; MAX_UPSTREAMS];
            for (score, (upstream, seed)) in scores
                .iter_mut()
                .zip(self.upstreams.iter().zip(&self.seeds))
            {
                *score = f64::from(upstream.weight) / -unit(xxh3_64_with_seed(key, *seed)).ln();
            }
            let used = order.get_mut(..len).unwrap_or_default();
            // Highest score first; equal scores keep config order (stable).
            used.sort_by(|&a, &b| {
                let (a, b) = (usize::from(a), usize::from(b));
                let score = |i: usize| scores.get(i).copied().unwrap_or_default();
                score(b).total_cmp(&score(a))
            });
        }
        Ranking { order, len }
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
        self.rank(key).first()
    }
}

/// The top 53 bits of `hash` as a float in (0, 1), never 0 or 1.
fn unit(hash: u64) -> f64 {
    const STEPS: f64 = (1u64 << 53) as f64;
    // The largest float below 1: the top step's midpoint rounds up to 1.0,
    // whose -ln is 0. Clamping moves that one value, so no session moves.
    const BELOW_ONE: f64 = 1.0 - f64::EPSILON / 2.0;
    (((hash >> 11) as f64 + 0.5) / STEPS).min(BELOW_ONE)
}

#[cfg(test)]
mod tests;
