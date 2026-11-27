//! `corpus-bench check`: runs every case of `expected.json` against a
//! running service, with the canary listener up, and compares each answer
//! with its expectation.
//!
//! The run is sequential, so a listener connection belongs to the case
//! that was running. After each case the harness asks `/healthz`.

pub mod evaluate;
pub mod fonts;
pub mod listener;
pub mod pdftext;

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use reqwest::multipart::{Form, Part};
use serde::Serialize;
use serde_json::Value;

use crate::expected::{Case, Expected, FileEntry};
use crate::validate::{self, Outcome, Paths};
use evaluate::{Context, Manifest, Observed, Verdict, evaluate};
use listener::Listener;

/// How long after a case the harness waits for a late canary connection.
const LISTENER_GRACE: Duration = Duration::from_millis(200);
/// How long the listener control may take.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// Added to the case's `timeout_seconds` before the harness gives up.
const TIMEOUT_SLACK: Duration = Duration::from_secs(15);

pub struct Args {
    pub paths: Paths,
    pub target: String,
    pub api_key: String,
    pub tags: Vec<String>,
    pub ids: Vec<String>,
    pub report: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct CaseResult {
    pub id: String,
    pub case: String,
    pub tags: Vec<String>,
    pub output: String,
    pub status: u16,
    pub code: Option<String>,
    pub passed: bool,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    pub millis: u128,
}

pub fn run(args: Args) -> ExitCode {
    let mut outcome = Outcome::default();
    let Some(expected) = validate::check_document(&args.paths, &mut outcome) else {
        for problem in &outcome.problems {
            eprintln!("check: {problem}");
        }
        return ExitCode::FAILURE;
    };
    if !outcome.passed() {
        for problem in &outcome.problems {
            eprintln!("check: {problem}");
        }
        return ExitCode::FAILURE;
    }
    let listener = match Listener::start(&expected.listener) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("check: {err}");
            return ExitCode::FAILURE;
        }
    };
    println!("check: canary listener on {}", listener.addr());
    match listener.control(CONTROL_TIMEOUT) {
        Ok(line) => println!("check: listener control passed: 1 connection from {line}"),
        Err(err) => {
            eprintln!("check: {err}");
            return ExitCode::FAILURE;
        }
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("check: failed to start the runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let results = match runtime.block_on(run_cases(&args, &expected, &listener)) {
        Ok(results) => results,
        Err(err) => {
            eprintln!("check: {err}");
            return ExitCode::FAILURE;
        }
    };
    for line in listener.seen().iter().skip(1) {
        eprintln!("check: listener connection from {line}");
    }
    summarize(&results);
    if let Some(path) = &args.report
        && let Err(err) = write_report(path, &results)
    {
        eprintln!("check: {err}");
        return ExitCode::FAILURE;
    }
    exit_code(&results)
}

/// Exit 0 only when every case passed (and at least one ran).
pub fn exit_code(results: &[CaseResult]) -> ExitCode {
    if !results.is_empty() && results.iter().all(|r| r.passed) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn selected<'a>(args: &Args, expected: &'a Expected) -> Vec<&'a FileEntry> {
    expected
        .files
        .iter()
        .filter(|f| !f.cases.is_empty())
        .filter(|f| args.ids.is_empty() || args.ids.contains(&f.id))
        .filter(|f| args.tags.is_empty() || f.tags.iter().any(|t| args.tags.contains(t)))
        .collect()
}

async fn run_cases(
    args: &Args,
    expected: &Expected,
    listener: &Listener,
) -> Result<Vec<CaseResult>, String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|err| format!("failed to build the HTTP client: {err}"))?;
    let target = args.target.trim_end_matches('/');
    let caps = capabilities(&client, target, &args.api_key).await?;
    let mut results = Vec::new();
    // Connections already charged to a case. One running total, so that a
    // connection between cases (during /healthz, say) goes to the next case.
    let mut charged = listener.count();
    for file in selected(args, expected) {
        for case in &file.cases {
            let started = Instant::now();
            let mut obs = request(&client, target, args, file, case, caps.default_timeout).await;
            let millis = started.elapsed().as_millis();
            tokio::time::sleep(LISTENER_GRACE).await;
            let now = listener.count();
            obs.connections = now - charged;
            charged = now;
            obs.healthy_after = healthy(&client, target).await;
            let verdict = evaluate(case, &obs, &caps.context);
            let result = result(file, case, &obs, verdict, millis);
            print_result(&result);
            results.push(result);
        }
    }
    tokio::time::sleep(LISTENER_GRACE).await;
    if let Some(late) = late_connections(listener.count() - charged) {
        print_result(&late);
        results.push(late);
    }
    Ok(results)
}

