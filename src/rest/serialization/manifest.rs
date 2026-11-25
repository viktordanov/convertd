//! `manifest.json`, the first entry of every ZIP response (filled in step 8).

use serde::Serialize;
use utoipa::ToSchema;
use utoipa::openapi::schema::{AdditionalProperties, ObjectBuilder, Type};
use utoipa::openapi::{RefOr, Schema};

use super::options::ImageFormatDto;
use crate::domain::conversion::{
    ConversionRequest, ConversionResult, ImageFormat, Output, PdfOrigin,
};
use crate::domain::conversion_service::UploadFacts;
use crate::domain::document::Family;
use crate::domain::verification::{DetailValue, Warning, WarningCode};

/// The `manifest.json` at the root of every ZIP response. It describes
/// the source, the PDF, each rendered page and anything the service
/// noticed during conversion.
#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "manifest_version": 1,
    "request_id": "7f3c2a9e-4b1d-4f6a-9a51-0c2e8d7b6a14",
    "source": {
        "filename": "deck.pptx", "format": "pptx", "family": "presentation",
        "size_bytes": 4821337,
        "sha256": "9f2c6c1e0a4b7d3f5e8a1b2c3d4e5f60718293a4b5c6d7e8f901122334455667",
        "encrypted": false
    },
    "page_count": 2,
    "pdf": {"path": "document.pdf", "size_bytes": 1203344, "origin": "converted"},
    "images": {"format": "jpeg", "dpi": 150, "quality": 85},
    "pages": [
        {"number": 1, "path": "pages/0001.jpg", "width_px": 2000, "height_px": 1125,
         "width_pt": 960, "height_pt": 540, "size_bytes": 184220, "blank": false},
        {"number": 2, "path": "pages/0002.jpg", "width_px": 2000, "height_px": 1125,
         "width_pt": 960, "height_pt": 540, "size_bytes": 201877, "blank": false}
    ],
    "warnings": [{
        "code": "FONT_SUBSTITUTED",
        "message": "Aptos is not installed and was replaced by Carlito",
        "detail": {"requested": "Aptos", "substitute": "Carlito"}
    }],
    "engine": {"service_version": "1.0.0", "libreoffice": "26.8.3.2", "pdfium": "8125",
               "fingerprint": "sha256:5b0f6e1c9a2d"},
    "timings_ms": {"queue": 3, "convert": 812, "render": 240, "total": 1071}
}))]
pub struct Manifest {
    /// Version of this manifest layout.
    #[schema(schema_with = manifest_version_schema)]
    pub manifest_version: u32,
    /// The correlation ID of the request that produced this bundle.
    pub request_id: String,
    pub source: SourceInfo,
    /// Number of pages in the PDF, whether or not all were rendered.
    #[schema(minimum = 1)]
    pub page_count: u32,
    #[schema(nullable = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf: Option<PdfInfo>,
    #[schema(nullable = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<ImageSettings>,
    /// One entry per rendered page, in page order.
    pub pages: Vec<PageInfo>,
    /// Things the service noticed that may affect quality. Empty when all checks passed.
    pub warnings: Vec<WarningDto>,
    pub engine: EngineInfo,
    pub timings_ms: Timings,
}

fn manifest_version_schema() -> impl Into<RefOr<Schema>> {
    ObjectBuilder::new()
        .schema_type(Type::Integer)
        .enum_values(Some([1]))
        .description(Some("Version of this manifest layout."))
}

/// What was uploaded.
#[derive(Debug, Serialize, ToSchema)]
pub struct SourceInfo {
    /// The file name sent with the upload, if any.
    #[schema(nullable = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// The detected format, as listed by `/v1/capabilities`.
    #[schema(example = "pptx")]
    pub format: String,
    pub family: FormatFamily,
    pub size_bytes: u64,
    /// SHA-256 of the uploaded bytes, in lowercase hex.
    #[schema(pattern = "^[0-9a-f]{64}$")]
    pub sha256: String,
    /// Whether the upload was password-protected.
    pub encrypted: bool,
}

/// The broad kind of input, which decides how it is routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FormatFamily {
    Document,
    Presentation,
    Spreadsheet,
    Pdf,
    Image,
}

impl From<Family> for FormatFamily {
    fn from(family: Family) -> Self {
        match family {
            Family::Document => FormatFamily::Document,
            Family::Presentation => FormatFamily::Presentation,
            Family::Spreadsheet => FormatFamily::Spreadsheet,
            Family::Pdf => FormatFamily::Pdf,
            Family::Image => FormatFamily::Image,
        }
    }
}

