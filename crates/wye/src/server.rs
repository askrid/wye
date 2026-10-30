//! Listener, connection lifecycle, and graceful shutdown.

use std::{convert::Infallible, future::Future, net::SocketAddr, sync::Arc, time::Duration};

use hyper::{server::conn::http1, service::service_fn};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::graceful::GracefulShutdown,
};
use tokio::{
    io::AsyncWrite,
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

use crate::{Gateway, auth::ConnectionAuth, usage_log::UsageLog};

mod write_timeout;

use write_timeout::WriteTimeout;

/// Upper bound on a client connection's read buffer (hyper's default is ~400 KiB).
///
/// hyper grows the buffer while a large request body arrives and keeps it for
/// the connection's lifetime. An agent resends its whole context (hundreds of
/// KB) and then holds the stream open for minutes, so the grown buffer
/// dominated per-stream memory: 150 KB of live heap per idle stream after a
/// 512 KB request on the TLS path, 49 KB with this cap, at no measurable
/// upload cost there. The cap also bounds request headers, so it stays well
/// above an 8 KiB gateway credential plus Claude Code's headers.
const MAX_CONNECTION_BUFFER: usize = 32 * 1024;

/// Kernel send buffer of a client connection, macOS's initial size.
///
/// Left to autotuning, the kernel grows the send buffer of a connection whose
/// client has stopped reading to as much as 4 MiB, kernel memory that Wye's own
/// bounds do not see. Fixed, 256 stalled clients hold 32 MB of send buffers
/// (64 MB on Linux, which doubles the value it is given, for bookkeeping).
/// It limits one response to 128 KiB per round trip: far above token pace, and
/// clients usually sit behind a load balancer less than a millisecond away.
const CLIENT_SEND_BUFFER: usize = 128 * 1024;

/// How long `serve` waits for the usage log to be written after connections close.
const LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
/// The pause after an accept error that is not one connection's (out of file
/// descriptors, say): the listener stays ready, so retrying at once spins a core.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

/// Serves connections until `shutdown` resolves, then shuts down in two phases:
///
/// 1. Draining, for the gateway's drain delay: `/readyz` fails, but new
///    connections and requests are still served, so load balancers stop
///    routing here before connections are refused.
/// 2. Grace: the listener closes and active requests get the shutdown grace
///    period to finish; the rest are cancelled.
///
/// Returns whether the usage log was written out within a second after
/// that. If not, `usage_out` is not draining (a log reader that stopped):
/// a write to it may block a thread for good, so the caller should not wait
/// on one.
pub async fn serve(
    listener: TcpListener,
    gateway: Arc<Gateway>,
    shutdown: impl Future<Output = ()>,
    usage_out: impl AsyncWrite + Send + Unpin + 'static,
) -> bool {
    let (log, writer) = UsageLog::start(usage_out, gateway.sinks());
    let graceful = GracefulShutdown::new();
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new());
    http.max_buf_size(MAX_CONNECTION_BUFFER);
    // Owns every connection task; aborted after the grace period.
    let mut connections = JoinSet::new();
    // Takes `connections` as an argument, not a capture, so the select loops can
    // also reap from it between calls.
    // Returns the accept error to pause after, if any.
    let accept = |connections: &mut JoinSet<_>,
                  accepted: std::io::Result<(TcpStream, SocketAddr)>| {
        // Accept errors (file descriptor exhaustion, say) are transient: they
        // must not stop the server.
        let (stream, _) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => return pause_after(&e),
        };
        let _ = stream.set_nodelay(true);
        // Best effort: without it the kernel autotunes, as before.
        let _ = socket2::SockRef::from(&stream).set_send_buffer_size(CLIENT_SEND_BUFFER);
        let io = TokioIo::new(WriteTimeout::new(stream, gateway.client_write_timeout));
        let gateway = Arc::clone(&gateway);
        let log = log.clone();
        // One per connection, dropped with it; see `auth`.
        let auth = Arc::new(ConnectionAuth::default());
        let service = service_fn(move |req| {
            let gateway = Arc::clone(&gateway);
            let log = log.clone();
            let auth = Arc::clone(&auth);
            async move { Ok::<_, Infallible>(gateway.handle(req, &log, &auth).await) }
        });
        // Watched by `graceful` so shutdown can ask it to finish its current request.
        connections.spawn(graceful.watch(http.serve_connection(io, service)));
        false
    };

    // Cancel safety: `accept` and `join_next` lose nothing when another
    // branch wins; a pause delays the other branches by at most
    // ACCEPT_ERROR_PAUSE.
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => if accept(&mut connections, accepted) {
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
            },
            // Reap finished connections so the set stays as large as the live ones.
            Some(_) = connections.join_next() => {}
            () = &mut shutdown => break,
        }
    }

    gateway.start_draining();
    let drain = tokio::time::sleep(gateway.drain_delay);
    tokio::pin!(drain);
    loop {
        tokio::select! {
            accepted = listener.accept() => if accept(&mut connections, accepted) {
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
            },
            Some(_) = connections.join_next() => {}
            () = &mut drain => break,
        }
    }

    drop(listener);
    let _ = tokio::time::timeout(gateway.shutdown_grace, graceful.shutdown()).await;
    // Past the grace period: cancel what is left. Dropping a connection drops
    // its response bodies, which closes their upstream requests.
    connections.shutdown().await;
    // The writer ends once the last handle is gone: here and in drained connections.
    drop(log);
    tokio::time::timeout(LOG_FLUSH_TIMEOUT, writer)
        .await
        .is_ok()
}

/// Whether an accept error calls for a pause: not when one connection failed
/// before it was accepted (reset, aborted), which the next accept moves past.
fn pause_after(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionReset, Interrupted, WouldBlock};
    !matches!(
        error.kind(),
        ConnectionAborted | ConnectionReset | Interrupted | WouldBlock
    )
}

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind};

    use super::pause_after;

    #[test]
    fn pauses_on_resource_errors_only() {
        // EMFILE: the process is out of file descriptors.
        assert!(pause_after(&Error::from_raw_os_error(24)));
        assert!(!pause_after(&Error::from(ErrorKind::ConnectionAborted)));
        assert!(!pause_after(&Error::from(ErrorKind::ConnectionReset)));
    }
}
