//! The multipart request and its `options` part.

use serde::Deserialize;
use utoipa::ToSchema;

use crate::domain::conversion::{ImageFormat, Output, RawOptions, SlideNotes};

// The multipart body. Documentation only: the handler reads the parts by
// hand (`rest/upload.rs`).
#[derive(ToSchema)]
#[schema(as = ConvertRequest)]
#[allow(dead_code)]
pub struct ConvertRequestDoc {
    /// The document to convert. The part's file name is used only to name
    /// the response and is echoed in the manifest. The type is detected
    /// from the bytes.
    #[schema(value_type = String, format = Binary)]
    file: Vec<u8>,
    #[schema(nullable = false)]
    options: Option<ConvertOptions>,
}

/// What to produce and how. Every property is optional. An empty object,
/// or no `options` part at all, returns a PDF with default settings.
// No `Debug`: it holds the password.
#[derive(Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(example = json!({
    "output": "bundle",
    "pages": "1-20",
    "images": {"format": "jpeg", "dpi": 150, "quality": 85, "max_edge": 2576}
}))]
pub struct ConvertOptions {
    #[schema(nullable = false, default = "pdf")]
    #[serde(default)]
    pub output: Option<OutputKind>,
    /// Which pages to render as images, 1-based. A comma-separated list of
    /// single pages (`3`), closed ranges (`1-5`) and open ranges (`10-`).
    /// Pages past the end of the document are ignored. If nothing is
    /// left the call fails with `VALIDATION_ERROR`.
    ///
    /// Applies to `images` and `bundle`. The PDF always holds every page.
    /// Default: all pages.
    #[schema(
        nullable = false,
        pattern = r"^[0-9]+(-[0-9]*)?(,[0-9]+(-[0-9]*)?)*$",
        max_length = 256,
        example = "1-5,8,10-"
    )]
    #[serde(default)]
    pub pages: Option<String>,
    /// Password for an encrypted input. It is used once to open the file,
    /// never logged and never stored. The resulting PDF is not encrypted.
    #[schema(nullable = false, write_only, max_length = 256)]
    #[serde(default)]
    pub password: Option<String>,
    /// Total time budget for this call, in seconds. Covers waiting for a
    /// worker, conversion, rendering and packaging. The server caps it at
    /// `limits.max_timeout_seconds`.
    #[schema(nullable = false, minimum = 5, maximum = 210, default = 120)]
    #[serde(default)]
    pub timeout_seconds: Option<u32>,
    #[schema(nullable = false)]
    #[serde(default)]
    pub images: Option<ImageOptions>,
    #[schema(nullable = false)]
    #[serde(default)]
    pub slides: Option<SlideOptions>,
    #[schema(nullable = false)]
    #[serde(default)]
    pub sheets: Option<SheetOptions>,
}

/// What the response contains.
///
/// - `pdf`: the PDF, as `application/pdf`.
/// - `images`: page images and a manifest, as `application/zstd` (tar.zst).
/// - `bundle`: the PDF, page images and a manifest, as `application/zstd` (tar.zst).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Pdf,
    Images,
    Bundle,
}

/// How pages are rendered to images. Used by `images` and `bundle`.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageOptions {
    #[schema(nullable = false, default = "jpeg")]
    #[serde(default)]
    pub format: Option<ImageFormatDto>,
    /// Render resolution in dots per inch. A US Letter page is
    /// 1275 x 1650 px at 150 DPI. The server caps it at `limits.max_dpi`.
    #[schema(nullable = false, minimum = 36, maximum = 600, default = 150)]
    #[serde(default)]
    pub dpi: Option<u32>,
    /// Encoder quality for `jpeg`. Ignored for `png`.
    #[schema(nullable = false, minimum = 1, maximum = 100, default = 85)]
    #[serde(default)]
    pub quality: Option<u32>,
    /// Longest allowed edge of an image, in pixels. A page that would be
    /// larger is rendered at a lower resolution so that it fits, keeping
    /// its aspect ratio. Unset means no clamp beyond `limits.max_edge_px`.
    #[schema(nullable = false, minimum = 64, maximum = 8192)]
    #[serde(default)]
    pub max_edge: Option<u32>,
}

/// Image encoding.
///
/// - `jpeg`: smallest and fastest. Best for photos, slides and model input.
/// - `png`: lossless. Larger and slower to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = ImageFormat)]
pub enum ImageFormatDto {
    Jpeg,
    Png,
}

/// Options that only affect presentations.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SlideOptions {
    /// Also export slides that are marked hidden.
    #[schema(nullable = false, default = false)]
    #[serde(default)]
    pub include_hidden: Option<bool>,
    #[schema(nullable = false, default = "none")]
    #[serde(default)]
    pub notes: Option<SlideNotesDto>,
}

/// What to do with speaker notes.
///
/// - `none`: slides only.
/// - `append`: all slides, followed by one notes page per slide.
/// - `only`: notes pages only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = SlideNotes)]
pub enum SlideNotesDto {
    None,
    Append,
    Only,
}

/// Options that only affect spreadsheets.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SheetOptions {
    /// Put each sheet on a single page sized to its content, instead of
    /// splitting it across pages by its print layout.
    #[schema(nullable = false, default = false)]
    #[serde(default)]
    pub one_page_per_sheet: Option<bool>,
}

/// Why the `options` part was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum OptionsError {
    /// Not JSON (400 `BAD_REQUEST`).
    Syntax(String),
    /// JSON, but not a valid `ConvertOptions` (422 `VALIDATION_ERROR`):
    /// unknown fields, wrong types, bad enum values, not an object.
    Invalid(String),
}

