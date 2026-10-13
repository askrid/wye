//! Listener, connection lifecycle, and graceful shutdown.

use std::{convert::Infallible, future::Future, sync::Arc, time::Duration};

use hyper::{server::conn::http1, service::service_fn};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::graceful::GracefulShutdown,
};
use tokio::{io::AsyncWrite, net::TcpListener};

use crate::{Gateway, usage_log::UsageLog};

/// How long `serve` waits for the usage log to be written after connections close.
const LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// Serves connections until `shutdown` resolves, then drains them for at most `grace`.
pub async fn serve(
    listener: TcpListener,
    gateway: Arc<Gateway>,
    shutdown: impl Future<Output = ()>,
    grace: Duration,
    usage_out: impl AsyncWrite + Send + Unpin + 'static,
) {
    let (log, writer) = UsageLog::start(usage_out);
    let graceful = GracefulShutdown::new();
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new());
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                // Transient accept errors (for example, file descriptor exhaustion) must not stop the server.
                let Ok((stream, _)) = accepted else { continue };
                let _ = stream.set_nodelay(true);
                let gateway = Arc::clone(&gateway);
                let log = log.clone();
                let service = service_fn(move |req| {
                    let gateway = Arc::clone(&gateway);
                    let log = log.clone();
                    async move { Ok::<_, Infallible>(gateway.handle(req, &log).await) }
                });
                let conn = graceful.watch(http.serve_connection(TokioIo::new(stream), service));
                // Owned by `graceful`: drained on shutdown, cancelled with the runtime after the deadline.
                tokio::spawn(conn);
            }
            () = &mut shutdown => break,
        }
    }
    drop(listener);
    let _ = tokio::time::timeout(grace, graceful.shutdown()).await;
    // The writer ends once the last handle is gone: here and in drained connections.
    drop(log);
    let _ = tokio::time::timeout(LOG_FLUSH_TIMEOUT, writer).await;
}
