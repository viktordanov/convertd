//! Warnings: quality signals that never fail a request.
//!
//! The rules compare what the source declares (`SourceFacts`, from the
//! office worker's pre-flight, INTERFACES §13) with what the engines
//! produced. The algorithms are 005 §5.

use super::conversion::{ConversionRequest, Output};
use super::document::Format;
use super::engine::{ProbedPage, RenderedPage};

/// A warning code, as in the spec's `WarningCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WarningCode {
    FontSubstituted,
    PageCountMismatch,
    BlankPage,
    MacrosIgnored,
    RenderClamped,
}

impl WarningCode {
    pub fn as_str(self) -> &'static str {
        match self {
            WarningCode::FontSubstituted => "FONT_SUBSTITUTED",
            WarningCode::PageCountMismatch => "PAGE_COUNT_MISMATCH",
            WarningCode::BlankPage => "BLANK_PAGE",
            WarningCode::MacrosIgnored => "MACROS_IGNORED",
            WarningCode::RenderClamped => "RENDER_CLAMPED",
        }
    }
}

/// One value in a warning's `detail` object.
#[derive(Debug, Clone, PartialEq)]
pub enum DetailValue {
    Str(String),
    Int(i64),
    Float(f64),
}

/// A quality signal.
#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    pub code: WarningCode,
    pub message: String,
    /// The page it is about, when it concerns one page.
    pub page: Option<u32>,
    /// Keys depend on `code` (005 §5); in this order in the manifest.
    pub detail: Vec<(&'static str, DetailValue)>,
}

/// The page count the source declares, and where it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredPages {
    pub count: u32,
    /// For example `docProps/app.xml` or `ppt/presentation.xml`.
    pub source: &'static str,
}

/// Where a document keeps its macros (V4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroKind {
    Vba,
    Xlm,
    Basic,
    Script,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroFact {
    pub kind: MacroKind,
    /// The part or storage that holds them.
    pub part: String,
}

/// A pre-flight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    PageCount,
    Fonts,
    Macros,
}

/// Why a check did not run. A skipped check is counted, never failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    CapReached,
    MissingPart,
    MalformedXml,
}

/// What pre-flight read from an office source (INTERFACES §13): the
/// office worker scans the file before LibreOffice loads it and returns
/// this in `Converted`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceFacts {
    pub declared_pages: Option<DeclaredPages>,
    /// Sorted, without duplicates, at most 256.
    pub requested_fonts: Vec<String>,
    pub macros: Option<MacroFact>,
    pub skipped: Vec<(Check, SkipReason)>,
}

/// What [`verify`] compares.
#[derive(Debug, Clone, Copy)]
pub struct VerifyInput<'a> {
    pub format: Format,
    pub request: &'a ConversionRequest,
    /// The office worker's pre-flight; `None` for PDF and image inputs.
    pub facts: Option<&'a SourceFacts>,
    /// Pages in the PDF.
    pub page_count: u32,
    /// The probe render of the inspection, if any.
    pub probe: &'a [ProbedPage],
    /// The rendered pages, for `images` and `bundle`.
    pub rendered: &'a [RenderedPage],
    /// `MAX_EDGE_PX`.
    pub max_edge_px: u32,
}

/// The warnings, and the checks that could not run with the reason.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Verdict {
    pub warnings: Vec<Warning>,
    /// For `convert_verify_skipped_total{check,reason}`.
    pub skipped: Vec<(Check, &'static str)>,
}

/// Applies the warning rules. Pure: no I/O, never fails.
pub fn verify(input: &VerifyInput<'_>) -> Verdict {
    let mut verdict = Verdict::default();
    if let Some(warning) = render_clamped(input) {
        verdict.warnings.push(warning);
    }
    verdict
}

