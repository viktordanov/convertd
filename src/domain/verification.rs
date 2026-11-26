//! Warnings: quality signals that never fail a request.
//!
//! The rules compare what the source declares (`SourceFacts`, from the
//! office worker's pre-flight, INTERFACES §13) with what the engines
//! produced. The algorithms are 005 §5.

use std::collections::{HashMap, HashSet};

use super::conversion::{ConversionRequest, Output, SlideNotes};
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
    GlyphsMissing,
}

impl WarningCode {
    pub fn as_str(self) -> &'static str {
        match self {
            WarningCode::FontSubstituted => "FONT_SUBSTITUTED",
            WarningCode::PageCountMismatch => "PAGE_COUNT_MISMATCH",
            WarningCode::BlankPage => "BLANK_PAGE",
            WarningCode::MacrosIgnored => "MACROS_IGNORED",
            WarningCode::RenderClamped => "RENDER_CLAMPED",
            WarningCode::GlyphsMissing => "GLYPHS_MISSING",
        }
    }
}

/// One value in a warning's `detail` object.
#[derive(Debug, Clone, PartialEq)]
pub enum DetailValue {
    Str(String),
    Int(i64),
    Float(f64),
    /// A JSON array of strings.
    List(Vec<String>),
    /// A JSON object of counts, in this order.
    Counts(Vec<(String, i64)>),
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
    /// `GLYPHS_MISSING`: the text code points.
    Glyphs,
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
    /// The requested families whose font file the source embeds, which
    /// LibreOffice uses instead of a substitute (the DOCX font table that the
    /// main part links). Sorted, without duplicates, a subset of
    /// `requested_fonts` up to case and white space.
    pub embedded_fonts: Vec<String>,
    /// The distinct code points of the document's text that a font must
    /// draw: no controls, white space, default-ignorable or private-use
    /// characters ([`super::text::needs_glyph`]). Sorted, at most
    /// [`MAX_CODE_POINTS`].
    pub text_code_points: Vec<u32>,
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
    /// The installed families and their substitutes.
    pub fonts: &'a FontCatalog,
}

/// The warnings, and the checks that could not run with the reason.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Verdict {
    pub warnings: Vec<Warning>,
    /// For `convert_verify_skipped_total{check,reason}`.
    pub skipped: Vec<(Check, &'static str)>,
}

/// At most this many distinct text code points cross from pre-flight.
pub const MAX_CODE_POINTS: usize = 4096;

/// At most this many code points are named in a `GLYPHS_MISSING` detail.
pub const LISTED_CODE_POINTS: usize = 8;

/// At most this many `FONT_SUBSTITUTED` warnings per request.
pub const MAX_FONT_WARNINGS: usize = 32;

/// Applies the warning rules. Pure: no I/O, never fails. The order is
/// `RENDER_CLAMPED`, `PAGE_COUNT_MISMATCH`, `FONT_SUBSTITUTED` (sorted by
/// family), `GLYPHS_MISSING`, `BLANK_PAGE` (by page), `MACROS_IGNORED`.
pub fn verify(input: &VerifyInput<'_>) -> Verdict {
    let mut verdict = Verdict::default();
    if let Some(warning) = render_clamped(input) {
        verdict.warnings.push(warning);
    }
    match input.facts {
        Some(facts) => {
            for (check, reason) in &facts.skipped {
                verdict.skipped.push((*check, reason.as_str()));
            }
            page_count(input, facts, &mut verdict);
            fonts(input, facts, &mut verdict);
            glyphs(input, facts, &mut verdict);
        }
        None if input.format.family().is_office() => {
            // The office engine ran no pre-flight.
            for check in [Check::PageCount, Check::Fonts, Check::Glyphs, Check::Macros] {
                verdict.skipped.push((check, "no_facts"));
            }
        }
        None => {}
    }
    blank_pages(input, &mut verdict);
    if let Some(fact) = input.facts.and_then(|f| f.macros.as_ref()) {
        verdict.warnings.push(Warning {
            code: WarningCode::MacrosIgnored,
            message: "the document contains macros; they were not run".to_owned(),
            page: None,
            detail: vec![
                ("kind", DetailValue::Str(fact.kind.as_str().to_owned())),
                ("part", DetailValue::Str(fact.part.clone())),
            ],
        });
    }
    verdict
}

