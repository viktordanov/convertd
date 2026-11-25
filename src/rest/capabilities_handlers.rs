//! `GET /v1/capabilities`.

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

use super::openapi::{InternalError, RequestIdParam, Unauthorized};
use super::serialization::capabilities::Capabilities;
use super::state::AppState;

/// List supported formats, limits and engine versions
///
/// Describes what this deployment can do: which input formats it accepts,
/// which outputs it produces, the limits it enforces, the engine versions
/// it runs and the fonts it has installed.
///
/// The answer is fixed for the lifetime of a deployment, so clients can
/// read it once at start-up.
#[utoipa::path(
    get,
    path = "/v1/capabilities",
    operation_id = "getCapabilities",
    tag = "discovery",
    params(RequestIdParam),
    responses(
        (status = 200, description = "The capabilities of this deployment.",
            headers(("X-Request-ID" = String, description = "The correlation ID of this request.")),
            body = Capabilities),
        (status = 401, response = Unauthorized),
        (status = 500, response = InternalError),
    ),
)]
pub async fn capabilities(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        state.capabilities_json.clone(),
    )
}
