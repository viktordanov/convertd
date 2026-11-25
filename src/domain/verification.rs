//! Warnings: quality signals that never fail a request.
//!
//! The rules V1 to V4 (005 §5) compare `SourceFacts` from the office
//! worker's pre-flight with the inspection; they land in step 11
//! (INTERFACES §13). This file holds the types the seam needs now.

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