/// A failed pseudo-case for connections that came after the last case.
fn late_connections(count: u64) -> Option<CaseResult> {
    (count > 0).then(|| CaseResult {
        id: "listener".into(),
        case: "after-the-last-case".into(),
        tags: Vec::new(),
        output: String::new(),
        status: 0,
        code: None,
        passed: false,
        failures: vec![format!(
            "the canary listener saw {count} connection(s) after the last case"
        )],
        notes: Vec::new(),
        millis: 0,
    })
}

struct Capabilities {
    context: Context,
    default_timeout: u64,
}

async fn capabilities(
    client: &reqwest::Client,
    target: &str,
    api_key: &str,
) -> Result<Capabilities, String> {
    let response = client
        .get(format!("{target}/v1/capabilities"))
        .bearer_auth(api_key)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|err| format!("failed to reach {target}/v1/capabilities: {err}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("failed to read /v1/capabilities ({status}): {err}"))?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|err| format!("failed to parse /v1/capabilities ({status}): {err}"))?;
    if !status.is_success() {
        return Err(format!("/v1/capabilities gave {status}: {body}"));
    }
    let limit = |key: &str| body["limits"][key].as_u64();
    Ok(Capabilities {
        context: Context {
            fonts: body["fonts"]
                .as_array()
                .map(|fonts| {
                    fonts
                        .iter()
                        .filter_map(|f| f.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            max_edge_px: limit("max_edge_px")
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(4096),
        },
        default_timeout: limit("default_timeout_seconds").unwrap_or(120),
    })
}

async fn healthy(client: &reqwest::Client, target: &str) -> bool {
    client
        .get(format!("{target}/healthz"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .is_ok_and(|r| r.status().as_u16() == 200)
}

/// Sends one case and reads the answer into an [`Observed`].
async fn request(
    client: &reqwest::Client,
    target: &str,
    args: &Args,
    file: &FileEntry,
    case: &Case,
    default_timeout: u64,
) -> Observed {
    let mut obs = Observed::default();
    let path = args.paths.corpus.join(&file.path);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.id.clone());
    let body = match tokio::fs::File::open(&path).await {
        Ok(handle) => handle,
        Err(err) => {
            obs.body_error = Some(format!("failed to open {}: {err}", path.display()));
            return obs;
        }
    };
    let len = body.metadata().await.map(|m| m.len()).unwrap_or(0);
    let file_part = Part::stream_with_length(reqwest::Body::from(body), len).file_name(name);
    let options = Value::Object(case.options.clone()).to_string();
    let form = match Part::text(options).mime_str("application/json") {
        Ok(options) => Form::new().part("file", file_part).part("options", options),
        Err(err) => {
            obs.body_error = Some(format!("failed to build the options part: {err}"));
            return obs;
        }
    };
    let timeout = case
        .options
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(default_timeout);
    let sent = client
        .post(format!("{target}/v1/convert"))
        .bearer_auth(&args.api_key)
        .multipart(form)
        .timeout(Duration::from_secs(timeout) + TIMEOUT_SLACK)
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(err) => {
            obs.timed_out = err.is_timeout();
            obs.body_error = Some(format!("request failed: {err}"));
            return obs;
        }
    };
    obs.status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    obs.content_type = header("content-type");
    obs.source_format = header("convert-source-format");
    obs.page_count = header("convert-page-count").and_then(|v| v.parse().ok());
    obs.warning_count = header("convert-warning-count").and_then(|v| v.parse().ok());
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => {
            obs.body_error = Some(format!("failed to read the body: {err}"));
            return obs;
        }
    };
    read_body(&mut obs, case.output(), PdfReads::of(&case.expect), &bytes);
    obs
}

/// What a case reads from the PDF beyond its markers: only some cases need
/// the image count, and fewer the text.
#[derive(Debug, Clone, Copy, Default)]
struct PdfReads {
    images: bool,
    text: bool,
}

impl PdfReads {
    fn of(expect: &crate::expected::Expect) -> Self {
        PdfReads {
            images: expect.reads_pdf(),
            text: expect.reads_pdf_text(),
        }
    }
}

/// Fills the body facts of `obs` from the response bytes.
fn read_body(obs: &mut Observed, output: &str, reads: PdfReads, bytes: &[u8]) {
    if obs.status != 200 {
        obs.error_code = serde_json::from_slice::<Value>(bytes)
            .ok()
            .and_then(|v| v["code"].as_str().map(str::to_owned));
        return;
    }
    let pdf = if output == "pdf" {
        Some(bytes.to_vec())
    } else {
        match read_bundle(bytes) {
            Ok(bundle) => {
                obs.manifest = Some(bundle.manifest);
                obs.entries = Some(bundle.entries);
                bundle.pdf
            }
            Err(err) => {
                obs.body_error = Some(err);
                None
            }
        }
    };
    if let Some(pdf) = pdf {
        obs.pdf_valid = Some(pdf.starts_with(b"%PDF-") && memfind(&pdf, b"%%EOF"));
        let warns_fonts = obs
            .manifest
            .as_ref()
            .is_some_and(|m| m.warnings.iter().any(|w| w.code == "FONT_SUBSTITUTED"));
        if warns_fonts {
            match fonts::base_fonts(&pdf) {
                Ok(names) => obs.base_fonts = Some(names),
                Err(err) => obs.body_error = Some(err),
            }
        }
        if reads.images {
            match pdftext::read(&pdf, reads.text) {
                Ok(facts) => obs.pdf_facts = Some(facts),
                Err(err) => obs.body_error = Some(err),
            }
        }
    }
}

fn memfind(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// What a tar.zst answer holds (`src/README.md`, the tar.zst bundle).
struct Bundle {
    manifest: Manifest,
    pdf: Option<Vec<u8>>,
    /// Every entry name, in archive order.
    entries: Vec<String>,
}

/// Reads a zstd-compressed ustar stream: `manifest.json`, then
/// `document.pdf` for `bundle`, then `pages/NNNN.<ext>`.
fn read_bundle(bytes: &[u8]) -> Result<Bundle, String> {
    let tar = zstd::stream::decode_all(bytes).map_err(|err| format!("not a zstd stream: {err}"))?;
    let mut archive = tar::Archive::new(Cursor::new(tar));
    let mut manifest = None;
    let mut pdf = None;
    let mut entries = Vec::new();
    let all = archive
        .entries()
        .map_err(|err| format!("not a tar stream: {err}"))?;
    for entry in all {
        let mut entry = entry.map_err(|err| format!("failed to read a tar entry: {err}"))?;
        let name = entry
            .path()
            .map_err(|err| format!("failed to read a tar entry name: {err}"))?
            .to_string_lossy()
            .into_owned();
        if name == "manifest.json" || name == "document.pdf" {
            let mut body = Vec::new();
            entry
                .read_to_end(&mut body)
                .map_err(|err| format!("failed to read {name}: {err}"))?;
            if name == "manifest.json" {
                manifest = Some(
                    serde_json::from_slice(&body)
                        .map_err(|err| format!("failed to parse manifest.json: {err}"))?,
                );
            } else {
                pdf = Some(body);
            }
        }
        entries.push(name);
    }
    let manifest = manifest.ok_or("the bundle has no manifest.json")?;
    Ok(Bundle {
        manifest,
        pdf,
        entries,
    })
}

fn result(
    file: &FileEntry,
    case: &Case,
    obs: &Observed,
    verdict: Verdict,
    millis: u128,
) -> CaseResult {
    CaseResult {
        id: file.id.clone(),
        case: case.name.clone(),
        tags: file.tags.clone(),
        output: case.output().to_owned(),
        status: obs.status,
        code: obs.error_code.clone(),
        passed: verdict.passed(),
        failures: verdict.failures,
        notes: verdict.notes,
        millis,
    }
}

fn print_result(r: &CaseResult) {
    let word = if r.passed { "PASS" } else { "FAIL" };
    let code = r
        .code
        .as_deref()
        .map(|c| format!(" {c}"))
        .unwrap_or_default();
    println!(
        "{word} {}/{} ({} {}{code}, {} ms)",
        r.id, r.case, r.output, r.status, r.millis
    );
    for failure in &r.failures {
        println!("    - {failure}");
    }
    for note in &r.notes {
        println!("    note: {note}");
    }
}

/// Passed and run cases per tag; a case counts under each of its file's tags.
pub fn per_tag(results: &[CaseResult]) -> BTreeMap<String, (usize, usize)> {
    let mut tags: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for r in results {
        for tag in &r.tags {
            let entry = tags.entry(tag.clone()).or_default();
            entry.1 += 1;
            if r.passed {
                entry.0 += 1;
            }
        }
    }
    tags
}

fn summarize(results: &[CaseResult]) {
    println!();
    println!("{:<10} {:>6} {:>6}", "tag", "passed", "cases");
    for (tag, (passed, run)) in per_tag(results) {
        println!("{tag:<10} {passed:>6} {run:>6}");
    }
    let passed = results.iter().filter(|r| r.passed).count();
    println!("{:<10} {passed:>6} {:>6}", "all", results.len());
}

fn write_report(path: &PathBuf, results: &[CaseResult]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(results)
        .map_err(|err| format!("failed to encode the report: {err}"))?;
    std::fs::write(path, text + "\n")
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(passed: bool, tags: &[&str]) -> CaseResult {
        CaseResult {
            id: "x".into(),
            case: "c".into(),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            output: "pdf".into(),
            status: 200,
            code: None,
            passed,
            failures: Vec::new(),
            notes: Vec::new(),
            millis: 0,
        }
    }

    #[test]
    fn one_failure_gives_a_non_zero_exit() {
        assert_eq!(exit_code(&[r(true, &["smoke"])]), ExitCode::SUCCESS);
        assert_eq!(
            exit_code(&[r(true, &["smoke"]), r(false, &["smoke"])]),
            ExitCode::FAILURE
        );
        assert_eq!(exit_code(&[]), ExitCode::FAILURE);
    }

    #[test]
    fn a_connection_after_the_last_case_fails_the_run() {
        assert!(late_connections(0).is_none());
        let late = late_connections(2).expect("a result");
        assert!(!late.passed);
        assert_eq!(exit_code(&[r(true, &["smoke"]), late]), ExitCode::FAILURE);
    }

    #[test]
    fn per_tag_counts_each_tag_of_a_case() {
        let tags = per_tag(&[r(true, &["smoke", "fonts"]), r(false, &["fonts"])]);
        assert_eq!(tags["smoke"], (1, 1));
        assert_eq!(tags["fonts"], (1, 2));
    }

    #[test]
    fn an_error_body_gives_its_code() {
        let mut obs = Observed {
            status: 422,
            ..Observed::default()
        };
        read_body(
            &mut obs,
            "bundle",
            PdfReads::default(),
            br#"{"error":"x","code":"LIMIT_EXCEEDED"}"#,
        );
        assert_eq!(obs.error_code.as_deref(), Some("LIMIT_EXCEEDED"));
    }

    #[test]
    fn a_pdf_body_is_checked_for_its_markers() {
        let mut obs = Observed {
            status: 200,
            ..Observed::default()
        };
        read_body(
            &mut obs,
            "pdf",
            PdfReads::default(),
            b"%PDF-1.4\n...\n%%EOF\n",
        );
        assert_eq!(obs.pdf_valid, Some(true));
        read_body(&mut obs, "pdf", PdfReads::default(), b"<html>");
        assert_eq!(obs.pdf_valid, Some(false));
    }

    /// A tar.zst stream with these entries, as the service writes it.
    fn tar_zst(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, body) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(&mut header, name, *body).expect("append");
        }
        let tar = tar.into_inner().expect("tar");
        zstd::stream::encode_all(&tar[..], 1).expect("zstd")
    }

    #[test]
    fn a_bundle_gives_its_manifest_pdf_and_entry_order() {
        let manifest: &[u8] = br#"{"page_count":1,"source":{"format":"pdf","family":"pdf","encrypted":false},
            "pdf":{"origin":"passthrough"},"pages":[{"number":1,"path":"pages/0001.png","width_px":1,
            "height_px":1,"blank":false}],"warnings":[]}"#;
        let bytes = tar_zst(&[
            ("manifest.json", manifest),
            ("document.pdf", b"%PDF-1.4\n%%EOF\n"),
            ("pages/0001.png", b"png"),
        ]);
        let mut obs = Observed {
            status: 200,
            ..Observed::default()
        };
        read_body(&mut obs, "bundle", PdfReads::default(), &bytes);
        assert_eq!(obs.body_error, None);
        assert_eq!(
            obs.entries.as_deref(),
            Some(
                &[
                    "manifest.json".to_string(),
                    "document.pdf".into(),
                    "pages/0001.png".into()
                ][..]
            )
        );
        assert_eq!(obs.manifest.map(|m| m.source.format), Some("pdf".into()));
        assert_eq!(obs.pdf_valid, Some(true));
    }

    #[test]
    fn a_zip_or_a_bundle_without_a_manifest_is_unreadable() {
        let mut obs = Observed {
            status: 200,
            ..Observed::default()
        };
        read_body(
            &mut obs,
            "bundle",
            PdfReads::default(),
            b"PK\x03\x04 not zstd",
        );
        assert!(
            obs.body_error
                .is_some_and(|e| e.starts_with("not a zstd stream"))
        );
        let mut obs = Observed {
            status: 200,
            ..Observed::default()
        };
        read_body(
            &mut obs,
            "images",
            PdfReads::default(),
            &tar_zst(&[("pages/0001.png", b"png")]),
        );
        assert_eq!(
            obs.body_error.as_deref(),
            Some("the bundle has no manifest.json")
        );
    }
}
