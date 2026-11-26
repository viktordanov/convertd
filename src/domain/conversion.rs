//! What a request asks for, after validation, and what it produced.

use std::path::PathBuf;
use std::time::Duration;

use super::conversion_service::ConversionError;
use super::document::{Family, Format};
use super::engine::{EngineTiming, RenderedPage};
use super::limits::Limits;
use super::pages::PageSelection;
use super::secret::Password;
use super::verification::{Check, Warning};

/// What the response contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Output {
    #[default]
    Pdf,
    Images,
    Bundle,
}

impl Output {
    pub fn as_str(self) -> &'static str {
        match self {
            Output::Pdf => "pdf",
            Output::Images => "images",
            Output::Bundle => "bundle",
        }
    }
}

/// Page image encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageFormat {
    #[default]
    Jpeg,
    Png,
}

impl ImageFormat {
    /// The API name: `jpeg` or `png`.
    pub fn as_str(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Png => "png",
        }
    }

    /// The file extension of `pages/NNNN.<ext>`.
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
        }
    }
}

/// What to do with speaker notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlideNotes {
    #[default]
    None,
    Append,
    Only,
}

/// The office export settings (`slides.*`, `sheets.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExportOptions {
    pub notes: SlideNotes,
    pub include_hidden_slides: bool,
    pub one_page_per_sheet: bool,
}

/// The image settings actually applied, after defaults and server caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSettings {
    pub format: ImageFormat,
    /// After `MAX_DPI`.
    pub dpi: u16,
    pub quality: u8,
    /// The request's `max_edge`, at most `MAX_EDGE_PX`.
    pub max_edge: Option<u32>,
}

/// The `options` part as sent, before range checks. Built by `rest/`
/// from its DTO; every field is optional. No `Debug`: it holds the
/// password in the clear.
#[derive(Clone, Default)]
pub struct RawOptions {
    pub output: Option<Output>,
    pub pages: Option<String>,
    pub password: Option<String>,
    pub timeout_seconds: Option<u32>,
    pub image_format: Option<ImageFormat>,
    pub dpi: Option<u32>,
    pub quality: Option<u32>,
    pub max_edge: Option<u32>,
    pub include_hidden_slides: Option<bool>,
    pub notes: Option<SlideNotes>,
    pub one_page_per_sheet: Option<bool>,
}

/// The spec's ranges (`ConvertOptions` in `api/openapi.json`).
pub const DPI_RANGE: (u32, u32) = (36, 600);
pub const QUALITY_RANGE: (u32, u32) = (1, 100);
pub const MAX_EDGE_RANGE: (u32, u32) = (64, 8192);
pub const TIMEOUT_RANGE: (u32, u32) = (5, 210);
pub const PASSWORD_MAX_CHARS: usize = 256;
pub const DEFAULT_DPI: u16 = 150;
pub const DEFAULT_QUALITY: u8 = 85;

/// One validated request.
#[derive(Debug, Clone)]
pub struct ConversionRequest {
    pub output: Output,
    pub pages: PageSelection,
    pub password: Option<Password>,
    /// After `TIMEOUT_MAX_SECONDS`.
    pub timeout: Duration,
    pub images: ImageSettings,
    /// The DPI the caller asked for, before `MAX_DPI`.
    pub requested_dpi: u16,
    /// The `max_edge` the caller asked for, before `MAX_EDGE_PX`.
    pub requested_max_edge: Option<u32>,
    pub export: ExportOptions,
}

