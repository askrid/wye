//! Choosing the upstream for a request.
//!
//! The choice is a pure function of the request's affinity key and the
//! config, so every replica and `wye explain` make the same one with no
//! shared state. Prompt caches are per provider account, so a session stays
//! on one upstream: weighted rendezvous hashing keyed by the session id (or
//! the gateway token's subject) sends a key to the upstream with the highest
//! score, and a key moves only when its upstream leaves the route. Standby
//! upstreams take no keys. With several upstreams, the rest of the ranking
//! is the order of failover's next attempts, the same on every replica too:
//! the others that are not standby by falling score, then the standbys in
//! config order.

use xxhash_rust::xxh3::{xxh3_64, xxh3_64_with_seed};

use crate::config::Upstream;

/// Most upstreams in a route.
pub(crate) const MAX_UPSTREAMS: usize = 16;
/// Largest `weight`.
pub(crate) const MAX_WEIGHT: u32 = 1000;

/// Largest request body a route with several upstreams keeps for
/// resending: larger ones go without failover. A Claude Code turn is 1-8 MB.
pub const FAILOVER_MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// A rejection that proves a request was not processed, so it goes to the
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

    /// Its name in usage records and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "429",
            Self::Overloaded => "529",
            Self::Connect => "connect",
        }
    }

    /// The rejecting status; `None` for a connection.
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

/// The key that picks a request's upstream and its sampling: the client's
/// session id, else the gateway token's subject. Every replica computes the
/// same key, so a session keeps its upstream across them. Without either (no
/// `[auth]`: one loopback user), [`Route::rank`] takes the empty key, and
/// sampling takes such requests only at 100%.
pub fn affinity_key<'k>(session: Option<&'k [u8]>, subject: Option<&'k str>) -> Option<&'k [u8]> {
    session.or(subject.map(str::as_bytes))
}

/// A route: the upstreams that serve one provider family's native APIs.
/// Each `[[upstreams]]` entry serves the route its provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteId {
    /// Anthropic Messages: providers `anthropic`, `claude-platform-aws`, and
    /// `bedrock`.
    Claude,
    /// OpenAI Responses and Chat Completions: provider `openai`.
    OpenAi,
}

impl RouteId {
    pub const ALL: [Self; 2] = [Self::Claude, Self::OpenAi];

    /// Its name in messages: `the Claude route`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::OpenAi => "OpenAI",
        }
    }

    /// The config entry that gives it an upstream, for messages that say
    /// what to add: an entry without a provider serves Claude.
    pub fn entry(self) -> &'static str {
        match self {
            Self::Claude => "an [[upstreams]] entry",
            Self::OpenAi => "an [[upstreams]] entry with provider = \"openai\"",
        }
    }
}

/// The upstreams that serve one API, and how a request picks one.
#[derive(Debug, Clone)]
pub struct Route {
    /// Non-empty, ids unique, at least one not standby.
    upstreams: Box<[Upstream]>,
    /// Per upstream, the hash seed derived from its id.
    seeds: Box<[u64]>,
    /// Upstream indexes, those that are not standby first, then the
    /// standbys, each in config order.
    order: [u8; MAX_UPSTREAMS],
    /// How many of `order` are not standby: the ones that share keys.
    sharing: usize,
}

impl Route {
    /// `upstreams` is validated: non-empty, at most [`MAX_UPSTREAMS`], ids
    /// unique, at least one not standby.
    pub(crate) fn new(upstreams: Vec<Upstream>) -> Self {
        let seeds = upstreams.iter().map(|u| xxh3_64(u.id.as_bytes())).collect();
        let of = |standby: bool| {
            upstreams
                .iter()
                .enumerate()
                .filter(move |(_, u)| u.standby == standby)
                .map(|(i, _)| i)
        };
        let mut order = [0u8; MAX_UPSTREAMS];
        for (slot, i) in order.iter_mut().zip(of(false).chain(of(true))) {
            // At most MAX_UPSTREAMS (16): fits a byte.
            *slot = i as u8;
        }
        let sharing = of(false).count();
        Self {
            upstreams: upstreams.into(),
            seeds,
            order,
            sharing,
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

    /// How many upstreams share the route's affinity keys: those that are
    /// not standby.
    pub fn sharing(&self) -> usize {
        self.sharing
    }

    /// Whether a rejected request moves to the next upstream: with two or
    /// more, on every [`FailoverReason`], for bodies up to
    /// [`FAILOVER_MAX_BODY_BYTES`].
    pub fn fails_over(&self) -> bool {
        self.upstreams.len() > 1
    }

    /// Every upstream in the order a request with affinity `key` tries
    /// them: [`Self::pick`] first, then (for failover) the others that are
    /// not standby by falling score, then the standbys in config order.
    pub fn rank(&self, key: &[u8]) -> Ranking {
        let len = self.upstreams.len().min(MAX_UPSTREAMS);
        let mut order = self.order;
        if self.sharing > 1 {
            let mut scores = [0f64; MAX_UPSTREAMS];
            for (score, (upstream, seed)) in scores
                .iter_mut()
                .zip(self.upstreams.iter().zip(&self.seeds))
                .filter(|(_, (upstream, _))| !upstream.standby)
            {
                *score = f64::from(upstream.weight) / -unit(xxh3_64_with_seed(key, *seed)).ln();
            }
            let used = order.get_mut(..self.sharing).unwrap_or_default();
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
    /// session id, else the gateway token's subject, else empty).
    ///
    /// Each upstream that is not standby scores `weight / -ln(u)`, with `u`
    /// uniform in (0, 1) from a hash of the key seeded by the upstream's id;
    /// the highest score wins. A key then lands on an upstream with
    /// probability proportional to its weight. Floating point only ranks; an exact tie, which needs
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
