//! The router, the listener and graceful shutdown.

use std::future::Future;
use std::io;
use std::net::IpAddr;

use axum::Router;
use axum::routing::get;
use axum::serve::ListenerExt;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use super::ops_handlers;

/// Every route the service serves.
pub fn router() -> Router {
    Router::new().route("/healthz", get(ops_handlers::healthz))
}

/// Binds `host:port`.
pub async fn bind(host: IpAddr, port: u16) -> io::Result<TcpListener> {
    TcpListener::bind((host, port)).await
}

/// Serves `app` on `listener` until `shutdown` completes, then waits for
/// open connections to finish. Every accepted socket gets `TCP_NODELAY`,
/// so a small write after the headers never waits for a delayed ACK.
pub async fn serve<F>(listener: TcpListener, app: Router, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = listener.tap_io(|tcp| {
        if let Err(err) = tcp.set_nodelay(true) {
            tracing::warn!(error = %err, "failed to set TCP_NODELAY");
        }
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
}

/// Installs the SIGTERM and SIGINT handlers now and returns a future that
/// completes on the first of them. As PID 1, a signal without a handler is
/// ignored, so this must run before the server starts.
pub fn shutdown_signal() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        let name = tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        };
        tracing::info!(signal = name, "shutdown requested");
    })
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::router;

    #[tokio::test]
    async fn healthz_answers_ok() {
        let response = router()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], br#"{"status":"ok"}"#);
    }
}
