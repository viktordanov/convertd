//! The router, the listener and graceful shutdown.

use std::future::Future;
use std::io;
use std::net::IpAddr;

use std::any::Any;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state, map_request};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
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