/// V1 · `PAGE_COUNT_MISMATCH`, PPTX family only. The declared count is
/// the visible slides, so the check is skipped when hidden slides or notes
/// pages are included, and for a declared count of 0 (a template).
fn page_count(input: &VerifyInput<'_>, facts: &SourceFacts, verdict: &mut Verdict) {
    if !matches!(
        input.format,
        Format::Pptx | Format::Pptm | Format::Ppsx | Format::Potx
    ) {
        return;
    }
    let Some(declared) = &facts.declared_pages else {
        return;
    };
    let export = input.request.export;
    let skip = if declared.count == 0 {
        Some("no_declared_count")
    } else if export.include_hidden_slides {
        Some("include_hidden")
    } else if export.notes != SlideNotes::None {
        Some("notes")
    } else {
        None
    };
    if let Some(reason) = skip {
        verdict.skipped.push((Check::PageCount, reason));
        return;
    }
    if declared.count != input.page_count {
        verdict.warnings.push(Warning {
            code: WarningCode::PageCountMismatch,
            message: format!(
                "the source declares {} pages and the PDF has {}",
                declared.count, input.page_count
            ),
            page: None,
            detail: vec![
                ("expected", DetailValue::Int(i64::from(declared.count))),
                ("actual", DetailValue::Int(i64::from(input.page_count))),
                ("source", DetailValue::Str(declared.source.to_owned())),
            ],
        });
    }
}

/// V2 · `FONT_SUBSTITUTED`. A requested family that is not installed
/// warns, unless the source embeds it, or it is metric-compatible and its
/// substitute is installed. Without a font catalog (no `build-info.json`)
/// the check is skipped.
fn fonts(input: &VerifyInput<'_>, facts: &SourceFacts, verdict: &mut Verdict) {
    if facts.requested_fonts.is_empty() {
        return;
    }
    let catalog = input.fonts;
    if catalog.is_empty() {
        verdict.skipped.push((Check::Fonts, "no_font_catalog"));
        return;
    }
    let embedded: HashSet<String> = facts.embedded_fonts.iter().map(|f| fold(f)).collect();
    let mut requested: Vec<&String> = facts.requested_fonts.iter().collect();
    requested.sort();
    requested.dedup();
    let mut raised = 0;
    for family in requested {
        if catalog.is_installed(family) || embedded.contains(&fold(family)) {
            continue;
        }
        let (substitute, source) = catalog.substitute(family);
        if catalog.is_metric_compatible(family) && catalog.is_installed(&substitute) {
            continue;
        }
        if raised == MAX_FONT_WARNINGS {
            break;
        }
        raised += 1;
        verdict.warnings.push(Warning {
            code: WarningCode::FontSubstituted,
            message: format!("{family} is not installed and was replaced by {substitute}"),
            page: None,
            detail: vec![
                ("requested", DetailValue::Str(family.clone())),
                ("substitute", DetailValue::Str(substitute)),
                ("substitute_source", DetailValue::Str(source.to_owned())),
            ],
        });
    }
}

/// `GLYPHS_MISSING`: text code points that no installed face covers
/// (`coverage` in `build-info.json`, the union of the faces' charsets).
/// LibreOffice then draws boxes or nothing. One warning with the count,
/// the first [`LISTED_CODE_POINTS`] code points and a count per script.
/// Without a coverage (outside the image) the check is skipped.
fn glyphs(input: &VerifyInput<'_>, facts: &SourceFacts, verdict: &mut Verdict) {
    if facts.text_code_points.is_empty() {
        return;
    }
    let catalog = input.fonts;
    if !catalog.has_coverage() {
        verdict.skipped.push((Check::Glyphs, "no_coverage"));
        return;
    }
    let mut missing: Vec<u32> = facts
        .text_code_points
        .iter()
        .copied()
        .filter(|&cp| !catalog.covers(cp))
        .collect();
    if missing.is_empty() {
        return;
    }
    missing.sort_unstable();
    missing.dedup();
    let mut scripts: Vec<(String, i64)> = Vec::new();
    for &cp in &missing {
        let name = super::text::script_name(cp);
        match scripts.iter_mut().find(|(script, _)| script == name) {
            Some((_, count)) => *count += 1,
            None => scripts.push((name.to_owned(), 1)),
        }
    }
    scripts.sort();
    let summary: Vec<String> = scripts
        .iter()
        .map(|(script, count)| format!("{script} {count}"))
        .collect();
    let count = missing.len();
    let noun = if count == 1 {
        "character has"
    } else {
        "characters have"
    };
    verdict.warnings.push(Warning {
        code: WarningCode::GlyphsMissing,
        message: format!(
            "{count} {noun} no font in this image: {}",
            summary.join(", ")
        ),
        page: None,
        detail: vec![
            (
                "missing",
                DetailValue::Int(i64::try_from(count).unwrap_or(i64::MAX)),
            ),
            (
                "code_points",
                DetailValue::List(
                    missing
                        .iter()
                        .take(LISTED_CODE_POINTS)
                        .map(|cp| format!("U+{cp:04X}"))
                        .collect(),
                ),
            ),
            ("scripts", DetailValue::Counts(scripts)),
        ],
    });
}

