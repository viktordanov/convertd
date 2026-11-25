//! The emitted OpenAPI 3.1 document (`convertd openapi`,
//! `/swagger/doc.json`). Names follow `api/openapi.yaml` exactly.

use utoipa::openapi::OpenApi as Spec;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::openapi::{RefOr, Schema};
use utoipa::{IntoParams, Modify, OpenApi, ToResponse};

use super::serialization::errors::ErrorBody;
use super::serialization::manifest::Manifest;

/// The API version, set in code (never `CARGO_PKG_VERSION`).
pub const API_VERSION: &str = "1.0.0-draft.1";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Convert API",
        version = "1.0.0-draft.1",
        description = include_str!("openapi_description.md"),
        license(name = "Apache-2.0", url = "https://www.apache.org/licenses/LICENSE-2.0"),
    ),
    servers((url = "http://localhost:8080", description = "Local development")),
    tags(
        (name = "conversion", description = "The one endpoint that does the work."),
        (name = "discovery", description = "What this deployment supports."),
        (name = "operations", description = "Probes and metrics for the platform. No API key."),
    ),
    modifiers(&BearerAuth),
    security(("ApiKeyBearer" = [])),
    components(
        schemas(Manifest, ErrorBody),
        responses(
            BadRequest,
            Unauthorized,
            FileTooLarge,
            UnsupportedFormat,
            Unprocessable,
            TooManyRequests,
            InternalError,
            ServiceUnavailable,
            ConversionTimeout,
        ),
    ),
)]
struct ApiDoc;

/// Adds the bearer scheme the global `security` names.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut Spec) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "ApiKeyBearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "The service API key, sent as `Authorization: Bearer <API_KEY>`.",
                    ))
                    .build(),
            ),
        );
    }
}

/// The document's head: everything but the paths, which the router adds.
pub(super) fn base() -> Spec {
    ApiDoc::openapi()
}

/// The whole document. Reads no environment and needs no state.
pub fn spec() -> Spec {
    let mut spec = super::server::api_router().split_for_parts().1;
    polish(&mut spec);
    spec
}

/// Header facts utoipa's header tuples cannot state: (name, minimum,
/// example), as in the reference.
const HEADERS: &[(&str, Option<f64>, HeaderExample)] = &[
    (
        "X-Request-ID",
        None,
        HeaderExample::Str("7f3c2a9e-4b1d-4f6a-9a51-0c2e8d7b6a14"),
    ),
    (
        "Content-Disposition",
        None,
        HeaderExample::Str("attachment; filename=\"deck.pdf\""),
    ),
    ("Convert-Source-Format", None, HeaderExample::Str("pptx")),
    ("Convert-Page-Count", Some(1.0), HeaderExample::Int(12)),
    ("Convert-Warning-Count", None, HeaderExample::Int(0)),
    (
        "Convert-Engine-Fingerprint",
        None,
        HeaderExample::Str("sha256:5b0f6e1c9a2d"),
    ),
    ("Retry-After", Some(1.0), HeaderExample::Int(5)),
];

enum HeaderExample {
    Str(&'static str),
    Int(u64),
}

/// Brings the emitted document to the reference's shape where utoipa
/// cannot express it directly: header minimums and examples, and
/// `security: []` (not `[{}]`) on the probes.
fn polish(spec: &mut Spec) {
    for item in spec.paths.paths.values_mut() {
        for operation in item.get.iter_mut().chain(item.post.iter_mut()) {
            if let Some(security) = &mut operation.security
                && security.len() == 1
                && security[0] == SecurityRequirement::default()
            {
                security.clear();
            }
        }
    }
    let responses = spec
        .components
        .iter_mut()
        .flat_map(|components| components.responses.values_mut())
        .chain(
            spec.paths
                .paths
                .values_mut()
                .flat_map(|item| item.post.iter_mut().chain(item.get.iter_mut()))
                .flat_map(|operation| operation.responses.responses.values_mut()),
        );
    for response in responses {
        let RefOr::T(response) = response else {
            continue;
        };
        for (name, header) in response.headers.iter_mut() {
            let (RefOr::T(header), Some((_, minimum, example))) =
                (header, HEADERS.iter().find(|(known, _, _)| known == name))
            else {
                continue;
            };
            header.example = Some(match example {
                HeaderExample::Str(text) => serde_json::Value::from(*text),
                HeaderExample::Int(n) => serde_json::Value::from(*n),
            });
            if let (Some(minimum), Some(RefOr::T(Schema::Object(object)))) =
                (minimum, &mut header.schema)
            {
                object.minimum = Some((*minimum).into());
            }
        }
    }
}

/// The pretty JSON `convertd openapi` prints, without the final newline.
pub fn spec_json() -> String {
    // Serialising a document of plain maps and strings cannot fail.
    spec()
        .to_pretty_json()
        .unwrap_or_else(|err| format!("{{\"error\":\"failed to serialise the spec: {err}\"}}"))
}

/// The `X-Request-ID` parameter of `convertDocument` and `getCapabilities`.
#[derive(IntoParams)]
#[into_params(parameter_in = Header)]
#[allow(dead_code)]
pub struct RequestId {
    /// Correlation ID. If present it is echoed in the response, the logs and
    /// the manifest. If absent the service generates one.
    #[param(
        rename = "X-Request-ID",
        nullable = false,
        max_length = 128,
        example = "7f3c2a9e-4b1d-4f6a-9a51-0c2e8d7b6a14"
    )]
    x_request_id: Option<String>,
}

