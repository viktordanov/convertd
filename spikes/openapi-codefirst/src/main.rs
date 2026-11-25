//! Spike: can the axum server emit the OpenAPI spec that oapi-codegen consumes?
//! DTOs mirror api/openapi.yaml. No engines; handlers are stubs.
#![allow(dead_code)]

use std::collections::BTreeMap;

use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use utoipa::{
    Modify, OpenApi, ToSchema,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};
use utoipa_axum::{router::OpenApiRouter, routes};

// ---------- request ----------

/// The multipart body. Documentation only: the handler reads parts by hand.
#[derive(ToSchema)]
struct ConvertRequest {
    /// The document to convert. The type is detected from the bytes.
    #[schema(value_type = String, format = Binary, content_media_type = "application/octet-stream")]
    file: Vec<u8>,
    #[schema(nullable = false)]
    options: Option<ConvertOptions>,
}

/// What to produce and how. Every property is optional.
#[derive(Deserialize, ToSchema, Default)]
#[serde(deny_unknown_fields)]
struct ConvertOptions {
    #[serde(default)]
    output: OutputKind,
    /// Which pages to render as images, 1-based.
    #[schema(pattern = r"^[0-9]+(-[0-9]*)?(,[0-9]+(-[0-9]*)?)*$", max_length = 256, example = "1-5,8,10-")]
    #[schema(nullable = false)]
    #[serde(default)]
    pages: Option<String>,
    /// Password for an encrypted input. Never logged or stored.
    #[schema(write_only, max_length = 256)]
    #[schema(nullable = false)]
    #[serde(default)]
    password: Option<String>,
    /// Total time budget for this call, in seconds.
    #[schema(minimum = 5, maximum = 210, default = 120)]
    #[schema(nullable = false)]
    #[serde(default)]
    timeout_seconds: Option<u32>,
    #[schema(nullable = false)]
    #[serde(default)]
    images: Option<ImageOptions>,
}

/// What the response contains.
#[derive(Deserialize, Serialize, ToSchema, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum OutputKind {
    #[default]
    Pdf,
    Images,
    Bundle,
}

/// How pages are rendered to images.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ImageOptions {
    #[schema(nullable = false)]
    #[serde(default)]
    format: Option<ImageFormat>,
    #[schema(minimum = 36, maximum = 600, default = 150)]
    #[schema(nullable = false)]
    #[serde(default)]
    dpi: Option<u32>,
    #[schema(minimum = 64, maximum = 8192)]
    #[schema(nullable = false)]
    #[serde(default)]
    max_edge: Option<u32>,
}

#[derive(Deserialize, Serialize, ToSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum ImageFormat {
    Jpeg,
    Png,
    Webp,
}

// ---------- manifest (lives inside the ZIP; no path references it) ----------

#[derive(Serialize, ToSchema)]
struct Manifest {
    manifest_version: u32,
    request_id: String,
    page_count: u32,
    #[schema(nullable = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pdf: Option<PdfInfo>,
    pages: Vec<PageInfo>,
    warnings: Vec<Warning>,
}

#[derive(Serialize, ToSchema)]
struct PdfInfo {
    path: String,
    size_bytes: u64,
    origin: PdfOrigin,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum PdfOrigin {
    Converted,
    Passthrough,
    Assembled,
}

#[derive(Serialize, ToSchema)]
struct PageInfo {
    number: u32,
    path: String,
    width_px: u32,
    height_px: u32,
    width_pt: f64,
    height_pt: f64,
    blank: bool,
}

#[derive(Serialize, ToSchema)]
struct Warning {
    code: WarningCode,
    message: String,
    #[schema(nullable = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    detail: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum WarningCode {
    FontSubstituted,
    PageCountMismatch,
    BlankPage,
    MacrosIgnored,
    RenderClamped,
}

// ---------- errors ----------

#[derive(Serialize, ToSchema)]
struct ErrorBody {
    error: String,
    code: ErrorCode,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ErrorCode {
    BadRequest,
    Unauthorized,
    UnsupportedFormat,
    ConversionFailed,
    TooManyRequests,
    ConversionTimeout,
}

#[derive(Serialize, ToSchema)]
struct Health {
    status: String,
}

// ---------- handlers ----------

/// Convert one document to PDF, page images, or both
#[utoipa::path(
    post,
    path = "/v1/convert",
    operation_id = "convertDocument",
    tag = "conversion",
    request_body(
        content = ConvertRequest,
        content_type = "multipart/form-data",
        encoding(
            ("file" = (content_type = "application/octet-stream")),
            ("options" = (content_type = "application/json")),
        ),
    ),
    responses(
        (status = 200, description = "The PDF or a ZIP, by `output`",
            headers(
                ("Convert-Page-Count" = u32, description = "Pages in the PDF"),
                ("Convert-Engine-Fingerprint" = String, description = "Engine build"),
            ),
            content(
                (Vec<u8> = "application/pdf"),
                (Vec<u8> = "application/zip"),
            )),
        (status = 415, description = "Unsupported format", body = ErrorBody),
        (status = 422, description = "Cannot convert this input", body = ErrorBody),
        (status = 429, description = "Queue full",
            headers(("Retry-After" = u32)), body = ErrorBody),
    ),
)]
async fn convert() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

/// Liveness probe
#[utoipa::path(get, path = "/healthz", operation_id = "getLiveness", tag = "operations",
    security(()),
    responses((status = 200, body = Health)))]
async fn healthz() -> Json<Health> {
    Json(Health { status: "ok".into() })
}

struct BearerAuth;
impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "ApiKeyBearer",
            SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Convert API", version = "1.0.0-draft.1"),
    modifiers(&BearerAuth),
    security(("ApiKeyBearer" = [])),
    components(schemas(Manifest)),
)]
struct ApiDoc;

fn main() {
    let (_router, api): (axum::Router, _) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(convert))
        .routes(routes!(healthz))
        .split_for_parts();
    println!("{}", api.to_pretty_json().expect("serialize spec"));
}