/// The PDF inside a `bundle`. Absent for `images`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PdfInfo {
    /// Path of the PDF inside the ZIP.
    #[schema(example = "document.pdf")]
    pub path: String,
    pub size_bytes: u64,
    #[schema(inline)]
    pub origin: PdfOriginDto,
}

/// How the PDF came to be.
///
/// - `converted`: produced by LibreOffice from an office file.
/// - `passthrough`: the upload was already a PDF and is returned unchanged, or decrypted when it was encrypted.
/// - `assembled`: built from an uploaded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PdfOriginDto {
    Converted,
    Passthrough,
    Assembled,
}

impl From<PdfOrigin> for PdfOriginDto {
    fn from(origin: PdfOrigin) -> Self {
        match origin {
            PdfOrigin::Converted => PdfOriginDto::Converted,
            PdfOrigin::Passthrough => PdfOriginDto::Passthrough,
            PdfOrigin::Assembled => PdfOriginDto::Assembled,
        }
    }
}

/// The image settings that were actually applied, after defaults and server caps.
#[derive(Debug, Serialize, ToSchema)]
pub struct ImageSettings {
    pub format: ImageFormatDto,
    pub dpi: u32,
    #[schema(nullable = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u32>,
    #[schema(nullable = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_edge: Option<u32>,
}

/// One rendered page.
#[derive(Debug, Serialize, ToSchema)]
pub struct PageInfo {
    /// 1-based page number in the PDF.
    #[schema(minimum = 1)]
    pub number: u32,
    /// Path of the image inside the ZIP.
    #[schema(example = "pages/0001.jpg")]
    pub path: String,
    #[schema(minimum = 1)]
    pub width_px: u32,
    #[schema(minimum = 1)]
    pub height_px: u32,
    /// Page width in PDF points (1/72 inch).
    #[schema(schema_with = width_pt_schema)]
    pub width_pt: f64,
    /// Page height in PDF points (1/72 inch).
    #[schema(schema_with = height_pt_schema)]
    pub height_pt: f64,
    pub size_bytes: u64,
    /// True when the rendered page is a single flat colour.
    pub blank: bool,
}

/// `type: number` with no `format`, as in the reference.
fn width_pt_schema() -> impl Into<RefOr<Schema>> {
    ObjectBuilder::new()
        .schema_type(Type::Number)
        .description(Some("Page width in PDF points (1/72 inch)."))
}

fn height_pt_schema() -> impl Into<RefOr<Schema>> {
    ObjectBuilder::new()
        .schema_type(Type::Number)
        .description(Some("Page height in PDF points (1/72 inch)."))
}

/// A quality signal. Warnings never fail the request.
#[derive(Debug, Serialize, ToSchema)]
#[schema(as = Warning)]
pub struct WarningDto {
    pub code: WarningCodeDto,
    /// A human-readable sentence.
    pub message: String,
    /// The page the warning is about, when it concerns one page.
    #[schema(nullable = false, minimum = 1)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    /// Structured data for the warning. Its keys depend on `code`.
    #[schema(nullable = false, schema_with = detail_schema)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Map<String, serde_json::Value>>,
}

fn detail_schema() -> impl Into<RefOr<Schema>> {
    ObjectBuilder::new()
        .schema_type(Type::Object)
        .additional_properties(Some(AdditionalProperties::<Schema>::FreeForm(true)))
        .description(Some(
            "Structured data for the warning. Its keys depend on `code`.",
        ))
}

/// - `FONT_SUBSTITUTED`: a font the document asks for is not installed.
///   Text may reflow. `detail` has `requested` and `substitute`.
/// - `PAGE_COUNT_MISMATCH`: the PDF has a different number of pages than
///   the source declares. `detail` has `expected` and `actual`.
/// - `BLANK_PAGE`: a page that contains content rendered as a flat colour.
/// - `MACROS_IGNORED`: the document contains macros. They were not run.
/// - `RENDER_CLAMPED`: a server limit reduced the requested resolution.
///   `detail` has `requested_dpi` and `effective_dpi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[schema(as = WarningCode)]
pub enum WarningCodeDto {
    FontSubstituted,
    PageCountMismatch,
    BlankPage,
    MacrosIgnored,
    RenderClamped,
}

