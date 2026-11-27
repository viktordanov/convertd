//! The typed form of `corpus/expected.json` (schema version 1). The JSON
//! Schema in `corpus/expected.schema.json` is checked first; these types
//! carry what `validate` and `check` read.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug, Deserialize)]
pub struct Expected {
    /// `host:port` of the canary listener.
    pub listener: String,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Deserialize)]
pub struct FileEntry {
    pub id: String,
    pub path: String,
    pub sha256: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub preflight: Option<Preflight>,
    /// Empty for a fixture, which is an input of the generator.
    #[serde(default)]
    pub cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
pub struct Preflight {
    pub requested_fonts: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct Case {
    pub name: String,
    /// Sent as the `options` part, as is.
    #[serde(default)]
    pub options: Map<String, Value>,
    pub expect: Expect,
}

impl Case {
    /// The `output` option, `pdf` when it is not set.
    pub fn output(&self) -> &str {
        self.options
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("pdf")
    }
}

/// `None` in a field means that the case does not check it.
#[derive(Debug, Default, Deserialize)]
pub struct Expect {
    pub status: Status,
    pub code: Option<String>,
    pub format: Option<String>,
    pub family: Option<String>,
    pub encrypted: Option<bool>,
    pub pdf_origin: Option<String>,
    pub page_count: Option<PageCount>,
    pub warning_count: Option<u32>,
    /// `Some(vec![])` means that the response must carry no warning.
    pub warnings: Option<Vec<ExpectedWarning>>,
    #[serde(default)]
    pub warnings_mode: WarningsMode,
    pub blank_pages: Option<Vec<u32>>,
    /// The pages whose manifest entry has `hidden: true`.
    pub hidden_pages: Option<Vec<u32>>,
    pub connections: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Status {
    One(u16),
    Any(Vec<u16>),
}

impl Default for Status {
    fn default() -> Self {
        Status::One(200)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::One(one) => write!(f, "{one}"),
            Status::Any(any) => {
                let all: Vec<String> = any.iter().map(u16::to_string).collect();
                write!(f, "{}", all.join(" or "))
            }
        }
    }
}

impl Status {
    pub fn allows(&self, status: u16) -> bool {
        match self {
            Status::One(one) => *one == status,
            Status::Any(any) => any.contains(&status),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WarningsMode {
    #[default]
    Exact,
    Contains,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum PageCount {
    Exact(u32),
    Range { min: u32, max: Option<u32> },
    Unrecorded(String),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExpectedWarning {
    pub code: String,
    pub page: Option<u32>,
    #[serde(default)]
    pub detail: BTreeMap<String, Value>,
    #[serde(default)]
    pub unless_installed: Vec<String>,
}

/// The detail keys each warning code may carry (`INTERFACES.md` §13, plan 005 §5,
/// the `WarningCode` schema of `api/openapi.json`).
pub fn detail_keys(code: &str) -> &'static [&'static str] {
    match code {
        "FONT_SUBSTITUTED" => &["requested", "substitute", "substitute_source"],
        "PAGE_COUNT_MISMATCH" => &["expected", "actual", "source"],
        "BLANK_PAGE" => &["colour"],
        "MACROS_IGNORED" => &["kind", "part"],
        "RENDER_CLAMPED" => &["requested_dpi", "effective_dpi"],
        "GLYPHS_MISSING" => &["missing", "code_points", "scripts"],
        _ => &[],
    }
}
