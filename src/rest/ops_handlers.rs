//! Probe and operations handlers: `/healthz` (later `/readyz`, `/metrics`,
//! `/swagger/doc.json`).

use axum::http::header;
use axum::response::IntoResponse;

/// The `Health` body of `getLiveness`.
const HEALTHY: &str = r#"{"status":"ok"}"#;

/// `GET /healthz`: 200 while the process runs. Checks nothing else.
pub async fn healthz() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], HEALTHY)
}