/// V3 · `BLANK_PAGE`: a page with content (`objects > 0`) that rendered as
/// one flat colour. The rendered pages decide where they exist; the probe
/// render covers the rest.
fn blank_pages(input: &VerifyInput<'_>, verdict: &mut Verdict) {
    let rendered: HashSet<u32> = input.rendered.iter().map(|p| p.number).collect();
    let mut blank: Vec<(u32, [u8; 3])> = input
        .rendered
        .iter()
        .filter(|p| p.objects > 0 && p.flat)
        .map(|p| (p.number, p.colour))
        .chain(
            input
                .probe
                .iter()
                .filter(|p| !rendered.contains(&p.number) && p.objects > 0 && p.flat)
                .map(|p| (p.number, p.colour)),
        )
        .collect();
    blank.sort_by_key(|(number, _)| *number);
    for (number, [r, g, b]) in blank {
        verdict.warnings.push(Warning {
            code: WarningCode::BlankPage,
            message: format!("page {number} has content but rendered as one flat colour"),
            page: Some(number),
            detail: vec![(
                "colour",
                DetailValue::Str(format!("#{r:02x}{g:02x}{b:02x}")),
            )],
        });
    }
}

/// The families the deployment has, from `build-info.json` (`fonts`,
/// `font_substitutes`, `default_substitute`) and `fonts.json`
/// (`{family: metric_compatible}`). Names compare trimmed, case-folded and
/// with runs of spaces collapsed.
#[derive(Debug, Clone, Default)]
pub struct FontCatalog {
    installed: HashSet<String>,
    substitutes: HashMap<String, String>,
    default_substitute: Option<String>,
    metric_compatible: HashMap<String, bool>,
    /// Sorted, merged, inclusive code point ranges that some face covers.
    coverage: Vec<(u32, u32)>,
}

impl FontCatalog {
    /// `installed` entries may list localized names separated by `,`, as
    /// `fc-list` prints them.
    pub fn new(
        installed: impl IntoIterator<Item = String>,
        substitutes: impl IntoIterator<Item = (String, String)>,
        default_substitute: Option<String>,
        metric_compatible: impl IntoIterator<Item = (String, bool)>,
    ) -> Self {
        Self {
            installed: installed
                .into_iter()
                .flat_map(|entry| entry.split(',').map(fold).collect::<Vec<_>>())
                .filter(|name| !name.is_empty())
                .collect(),
            substitutes: substitutes
                .into_iter()
                .map(|(requested, substitute)| (fold(&requested), substitute))
                .collect(),
            default_substitute: default_substitute.filter(|s| !s.trim().is_empty()),
            coverage: Vec::new(),
            metric_compatible: metric_compatible
                .into_iter()
                .map(|(family, compatible)| (fold(&family), compatible))
                .collect(),
        }
    }

    /// No installed families known: V2 cannot run.
    pub fn is_empty(&self) -> bool {
        self.installed.is_empty()
    }

