//! `GET /v1/capabilities`.

use serde::Serialize;
use utoipa::ToSchema;

use super::manifest::{EngineInfo, FormatFamily};
use super::options::{ImageFormatDto, OutputKind};
use crate::domain::conversion::{ImageFormat, Output};
use crate::domain::document::{EngineKind, FORMATS};
use crate::domain::limits::Limits;

#[derive(Debug, Serialize, ToSchema)]
pub struct Capabilities {
    pub service: ServiceInfo,
    pub engine: EngineInfo,
    /// Every input format this deployment accepts.
    pub inputs: Vec<InputFormat>,
    pub outputs: Vec<OutputKind>,
    pub image_formats: Vec<ImageFormatDto>,
    pub limits: LimitsDto,
    /// Installed font families, sorted by name.
    #[schema(example = json!(["Caladea", "Carlito", "DejaVu Sans", "Liberation Sans", "Noto Sans"]))]
    pub fonts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ServiceInfo {
    #[schema(example = "convert")]
    pub name: String,
    #[schema(example = "1.0.0")]
    pub version: String,
    /// Git commit the binary was built from.
    #[schema(nullable = false, example = "3f9c2ab")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InputFormat {
    /// Short identifier, used in `Convert-Source-Format` and the manifest.
    #[schema(example = "docx")]
    pub format: String,
    pub family: FormatFamily,
    #[schema(example = json!(["docx"]))]
    pub extensions: Vec<String>,
    #[schema(example = json!(["application/vnd.openxmlformats-officedocument.wordprocessingml.document"]))]
    pub media_types: Vec<String>,
    #[schema(inline)]
    pub engine: InputEngine,
}

/// Which engine turns this format into a PDF. `pdfium` means the file already is one.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputEngine {
    Libreoffice,
    Pdfium,
    Image,
}

impl From<EngineKind> for InputEngine {
    fn from(kind: EngineKind) -> Self {
        match kind {
            EngineKind::LibreOffice => InputEngine::Libreoffice,
            EngineKind::Pdfium => InputEngine::Pdfium,
            EngineKind::Image => InputEngine::Image,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(as = Limits)]
pub struct LimitsDto {
    #[schema(example = 125829120)]
    pub max_upload_bytes: u64,
    /// Most pages one call may render to images.
    #[schema(example = 500)]
    pub max_render_pages: u32,
    #[schema(example = 300)]
    pub max_dpi: u32,
    #[schema(example = 4096)]
    pub max_edge_px: u32,
    #[schema(example = 120)]
    pub default_timeout_seconds: u64,
    #[schema(example = 210)]
    pub max_timeout_seconds: u64,
}

impl Capabilities {
    /// The capabilities of this deployment, fixed for its lifetime.
    pub fn new(
        service: ServiceInfo,
        engine: EngineInfo,
        limits: &Limits,
        fonts: Vec<String>,
    ) -> Self {
        Self {
            service,
            engine,
            inputs: FORMATS
                .iter()
                .map(|row| InputFormat {
                    format: row.id.to_owned(),
                    family: row.family.into(),
                    extensions: row.extensions.iter().map(|&e| e.to_owned()).collect(),
                    media_types: row.media_types.iter().map(|&m| m.to_owned()).collect(),
                    engine: row.engine.into(),
                })
                .collect(),
            outputs: [Output::Pdf, Output::Images, Output::Bundle]
                .into_iter()
                .map(OutputKind::from)
                .collect(),
            image_formats: [ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::Webp]
                .into_iter()
                .map(ImageFormatDto::from)
                .collect(),
            limits: LimitsDto {
                max_upload_bytes: limits.max_upload_bytes,
                max_render_pages: limits.max_render_pages,
                max_dpi: u32::from(limits.max_dpi),
                max_edge_px: limits.max_edge_px,
                default_timeout_seconds: limits.timeout_default.as_secs(),
                max_timeout_seconds: limits.timeout_max.as_secs(),
            },
            fonts,
        }
    }
}
