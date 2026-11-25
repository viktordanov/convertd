//! The typed form of `corpus/expected.json` (schema version 1). The JSON
//! Schema in `corpus/expected.schema.json` is checked first; these types
//! carry only what the semantic checks and later subcommands read.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct Expected {
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Deserialize)]
pub struct FileEntry {
    pub id: String,
    pub path: String,
    pub sha256: Option<String>,
    pub preflight: Option<Preflight>,
    pub cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
pub struct Preflight {
    pub requested_fonts: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct Case {
    pub name: String,
    pub expect: Expect,
}

#[derive(Debug, Deserialize)]
pub struct Expect {
    pub page_count: Option<PageCount>,
    #[serde(default)]
    pub warnings: Vec<ExpectedWarning>,
    #[serde(default)]
    pub blank_pages: Vec<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum PageCount {
    Exact(u32),
    Range { min: u32, max: Option<u32> },
    Unrecorded(String),
}

#[derive(Debug, Deserialize)]
pub struct ExpectedWarning {
    pub code: String,
    #[serde(default)]
    pub detail: BTreeMap<String, Value>,
}

/// The detail keys each warning code may carry (`INTERFACES.md` §13, plan 005 §5,
/// `api/openapi.yaml` `WarningCode`).
pub fn detail_keys(code: &str) -> &'static [&'static str] {
    match code {
        "FONT_SUBSTITUTED" => &["requested", "substitute", "substitute_source"],
        "PAGE_COUNT_MISMATCH" => &["expected", "actual", "source"],
        "BLANK_PAGE" => &["colour"],
        "MACROS_IGNORED" => &["kind", "part"],
        "RENDER_CLAMPED" => &["requested_dpi", "effective_dpi"],
        _ => &[],
    }
}
