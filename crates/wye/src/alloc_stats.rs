//! Heap allocation counters for benchmark builds (Cargo feature `alloc-stats`).
//!
//! [`Counting`] wraps the system allocator and counts every allocation; the
//! binary installs it as the global allocator only when the feature is on, and
//! the gateway then serves the counters at `GET /debug/alloc`. Normal builds
//! contain none of this. Never ship an `alloc-stats` build: the endpoint is
//! unauthenticated and the counters cost an atomic operation per allocation.
//!
//! `Relaxed` ordering: the counters are statistics read between benchmark
//! phases; they publish no other memory.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicU64, Ordering},
};

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_LIVE_BYTES: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting allocations.
pub struct Counting;

fn record_alloc(size: usize) {
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    let live = LIVE_BYTES.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
    PEAK_LIVE_BYTES.fetch_max(live, Ordering::Relaxed);
}

fn record_free(size: usize) {
    LIVE_BYTES.fetch_sub(size as u64, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged and returns its result unchanged, so `Counting` upholds exactly the
// contract `System` does. The bookkeeping only touches atomics and never
// allocates, so it cannot recurse into the allocator.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` is passed through to `System`.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees `ptr` came from this allocator with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        record_free(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract for `realloc` is passed through to `System`.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            // A resize counts as one allocation of the new size replacing the old block.
            record_free(layout.size());
            record_alloc(new_size);
        }
        new
    }
}

/// Counter values since process start.
pub struct Snapshot {
    pub allocations: u64,
    pub allocated_bytes: u64,
    pub live_bytes: u64,
    pub peak_live_bytes: u64,
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
        live_bytes: LIVE_BYTES.load(Ordering::Relaxed),
        peak_live_bytes: PEAK_LIVE_BYTES.load(Ordering::Relaxed),
    }
}

impl Snapshot {
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"allocations":{},"allocated_bytes":{},"live_bytes":{},"peak_live_bytes":{}}}"#,
            self.allocations, self.allocated_bytes, self.live_bytes, self.peak_live_bytes
        )
    }
}
