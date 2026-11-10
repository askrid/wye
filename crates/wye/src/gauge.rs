//! In-flight counters released by RAII.
//!
//! `Relaxed` ordering throughout: a gauge only counts; it never publishes other
//! memory, and each change is a read-modify-write, which is totally ordered per
//! atomic, so concurrent holders never lose an update.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Sum of the units of live [`Reserved`] guards.
#[derive(Clone, Default)]
pub(crate) struct Gauge(Arc<AtomicUsize>);

/// Counts units on a [`Gauge`] until dropped, however the work ends.
pub(crate) struct Reserved {
    gauge: Arc<AtomicUsize>,
    units: usize,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        self.gauge.fetch_sub(self.units, Ordering::Relaxed);
    }
}

impl Reserved {
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

    /// The units this guard holds.
    pub(crate) fn units(&self) -> usize {
        self.units
    }

    /// Releases up to `units` before the guard is dropped.
    pub(crate) fn release(&mut self, units: usize) {
        let units = units.min(self.units);
        self.units -= units;
        self.gauge.fetch_sub(units, Ordering::Relaxed);
    }
}

impl Gauge {
    /// The units held now.
    pub(crate) fn current(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// A guard of one unit, whatever the gauge holds.
    pub(crate) fn reserve(&self) -> Reserved {
        self.0.fetch_add(1, Ordering::Relaxed);
        self.reserved(1)
    }

    /// A guard of no units yet, to grow with [`Reserved::try_grow`].
    pub(crate) fn reserve_none(&self) -> Reserved {
        self.reserved(0)
    }

    fn reserved(&self, units: usize) -> Reserved {
        Reserved {
            gauge: Arc::clone(&self.0),
            units,
        }
    }

    /// Reserves one unit unless `max` are already reserved. Concurrent
    /// callers cannot both take the last unit: the increment itself decides.
    pub(crate) fn try_reserve(&self, max: usize) -> Option<Reserved> {
        if self.0.fetch_add(1, Ordering::Relaxed) < max {
            Some(self.reserved(1))
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
        let a = gauge.reserve();
        let b = gauge.reserve();
        assert_eq!(gauge.current(), 2);
        drop(a);
        drop(b);
        assert_eq!(gauge.current(), 0);
    }

    #[test]
    fn try_reserve_stops_at_max_and_reopens_after_release() {
        let gauge = Gauge::default();
        let first = gauge.try_reserve(1).unwrap();
        assert!(gauge.try_reserve(1).is_none());
        assert_eq!(gauge.current(), 1);
        drop(first);
        assert!(gauge.try_reserve(1).is_some());
    }

    #[test]
    fn grown_units_stop_at_max_and_release_in_parts() {
        let gauge = Gauge::default();
        let mut bytes = gauge.reserve_none();
        assert!(bytes.try_grow(60, 100));
        assert!(!bytes.try_grow(41, 100));
        assert_eq!(gauge.current(), 60);
        bytes.release(20);
        assert_eq!(gauge.current(), 40);
        drop(bytes);
        assert_eq!(gauge.current(), 0);
    }
}
