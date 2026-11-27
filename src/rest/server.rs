//! The router, the listener and graceful shutdown.

use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use std::any::Any;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state, map_request};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, watch};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::timeout::RequestBodyTimeoutLayer;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::middleware::{access_log, auth, linger, request_id};
use super::response::ApiError;
use super::state::AppState;
use super::{capabilities_handlers, convert_handlers, openapi, ops_handlers};

/// The `/v1` routes, behind the API key.
fn v1_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(convert_handlers::convert))
        .routes(routes!(capabilities_handlers::capabilities))
}

/// The probes and metrics, without a key.
fn ops_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(ops_handlers::healthz))
        .routes(routes!(ops_handlers::readyz))
        .routes(routes!(ops_handlers::metrics))
}

/// Every route of the spec, with its documentation. Built from the same
/// route sets as the server, so `convertd openapi` cannot drift from it.
pub fn api_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(openapi::base())
        .merge(v1_routes())
        .merge(ops_routes())
}

/// Every route the service serves.
pub fn router(state: AppState) -> Router {
    router_with(state, Router::new())
}

/// [`router`] plus `extra` routes (without auth), under the same layers.
/// For tests that need a route of their own, such as one that panics.
#[doc(hidden)]
pub fn router_with(state: AppState, extra: Router<AppState>) -> Router {
    let (v1, _) = v1_routes().split_for_parts();
    let v1 = v1
        .route_layer(from_fn_with_state(state.api_keys.clone(), auth::auth))
        .layer(RequestBodyTimeoutLayer::new(state.io_idle_timeout));
    let (ops, _) = ops_routes().split_for_parts();
    Router::new()
        .merge(v1)
        .merge(ops)
        .route("/swagger/doc.json", get(ops_handlers::swagger_doc))
        .merge(extra)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(from_fn(connection_gate))
        // Outermost last: linger, request_id, access_log, then the panic
        // layer. linger wraps the body before anything can drop it early.
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(from_fn(access_log::access_log))
        .layer(from_fn(request_id::request_id))
        .layer(map_request(linger::linger))
        .with_state(state)
}

async fn not_found() -> Response {
    ApiError::not_found().into_response()
}

async fn method_not_allowed() -> Response {
    ApiError::method_not_allowed().into_response()
}

fn panic_response(panic: Box<dyn Any + Send + 'static>) -> Response {
    let message = panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    ApiError::internal(format!("handler panicked: {message}")).into_response()
}

