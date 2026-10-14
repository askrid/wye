//! In-flight counters released by RAII.
//!
//! `Relaxed` ordering throughout: a gauge only counts; it never publishes other
//! memory, and each change is a read-modify-write, which is totally ordered per
//! atomic, so concurrent holders never lose an update.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Sum of the units of live [`Held`] guards.
#[derive(Clone, Default)]
pub(crate) struct Gauge(Arc<AtomicUsize>);

/// Counts units on a [`Gauge`] until dropped, however the work ends.
pub(crate) struct Held {
    gauge: Arc<AtomicUsize>,
    units: usize,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.gauge.fetch_sub(self.units, Ordering::Relaxed);
    }
}

impl Held {
    /// Adds `units` unless the gauge would exceed `max`; the add itself
    /// decides between concurrent callers.
    pub(crate) fn try_grow(&mut self, units: usize, max: usize) -> bool {
        let before = self.gauge.fetch_add(units, Ordering::Relaxed);
        if before.saturating_add(units) <= max {
            self.units += units;
            true
        } else {
            self.gauge.fetch_sub(units, Ordering::Relaxed);
            false
        }
    }

    /// Releases up to `units` before the guard is dropped.
    pub(crate) fn release(&mut self, units: usize) {
        let units = units.min(self.units);
        self.units -= units;
        self.gauge.fetch_sub(units, Ordering::Relaxed);
    }
}

impl Gauge {
    pub(crate) fn current(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn hold(&self) -> Held {
        self.0.fetch_add(1, Ordering::Relaxed);
        self.held(1)
    }

    /// A guard holding nothing yet, to grow with [`Held::try_grow`].
    pub(crate) fn hold_none(&self) -> Held {
        self.held(0)
    }

    fn held(&self, units: usize) -> Held {
        Held {
            gauge: Arc::clone(&self.0),
            units,
        }
    }

    /// Holds one unit unless `max` are already held. Concurrent callers cannot
    /// both take the last unit: the increment itself decides.
    pub(crate) fn try_hold(&self, max: usize) -> Option<Held> {
        if self.0.fetch_add(1, Ordering::Relaxed) < max {
            Some(self.held(1))
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

    #[test]
    fn grown_units_stop_at_max_and_release_in_parts() {
        let gauge = Gauge::default();
        let mut bytes = gauge.hold_none();
        assert!(bytes.try_grow(60, 100));
        assert!(!bytes.try_grow(41, 100));
        assert_eq!(gauge.current(), 60);
        bytes.release(20);
        assert_eq!(gauge.current(), 40);
        drop(bytes);
        assert_eq!(gauge.current(), 0);
    }
}