/// `RENDER_CLAMPED`: a server limit reduced the requested resolution.
/// `MAX_DPI` lowers the DPI of every page; `MAX_EDGE_PX` lowers the DPI of
/// a page that would be larger. A `max_edge` the caller chose is not a
/// server limit, so the caller's own clamp raises nothing. One warning per
/// request, with the lowest effective DPI.
fn render_clamped(input: &VerifyInput<'_>) -> Option<Warning> {
    let request = input.request;
    if request.output == Output::Pdf || input.rendered.is_empty() {
        return None;
    }
    let dpi = f32::from(request.images.dpi);
    let requested = request.requested_dpi;
    let lowest = input
        .rendered
        .iter()
        .map(|page| page.effective_dpi)
        .fold(dpi, f32::min);
    let server_edge = request
        .requested_max_edge
        .is_none_or(|edge| edge > input.max_edge_px);
    let effective = if server_edge { lowest } else { dpi };
    if effective >= f32::from(requested) {
        return None;
    }
    Some(Warning {
        code: WarningCode::RenderClamped,
        message: format!(
            "the requested {requested} DPI was reduced to {} DPI by a server limit",
            round(effective)
        ),
        page: None,
        detail: vec![
            ("requested_dpi", DetailValue::Int(i64::from(requested))),
            ("effective_dpi", number(effective)),
        ],
    })
}

/// A DPI as an integer when it is one, else rounded to two decimals.
fn number(value: f32) -> DetailValue {
    let value = f64::from(value);
    if value.fract() == 0.0 {
        DetailValue::Int(value as i64)
    } else {
        DetailValue::Float((value * 100.0).round() / 100.0)
    }
}

fn round(value: f32) -> String {
    match number(value) {
        DetailValue::Int(n) => n.to_string(),
        DetailValue::Float(f) => f.to_string(),
        DetailValue::Str(s) => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::conversion::RawOptions;
    use crate::domain::limits::test_limits;

    fn request(dpi: u32, max_edge: Option<u32>) -> ConversionRequest {
        let raw = RawOptions {
            output: Some(Output::Images),
            dpi: Some(dpi),
            max_edge,
            ..Default::default()
        };
        ConversionRequest::new(raw, &test_limits()).unwrap()
    }

    fn page(number: u32, effective_dpi: f32) -> RenderedPage {
        RenderedPage {
            number,
            file_name: format!("{number:04}.jpg"),
            width_px: 10,
            height_px: 10,
            width_pt: 595.0,
            height_pt: 842.0,
            size_bytes: 1,
            objects: 1,
            flat: false,
            colour: [0, 0, 0],
            effective_dpi,
            crc32: None,
        }
    }

    fn warnings(request: &ConversionRequest, rendered: &[RenderedPage]) -> Vec<Warning> {
        verify(&VerifyInput {
            format: Format::Pdf,
            request,
            facts: None,
            page_count: rendered.len() as u32,
            probe: &[],
            rendered,
            max_edge_px: 4096,
        })
        .warnings
    }

    #[test]
    fn max_dpi_raises_render_clamped_once() {
        let request = request(600, None);
        let got = warnings(&request, &[page(1, 300.0), page(2, 300.0)]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].code, WarningCode::RenderClamped);
        assert_eq!(
            got[0].detail,
            [
                ("requested_dpi", DetailValue::Int(600)),
                ("effective_dpi", DetailValue::Int(300))
            ]
        );
    }

    #[test]
    fn max_edge_px_raises_it_with_the_lowest_page() {
        let request = request(300, None);
        let got = warnings(&request, &[page(1, 300.0), page(2, 212.5)]);
        assert_eq!(
            got[0].detail[1],
            ("effective_dpi", DetailValue::Float(212.5))
        );
    }

    #[test]
    fn a_callers_own_clamp_and_no_clamp_raise_nothing() {
        assert!(warnings(&request(150, Some(320)), &[page(1, 40.0)]).is_empty());
        assert!(warnings(&request(150, None), &[page(1, 150.0)]).is_empty());
        let mut pdf = request(600, None);
        pdf.output = Output::Pdf;
        assert!(warnings(&pdf, &[]).is_empty());
    }
}