/// The first step of a graceful stop: readyz answers `draining` and new
/// conversions answer 503, while requests in flight go on.
pub fn begin_drain(state: &AppState) {
    state
        .draining
        .store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Waits until no conversion holds a permit (the response pumps included),
/// or `grace` has passed. Returns whether the service went idle.
pub async fn wait_idle(state: &AppState, grace: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + grace;
    while state.inflight.available_permits() < state.max_inflight {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    true
}

/// Binds `host:port`.
pub async fn bind(host: IpAddr, port: u16) -> io::Result<TcpListener> {
    TcpListener::bind((host, port)).await
}

/// The bounds on connections before any request is read
/// (`HEADER_READ_TIMEOUT_SECONDS`, `MAX_CONNECTIONS`).
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimits {
    /// The longest a connection may take to send a request's headers,
    /// from when the server is ready to read them: on a new connection and
    /// on an idle keep-alive one alike.
    pub header_read_timeout: Duration,
    /// The most open connections that serve every route.
    pub max_connections: usize,
}

/// Connections past [`ConnectionLimits::max_connections`] that still get an
/// answer, so the probes work while the cap is full: each serves one
/// request without keep-alive, under [`PROBE_HEADER_READ_TIMEOUT`], and only
/// `/healthz`, `/readyz` and `/metrics`; any other path gets 503. Past
/// these, a connection is closed as it is accepted.
pub const PROBE_CONNECTIONS: usize = 16;
/// The header timeout of a probe connection.
pub const PROBE_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// The pause after an accept error such as `EMFILE`, so the loop does not
/// spin while descriptors are short.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

/// Serves `app` on `listener` until `shutdown` completes, then stops
/// accepting, asks each open connection to close after its current
/// request, and waits for them. Every accepted socket gets `TCP_NODELAY`,
/// so a small write after the headers never waits for a delayed ACK.
///
/// hyper serves each connection with a timer, so the header read timeout
/// holds; `axum::serve` builds connections without one. At most
/// `max_connections` connections serve every route, and
/// [`PROBE_CONNECTIONS`] more serve only the probes.
pub async fn serve<F>(
    listener: TcpListener,
    app: Router,
    limits: ConnectionLimits,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let open = Arc::new(Semaphore::new(limits.max_connections));
    let probes = Arc::new(Semaphore::new(PROBE_CONNECTIONS));
    // The mark is set outermost; the gate that reads it runs inside the
    // common layers (`router_with`), so a refusal still gets a request id,
    // an access log line and the lingering close.
    let probe_app = app.clone().layer(map_request(mark_probe_only));

    // Dropping the receiver tells every connection task to close.
    let (signal_tx, signal_rx) = watch::channel(());
    tokio::spawn(async move {
        shutdown.await;
        drop(signal_rx);
    });
    // Each connection task holds a receiver; all gone means all closed.
    let (close_tx, close_rx) = watch::channel(());

    loop {
        let (tcp, _) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(err) => {
                    tracing::warn!(error = %err, "failed to accept a connection");
                    tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                    continue;
                }
            },
            () = signal_tx.closed() => break,
        };
        let (permit, probe_only) = match Arc::clone(&open).try_acquire_owned() {
            Ok(permit) => (permit, false),
            Err(_) => match Arc::clone(&probes).try_acquire_owned() {
                Ok(permit) => (permit, true),
                Err(_) => {
                    // Closed unread: the probes' reserve is full too.
                    tracing::debug!("connection limit reached; closing a new connection");
                    drop(tcp);
                    continue;
                }
            },
        };
        if let Err(err) = tcp.set_nodelay(true) {
            tracing::warn!(error = %err, "failed to set TCP_NODELAY");
        }
        let service = if probe_only {
            probe_app.clone()
        } else {
            app.clone()
        };
        let header_read_timeout = if probe_only {
            limits.header_read_timeout.min(PROBE_HEADER_READ_TIMEOUT)
        } else {
            limits.header_read_timeout
        };
        let signal_tx = signal_tx.clone();
        let close_rx = close_rx.clone();
        tokio::spawn(async move {
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(header_read_timeout)
                .keep_alive(!probe_only);
            let conn =
                builder.serve_connection(TokioIo::new(tcp), TowerToHyperService::new(service));
            let mut conn = pin!(conn);
            let mut signal_closed = pin!(signal_tx.closed());
            let mut draining = false;
            loop {
                tokio::select! {
                    result = conn.as_mut() => {
                        if let Err(err) = result {
                            tracing::debug!(error = %err, "connection ended with an error");
                        }
                        break;
                    }
                    () = &mut signal_closed, if !draining => {
                        draining = true;
                        conn.as_mut().graceful_shutdown();
                    }
                }
            }
            drop(permit);
            drop(close_rx);
        });
    }
    drop(close_rx);
    drop(listener);
    close_tx.closed().await;
    Ok(())
}

/// Marks a request that came on a probe connection.
#[derive(Debug, Clone, Copy)]
struct ProbeOnly;

async fn mark_probe_only(mut request: axum::extract::Request) -> axum::extract::Request {
    request.extensions_mut().insert(ProbeOnly);
    request
}

/// On a probe connection: the probes and metrics pass, anything else is
/// 503, because the connection cap is full. Inside the common layers, so
/// the refusal is logged, has a request id, and an upload in progress is
/// drained before the connection closes.
async fn connection_gate(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if request.extensions().get::<ProbeOnly>().is_none() {
        return next.run(request).await;
    }
    match request.uri().path() {
        "/healthz" | "/readyz" | "/metrics" => next.run(request).await,
        _ => ApiError::unavailable("too many open connections; retry later")
            .with_detail("connections")
            .into_response(),
    }
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

    use std::sync::Arc;

    use super::router;
    use crate::config::{Config, Env};
    use crate::engines::fake::Unwired;
    use crate::rest::state::{AppState, Engines};

    fn state() -> AppState {
        let config = Config::from_env(&Env::from_pairs([("API_KEY", "k")], 2)).unwrap();
        let unwired = Arc::new(Unwired);
        let engines = Engines {
            office: unwired.clone(),
            pdf: unwired.clone(),
            readiness: unwired,
        };
        AppState::new(&config, engines)
    }

    #[tokio::test]
    async fn healthz_answers_ok() {
        let response = router(state())
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