impl ConversionRequest {
    /// Checks the spec's ranges, then applies the server caps: the timeout
    /// is lowered to `TIMEOUT_MAX_SECONDS` silently, the DPI is clamped to
    /// `MAX_DPI` and `max_edge` is capped at `MAX_EDGE_PX`. Verification
    /// raises `RENDER_CLAMPED` once it knows each page's effective DPI.
    pub fn new(raw: RawOptions, limits: &Limits) -> Result<Self, ConversionError> {
        let output = raw.output.unwrap_or_default();
        let pages = match raw.pages.as_deref() {
            None => PageSelection::All,
            Some(text) => PageSelection::parse(text)?,
        };
        let password = match raw.password {
            Some(p) if p.chars().count() > PASSWORD_MAX_CHARS => {
                return Err(ConversionError::Validation(format!(
                    "password must be at most {PASSWORD_MAX_CHARS} characters"
                )));
            }
            Some(p) if p.is_empty() => None,
            other => other.map(Password::new),
        };
        let timeout = match raw.timeout_seconds {
            None => limits.timeout_default,
            Some(seconds) => {
                in_range("timeout_seconds", seconds, TIMEOUT_RANGE)?;
                Duration::from_secs(u64::from(seconds)).min(limits.timeout_max)
            }
        };
        let requested_dpi = match raw.dpi {
            None => DEFAULT_DPI,
            Some(dpi) => to_u16(in_range("images.dpi", dpi, DPI_RANGE)?),
        };
        let quality = match raw.quality {
            None => DEFAULT_QUALITY,
            Some(q) => to_u8(in_range("images.quality", q, QUALITY_RANGE)?),
        };
        let requested_max_edge = raw.max_edge;
        let max_edge = match raw.max_edge {
            None => None,
            Some(edge) => {
                Some(in_range("images.max_edge", edge, MAX_EDGE_RANGE)?.min(limits.max_edge_px))
            }
        };
        let dpi = requested_dpi.min(limits.max_dpi);
        Ok(ConversionRequest {
            output,
            pages,
            password,
            timeout,
            images: ImageSettings {
                format: raw.image_format.unwrap_or_default(),
                dpi,
                quality,
                max_edge,
            },
            requested_dpi,
            requested_max_edge,
            export: ExportOptions {
                notes: raw.notes.unwrap_or_default(),
                include_hidden_slides: raw.include_hidden_slides.unwrap_or(false),
                one_page_per_sheet: raw.one_page_per_sheet.unwrap_or(false),
            },
        })
    }
}

fn in_range(name: &str, value: u32, (lo, hi): (u32, u32)) -> Result<u32, ConversionError> {
    if (lo..=hi).contains(&value) {
        Ok(value)
    } else {
        Err(ConversionError::Validation(format!(
            "{name} must be between {lo} and {hi}"
        )))
    }
}

fn to_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn to_u8(value: u32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

/// How the PDF came to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfOrigin {
    /// Produced by the office engine.
    Converted,
    /// The upload was a PDF: returned unchanged, or decrypted.
    Passthrough,
}

/// Where the time went, per stage. `None` means the stage did not run.
/// Queue, lease, spawn and bind are summed over the engine calls.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StageTimings {
    pub queue: Duration,
    pub lease: Duration,
    pub spawn: Duration,
    pub bind: Duration,
    pub convert: Option<Duration>,
    pub convert_engine: Option<Duration>,
    pub inspect: Option<Duration>,
    /// Inspect engine time without the probe render.
    pub inspect_engine: Option<Duration>,
    pub probe_engine: Option<Duration>,
    /// The office worker's pre-flight scan, inside `convert`: worker time,
    /// not engine time, so it stays out of `engine_total` and counts as
    /// overhead. `None` for inputs without pre-flight (PDFs).
    pub preflight: Option<Duration>,
    /// Not one of the 18 `Server-Timing` names (INTERFACES §9); logged only.
    pub decrypt: Option<Duration>,
    pub decrypt_engine: Option<Duration>,
    pub render: Option<Duration>,
    pub render_engine: Option<Duration>,
    pub verify: Option<Duration>,
}

impl StageTimings {
    /// Adds one engine call's queue, lease, spawn and bind.
    pub fn add_call(&mut self, timing: &EngineTiming) {
        self.queue += timing.queue;
        self.lease += timing.lease;
        self.spawn += timing.spawn.unwrap_or_default();
        self.bind += timing.bind.unwrap_or_default();
    }

    /// Σ worker engine time of INTERFACES §11: convert, inspect, probe and
    /// render.
    pub fn engine_total(&self) -> Duration {
        [
            self.convert_engine,
            self.inspect_engine,
            self.probe_engine,
            self.render_engine,
        ]
        .into_iter()
        .flatten()
        .sum()
    }

    /// The inspect and probe engine time: what the service's own checks
    /// cost (`overhead_strict` − `overhead`).
    pub fn checks_engine(&self) -> Duration {
        self.inspect_engine.unwrap_or_default() + self.probe_engine.unwrap_or_default()
    }
}

