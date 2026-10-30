//! Spreads upstream requests over several connections.
//!
//! hyper-util's pool shares one HTTP/2 connection per host and never opens a
//! second: once the peer's concurrent-stream limit is reached, further requests
//! wait inside that connection. Each lane here is a separate `Client` with its
//! own pool, so each holds its own HTTP/2 connection. A request takes a slot on
//! the least loaded lane; the slot is held until the response body ends. Lanes
//! are created at startup and connect lazily, so unused lanes cost no sockets.
//!
//! Spreading rather than packing is deliberate: `h2` guards all streams of one
//! connection with a single mutex, and packing streams onto one connection made
//! that lock the dominant cost (A/B on the TLS path: 1.5 ms added unary latency
//! and 12.3 ms CPU per MB packed, against 0.3 ms and 4.1 ms spread).

use super::UpstreamClient;
use crate::gauge::{Gauge, Held};

/// Upper bound on upstream connections per process.
const LANES: usize = 64;

pub(crate) struct ClientPool {
    lanes: Box<[Lane]>,
}

struct Lane {
    client: UpstreamClient,
    in_flight: Gauge,
}

impl ClientPool {
    pub(crate) fn new(client: impl Fn() -> UpstreamClient) -> Self {
        Self {
            lanes: (0..LANES)
                .map(|_| Lane {
                    client: client(),
                    in_flight: Gauge::default(),
                })
                .collect(),
        }
    }

    /// Picks the least loaded lane; ties go to the earliest, so sequential
    /// traffic reuses one connection and `k` concurrent requests use up to `k`.
    /// Beyond `LANES` concurrent requests, load stays even; a provider's
    /// per-connection stream limit is reached only at `LANES` times that limit,
    /// after which requests queue inside the least loaded connection.
    ///
    /// Two requests may pick the same lane concurrently; that only shifts load.
    /// The returned guard holds the lane until the response body is dropped.
    pub(crate) fn acquire(&self) -> (&UpstreamClient, Held) {
        let lane = self
            .lanes
            .iter()
            .min_by_key(|lane| lane.in_flight.current())
            .unwrap_or_else(|| unreachable!("LANES > 0"));
        (&lane.client, lane.in_flight.hold())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn pool() -> ClientPool {
        ClientPool::new(|| {
            let http = hyper_util::client::legacy::connect::HttpConnector::new();
            let https = hyper_rustls::HttpsConnectorBuilder::new()
                .with_tls_config(
                    rustls::ClientConfig::builder_with_provider(Arc::new(
                        rustls::crypto::ring::default_provider(),
                    ))
                    .with_safe_default_protocol_versions()
                    .unwrap()
                    .with_root_certificates(rustls::RootCertStore::empty())
                    .with_no_client_auth(),
                )
                .https_or_http()
                .enable_http1()
                .wrap_connector(http);
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(
                super::super::ConnectTimeout::new(https, std::time::Duration::from_secs(1)),
            )
        })
    }

    fn loads(pool: &ClientPool) -> Vec<usize> {
        pool.lanes
            .iter()
            .map(|lane| lane.in_flight.current())
            .take(3)
            .collect()
    }

    #[test]
    fn spreads_concurrent_requests_and_releases_on_drop() {
        let pool = pool();
        let slots: Vec<Held> = (0..3).map(|_| pool.acquire().1).collect();
        assert_eq!(loads(&pool), [1, 1, 1]);
        drop(slots);
        assert_eq!(loads(&pool), [0, 0, 0]);
    }

    #[test]
    fn sequential_requests_reuse_the_first_lane() {
        let pool = pool();
        for _ in 0..3 {
            drop(pool.acquire().1);
        }
        let _slot = pool.acquire().1;
        assert_eq!(loads(&pool), [1, 0, 0]);
    }

    #[test]
    fn beyond_one_per_lane_load_stays_even() {
        let pool = pool();
        let _all: Vec<Held> = (0..LANES * 2 + 1).map(|_| pool.acquire().1).collect();
        assert_eq!(loads(&pool), [3, 2, 2]);
    }
}
