//! `corpus-bench validate`: checks `expected.json` against its schema and
//! against the corpus on disk. It needs no server.
//!
//! The checks, in order:
//! 1. the JSON Schema (`expected.schema.json`);
//! 2. the rules a schema cannot state: unique ids, paths and case names,
//!    safe relative paths, `min <= max`, known warning detail keys, sorted
//!    requested fonts;
//! 3. every listed file exists as a regular file and matches its SHA-256;
//! 4. no file under `out/` or `fixtures/` is unlisted, except
//!    `fixtures/SOURCES.md`, and the files under `fixtures/` stay within
//!    [`FIXTURES_BUDGET`];
//! 5. git tracks nothing under `out/` or `private/`.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::expected::{Expected, PageCount, detail_keys};

/// The directories whose every file must be listed.
const LISTED_DIRS: [&str; 2] = ["out", "fixtures"];
/// Directories git must never track.
const UNTRACKED_DIRS: [&str; 2] = ["out", "private"];
/// Files the operating system drops into directories on its own.
const IGNORED_NAMES: [&str; 1] = [".DS_Store"];
/// The record of where each fixture comes from, the one unlisted file allowed.
const FIXTURE_SOURCES: &str = "fixtures/SOURCES.md";
/// The most bytes all committed fixtures may take together.
pub const FIXTURES_BUDGET: u64 = 2 << 20;
/// The `page_count` of a case whose count is typed in from a reviewed `check` run.
const UNRECORDED: &str = "unrecorded";

pub struct Paths {
    /// The directory the `path` fields are relative to.
    pub corpus: PathBuf,
    pub expected: PathBuf,
    pub schema: PathBuf,
}

impl Paths {
    pub fn in_corpus(corpus: &Path) -> Self {
        Self {
            corpus: corpus.to_path_buf(),
            expected: corpus.join("expected.json"),
            schema: corpus.join("expected.schema.json"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// Each one fails the run.
    pub problems: Vec<String>,
    /// Checks that could not run, such as git outside a repository.
    pub notes: Vec<String>,
    pub files: usize,
    pub cases: usize,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.problems.is_empty()
    }
}

pub fn run(paths: &Paths) -> Outcome {
    let mut outcome = Outcome::default();
    let Some(expected) = check_document(paths, &mut outcome) else {
        return outcome;
    };
    check_files(&paths.corpus, &expected, &mut outcome);
    check_unlisted(&paths.corpus, &expected, &mut outcome);
    check_git(&paths.corpus, &mut outcome);
    outcome
}

/// Steps 1 and 2. Returns the typed document when the schema holds.
pub fn check_document(paths: &Paths, outcome: &mut Outcome) -> Option<Expected> {
    let schema = read_json(&paths.schema, outcome)?;
    let document = read_json(&paths.expected, outcome)?;
    let validator = match jsonschema::validator_for(&schema) {
        Ok(validator) => validator,
        Err(err) => {
            outcome.problems.push(format!(
                "failed to compile schema {}: {err}",
                paths.schema.display()
            ));
            return None;
        }
    };
    let before = outcome.problems.len();
    for err in validator.iter_errors(&document) {
        let at = err.instance_path().to_string();
        let at = if at.is_empty() { "/".to_owned() } else { at };
        outcome.problems.push(format!("schema: {at}: {err}"));
    }
    if outcome.problems.len() > before {
        return None;
    }
    let expected: Expected = match serde_json::from_value(document) {
        Ok(expected) => expected,
        Err(err) => {
            outcome.problems.push(format!(
                "failed to read {}: {err}",
                paths.expected.display()
            ));
            return None;
        }
    };
    check_semantics(&expected, outcome);
    Some(expected)
}

fn read_json(path: &Path, outcome: &mut Outcome) -> Option<Value> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            outcome
                .problems
                .push(format!("failed to read {}: {err}", path.display()));
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(err) => {
            outcome
                .problems
                .push(format!("failed to parse {}: {err}", path.display()));
            None
        }
    }
}