/// A finished conversion, ready to be sent.
#[derive(Debug, Clone)]
pub struct ConversionResult {
    /// `source.pdf` for a passthrough, `document.pdf` otherwise.
    pub pdf: PathBuf,
    pub origin: PdfOrigin,
    pub format: Format,
    pub family: Family,
    pub encrypted: bool,
    pub page_count: u32,
    /// Empty for `output=pdf`.
    pub pages: Vec<RenderedPage>,
    pub warnings: Vec<Warning>,
    /// The checks that could not run, for the metrics.
    pub skipped: Vec<(Check, &'static str)>,
    /// Pages past `VERIFY_PROBE_MAX_PAGES` that the blank-page probe did
    /// not look at, for `convert_verify_skipped_total{check="blank_page",
    /// reason="pages"}`.
    pub probe_skipped_pages: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::limits::test_limits;

    fn new(raw: RawOptions) -> Result<ConversionRequest, ConversionError> {
        ConversionRequest::new(raw, &test_limits())
    }

    fn validation(raw: RawOptions) -> String {
        match new(raw) {
            Err(ConversionError::Validation(message)) => message,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let request = new(RawOptions::default()).unwrap();
        assert_eq!(request.output, Output::Pdf);
        assert_eq!(request.pages, PageSelection::All);
        assert!(request.password.is_none());
        assert_eq!(request.timeout, Duration::from_secs(120));
        assert_eq!(
            request.images,
            ImageSettings {
                format: ImageFormat::Jpeg,
                dpi: 150,
                quality: 85,
                max_edge: None
            }
        );
        assert_eq!(request.export, ExportOptions::default());
    }

    type RangeCase = (&'static str, fn(u32) -> RawOptions, (u32, u32));

    #[test]
    fn every_range_bound() {
        let cases: [RangeCase; 4] = [
            (
                "images.dpi",
                |v| RawOptions {
                    dpi: Some(v),
                    ..Default::default()
                },
                DPI_RANGE,
            ),
            (
                "images.quality",
                |v| RawOptions {
                    quality: Some(v),
                    ..Default::default()
                },
                QUALITY_RANGE,
            ),
            (
                "images.max_edge",
                |v| RawOptions {
                    max_edge: Some(v),
                    ..Default::default()
                },
                MAX_EDGE_RANGE,
            ),
            (
                "timeout_seconds",
                |v| RawOptions {
                    timeout_seconds: Some(v),
                    ..Default::default()
                },
                TIMEOUT_RANGE,
            ),
        ];
        for (name, raw, (lo, hi)) in cases {
            assert!(new(raw(lo)).is_ok(), "{name} {lo}");
            assert!(new(raw(hi)).is_ok(), "{name} {hi}");
            assert_eq!(
                validation(raw(lo - 1)),
                format!("{name} must be between {lo} and {hi}")
            );
            assert_eq!(
                validation(raw(hi + 1)),
                format!("{name} must be between {lo} and {hi}")
            );
        }
    }

    #[test]
    fn timeout_is_capped_silently() {
        let mut limits = test_limits();
        limits.timeout_max = Duration::from_secs(60);
        let raw = RawOptions {
            timeout_seconds: Some(200),
            ..Default::default()
        };
        let request = ConversionRequest::new(raw, &limits).unwrap();
        assert_eq!(request.timeout, Duration::from_secs(60));
    }

    #[test]
    fn dpi_and_edge_are_clamped() {
        let raw = RawOptions {
            output: Some(Output::Images),
            dpi: Some(600),
            max_edge: Some(8000),
            ..Default::default()
        };
        let request = new(raw).unwrap();
        assert_eq!(request.images.dpi, 300);
        assert_eq!(request.requested_dpi, 600);
        assert_eq!(request.images.max_edge, Some(4096));
        assert_eq!(request.requested_max_edge, Some(8000));
    }

    #[test]
    fn max_edge_is_capped() {
        let raw = RawOptions {
            max_edge: Some(8192),
            ..Default::default()
        };
        assert_eq!(new(raw).unwrap().images.max_edge, Some(4096));
    }

    #[test]
    fn password_rules() {
        let raw = RawOptions {
            password: Some("x".repeat(257)),
            ..Default::default()
        };
        assert_eq!(validation(raw), "password must be at most 256 characters");
        let raw = RawOptions {
            password: Some("é".repeat(256)),
            ..Default::default()
        };
        assert!(new(raw).unwrap().password.is_some());
        let raw = RawOptions {
            password: Some(String::new()),
            ..Default::default()
        };
        assert!(new(raw).unwrap().password.is_none());
    }

    #[test]
    fn bad_pages_fail_early() {
        let raw = RawOptions {
            pages: Some("5-3".into()),
            ..Default::default()
        };
        assert!(matches!(new(raw), Err(ConversionError::Validation(_))));
    }
}
