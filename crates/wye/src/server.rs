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

/// Upper bound on a client connection's read buffer (hyper's default is ~400 KiB).
///
/// hyper grows the buffer while a large request body arrives and keeps it for
/// the connection's lifetime. An agent resends its whole context (hundreds of
/// KB) and then holds the stream open for minutes, so the grown buffer
/// dominated per-stream memory: 150 KB of live heap per idle stream after a
/// 512 KB request on the TLS path, 48 KB with this cap, at no measurable
/// upload cost there. The cap also bounds request headers, so it stays well
/// above an 8 KiB gateway credential plus Claude Code's headers.
const MAX_CONNECTION_BUFFER: usize = 32 * 1024;

/// How long `serve` waits for the usage log to be written after connections close.
const LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// Serves connections until `shutdown` resolves, then shuts down in two phases:
///
/// 1. Draining, for the gateway's drain delay: `/readyz` fails, but new
///    connections and requests are still served, so load balancers stop
///    routing here before connections are refused.
/// 2. Grace: the listener closes and active requests get the shutdown grace
///    period to finish; the rest are cancelled.
pub async fn serve(
    listener: TcpListener,
    gateway: Arc<Gateway>,
    shutdown: impl Future<Output = ()>,
    usage_out: impl AsyncWrite + Send + Unpin + 'static,
) {
    let (log, writer) = UsageLog::start(usage_out, gateway.prompt_key(), gateway.otlp());
    let graceful = GracefulShutdown::new();
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new());
    http.max_buf_size(MAX_CONNECTION_BUFFER);
    // Owns every connection task; aborted after the grace period.
    let mut connections = JoinSet::new();
    // Takes `connections` as an argument, not a capture, so the select loops can
    // also reap from it between calls.
    let accept = |connections: &mut JoinSet<_>,
                  accepted: std::io::Result<(TcpStream, SocketAddr)>| {
        // Transient accept errors (for example, file descriptor exhaustion) must not stop the server.
        let Ok((stream, _)) = accepted else { return };
        let _ = stream.set_nodelay(true);
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
        connections.spawn(graceful.watch(http.serve_connection(TokioIo::new(stream), service)));
    };

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => accept(&mut connections, accepted),
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
            accepted = listener.accept() => accept(&mut connections, accepted),
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
    let _ = tokio::time::timeout(LOG_FLUSH_TIMEOUT, writer).await;
}