    /// The catalog with the code point ranges that the faces cover
    /// (`coverage` in `build-info.json`). They are sorted and merged here,
    /// so their order and overlaps do not matter; a range with its end
    /// before its start is dropped.
    pub fn with_coverage(mut self, ranges: impl IntoIterator<Item = (u32, u32)>) -> Self {
        let mut ranges: Vec<(u32, u32)> = ranges.into_iter().filter(|(a, b)| a <= b).collect();
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some((_, last)) if start <= last.saturating_add(1) => *last = (*last).max(end),
                _ => merged.push((start, end)),
            }
        }
        self.coverage = merged;
        self
    }

    /// Whether a coverage is known: without one `GLYPHS_MISSING` cannot run.
    pub fn has_coverage(&self) -> bool {
        !self.coverage.is_empty()
    }

    /// Whether some face covers `cp`.
    pub fn covers(&self, cp: u32) -> bool {
        let at = self.coverage.partition_point(|&(_, end)| end < cp);
        self.coverage.get(at).is_some_and(|&(start, _)| start <= cp)
    }

    pub fn is_installed(&self, family: &str) -> bool {
        self.installed.contains(&fold(family))
    }

    /// The substitute fontconfig picks, and where the answer comes from:
    /// `fc-match` for a probed family, `default` otherwise.
    pub fn substitute(&self, family: &str) -> (String, &'static str) {
        match self.substitutes.get(&fold(family)) {
            Some(substitute) => (substitute.clone(), "fc-match"),
            None => (
                self.default_substitute
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned()),
                "default",
            ),
        }
    }

    pub fn is_metric_compatible(&self, family: &str) -> bool {
        self.metric_compatible
            .get(&fold(family))
            .copied()
            .unwrap_or(false)
    }
}

/// Trimmed, case-folded, runs of whitespace collapsed to one space.
fn fold(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl MacroKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MacroKind::Vba => "vba",
            MacroKind::Xlm => "xlm",
            MacroKind::Basic => "basic",
            MacroKind::Script => "script",
        }
    }
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::CapReached => "cap_reached",
            SkipReason::MissingPart => "missing_part",
            SkipReason::MalformedXml => "malformed_xml",
        }
    }
}