/// The request is malformed, for example no `file` part or `options` is not valid JSON.
#[derive(ToResponse)]
#[response(
    headers(("X-Request-ID" = String, description = "The correlation ID of this request.")),
    example = json!({"error": "missing multipart part \"file\"", "code": "BAD_REQUEST"}),
)]
#[allow(dead_code)]
pub struct BadRequest(ErrorBody);

/// The API key is missing or wrong.
#[derive(ToResponse)]
#[response(
    headers(("X-Request-ID" = String, description = "The correlation ID of this request.")),
    example = json!({"error": "missing or invalid API key", "code": "UNAUTHORIZED"}),
)]
#[allow(dead_code)]
pub struct Unauthorized(ErrorBody);

/// The upload exceeds `limits.max_upload_bytes`.
#[derive(ToResponse)]
#[response(
    headers(("X-Request-ID" = String, description = "The correlation ID of this request.")),
    example = json!({"error": "file exceeds the 125829120 byte limit", "code": "FILE_TOO_LARGE"}),
)]
#[allow(dead_code)]
pub struct FileTooLarge(ErrorBody);

/// The detected file type is not on the allow-list. The message names
/// what was detected. `/v1/capabilities` lists what is accepted.
#[derive(ToResponse)]
#[response(
    headers(("X-Request-ID" = String, description = "The correlation ID of this request.")),
    example = json!({"error": "unsupported format: html", "code": "UNSUPPORTED_FORMAT"}),
)]
#[allow(dead_code)]
pub struct UnsupportedFormat(ErrorBody);

/// The request was understood but cannot be carried out for this input.
/// Retrying the same request will fail the same way.
///
/// | Code | Meaning |
/// | --- | --- |
/// | `VALIDATION_ERROR` | An option is out of range, unknown, or `pages` selects nothing. |
/// | `PASSWORD_REQUIRED` | The file is encrypted and no `password` was sent. |
/// | `INVALID_PASSWORD` | The `password` does not open the file. |
/// | `LIMIT_EXCEEDED` | The job would exceed a server limit, for example more pages to render than `limits.max_render_pages`. |
/// | `CONVERSION_FAILED` | The engine could not convert the document. It is corrupt, or it crashed the engine twice. |
#[derive(ToResponse)]
#[response(
    headers(("X-Request-ID" = String, description = "The correlation ID of this request.")),
    examples(
        ("validation" = (summary = "Option out of range",
            value = json!({"error": "images.dpi must be between 36 and 600", "code": "VALIDATION_ERROR"}))),
        ("passwordRequired" = (summary = "Encrypted file without a password",
            value = json!({"error": "the document is encrypted and no password was provided", "code": "PASSWORD_REQUIRED"}))),
        ("conversionFailed" = (summary = "The engine could not convert the file",
            value = json!({"error": "the document could not be converted", "code": "CONVERSION_FAILED"}))),
    ),
)]
#[allow(dead_code)]
pub struct Unprocessable(ErrorBody);

/// All workers are busy and the wait queue is full. Retry after the indicated delay.
#[derive(ToResponse)]
#[response(
    headers(
        ("X-Request-ID" = String, description = "The correlation ID of this request."),
        ("Retry-After" = u32, description = "Seconds to wait before retrying."),
    ),
    example = json!({"error": "the conversion queue is full", "code": "TOO_MANY_REQUESTS"}),
)]
#[allow(dead_code)]
pub struct TooManyRequests(ErrorBody);

/// An unexpected failure inside the service. Details are logged under the request ID and not returned.
#[derive(ToResponse)]
#[response(
    headers(("X-Request-ID" = String, description = "The correlation ID of this request.")),
    example = json!({"error": "internal server error", "code": "INTERNAL_ERROR"}),
)]
#[allow(dead_code)]
pub struct InternalError(ErrorBody);

/// The service is starting, draining for shutdown, or has no healthy worker.
#[derive(ToResponse)]
#[response(
    headers(
        ("X-Request-ID" = String, description = "The correlation ID of this request."),
        ("Retry-After" = u32, description = "Seconds to wait before retrying."),
    ),
    example = json!({"error": "the service is not ready", "code": "SERVICE_UNAVAILABLE"}),
)]
#[allow(dead_code)]
pub struct ServiceUnavailable(ErrorBody);

/// The job did not finish within `timeout_seconds`. The engine process
/// was killed and replaced.
#[derive(ToResponse)]
#[response(
    headers(("X-Request-ID" = String, description = "The correlation ID of this request.")),
    example = json!({"error": "the conversion did not finish within 120 seconds", "code": "CONVERSION_TIMEOUT"}),
)]
#[allow(dead_code)]
pub struct ConversionTimeout(ErrorBody);