impl ConvertOptions {
    /// Parses the `options` part. serde's error category decides the
    /// status: syntax and early EOF are 400, data errors 422.
    pub fn parse(bytes: &[u8]) -> Result<Self, OptionsError> {
        match serde_json::from_slice::<ConvertOptions>(bytes) {
            Ok(options) => {
                // serde also accepts a JSON array for a struct.
                if bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{') {
                    Ok(options)
                } else {
                    Err(OptionsError::Invalid(
                        "options must be a JSON object".into(),
                    ))
                }
            }
            Err(err) => match err.classify() {
                serde_json::error::Category::Data => {
                    Err(OptionsError::Invalid(format!("invalid options: {err}")))
                }
                _ => Err(OptionsError::Syntax(format!(
                    "options is not valid JSON: {err}"
                ))),
            },
        }
    }
}

impl From<OutputKind> for Output {
    fn from(kind: OutputKind) -> Self {
        match kind {
            OutputKind::Pdf => Output::Pdf,
            OutputKind::Images => Output::Images,
            OutputKind::Bundle => Output::Bundle,
        }
    }
}

impl From<Output> for OutputKind {
    fn from(output: Output) -> Self {
        match output {
            Output::Pdf => OutputKind::Pdf,
            Output::Images => OutputKind::Images,
            Output::Bundle => OutputKind::Bundle,
        }
    }
}

impl From<ImageFormatDto> for ImageFormat {
    fn from(format: ImageFormatDto) -> Self {
        match format {
            ImageFormatDto::Jpeg => ImageFormat::Jpeg,
            ImageFormatDto::Png => ImageFormat::Png,
        }
    }
}

impl From<ImageFormat> for ImageFormatDto {
    fn from(format: ImageFormat) -> Self {
        match format {
            ImageFormat::Jpeg => ImageFormatDto::Jpeg,
            ImageFormat::Png => ImageFormatDto::Png,
        }
    }
}

impl From<SlideNotesDto> for SlideNotes {
    fn from(notes: SlideNotesDto) -> Self {
        match notes {
            SlideNotesDto::None => SlideNotes::None,
            SlideNotesDto::Append => SlideNotes::Append,
            SlideNotesDto::Only => SlideNotes::Only,
        }
    }
}

impl From<ConvertOptions> for RawOptions {
    fn from(dto: ConvertOptions) -> Self {
        let images = dto.images.unwrap_or_default();
        let slides = dto.slides.unwrap_or_default();
        let sheets = dto.sheets.unwrap_or_default();
        RawOptions {
            output: dto.output.map(Output::from),
            pages: dto.pages,
            password: dto.password,
            timeout_seconds: dto.timeout_seconds,
            image_format: images.format.map(ImageFormat::from),
            dpi: images.dpi,
            quality: images.quality,
            max_edge: images.max_edge,
            include_hidden_slides: slides.include_hidden,
            notes: slides.notes.map(SlideNotes::from),
            one_page_per_sheet: sheets.one_page_per_sheet,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<ConvertOptions, OptionsError> {
        ConvertOptions::parse(json.as_bytes())
    }

    #[test]
    fn every_field_reaches_the_domain() {
        let dto = parse(
            r#"{"output":"bundle","pages":"1-3","password":"pw","timeout_seconds":30,
               "images":{"format":"png","dpi":72,"quality":50,"max_edge":320},
               "slides":{"include_hidden":true,"notes":"append"},
               "sheets":{"one_page_per_sheet":true}}"#,
        )
        .unwrap();
        let raw = RawOptions::from(dto);
        assert_eq!(raw.output, Some(Output::Bundle));
        assert_eq!(raw.pages.as_deref(), Some("1-3"));
        assert_eq!(raw.password.as_deref(), Some("pw"));
        assert_eq!(raw.timeout_seconds, Some(30));
        assert_eq!(raw.image_format, Some(ImageFormat::Png));
        assert_eq!(
            (raw.dpi, raw.quality, raw.max_edge),
            (Some(72), Some(50), Some(320))
        );
        assert_eq!(raw.include_hidden_slides, Some(true));
        assert_eq!(raw.notes, Some(SlideNotes::Append));
        assert_eq!(raw.one_page_per_sheet, Some(true));
    }

    #[test]
    fn empty_and_null_are_defaults() {
        let raw = RawOptions::from(parse("{}").unwrap());
        assert!(raw.output.is_none() && raw.dpi.is_none());
        let raw = RawOptions::from(parse(r#"{"images":null,"pages":null}"#).unwrap());
        assert!(raw.pages.is_none());
    }

    #[test]
    fn unknown_fields_and_wrong_types_are_data_errors() {
        for json in [
            r#"{"outptu":"pdf"}"#,
            r#"{"images":{"dpi":150,"colour":true}}"#,
            r#"{"output":"html"}"#,
            r#"{"timeout_seconds":"60"}"#,
            r#"{"images":{"dpi":-1}}"#,
            r#"{"images":{"dpi":99999999999}}"#,
            r#"{"slides":{"notes":"some"}}"#,
            r#"[]"#,
            r#""pdf""#,
            "1",
        ] {
            assert!(
                matches!(parse(json), Err(OptionsError::Invalid(_))),
                "{json}"
            );
        }
        for json in ["{", "{\"output\":", "nope", "", "{} x"] {
            assert!(
                matches!(parse(json), Err(OptionsError::Syntax(_))),
                "{json}"
            );
        }
        assert!(parse(" \n{}").is_ok());
    }
}
