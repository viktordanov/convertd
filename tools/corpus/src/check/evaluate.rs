//! The comparison of one case's expectation with one observed response.
//! A pure function: the HTTP side builds an [`Observed`], and the unit
//! tests build one by hand.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::check::fonts::font_key;
use crate::expected::{Case, ExpectedWarning, PageCount, WarningsMode};

/// What the harness saw for one request.
#[derive(Debug, Default)]
pub struct Observed {
    pub status: u16,
    pub content_type: Option<String>,
    /// `Convert-Source-Format`.
    pub source_format: Option<String>,
    /// `Convert-Page-Count`.
    pub page_count: Option<u32>,
    /// `Convert-Warning-Count`.
    pub warning_count: Option<u32>,
    /// `code` of a JSON error body.
    pub error_code: Option<String>,
    /// Whether the PDF (the body, or `document.pdf` in a bundle) starts with
    /// `%PDF-` and holds `%%EOF`. `None` when the response has no PDF.
    pub pdf_valid: Option<bool>,
    pub manifest: Option<Manifest>,
    /// The entry names of a tar.zst answer, in archive order.
    pub entries: Option<Vec<String>>,
    /// The `/BaseFont` names in the PDF, when it was read.
    pub base_fonts: Option<BTreeSet<String>>,
    /// A body that could not be read as the content type says.
    pub body_error: Option<String>,
    /// Canary listener connections during the case.
    pub connections: u64,
    /// `GET /healthz` gave 200 after the case.
    pub healthy_after: bool,
    /// No answer within the case's timeout.
    pub timed_out: bool,
}

