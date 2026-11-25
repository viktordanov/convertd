//! Probe and operations handlers: `/healthz`, `/readyz`, `/metrics` and
//! `/swagger/doc.json`.

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::serialization::probes::{Health, Readiness};
use super::state::AppState;
use crate::domain::readiness::ReadinessStatus;

/// The `Health` body of `getLiveness`.
const HEALTHY: &str = r#"{"status":"ok"}"#;

/// Liveness probe
///
/// Returns 200 while the supervisor process is running. Checks nothing else.
#[utoipa::path(
    get,
    path = "/healthz",
    operation_id = "getLiveness",
    tag = "operations",
    security(()),
    responses((status = 200, description = "The process is alive.", body = Health)),
)]
pub async fn healthz() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], HEALTHY)
}

/// Readiness probe
///
/// Returns 200 when every worker pool has at least one warm worker and
/// the service is not shutting down. Returns 503 while starting, while
/// draining, or when a pool has no healthy worker.
#[utoipa::path(
    get,
    path = "/readyz",
    operation_id = "getReadiness",
    tag = "operations",
    security(()),
    responses(
        (status = 200, description = "Ready to take conversions.", body = Readiness),
        (status = 503, description = "Not ready. Do not route traffic here.", body = Readiness),
    ),
)]
pub async fn readyz(State(state): State<AppState>) -> Response {
    let snapshot = state.readiness.readiness();
    let status = if snapshot.status == ReadinessStatus::Ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(Readiness::from(snapshot))).into_response()
}

/// Prometheus metrics
///
/// Metrics in the Prometheus text exposition format.
#[utoipa::path(
    get,
    path = "/metrics",
    operation_id = "getMetrics",
    tag = "operations",
    security(()),
    responses((status = 200, description = "Current metric values.", body = String, content_type = "text/plain")),
)]
pub async fn metrics() -> impl IntoResponse {
    // The metric set arrives in plan step 9.
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], "")
}

/// `GET /swagger/doc.json`: the same bytes as `convertd openapi`. Not in
/// the spec, like the reference.
pub async fn swagger_doc(State(state): State<AppState>) -> Response {
    if !state.swagger_enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        [(header::CONTENT_TYPE, "application/json")],
        state.spec_json.clone(),
    )
        .into_response()
}