impl From<WarningCode> for WarningCodeDto {
    fn from(code: WarningCode) -> Self {
        match code {
            WarningCode::FontSubstituted => WarningCodeDto::FontSubstituted,
            WarningCode::PageCountMismatch => WarningCodeDto::PageCountMismatch,
            WarningCode::BlankPage => WarningCodeDto::BlankPage,
            WarningCode::MacrosIgnored => WarningCodeDto::MacrosIgnored,
            WarningCode::RenderClamped => WarningCodeDto::RenderClamped,
        }
    }
}

/// The software that produced the result.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EngineInfo {
    #[schema(example = "1.0.0")]
    pub service_version: String,
    /// LibreOffice version. Absent from a manifest when LibreOffice was not used.
    #[schema(nullable = false, example = "26.8.3.2")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libreoffice: Option<String>,
    /// PDFium build number.
    #[schema(example = "8125")]
    pub pdfium: String,
    /// Same value as the `Convert-Engine-Fingerprint` header.
    #[schema(example = "sha256:5b0f6e1c9a2d")]
    pub fingerprint: String,
}

/// Where the time went, in milliseconds.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Timings {
    /// Waiting for a free worker.
    pub queue: u64,
    /// Producing the PDF. Zero for PDF uploads.
    pub convert: u64,
    /// Rendering and encoding page images.
    pub render: u64,
    /// From the end of the upload to the start of the response.
    pub total: u64,
}

/// What [`Manifest::new`] needs besides the conversion result.
pub struct ManifestParts<'a> {
    pub request_id: &'a str,
    pub upload: &'a UploadFacts,
    pub request: &'a ConversionRequest,
    pub result: &'a ConversionResult,
    /// The `fstat` size of the PDF, for `bundle`.
    pub pdf_size: Option<u64>,
    /// The `fstat` size of each rendered page, in page order.
    pub page_sizes: &'a [u64],
    pub engine: &'a EngineInfo,
    pub timings: Timings,
}

/// The ZIP path of a page image: `pages/NNNN.<ext>`, at least 4 digits.
pub fn page_path(number: u32, format: ImageFormat) -> String {
    format!("pages/{number:04}.{}", format.extension())
}

impl Manifest {
    pub fn new(parts: ManifestParts<'_>) -> Self {
        let ManifestParts {
            request_id,
            upload,
            request,
            result,
            pdf_size,
            page_sizes,
            engine,
            timings,
        } = parts;
        let images = request.images;
        let mut engine = engine.clone();
        if result.origin != PdfOrigin::Converted {
            // LibreOffice was not used.
            engine.libreoffice = None;
        }
        Manifest {
            manifest_version: 1,
            request_id: request_id.to_owned(),
            source: SourceInfo {
                filename: upload.file_name.clone(),
                format: result.format.id().to_owned(),
                family: result.family.into(),
                size_bytes: upload.size_bytes,
                sha256: crate::rest::state::hex(&upload.sha256),
                encrypted: result.encrypted,
            },
            page_count: result.page_count,
            pdf: match (request.output, pdf_size) {
                (Output::Bundle, Some(size)) => Some(PdfInfo {
                    path: "document.pdf".to_owned(),
                    size_bytes: size,
                    origin: result.origin.into(),
                }),
                _ => None,
            },
            images: Some(ImageSettings {
                format: images.format.into(),
                dpi: u32::from(images.dpi),
                quality: (images.format != ImageFormat::Png).then_some(u32::from(images.quality)),
                max_edge: images.max_edge,
            }),
            pages: result
                .pages
                .iter()
                .zip(page_sizes)
                .map(|(page, &size)| PageInfo {
                    number: page.number,
                    path: page_path(page.number, images.format),
                    width_px: page.width_px,
                    height_px: page.height_px,
                    width_pt: f64::from(page.width_pt),
                    height_pt: f64::from(page.height_pt),
                    size_bytes: size,
                    blank: page.flat,
                })
                .collect(),
            warnings: result.warnings.iter().map(WarningDto::from).collect(),
            engine,
            timings_ms: timings,
        }
    }
}

impl From<&Warning> for WarningDto {
    fn from(warning: &Warning) -> Self {
        let detail: serde_json::Map<String, serde_json::Value> = warning
            .detail
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    DetailValue::Str(text) => serde_json::Value::from(text.as_str()),
                    DetailValue::Int(n) => serde_json::Value::from(*n),
                    DetailValue::Float(f) => serde_json::Value::from(*f),
                };
                ((*key).to_owned(), value)
            })
            .collect();
        WarningDto {
            code: warning.code.into(),
            message: warning.message.clone(),
            page: warning.page,
            detail: (!detail.is_empty()).then_some(detail),
        }
    }
}
