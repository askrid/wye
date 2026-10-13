//! Spreads upstream requests over several connections.
//!
//! hyper-util's pool shares one HTTP/2 connection per host and never opens a
//! second: once the peer's concurrent-stream limit is reached, further requests
//! wait inside that connection. Each lane here is a separate `Client` with its
//! own pool, so each holds its own HTTP/2 connection. A request takes a slot on
//! the first lane below `max_streams`; the slot is held until the response body
//! ends. Lanes are created at startup and connect lazily, so unused lanes cost
//! no sockets.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::UpstreamClient;

/// Upper bound on upstream connections per process; beyond
/// `LANES × max_streams` concurrent requests, the least loaded lane queues them.
const LANES: usize = 64;

pub(crate) struct ClientPool {
    lanes: Box<[Lane]>,
    max_streams: usize,
}

struct Lane {
    client: UpstreamClient,
    in_flight: Arc<AtomicUsize>,
}

/// A reserved stream on one lane; released on drop.
pub(crate) struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ClientPool {
    pub(crate) fn new(client: impl Fn() -> UpstreamClient, max_streams: u32) -> Self {
        Self {
            lanes: (0..LANES)
                .map(|_| Lane {
                    client: client(),
                    in_flight: Arc::default(),
                })
                .collect(),
            max_streams: max_streams as usize,
        }
    }

    /// Picks a lane with free capacity, filling earlier lanes first so as few
    /// connections as possible stay open.
    ///
    /// `Relaxed` is enough: each counter is only read-modify-written, and those
    /// operations on one atomic are totally ordered, so no two requests can
    /// take the same last slot. No other memory is published through it.
    pub(crate) fn acquire(&self) -> (&UpstreamClient, Slot) {
        for lane in &self.lanes {
            if lane.in_flight.fetch_add(1, Ordering::Relaxed) < self.max_streams {
                return (&lane.client, Slot(Arc::clone(&lane.in_flight)));
            }
            lane.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
        // Every lane is full: queue on the least loaded connection.
        let lane = self
            .lanes
            .iter()
            .min_by_key(|lane| lane.in_flight.load(Ordering::Relaxed))
            .unwrap_or_else(|| unreachable!("LANES > 0"));
        lane.in_flight.fetch_add(1, Ordering::Relaxed);
        (&lane.client, Slot(Arc::clone(&lane.in_flight)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(max_streams: u32) -> ClientPool {
        ClientPool::new(
            || {
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
                hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                    .build(https)
            },
            max_streams,
        )
    }

    fn loads(pool: &ClientPool) -> Vec<usize> {
        pool.lanes
            .iter()
            .map(|lane| lane.in_flight.load(Ordering::Relaxed))
            .take(3)
            .collect()
    }

    #[test]
    fn fills_lanes_in_order_and_releases_on_drop() {
        let pool = pool(2);
        let slots: Vec<Slot> = (0..5).map(|_| pool.acquire().1).collect();
        assert_eq!(loads(&pool), [2, 2, 1]);
        drop(slots);
        assert_eq!(loads(&pool), [0, 0, 0]);
    }

    #[test]
    fn freed_slot_on_an_earlier_lane_is_reused_first() {
        let pool = pool(1);
        let first = pool.acquire().1;
        let _second = pool.acquire().1;
        drop(first);
        let _third = pool.acquire().1;
        assert_eq!(loads(&pool), [1, 1, 0]);
    }

    #[test]
    fn all_lanes_full_queues_on_least_loaded() {
        let pool = pool(1);
        let _all: Vec<Slot> = (0..LANES + 1).map(|_| pool.acquire().1).collect();
        assert_eq!(loads(&pool), [2, 1, 1]);
    }
}
