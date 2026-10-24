//! Closes a client connection whose writes stop making progress.
//!
//! A client that stops reading (a laptop lid closed mid-stream, a stuck
//! consumer) leaves its response unpolled, so no body-level timer runs; its
//! stream and upstream buffers would be held until TCP gives up, or forever
//! if the peer keeps acknowledging with a zero window. Downstream connections
//! are HTTP/1.1, one response at a time, so a blocked socket write is exactly
//! a stalled response.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::{Instant, Sleep},
};

/// `io` whose writes fail with `TimedOut` once none has made progress for
/// `timeout`. The timer is allocated at the first blocked write, so
/// connections whose writes never block pay nothing.
pub(super) struct WriteTimeout<T> {
    io: T,
    timeout: Duration,
    /// Armed while a write is blocked; `None` until the first one.
    stalled: Option<Pin<Box<Sleep>>>,
    armed: bool,
}

impl<T> WriteTimeout<T> {
    pub(super) fn new(io: T, timeout: Duration) -> Self {
        Self {
            io,
            timeout,
            stalled: None,
            armed: false,
        }
    }

    /// Tracks the outcome of a write: progress disarms the timer; a blocked
    /// write arms it (once per stall) and fails when it fires. Polling the
    /// timer registers the waker, so the connection task is woken to see it.
    fn observe<R>(
        &mut self,
        cx: &mut Context<'_>,
        poll: Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        if poll.is_ready() {
            self.armed = false;
            return poll;
        }
        let deadline = Instant::now() + self.timeout;
        let sleep = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
        if !self.armed {
            sleep.as_mut().reset(deadline);
            self.armed = true;
        }
        ready!(sleep.as_mut().poll(cx));
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "client accepted no response bytes within client_write_timeout_secs",
        )))
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for WriteTimeout<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for WriteTimeout<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.io).poll_write(cx, buf);
        self.observe(cx, poll)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.io).poll_write_vectored(cx, bufs);
        self.observe(cx, poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.io).poll_flush(cx);
        self.observe(cx, poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_write_blocked_past_the_timeout_fails() {
        let (client, mut server) = duplex(64);
        let mut io = WriteTimeout::new(client, Duration::from_secs(60));
        io.write_all(&[0; 64]).await.unwrap();
        // The pipe is full and nobody reads: the next write blocks, then fails.
        let err = io.write_all(&[0; 1]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let mut buf = [0; 64];
        server.read_exact(&mut buf).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_reader_that_keeps_reading_is_not_cut_off() {
        let (client, mut server) = duplex(64);
        let mut io = WriteTimeout::new(client, Duration::from_secs(60));
        let reader = tokio::spawn(async move {
            let mut buf = [0; 64];
            for _ in 0..10 {
                // Each read comes 50 s after the last: never 60 s without progress.
                tokio::time::sleep(Duration::from_secs(50)).await;
                server.read_exact(&mut buf).await.unwrap();
            }
        });
        for _ in 0..10 {
            io.write_all(&[0; 64]).await.unwrap();
        }
        reader.await.unwrap();
    }
}
