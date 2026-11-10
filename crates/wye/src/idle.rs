//! A timer that fails a wait once it has lasted a timeout, for the waits
//! that have no deadline of their own: an upstream body that sends nothing,
//! a client that accepts no response bytes.
//!
//! Only time spent waiting counts: progress restarts the next wait from
//! zero. The timer is allocated at the first wait, so a body or connection
//! that never waits pays nothing, and the clock is read once per wait.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use tokio::time::{Instant, Sleep, sleep_until};

/// The timer of one body's or connection's waits.
pub(crate) struct IdleTimer {
    timeout: Duration,
    /// `None` until the first wait.
    sleep: Option<Pin<Box<Sleep>>>,
    /// Whether `sleep` is set for the current wait.
    armed: bool,
}

impl IdleTimer {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            sleep: None,
            armed: false,
        }
    }

    /// Progress: the next wait counts from zero.
    pub(crate) fn progressed(&mut self) {
        self.armed = false;
    }

    /// Waiting: `Ready` once this wait has lasted the timeout. Registers the
    /// waker, so the task is woken to see it fire.
    pub(crate) fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if !self.armed {
            self.armed = true;
            let deadline = Instant::now() + self.timeout;
            match &mut self.sleep {
                Some(sleep) => sleep.as_mut().reset(deadline),
                None => self.sleep = Some(Box::pin(sleep_until(deadline))),
            }
        }
        // Set above if it was not.
        self.sleep
            .as_mut()
            .map_or(Poll::Pending, |sleep| sleep.as_mut().poll(cx))
    }
}