fn check_semantics(expected: &Expected, outcome: &mut Outcome) {
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    let mut unrecorded = Vec::new();
    for file in &expected.files {
        let id = &file.id;
        outcome.files += 1;
        if !ids.insert(id.as_str()) {
            outcome.problems.push(format!("{id}: duplicate id"));
        }
        if !paths.insert(file.path.as_str()) {
            outcome
                .problems
                .push(format!("{id}: duplicate path {}", file.path));
        }
        if file.path.split('/').any(|part| part == "." || part == "..") {
            outcome
                .problems
                .push(format!("{id}: path {} has a . or .. component", file.path));
        }
        if let Some(fonts) = file
            .preflight
            .as_ref()
            .and_then(|p| p.requested_fonts.as_ref())
        {
            let sorted: BTreeSet<&String> = fonts.iter().collect();
            if sorted.len() != fonts.len() || !sorted.iter().copied().eq(fonts.iter()) {
                outcome.problems.push(format!(
                    "{id}: preflight.requested_fonts must be sorted and unique"
                ));
            }
        }
        let mut names = HashSet::new();
        for case in &file.cases {
            outcome.cases += 1;
            let at = format!("{id}/{}", case.name);
            if !names.insert(case.name.as_str()) {
                outcome.problems.push(format!("{at}: duplicate case name"));
            }
            match &case.expect.page_count {
                Some(PageCount::Range {
                    min,
                    max: Some(max),
                }) if min > max => outcome
                    .problems
                    .push(format!("{at}: page_count min {min} > max {max}")),
                Some(PageCount::Unrecorded(marker)) if marker == UNRECORDED => {
                    unrecorded.push(at.clone());
                }
                Some(PageCount::Unrecorded(other)) => outcome.problems.push(format!(
                    "{at}: page_count {other:?} is not a number, range or {UNRECORDED:?}"
                )),
                _ => {}
            }
            if let Some(PageCount::Exact(pages)) = case.expect.page_count
                && let Some(page) = case.expect.blank_pages.iter().find(|&&p| p > pages)
            {
                outcome.problems.push(format!(
                    "{at}: blank page {page} is past page_count {pages}"
                ));
            }
            for warning in &case.expect.warnings {
                let allowed = detail_keys(&warning.code);
                for key in warning.detail.keys() {
                    if !allowed.contains(&key.as_str()) {
                        outcome.problems.push(format!(
                            "{at}: {} has no detail key {key:?} (allowed: {})",
                            warning.code,
                            allowed.join(", ")
                        ));
                    }
                }
            }
        }
    }
    if !unrecorded.is_empty() {
        outcome.notes.push(format!(
            "page_count is still unrecorded for {}",
            unrecorded.join(", ")
        ));
    }
}

/// Step 3.
fn check_files(corpus: &Path, expected: &Expected, outcome: &mut Outcome) {
    for file in &expected.files {
        let path = corpus.join(&file.path);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                outcome
                    .problems
                    .push(format!("{}: {} is not a regular file", file.id, file.path));
                continue;
            }
            Err(err) => {
                outcome
                    .problems
                    .push(format!("{}: missing {} ({err})", file.id, file.path));
                continue;
            }
        }
        let Some(want) = &file.sha256 else {
            continue;
        };
        match sha256_file(&path) {
            Ok(got) if &got == want => {}
            Ok(got) => outcome.problems.push(format!(
                "{}: {} has sha256 {got}, expected.json says {want}",
                file.id, file.path
            )),
            Err(err) => outcome
                .problems
                .push(format!("{}: failed to hash {}: {err}", file.id, file.path)),
        }
    }
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Step 4.
fn check_unlisted(corpus: &Path, expected: &Expected, outcome: &mut Outcome) {
    let listed: HashSet<&str> = expected.files.iter().map(|f| f.path.as_str()).collect();
    for dir in LISTED_DIRS {
        let mut found = Vec::new();
        if let Err(err) = walk(&corpus.join(dir), dir, &mut found) {
            if err.kind() != io::ErrorKind::NotFound {
                outcome
                    .problems
                    .push(format!("failed to list {dir}/: {err}"));
            }
            continue;
        }
        found.sort();
        if dir == "fixtures" {
            check_fixtures_budget(corpus, &found, outcome);
        }
        for rel in found {
            if !listed.contains(rel.as_str()) && rel != FIXTURE_SOURCES {
                outcome
                    .problems
                    .push(format!("{rel}: not listed in expected.json"));
            }
        }
    }
}

/// The committed fixtures, `SOURCES.md` included, must fit [`FIXTURES_BUDGET`].
fn check_fixtures_budget(corpus: &Path, found: &[String], outcome: &mut Outcome) {
    let mut total = 0u64;
    for rel in found {
        match fs::symlink_metadata(corpus.join(rel)) {
            Ok(meta) => total += meta.len(),
            Err(err) => outcome
                .problems
                .push(format!("failed to read the size of {rel}: {err}")),
        }
    }
    if total > FIXTURES_BUDGET {
        outcome.problems.push(format!(
            "fixtures/ holds {total} bytes, more than the {FIXTURES_BUDGET} byte budget"
        ));
    }
}