impl Check {
    pub fn as_str(self) -> &'static str {
        match self {
            Check::PageCount => "page_count",
            Check::Fonts => "fonts",
            Check::Macros => "macros",
            Check::Glyphs => "glyphs",
        }
    }
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
        DetailValue::List(_) | DetailValue::Counts(_) => String::new(),
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
            fonts: &FontCatalog::default(),
        })
        .warnings
    }

    fn pdf_request() -> ConversionRequest {
        ConversionRequest::new(RawOptions::default(), &test_limits()).unwrap()
    }

    struct Case<'a> {
        format: Format,
        request: ConversionRequest,
        facts: Option<SourceFacts>,
        page_count: u32,
        probe: Vec<ProbedPage>,
        rendered: Vec<RenderedPage>,
        fonts: &'a FontCatalog,
    }

    impl Case<'_> {
        fn verdict(&self) -> Verdict {
            verify(&VerifyInput {
                format: self.format,
                request: &self.request,
                facts: self.facts.as_ref(),
                page_count: self.page_count,
                probe: &self.probe,
                rendered: &self.rendered,
                max_edge_px: 4096,
                fonts: self.fonts,
            })
        }
    }

    fn case(format: Format, facts: SourceFacts, page_count: u32, fonts: &FontCatalog) -> Case<'_> {
        Case {
            format,
            request: pdf_request(),
            facts: Some(facts),
            page_count,
            probe: Vec::new(),
            rendered: Vec::new(),
            fonts,
        }
    }

    fn declared(count: u32) -> SourceFacts {
        SourceFacts {
            declared_pages: Some(DeclaredPages {
                count,
                source: "ppt/presentation.xml",
            }),
            ..Default::default()
        }
    }

    fn catalog() -> FontCatalog {
        FontCatalog::new(
            [
                "Carlito".into(),
                "Caladea".into(),
                "DejaVu Sans,DejaVu Sans Condensed".into(),
            ],
            [
                ("Calibri".into(), "Carlito".into()),
                ("Aptos".into(), "DejaVu Sans".into()),
            ],
            Some("DejaVu Sans".into()),
            [("Calibri".into(), true), ("Aptos".into(), false)],
        )
    }

    #[test]
    fn v1_page_count_mismatch_for_pptx() {
        let fonts = FontCatalog::default();
        let verdict = case(Format::Pptx, declared(4), 3, &fonts).verdict();
        assert_eq!(verdict.warnings.len(), 1);
        let warning = &verdict.warnings[0];
        assert_eq!(warning.code, WarningCode::PageCountMismatch);
        assert_eq!(
            warning.detail,
            [
                ("expected", DetailValue::Int(4)),
                ("actual", DetailValue::Int(3)),
                ("source", DetailValue::Str("ppt/presentation.xml".into()))
            ]
        );
        assert!(
            case(Format::Pptx, declared(3), 3, &fonts)
                .verdict()
                .warnings
                .is_empty()
        );
        // DOCX is off even with a declared count.
        assert!(
            case(Format::Docx, declared(4), 3, &fonts)
                .verdict()
                .warnings
                .is_empty()
        );
    }

    /// The corpus can no longer prove V1 on real engines (LibreOffice
    /// renders a slide whose root is not `p:sld` as a blank page), so these
    /// cases stand in for it: every deck format, fewer and more pages.
    #[test]
    fn v1_fires_for_every_deck_format_in_both_directions() {
        let fonts = FontCatalog::default();
        for format in [Format::Pptx, Format::Pptm, Format::Ppsx, Format::Potx] {
            for (declared_count, produced) in [(5, 2), (2, 5)] {
                let verdict = case(format, declared(declared_count), produced, &fonts).verdict();
                let codes: Vec<WarningCode> = verdict.warnings.iter().map(|w| w.code).collect();
                assert_eq!(
                    codes,
                    [WarningCode::PageCountMismatch],
                    "{format:?}: {declared_count} declared, {produced} produced"
                );
                assert_eq!(
                    verdict.warnings[0].detail[..2],
                    [
                        ("expected", DetailValue::Int(i64::from(declared_count))),
                        ("actual", DetailValue::Int(i64::from(produced))),
                    ]
                );
                assert!(verdict.skipped.is_empty(), "{format:?}");
            }
        }
    }

    #[test]
    fn v1_skips() {
        let fonts = FontCatalog::default();
        let template = case(Format::Potx, declared(0), 1, &fonts).verdict();
        assert!(template.warnings.is_empty());
        assert!(
            template
                .skipped
                .contains(&(Check::PageCount, "no_declared_count"))
        );

        let mut hidden = case(Format::Pptx, declared(3), 5, &fonts);
        hidden.request.export.include_hidden_slides = true;
        let verdict = hidden.verdict();
        assert!(verdict.warnings.is_empty());
        assert!(
            verdict
                .skipped
                .contains(&(Check::PageCount, "include_hidden"))
        );

        let mut notes = case(Format::Pptx, declared(3), 6, &fonts);
        notes.request.export.notes = SlideNotes::Append;
        assert!(
            notes
                .verdict()
                .skipped
                .contains(&(Check::PageCount, "notes"))
        );
    }

    #[test]
    fn v2_font_substituted() {
        let fonts = catalog();
        let facts = SourceFacts {
            requested_fonts: vec![
                "Zzyzx".into(),
                "Aptos".into(),
                "Calibri".into(),
                "carlito".into(),
                "DejaVu  Sans Condensed".into(),
            ],
            ..Default::default()
        };
        let verdict = case(Format::Docx, facts, 1, &fonts).verdict();
        let got: Vec<(String, &Vec<(&str, DetailValue)>)> = verdict
            .warnings
            .iter()
            .map(|w| (w.message.clone(), &w.detail))
            .collect();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(
            got[0].0,
            "Aptos is not installed and was replaced by DejaVu Sans"
        );
        assert_eq!(
            *got[0].1,
            [
                ("requested", DetailValue::Str("Aptos".into())),
                ("substitute", DetailValue::Str("DejaVu Sans".into())),
                ("substitute_source", DetailValue::Str("fc-match".into()))
            ]
        );
        assert_eq!(
            got[1].1[2],
            ("substitute_source", DetailValue::Str("default".into()))
        );
        assert_eq!(got[1].1[0], ("requested", DetailValue::Str("Zzyzx".into())));
    }

    #[test]
    fn v2_skips_a_family_the_document_embeds() {
        // F1: LibreOffice uses the font that the DOCX embeds, so nothing is
        // substituted. Names compare as the catalog compares them.
        let fonts = catalog();
        let facts = SourceFacts {
            requested_fonts: vec!["Aptos".into(), "Zzyzx".into()],
            embedded_fonts: vec!["aptos".into()],
            ..Default::default()
        };
        let verdict = case(Format::Docx, facts, 1, &fonts).verdict();
        let requested: Vec<&DetailValue> =
            verdict.warnings.iter().map(|w| &w.detail[0].1).collect();
        assert_eq!(requested, [&DetailValue::Str("Zzyzx".into())]);
    }

    /// The catalog with a coverage of Basic Latin and Latin-1 only.
    fn latin_coverage() -> FontCatalog {
        catalog().with_coverage(vec![(0x20, 0x7e), (0xa0, 0xff)])
    }

    #[test]
    fn glyphs_missing_counts_per_script() {
        // F2: Thai and Ethiopic letters that no installed face covers; the
        // Latin text is covered.
        let fonts = latin_coverage();
        let facts = SourceFacts {
            text_code_points: vec![0x41, 0x42, 0x1200, 0x1201, 0x0e01, 0x0e02, 0x0e03],
            ..Default::default()
        };
        let verdict = case(Format::Docx, facts, 1, &fonts).verdict();
        let missing: Vec<&Warning> = verdict
            .warnings
            .iter()
            .filter(|w| w.code == WarningCode::GlyphsMissing)
            .collect();
        assert_eq!(missing.len(), 1, "{:?}", verdict.warnings);
        let w = missing[0];
        assert_eq!(w.code.as_str(), "GLYPHS_MISSING");
        assert_eq!(w.page, None);
        assert_eq!(
            w.detail,
            vec![
                ("missing", DetailValue::Int(5)),
                (
                    "code_points",
                    DetailValue::List(
                        ["U+0E01", "U+0E02", "U+0E03", "U+1200", "U+1201"]
                            .map(String::from)
                            .to_vec()
                    )
                ),
                (
                    "scripts",
                    DetailValue::Counts(vec![("Ethiopic".into(), 2), ("Thai".into(), 3)])
                ),
            ]
        );
        assert_eq!(
            w.message,
            "5 characters have no font in this image: Ethiopic 2, Thai 3"
        );
    }

    #[test]
    fn glyphs_missing_lists_at_most_eight_code_points() {
        let fonts = latin_coverage();
        let facts = SourceFacts {
            text_code_points: (0x0e01..0x0e11).collect(),
            ..Default::default()
        };
        let verdict = case(Format::Docx, facts, 1, &fonts).verdict();
        let w = &verdict.warnings[0];
        assert_eq!(w.detail[0], ("missing", DetailValue::Int(16)));
        let DetailValue::List(listed) = &w.detail[1].1 else {
            panic!("{:?}", w.detail);
        };
        assert_eq!(listed.len(), 8);
        assert_eq!(listed[0], "U+0E01");
    }

    #[test]
    fn glyphs_missing_for_one_character_and_despite_an_embedded_font() {
        // The image's coverage decides: an embedded font may cover the
        // character, but its cmap is not read, so the warning stays (a false
        // warning is better than silent boxes).
        let fonts = latin_coverage();
        let facts = SourceFacts {
            requested_fonts: vec!["Noto Sans Thai".into()],
            embedded_fonts: vec!["Noto Sans Thai".into()],
            text_code_points: vec![0x41, 0x0e01],
            ..Default::default()
        };
        let verdict = case(Format::Docx, facts, 1, &fonts).verdict();
        let w: Vec<&Warning> = verdict
            .warnings
            .iter()
            .filter(|w| w.code == WarningCode::GlyphsMissing)
            .collect();
        assert_eq!(w.len(), 1, "{:?}", verdict.warnings);
        assert_eq!(
            w[0].message,
            "1 character has no font in this image: Thai 1"
        );
    }

    #[test]
    fn glyphs_covered_text_raises_nothing() {
        let fonts = latin_coverage();
        let facts = SourceFacts {
            text_code_points: vec![0x41, 0xe9],
            ..Default::default()
        };
        let verdict = case(Format::Docx, facts, 1, &fonts).verdict();
        assert!(verdict.warnings.is_empty(), "{:?}", verdict.warnings);
        assert!(verdict.skipped.is_empty(), "{:?}", verdict.skipped);
    }

    #[test]
    fn glyphs_without_coverage_is_skipped() {
        // Outside the image build-info.json has no "coverage".
        let fonts = catalog();
        let facts = SourceFacts {
            text_code_points: vec![0x0e01],
            ..Default::default()
        };
        let verdict = case(Format::Docx, facts, 1, &fonts).verdict();
        assert!(verdict.warnings.is_empty());
        assert_eq!(verdict.skipped, [(Check::Glyphs, "no_coverage")]);
    }

    #[test]
    fn v2_without_a_catalog_is_skipped() {
        let fonts = FontCatalog::default();
        let facts = SourceFacts {
            requested_fonts: vec!["Aptos".into()],
            ..Default::default()
        };
        let verdict = case(Format::Docx, facts, 1, &fonts).verdict();
        assert!(verdict.warnings.is_empty());
        assert_eq!(verdict.skipped, [(Check::Fonts, "no_font_catalog")]);
    }

    #[test]
    fn v2_caps_the_warnings() {
        let fonts = catalog();
        let facts = SourceFacts {
            requested_fonts: (0..40).map(|i| format!("Missing {i:02}")).collect(),
            ..Default::default()
        };
        assert_eq!(
            case(Format::Docx, facts, 1, &fonts)
                .verdict()
                .warnings
                .len(),
            MAX_FONT_WARNINGS
        );
    }

    #[test]
    fn v3_blank_page_needs_content() {
        let fonts = FontCatalog::default();
        let probe = |number, objects, flat| ProbedPage {
            number,
            objects,
            flat,
            colour: [255, 255, 255],
        };
        let mut pdf = case(Format::Pdf, SourceFacts::default(), 3, &fonts);
        pdf.facts = None;
        pdf.probe = vec![probe(1, 1, true), probe(2, 0, true), probe(3, 4, false)];
        let verdict = pdf.verdict();
        assert_eq!(verdict.warnings.len(), 1);
        assert_eq!(verdict.warnings[0].code, WarningCode::BlankPage);
        assert_eq!(verdict.warnings[0].page, Some(1));
        assert_eq!(
            verdict.warnings[0].detail,
            [("colour", DetailValue::Str("#ffffff".into()))]
        );
        // A rendered page decides over its probe.
        let mut page1 = page(1, 150.0);
        page1.flat = false;
        pdf.rendered = vec![page1];
        assert!(pdf.verdict().warnings.is_empty());
    }

    /// With a page selection and page images the service sends no probe:
    /// a rendered page carries its own object count and flatness, so V3
    /// still fires for it with an empty probe (review, lane 001 item 9).
    #[test]
    fn v3_needs_no_probe_for_rendered_pages() {
        let fonts = FontCatalog::default();
        let mut pdf = case(Format::Pdf, SourceFacts::default(), 10, &fonts);
        pdf.facts = None;
        pdf.probe = Vec::new();
        let mut flat = page(4, 150.0);
        flat.objects = 3;
        flat.flat = true;
        pdf.rendered = vec![flat];
        let verdict = pdf.verdict();
        assert_eq!(verdict.warnings.len(), 1);
        assert_eq!(verdict.warnings[0].code, WarningCode::BlankPage);
        assert_eq!(verdict.warnings[0].page, Some(4));
    }

    #[test]
    fn v4_macros_ignored_and_missing_facts() {
        let fonts = FontCatalog::default();
        let facts = SourceFacts {
            macros: Some(MacroFact {
                kind: MacroKind::Basic,
                part: "Basic/Standard/Module1.xml".into(),
            }),
            skipped: vec![(Check::Fonts, SkipReason::CapReached)],
            ..Default::default()
        };
        let verdict = case(Format::Odt, facts, 1, &fonts).verdict();
        assert_eq!(verdict.warnings[0].code, WarningCode::MacrosIgnored);
        assert_eq!(
            verdict.warnings[0].detail,
            [
                ("kind", DetailValue::Str("basic".into())),
                (
                    "part",
                    DetailValue::Str("Basic/Standard/Module1.xml".into())
                )
            ]
        );
        assert_eq!(verdict.skipped, [(Check::Fonts, "cap_reached")]);

        let mut none = case(Format::Docx, SourceFacts::default(), 1, &fonts);
        none.facts = None;
        assert_eq!(none.verdict().skipped.len(), 4);
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
