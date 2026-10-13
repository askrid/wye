//! In-flight counters released by RAII.
//!
//! `Relaxed` ordering throughout: a gauge only counts; it never publishes other
//! memory, and each change is a read-modify-write, which is totally ordered per
//! atomic, so concurrent holders never lose an update.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Number of live [`Held`] guards.
#[derive(Clone, Default)]
pub(crate) struct Gauge(Arc<AtomicUsize>);

/// Counts one unit on a [`Gauge`] until dropped, however the work ends.
pub(crate) struct Held(Arc<AtomicUsize>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Gauge {
    pub(crate) fn current(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn hold(&self) -> Held {
        self.0.fetch_add(1, Ordering::Relaxed);
        Held(Arc::clone(&self.0))
    }

    /// Holds one unit unless `max` are already held. Concurrent callers cannot
    /// both take the last unit: the increment itself decides.
    pub(crate) fn try_hold(&self, max: usize) -> Option<Held> {
        if self.0.fetch_add(1, Ordering::Relaxed) < max {
            Some(Held(Arc::clone(&self.0)))
        } else {
            self.0.fetch_sub(1, Ordering::Relaxed);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_units_are_released_on_drop() {
        let gauge = Gauge::default();
        let a = gauge.hold();
        let b = gauge.hold();
        assert_eq!(gauge.current(), 2);
        drop(a);
        drop(b);
        assert_eq!(gauge.current(), 0);
    }

    #[test]
    fn try_hold_stops_at_max_and_reopens_after_release() {
        let gauge = Gauge::default();
        let first = gauge.try_hold(1).unwrap();
        assert!(gauge.try_hold(1).is_none());
        assert_eq!(gauge.current(), 1);
        drop(first);
        assert!(gauge.try_hold(1).is_some());
    }
}
