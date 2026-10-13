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
}