/// Collects the paths of all non-directory entries under `dir`, relative to the corpus.
fn walk(dir: &Path, rel: &str, found: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = format!("{rel}/{name}");
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), &child, found)?;
        } else if !IGNORED_NAMES.contains(&name.as_str()) {
            found.push(child);
        }
    }
    Ok(())
}

/// Step 5. Outside a git work tree the check is skipped with a note.
fn check_git(corpus: &Path, outcome: &mut Outcome) {
    let result = Command::new("git")
        .arg("-C")
        .arg(corpus)
        .args(["ls-files", "-z", "--"])
        .args(UNTRACKED_DIRS)
        .output();
    match result {
        Ok(out) if out.status.success() => {
            for path in out.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()) {
                outcome.problems.push(format!(
                    "{}: tracked by git, but out/ and private/ must stay untracked",
                    String::from_utf8_lossy(path)
                ));
            }
        }
        Ok(out) => outcome.notes.push(format!(
            "skipped the git check: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(err) => outcome
            .notes
            .push(format!("skipped the git check: failed to run git: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCHEMA: &str = include_str!("../../../corpus/expected.schema.json");
    const SMALL: &[u8] = b"small file";
    const FIXTURE: &[u8] = b"fixture bytes";

    fn sha(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn document() -> Value {
        json!({
            "schema_version": 1,
            "listener": "127.0.0.1:47123",
            "files": [
                {
                    "id": "docx-small", "path": "out/docx/small.docx", "origin": "generated",
                    "sha256": sha(SMALL), "tags": ["smoke"],
                    "preflight": {"format": "docx", "encrypted": false, "declared_pages": null,
                                  "requested_fonts": ["Calibri", "Cambria"], "macros": []},
                    "cases": [{"name": "bundle", "options": {"output": "bundle"},
                               "expect": {"status": 200, "page_count": 1, "warnings": [
                                   {"code": "MACROS_IGNORED", "detail": {"kind": "vba"}}]}}]
                },
                {
                    "id": "docx-encrypted", "path": "out/docx/encrypted.docx", "origin": "generated",
                    "unhashed": "random salt", "tags": ["passwords"],
                    "cases": [{"name": "no-password", "options": {},
                               "expect": {"status": 422, "code": "PASSWORD_REQUIRED"}}]
                },
                {
                    "id": "vba-project", "path": "fixtures/vbaProject.bin", "origin": "fixture",
                    "sha256": sha(FIXTURE), "licence": {"spdx": "Apache-2.0", "source": "test"},
                    "tags": ["macros"]
                }
            ]
        })
    }

    /// A corpus directory that passes, built from `doc`.
    fn corpus(doc: &Value) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        fs::write(root.join("expected.schema.json"), SCHEMA).expect("schema");
        fs::write(root.join("expected.json"), doc.to_string()).expect("expected");
        fs::create_dir_all(root.join("out/docx")).expect("out");
        fs::create_dir_all(root.join("fixtures")).expect("fixtures");
        fs::write(root.join("out/docx/small.docx"), SMALL).expect("small");
        fs::write(root.join("out/docx/encrypted.docx"), b"random").expect("encrypted");
        fs::write(root.join("fixtures/vbaProject.bin"), FIXTURE).expect("fixture");
        fs::write(root.join("fixtures/SOURCES.md"), b"# Fixture sources").expect("sources");
        dir
    }

    fn validate(dir: &tempfile::TempDir) -> Outcome {
        run(&Paths::in_corpus(dir.path()))
    }

    fn assert_fails_with(outcome: &Outcome, needle: &str) {
        assert!(
            outcome.problems.iter().any(|p| p.contains(needle)),
            "expected a problem containing {needle:?}, got {:?}",
            outcome.problems
        );
    }

    #[test]
    fn a_complete_corpus_passes() {
        let dir = corpus(&document());
        let outcome = validate(&dir);
        assert!(outcome.passed(), "{:?}", outcome.problems);
        assert_eq!((outcome.files, outcome.cases), (3, 2));
    }

    #[test]
    fn a_missing_file_fails() {
        let dir = corpus(&document());
        fs::remove_file(dir.path().join("out/docx/encrypted.docx")).expect("remove");
        assert_fails_with(
            &validate(&dir),
            "docx-encrypted: missing out/docx/encrypted.docx",
        );
    }

    #[test]
    fn a_wrong_sha_fails() {
        let dir = corpus(&document());
        fs::write(dir.path().join("out/docx/small.docx"), b"changed").expect("write");
        assert_fails_with(
            &validate(&dir),
            "docx-small: out/docx/small.docx has sha256",
        );
    }

    #[test]
    fn a_wrong_fixture_sha_fails() {
        let dir = corpus(&document());
        fs::write(dir.path().join("fixtures/vbaProject.bin"), b"changed").expect("write");
        assert_fails_with(
            &validate(&dir),
            "vba-project: fixtures/vbaProject.bin has sha256",
        );
    }

    #[test]
    fn an_unlisted_generated_file_fails() {
        let dir = corpus(&document());
        fs::write(dir.path().join("out/docx/stale.docx"), b"old").expect("write");
        assert_fails_with(&validate(&dir), "out/docx/stale.docx: not listed");
    }

    #[test]
    fn an_unlisted_fixture_fails() {
        let dir = corpus(&document());
        fs::write(dir.path().join("fixtures/extra.bin"), b"extra").expect("write");
        assert_fails_with(&validate(&dir), "fixtures/extra.bin: not listed");
    }

    #[test]
    fn a_fixture_without_a_licence_fails_the_schema() {
        let mut doc = document();
        doc["files"][2]
            .as_object_mut()
            .expect("object")
            .remove("licence");
        assert_fails_with(&validate(&corpus(&doc)), "schema: /files/2");
    }

    #[test]
    fn a_generated_file_needs_a_hash_or_a_reason() {
        let mut doc = document();
        doc["files"][1]
            .as_object_mut()
            .expect("object")
            .remove("unhashed");
        assert_fails_with(&validate(&corpus(&doc)), "schema: /files/1");
    }

    #[test]
    fn an_error_status_needs_a_code() {
        let mut doc = document();
        doc["files"][1]["cases"][0]["expect"]
            .as_object_mut()
            .expect("object")
            .remove("code");
        assert_fails_with(&validate(&corpus(&doc)), "schema: /files/1/cases/0/expect");
    }

    #[test]
    fn a_fixture_with_cases_fails_the_schema() {
        let mut doc = document();
        doc["files"][2]["cases"] = doc["files"][0]["cases"].clone();
        assert_fails_with(&validate(&corpus(&doc)), "schema: /files/2");
    }

    #[test]
    fn a_file_other_than_a_fixture_needs_cases() {
        let mut doc = document();
        doc["files"][0]
            .as_object_mut()
            .expect("object")
            .remove("cases");
        assert_fails_with(&validate(&corpus(&doc)), "schema: /files/0");
    }

    #[test]
    fn fixtures_over_the_budget_fail() {
        let dir = corpus(&document());
        let big = vec![0u8; usize::try_from(FIXTURES_BUDGET).expect("fits")];
        fs::write(dir.path().join("fixtures/SOURCES.md"), big).expect("write");
        assert_fails_with(&validate(&dir), "more than the 2097152 byte budget");
    }

    #[test]
    fn a_duplicate_id_fails() {
        let mut doc = document();
        doc["files"][1]["id"] = json!("docx-small");
        assert_fails_with(&validate(&corpus(&doc)), "docx-small: duplicate id");
    }

    #[test]
    fn an_unknown_detail_key_fails() {
        let mut doc = document();
        doc["files"][0]["cases"][0]["expect"]["warnings"][0]["detail"] = json!({"type": "vba"});
        assert_fails_with(
            &validate(&corpus(&doc)),
            "MACROS_IGNORED has no detail key \"type\"",
        );
    }

    #[test]
    fn unsorted_requested_fonts_fail() {
        let mut doc = document();
        doc["files"][0]["preflight"]["requested_fonts"] = json!(["Cambria", "Calibri"]);
        assert_fails_with(&validate(&corpus(&doc)), "requested_fonts must be sorted");
    }

    #[test]
    fn a_parent_path_component_fails() {
        let mut doc = document();
        doc["files"][0]["path"] = json!("out/../small.docx");
        assert_fails_with(&validate(&corpus(&doc)), "has a . or .. component");
    }

    /// The committed expectations pass the schema and the semantic rules.
    /// The files themselves are generated, so they are not checked here.
    #[test]
    fn the_committed_expected_json_is_valid() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus");
        let mut outcome = Outcome::default();
        let expected = check_document(&Paths::in_corpus(&corpus), &mut outcome);
        assert!(outcome.passed(), "{:?}", outcome.problems);
        assert!(expected.is_some_and(|e| !e.files.is_empty()));
    }
}