/// The parts of `manifest.json` that the expectations use (the `Manifest` schema of `api/openapi.json`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Manifest {
    pub page_count: u32,
    pub source: Source,
    pub pdf: Option<PdfInfo>,
    #[serde(default)]
    pub pages: Vec<PageInfo>,
    #[serde(default)]
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Source {
    pub format: String,
    pub family: String,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PdfInfo {
    pub origin: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PageInfo {
    pub number: u32,
    #[serde(default)]
    pub path: String,
    pub width_px: u32,
    pub height_px: u32,
    pub blank: bool,
    /// Absent unless the page comes from a hidden slide; never `false`.
    #[serde(default)]
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Warning {
    pub code: String,
    pub page: Option<u32>,
    #[serde(default)]
    pub detail: Map<String, Value>,
}

/// What the service reports about itself (`/v1/capabilities`).
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub fonts: Vec<String>,
    pub max_edge_px: u32,
}

#[derive(Debug, Default)]
pub struct Verdict {
    /// Each one fails the case.
    pub failures: Vec<String>,
    /// Observed values for review, such as an unrecorded page count.
    pub notes: Vec<String>,
}

impl Verdict {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

pub fn evaluate(case: &Case, obs: &Observed, ctx: &Context) -> Verdict {
    let mut v = Verdict::default();
    invariants(case, obs, ctx, &mut v);
    let expect = &case.expect;
    if obs.timed_out || obs.status == 0 {
        return v;
    }
    if !expect.status.allows(obs.status) {
        v.failures.push(format!(
            "status {}{}, expected {}",
            obs.status,
            obs.error_code
                .as_ref()
                .map(|c| format!(" {c}"))
                .unwrap_or_default(),
            expect.status
        ));
        return v;
    }
    if obs.status != 200 {
        if let (Some(want), Some(got)) = (&expect.code, &obs.error_code)
            && want != got
        {
            v.failures.push(format!("code {got}, expected {want}"));
        }
        return v;
    }
    let manifest = obs.manifest.as_ref();
    let archive = case.output() != "pdf";
    if let Some(want) = &expect.format {
        let got = manifest
            .map(|m| m.source.format.clone())
            .or_else(|| obs.source_format.clone());
        compare(&mut v, "format", got.as_deref(), Some(want.as_str()));
    }
    if archive {
        if let Some(want) = &expect.family {
            compare(
                &mut v,
                "family",
                manifest.map(|m| m.source.family.as_str()),
                Some(want),
            );
        }
        if let Some(want) = expect.encrypted {
            let got = manifest.map(|m| m.source.encrypted);
            if got != Some(want) {
                v.failures
                    .push(format!("encrypted {got:?}, expected {want}"));
            }
        }
        if let Some(want) = &expect.pdf_origin {
            let got = manifest
                .and_then(|m| m.pdf.as_ref())
                .map(|p| p.origin.as_str());
            compare(&mut v, "pdf origin", got, Some(want));
        }
    }
    if let Some(want) = &expect.page_count {
        page_count(&mut v, obs.page_count, want);
    }
    if let Some(want) = expect.warning_count
        && obs.warning_count != Some(want)
    {
        v.failures.push(format!(
            "warning count {:?}, expected {want}",
            obs.warning_count
        ));
    }
    if let (Some(want), true) = (&expect.warnings, archive) {
        match manifest {
            Some(m) => warnings(&mut v, &m.warnings, want, expect.warnings_mode, ctx),
            None => v
                .failures
                .push("no manifest to read the warnings from".into()),
        }
    }
    if let (Some(want), true) = (&expect.blank_pages, archive)
        && let Some(m) = manifest
    {
        let got: BTreeSet<u32> = m
            .pages
            .iter()
            .filter(|p| p.blank)
            .map(|p| p.number)
            .collect();
        let want: BTreeSet<u32> = want.iter().copied().collect();
        if got != want {
            v.failures
                .push(format!("blank pages {got:?}, expected {want:?}"));
        }
    }
    // The service leaves `hidden` out of an unmarked page, in every case.
    if let (true, Some(m)) = (archive, manifest) {
        for page in m.pages.iter().filter(|p| p.hidden == Some(false)) {
            v.failures.push(format!(
                "page {} has hidden: false; an unmarked page has no hidden key",
                page.number
            ));
        }
    }
    if let (Some(want), true) = (&expect.hidden_pages, archive)
        && let Some(m) = manifest
    {
        let got: BTreeSet<u32> = m
            .pages
            .iter()
            .filter(|p| p.hidden == Some(true))
            .map(|p| p.number)
            .collect();
        let want: BTreeSet<u32> = want.iter().copied().collect();
        if got != want {
            v.failures
                .push(format!("hidden pages {got:?}, expected {want:?}"));
        }
    }
    // Only a bundle carries the PDF; `images` has no /BaseFont to read.
    if let (Some(m), "bundle") = (manifest, case.output()) {
        font_arbiter(&mut v, &m.warnings, obs.base_fonts.as_ref());
    }
    v
}

/// The checks of plan 005 section 3 that hold for every case.
fn invariants(case: &Case, obs: &Observed, ctx: &Context, v: &mut Verdict) {
    if obs.status == 0 && !obs.timed_out {
        let why = obs.body_error.as_deref().unwrap_or("no response");
        v.failures.push(format!("no response: {why}"));
    }
    if obs.timed_out {
        v.failures
            .push("no answer within the case's timeout".into());
    }
    if !obs.healthy_after {
        v.failures
            .push("/healthz did not give 200 after the case".into());
    }
    // Every case allows 0 connections; a canary says so explicitly.
    let allowed = case.expect.connections.unwrap_or(0);
    if obs.connections > allowed {
        v.failures.push(format!(
            "the canary listener saw {} connection(s)",
            obs.connections
        ));
    }
    if obs.timed_out || obs.status == 0 {
        return;
    }
    if obs.status != 200 {
        if obs.error_code.is_none() {
            v.failures
                .push(format!("status {} without an error code", obs.status));
        }
        return;
    }
    if let Some(err) = &obs.body_error {
        v.failures.push(format!("unreadable body: {err}"));
    }
    let want_type = if case.output() == "pdf" {
        "application/pdf"
    } else {
        "application/zstd"
    };
    let got_type = obs
        .content_type
        .as_deref()
        .map(|t| t.split(';').next().unwrap_or(t).trim());
    if got_type != Some(want_type) {
        v.failures
            .push(format!("Content-Type {got_type:?}, expected {want_type}"));
    }
    if case.output() != "images" && obs.pdf_valid != Some(true) {
        v.failures
            .push("the PDF does not start with %PDF- or has no %%EOF".into());
    }
    let Some(m) = &obs.manifest else {
        if case.output() != "pdf" && obs.body_error.is_none() {
            v.failures.push("the bundle has no manifest.json".into());
        }
        return;
    };
    if let Some(entries) = &obs.entries {
        let mut want = vec!["manifest.json".to_string()];
        if case.output() == "bundle" {
            want.push("document.pdf".into());
        }
        want.extend(m.pages.iter().map(|p| p.path.clone()));
        if *entries != want {
            v.failures
                .push(format!("bundle entries {entries:?}, expected {want:?}"));
        }
    }
    if obs.page_count != Some(m.page_count) {
        v.failures.push(format!(
            "Convert-Page-Count {:?}, manifest page_count {}",
            obs.page_count, m.page_count
        ));
    }
    if obs.warning_count != Some(m.warnings.len() as u32) {
        v.failures.push(format!(
            "Convert-Warning-Count {:?}, manifest has {} warnings",
            obs.warning_count,
            m.warnings.len()
        ));
    }
    let pages_option = case.options.get("pages").and_then(Value::as_str);
    let requested = requested_pages(pages_option, m.page_count);
    let got: Vec<u32> = m.pages.iter().map(|p| p.number).collect();
    if got != requested {
        v.failures
            .push(format!("pages {got:?}, requested {requested:?}"));
    }
    if let Some(p) = m
        .pages
        .iter()
        .find(|p| p.width_px.max(p.height_px) > ctx.max_edge_px)
    {
        v.failures.push(format!(
            "page {} is {} x {} px, more than MAX_EDGE_PX {}",
            p.number, p.width_px, p.height_px, ctx.max_edge_px
        ));
    }
}

fn compare(v: &mut Verdict, what: &str, got: Option<&str>, want: Option<&str>) {
    if got != want {
        v.failures
            .push(format!("{what} {got:?}, expected {want:?}"));
    }
}

fn page_count(v: &mut Verdict, got: Option<u32>, want: &PageCount) {
    let Some(got) = got else {
        v.failures.push("no Convert-Page-Count".into());
        return;
    };
    match want {
        PageCount::Exact(want) if got != *want => {
            v.failures
                .push(format!("page count {got}, expected {want}"));
        }
        PageCount::Range { min, max } if got < *min || max.is_some_and(|max| got > max) => {
            v.failures.push(format!(
                "page count {got}, expected {min}..{}",
                max.map(|m| m.to_string()).unwrap_or_default()
            ));
        }
        PageCount::Unrecorded(_) => v
            .notes
            .push(format!("observed page count {got} (unrecorded)")),
        _ => {}
    }
}

/// The 1-based pages that `pages` selects from a document of `count` pages;
/// all pages when it is absent. Pages past the end are ignored, as the API says.
pub fn requested_pages(pages: Option<&str>, count: u32) -> Vec<u32> {
    let Some(spec) = pages else {
        return (1..=count).collect();
    };
    let mut set = BTreeSet::new();
    for part in spec.split(',') {
        let (from, to) = match part.split_once('-') {
            Some((from, "")) => (from.parse().unwrap_or(0), count),
            Some((from, to)) => (from.parse().unwrap_or(0), to.parse().unwrap_or(0)),
            None => {
                let one = part.parse().unwrap_or(0);
                (one, one)
            }
        };
        set.extend((from.max(1)..=to.min(count)).filter(|p| *p >= 1));
    }
    set.into_iter().collect()
}

/// Matches the observed warnings against the expected ones: a multiset on
/// `(code, page)`, with `detail` as a subset and `"*"` as any value. A
/// warning whose `unless_installed` families are all installed is dropped.
fn warnings(
    v: &mut Verdict,
    got: &[Warning],
    want: &[ExpectedWarning],
    mode: WarningsMode,
    ctx: &Context,
) {
    let installed: BTreeSet<String> = ctx.fonts.iter().map(|f| font_key(f)).collect();
    let want: Vec<&ExpectedWarning> = want
        .iter()
        .filter(|w| {
            w.unless_installed.is_empty()
                || !w
                    .unless_installed
                    .iter()
                    .all(|f| installed.contains(&font_key(f)))
        })
        .collect();
    let mut unused: Vec<&Warning> = got.iter().collect();
    for expected in &want {
        match unused.iter().position(|g| matches(g, expected)) {
            Some(at) => {
                unused.remove(at);
            }
            None => v.failures.push(format!(
                "missing warning {}{} {}",
                expected.code,
                expected
                    .page
                    .map(|p| format!(" page {p}"))
                    .unwrap_or_default(),
                Value::Object(expected.detail.clone().into_iter().collect())
            )),
        }
    }
    if mode == WarningsMode::Exact {
        for extra in unused {
            v.failures.push(format!(
                "unexpected warning {}{} {}",
                extra.code,
                extra.page.map(|p| format!(" page {p}")).unwrap_or_default(),
                Value::Object(extra.detail.clone())
            ));
        }
    }
}

fn matches(got: &Warning, want: &ExpectedWarning) -> bool {
    got.code == want.code
        && got.page == want.page
        && want
            .detail
            .iter()
            .all(|(key, value)| match got.detail.get(key) {
                Some(_) if value.as_str() == Some("*") => true,
                Some(seen) => seen == value,
                None => false,
            })
}

/// Plan 005 section 5, V2 arbiter: for each FONT_SUBSTITUTED warning, the
/// requested family must be absent from the PDF's `/BaseFont` names and
/// the claimed substitute must be present.
fn font_arbiter(v: &mut Verdict, got: &[Warning], base_fonts: Option<&BTreeSet<String>>) {
    let subs: Vec<&Warning> = got
        .iter()
        .filter(|w| w.code == "FONT_SUBSTITUTED")
        .collect();
    if subs.is_empty() {
        return;
    }
    let Some(names) = base_fonts else {
        v.failures
            .push("FONT_SUBSTITUTED without a PDF to read /BaseFont from".into());
        return;
    };
    let keys: BTreeSet<String> = names.iter().map(|n| font_key(n)).collect();
    for w in subs {
        let field = |k: &str| w.detail.get(k).and_then(Value::as_str).unwrap_or("");
        let (requested, substitute) = (field("requested"), field("substitute"));
        if keys.contains(&font_key(requested)) {
            v.failures.push(format!(
                "/BaseFont has {requested}, which the warning says was substituted; \
                 fix font_substitutes"
            ));
        }
        if !keys.contains(&font_key(substitute)) {
            v.failures.push(format!(
                "/BaseFont has no {substitute}, the claimed substitute for {requested} \
                 (found {names:?}); fix the font_substitutes entry"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expected::{Expect, Status};
    use serde_json::json;

    fn case(options: Value, expect: Value) -> Case {
        serde_json::from_value(json!({"name": "c", "options": options, "expect": expect}))
            .expect("case")
    }

    fn ctx() -> Context {
        Context {
            fonts: vec!["Carlito".into(), "Liberation Serif".into()],
            max_edge_px: 4096,
        }
    }

    fn warning(code: &str, page: Option<u32>, detail: Value) -> Warning {
        Warning {
            code: code.into(),
            page,
            detail: detail.as_object().cloned().unwrap_or_default(),
        }
    }

    /// A healthy bundle answer with `pages` pages and these warnings.
    fn bundle(pages: u32, warnings: Vec<Warning>) -> Observed {
        Observed {
            status: 200,
            content_type: Some("application/zstd".into()),
            source_format: Some("docx".into()),
            page_count: Some(pages),
            warning_count: Some(warnings.len() as u32),
            pdf_valid: Some(true),
            manifest: Some(Manifest {
                page_count: pages,
                source: Source {
                    format: "docx".into(),
                    family: "document".into(),
                    encrypted: false,
                },
                pdf: Some(PdfInfo {
                    origin: "converted".into(),
                }),
                pages: (1..=pages)
                    .map(|number| PageInfo {
                        number,
                        path: format!("pages/{number:04}.png"),
                        width_px: 413,
                        height_px: 584,
                        blank: false,
                        hidden: None,
                    })
                    .collect(),
                warnings,
            }),
            entries: Some(
                ["manifest.json".to_string(), "document.pdf".into()]
                    .into_iter()
                    .chain((1..=pages).map(|n| format!("pages/{n:04}.png")))
                    .collect(),
            ),
            base_fonts: Some(BTreeSet::from(["BAAAAA+Carlito".into()])),
            healthy_after: true,
            ..Observed::default()
        }
    }

    fn failures(case: &Case, obs: &Observed) -> Vec<String> {
        evaluate(case, obs, &ctx()).failures
    }

    #[test]
    fn a_matching_bundle_passes() {
        let c = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "format": "docx", "family": "document", "encrypted": false,
                   "pdf_origin": "converted", "page_count": 2, "warnings": [], "blank_pages": []}),
        );
        assert_eq!(failures(&c, &bundle(2, vec![])), Vec::<String>::new());
    }

    #[test]
    fn the_three_page_count_forms() {
        let obs = bundle(3, vec![]);
        let exact = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "page_count": 4}),
        );
        assert_eq!(failures(&exact, &obs), ["page count 3, expected 4"]);
        let range = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "page_count": {"min": 2, "max": 3}}),
        );
        assert!(failures(&range, &obs).is_empty());
        let low = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "page_count": {"min": 4}}),
        );
        assert_eq!(failures(&low, &obs), ["page count 3, expected 4.."]);
        let unrecorded = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "page_count": "unrecorded"}),
        );
        let verdict = evaluate(&unrecorded, &obs, &ctx());
        assert!(verdict.passed());
        assert_eq!(verdict.notes, ["observed page count 3 (unrecorded)"]);
    }

    #[test]
    fn a_status_array_takes_any_member_and_checks_the_code() {
        let c = case(
            json!({}),
            json!({"status": [200, 422], "code": "CONVERSION_FAILED"}),
        );
        let mut obs = Observed {
            status: 422,
            error_code: Some("CONVERSION_FAILED".into()),
            healthy_after: true,
            ..Observed::default()
        };
        assert!(failures(&c, &obs).is_empty());
        obs.error_code = Some("LIMIT_EXCEEDED".into());
        assert_eq!(
            failures(&c, &obs),
            ["code LIMIT_EXCEEDED, expected CONVERSION_FAILED"]
        );
        obs.status = 415;
        obs.error_code = Some("UNSUPPORTED_FORMAT".into());
        assert_eq!(failures(&c, &obs).len(), 1);
    }

    #[test]
    fn an_error_without_a_code_breaks_the_invariant() {
        let c = case(json!({}), json!({"status": 422, "code": "LIMIT_EXCEEDED"}));
        let obs = Observed {
            status: 422,
            healthy_after: true,
            ..Observed::default()
        };
        assert_eq!(failures(&c, &obs), ["status 422 without an error code"]);
    }

    #[test]
    fn warnings_are_a_multiset_with_detail_as_a_subset() {
        let c = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "warnings": [
                {"code": "BLANK_PAGE", "page": 1},
                {"code": "MACROS_IGNORED", "detail": {"kind": "vba", "part": "*"}}]}),
        );
        let good = vec![
            warning(
                "MACROS_IGNORED",
                None,
                json!({"kind": "vba", "part": "word/vbaProject.bin"}),
            ),
            warning("BLANK_PAGE", Some(1), json!({"colour": "#ffffff"})),
        ];
        assert!(failures(&c, &bundle(1, good)).is_empty());
        let wrong_page = vec![
            warning("MACROS_IGNORED", None, json!({"kind": "vba", "part": "x"})),
            warning("BLANK_PAGE", Some(2), json!({})),
        ];
        let f = failures(&c, &bundle(2, wrong_page));
        assert!(
            f.iter()
                .any(|f| f.starts_with("missing warning BLANK_PAGE page 1")),
            "{f:?}"
        );
        assert!(
            f.iter()
                .any(|f| f.starts_with("unexpected warning BLANK_PAGE page 2")),
            "{f:?}"
        );
        let twice = vec![
            warning("BLANK_PAGE", Some(1), json!({})),
            warning("BLANK_PAGE", Some(1), json!({})),
            warning(
                "MACROS_IGNORED",
                None,
                json!({"kind": "basic", "part": "x"}),
            ),
        ];
        let f = failures(&c, &bundle(1, twice));
        assert!(
            f.iter()
                .any(|f| f.starts_with("missing warning MACROS_IGNORED")),
            "{f:?}"
        );
        assert!(
            f.iter()
                .any(|f| f.starts_with("unexpected warning BLANK_PAGE")),
            "{f:?}"
        );
    }

    #[test]
    fn contains_mode_allows_extra_warnings() {
        let c = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "warnings_mode": "contains",
                   "warnings": [{"code": "BLANK_PAGE", "page": 1}]}),
        );
        let got = vec![
            warning("BLANK_PAGE", Some(1), json!({})),
            warning("RENDER_CLAMPED", None, json!({})),
        ];
        assert!(failures(&c, &bundle(1, got)).is_empty());
    }

    #[test]
    fn unless_installed_drops_the_warning_when_the_font_is_installed() {
        let expect = |family: &str| {
            json!({"status": 200, "warnings": [{"code": "FONT_SUBSTITUTED",
                "detail": {"requested": family, "substitute": "*"}, "unless_installed": [family]}]})
        };
        let installed = case(json!({"output": "bundle"}), expect("Liberation Serif"));
        assert!(failures(&installed, &bundle(1, vec![])).is_empty());
        let missing = case(json!({"output": "bundle"}), expect("Aptos"));
        let f = failures(&missing, &bundle(1, vec![]));
        assert!(
            f[0].starts_with("missing warning FONT_SUBSTITUTED"),
            "{f:?}"
        );
    }

    #[test]
    fn the_font_arbiter_reads_base_fonts() {
        let c = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "warnings": [{"code": "FONT_SUBSTITUTED",
                   "detail": {"requested": "Aptos", "substitute": "*"}}]}),
        );
        let claim = |substitute: &str| {
            vec![warning(
                "FONT_SUBSTITUTED",
                None,
                json!({"requested": "Aptos", "substitute": substitute}),
            )]
        };
        assert!(failures(&c, &bundle(1, claim("Carlito"))).is_empty());
        let f = failures(&c, &bundle(1, claim("DejaVu Sans")));
        assert!(f[0].contains("no DejaVu Sans"), "{f:?}");
        let mut still_there = bundle(1, claim("Carlito"));
        still_there.base_fonts = Some(BTreeSet::from(["Carlito".into(), "CAAAAA+Aptos".into()]));
        let f = failures(&c, &still_there);
        assert!(f[0].contains("/BaseFont has Aptos"), "{f:?}");
    }

    #[test]
    fn the_font_arbiter_skips_images_output() {
        let c = case(json!({"output": "images"}), json!({"status": 200}));
        let mut obs = bundle(
            1,
            vec![warning(
                "FONT_SUBSTITUTED",
                None,
                json!({"requested": "Aptos", "substitute": "Carlito"}),
            )],
        );
        obs.base_fonts = None;
        obs.pdf_valid = None;
        obs.entries = Some(vec!["manifest.json".into(), "pages/0001.png".into()]);
        if let Some(m) = obs.manifest.as_mut() {
            m.pdf = None;
        }
        assert!(failures(&c, &obs).is_empty());
    }

    #[test]
    fn blank_pages_must_match_exactly() {
        let c = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "blank_pages": [2]}),
        );
        let mut obs = bundle(2, vec![]);
        assert_eq!(failures(&c, &obs), ["blank pages {}, expected {2}"]);
        if let Some(m) = obs.manifest.as_mut() {
            m.pages[1].blank = true;
        }
        assert!(failures(&c, &obs).is_empty());
    }

    #[test]
    fn hidden_pages_must_match_exactly() {
        let c = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "hidden_pages": [2]}),
        );
        let mut obs = bundle(3, vec![]);
        assert_eq!(failures(&c, &obs), ["hidden pages {}, expected {2}"]);
        if let Some(m) = obs.manifest.as_mut() {
            m.pages[1].hidden = Some(true);
        }
        assert!(failures(&c, &obs).is_empty());
        // The manifest leaves the key out of an unmarked page.
        if let Some(m) = obs.manifest.as_mut() {
            m.pages[0].hidden = Some(false);
        }
        assert_eq!(
            failures(&c, &obs),
            ["page 1 has hidden: false; an unmarked page has no hidden key"]
        );
        if let Some(m) = obs.manifest.as_mut() {
            m.pages[0].hidden = None;
        }
        // An empty list: no page may carry the mark.
        let none = case(
            json!({"output": "bundle"}),
            json!({"status": 200, "hidden_pages": []}),
        );
        assert_eq!(failures(&none, &obs), ["hidden pages {2}, expected {}"]);
    }

    #[test]
    fn bundle_entries_must_come_in_the_contract_order() {
        let c = case(json!({"output": "bundle"}), json!({"status": 200}));
        let mut obs = bundle(1, vec![]);
        obs.entries = Some(vec![
            "document.pdf".into(),
            "manifest.json".into(),
            "pages/0001.png".into(),
        ]);
        let f = failures(&c, &obs);
        assert!(
            f[0].starts_with("bundle entries [\"document.pdf\""),
            "{f:?}"
        );
    }

    #[test]
    fn headers_must_agree_with_the_manifest() {
        let c = case(json!({"output": "bundle"}), json!({"status": 200}));
        let mut obs = bundle(2, vec![]);
        obs.page_count = Some(3);
        obs.warning_count = Some(1);
        let f = failures(&c, &obs);
        assert!(
            f.contains(&"Convert-Page-Count Some(3), manifest page_count 2".to_string()),
            "{f:?}"
        );
        assert!(
            f.contains(&"Convert-Warning-Count Some(1), manifest has 0 warnings".to_string()),
            "{f:?}"
        );
    }

    #[test]
    fn rendered_pages_follow_the_pages_option_and_the_edge_limit() {
        let c = case(
            json!({"output": "bundle", "pages": "2-"}),
            json!({"status": 200}),
        );
        let mut obs = bundle(3, vec![]);
        let f = failures(&c, &obs);
        assert_eq!(f, ["pages [1, 2, 3], requested [2, 3]"]);
        if let Some(m) = obs.manifest.as_mut() {
            m.pages.remove(0);
            m.pages[0].width_px = 5000;
        }
        obs.entries = None;
        assert_eq!(
            failures(&c, &obs),
            ["page 2 is 5000 x 584 px, more than MAX_EDGE_PX 4096"]
        );
    }

    #[test]
    fn requested_pages_parse_like_the_api() {
        assert_eq!(requested_pages(None, 3), [1, 2, 3]);
        assert_eq!(
            requested_pages(Some("1-5,8,10-"), 11),
            [1, 2, 3, 4, 5, 8, 10, 11]
        );
        assert_eq!(requested_pages(Some("5-9"), 6), [5, 6]);
        assert_eq!(requested_pages(Some("1"), 3), [1]);
    }

    #[test]
    fn a_pdf_case_checks_the_headers_only() {
        let c = case(
            json!({"output": "pdf"}),
            json!({"status": 200, "format": "pptx", "page_count": 20, "warning_count": 0}),
        );
        let obs = Observed {
            status: 200,
            content_type: Some("application/pdf".into()),
            source_format: Some("pptx".into()),
            page_count: Some(1),
            warning_count: Some(0),
            pdf_valid: Some(true),
            healthy_after: true,
            ..Observed::default()
        };
        assert_eq!(failures(&c, &obs), ["page count 1, expected 20"]);
    }

    #[test]
    fn a_connection_or_an_unhealthy_service_fails_any_case() {
        let c = case(
            json!({}),
            json!({"status": 415, "code": "UNSUPPORTED_FORMAT"}),
        );
        let obs = Observed {
            status: 415,
            error_code: Some("UNSUPPORTED_FORMAT".into()),
            connections: 1,
            healthy_after: false,
            ..Observed::default()
        };
        assert_eq!(failures(&c, &obs).len(), 2);
    }

    #[test]
    fn a_wrong_content_type_fails() {
        let c = Case {
            name: "c".into(),
            options: Map::new(),
            expect: Expect {
                status: Status::One(200),
                ..Expect::default()
            },
        };
        let obs = Observed {
            status: 200,
            content_type: Some("application/zstd".into()),
            pdf_valid: Some(true),
            healthy_after: true,
            ..Observed::default()
        };
        assert_eq!(
            failures(&c, &obs),
            ["Content-Type Some(\"application/zstd\"), expected application/pdf"]
        );
    }
}
