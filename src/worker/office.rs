//! The office worker: LibreOffice through LibreOfficeKit (plan 002 step 8).
//!
//! Duties (AGENT-RULES.md rule 9):
//!
//! - One `Office` per process, for life. It sits in a `ManuallyDrop` from
//!   the line after `Office::new`, so no error path drops it, and the
//!   worker leaves with `_exit`.
//! - `trim_memory` is never called: on 26.8.3 the next load then crashes.
//! - The password callback is registered once. It shares a counter with
//!   the job loop, which tells "required" (no password, one prompt) from
//!   "invalid" (a password, a second prompt).

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use libreofficekit::{CallbackType, DocUrl, DocumentType, Office, OfficeOptionalFeatures};

use crate::domain::document::Family as DomainFamily;
use crate::ipc::{
    ConvertArgs, ConvertedOut, EngineTimes, ExportOptions, Family, Init, Op, Output, SlideNotes,
    SourceFactsWire, WorkerError, WorkerErrorKind,
};
use crate::worker::output::io_error;
use crate::worker::sys;
use crate::worker::{Engine, Handled, micros};

/// The output cap of a warm-up conversion.
const WARMUP_MAX_OUTPUT: u64 = 16 << 20;

/// A disk with less free space than this counts as full.
pub const MIN_FREE_BYTES: u64 = 1024 * 1024;

#[derive(Default)]
struct PasswordState {
    /// The password for the current load; `None` stops the prompts.
    password: Option<String>,
    /// Prompts during the current load.
    calls: u32,
}

/// The office engine of one worker process.
pub struct OfficeWorker {
    office: ManuallyDrop<Office>,
    state: Rc<RefCell<PasswordState>>,
    version: String,
}

impl OfficeWorker {
    pub fn start(init: &Init) -> Result<Self, String> {
        let dir = init
            .lo_program_dir
            .as_deref()
            .ok_or("failed to start the office engine: no LibreOffice program directory")?;
        let office = ManuallyDrop::new(
            Office::new(dir).map_err(|err| format!("failed to start the office engine: {err}"))?,
        );
        office
            .set_optional_features(OfficeOptionalFeatures::DOCUMENT_PASSWORD)
            .map_err(|err| format!("failed to enable the password callback: {err}"))?;
        let state = Rc::new(RefCell::new(PasswordState::default()));
        let shared = Rc::clone(&state);
        office
            .register_callback(move |callback, kind, payload| {
                if kind != CallbackType::DocumentPassword || payload.is_null() {
                    return;
                }
                // The borrow ends before the call back into LibreOffice.
                let answer = {
                    let mut state = shared.borrow_mut();
                    state.calls += 1;
                    if state.calls == 1 {
                        state.password.clone()
                    } else {
                        None
                    }
                };
                // SAFETY: for DocumentPassword, LibreOfficeKit passes the
                // document URL as a NUL-terminated string that lives for the
                // duration of the callback.
                let url = unsafe { std::ffi::CStr::from_ptr(payload) }.to_string_lossy();
                if let Ok(url) = DocUrl::from_remote_uri(&*url) {
                    let _ = callback.set_document_password(&url, answer.as_deref());
                }
            })
            .map_err(|err| format!("failed to register the password callback: {err}"))?;
        let version = office
            .get_version_info()
            .map(|v| {
                format!(
                    "{}.{} ({})",
                    v.product_version.major, v.product_version.minor, v.build_id
                )
            })
            .unwrap_or_default();
        let mut worker = Self {
            office,
            state,
            version,
        };
        if init.warmup {
            worker.warm_up();
        }
        Ok(worker)
    }

    /// Converts one tiny document of each family, so the first real job
    /// does not load the import filters and the PDF export. A failure is
    /// reported on stderr and does not stop the worker.
    fn warm_up(&mut self) {
        let dir = std::env::temp_dir().join("convertd-warmup");
        if let Err(err) = std::fs::create_dir_all(&dir) {
            eprintln!("convertd worker: failed to create the warm-up directory: {err}");
            return;
        }
        for file in crate::worker::warmup::files() {
            let input = dir.join(file.name);
            let output = input.with_extension("pdf");
            if let Err(err) = std::fs::write(&input, &file.bytes) {
                eprintln!("convertd worker: failed to write {}: {err}", file.name);
                continue;
            }
            let args = ConvertArgs {
                input: input.clone(),
                output: output.clone(),
                family: file.family,
                password: None,
                export: ExportOptions::default(),
                max_output_bytes: WARMUP_MAX_OUTPUT,
            };
            let started = Instant::now();
            match self.convert(&args, &mut EngineTimes::default()) {
                Ok(_) => eprintln!(
                    "convertd worker: warm-up {} took {} ms",
                    file.name,
                    started.elapsed().as_millis()
                ),
                Err(err) => eprintln!("convertd worker: warm-up {} failed: {err}", file.name),
            }
            let _ = std::fs::remove_file(&input);
            let _ = std::fs::remove_file(&output);
        }
        let _ = std::fs::remove_dir(&dir);
    }

    fn convert(
        &mut self,
        args: &ConvertArgs,
        times: &mut EngineTimes,
    ) -> Result<Output, WorkerError> {
        let preflight = Instant::now();
        let facts = crate::preflight::scan(&args.input, domain_family(args.family));
        times.preflight_us = micros(preflight.elapsed());

        let input = doc_url(&args.input)?;
        let output = doc_url(&args.output)?;
        let password = args
            .password
            .as_ref()
            .map(|p| p.expose().to_owned())
            .filter(|p| !p.is_empty());
        let has_password = password.is_some();
        {
            let mut state = self.state.borrow_mut();
            state.password = password;
            state.calls = 0;
        }
        sys::set_file_size_limit(Some(args.max_output_bytes))
            .map_err(|err| io_error("cap the output size", &err))?;
        let started = Instant::now();
        let result = self.load_and_export(&input, &output, args, has_password);
        times.engine_us = micros(started.elapsed());
        times.convert_us = times.engine_us;
        // Lift the cap again, so the profile and the next load are not capped.
        let _ = sys::set_file_size_limit(None);
        self.state.borrow_mut().password = None;

        let doc_type = match result {
            Ok(doc_type) => doc_type,
            Err(err) => {
                let _ = std::fs::remove_file(&args.output);
                return Err(err);
            }
        };
        let pdf_bytes = std::fs::metadata(&args.output)
            .map_err(|err| io_error("read the converted PDF", &err))?
            .len();
        if pdf_bytes > args.max_output_bytes {
            let _ = std::fs::remove_file(&args.output);
            return Err(limit_exceeded());
        }
        Ok(Output::Converted(ConvertedOut {
            pdf_bytes,
            doc_type: doc_type.into(),
            facts: Some(SourceFactsWire::from(&facts)),
        }))
    }

    fn load_and_export(
        &mut self,
        input: &DocUrl,
        output: &DocUrl,
        args: &ConvertArgs,
        has_password: bool,
    ) -> Result<&'static str, WorkerError> {
        let mut document = match self.office.document_load(input) {
            Ok(document) => document,
            Err(err) => {
                let calls = self.state.borrow().calls;
                return Err(load_error(has_password, calls, &err.to_string()));
            }
        };
        let doc_type = match document.get_document_type() {
            Ok(DocumentType::Text) => "text",
            Ok(DocumentType::Spreadsheet) => "spreadsheet",
            Ok(DocumentType::Presentation) => "presentation",
            Ok(DocumentType::Drawing) => "drawing",
            Ok(DocumentType::Other(_)) | Err(_) => "other",
        };
        let filter = filter_options(args.family, &args.export);
        let saved = document.save_as(output, "pdf", Some(&filter));
        // The document is closed here; the instance stays (rule 9).
        drop(document);
        match saved {
            Ok(true) => Ok(doc_type),
            Ok(false) | Err(_) => Err(export_error(&args.output, args.max_output_bytes)),
        }
    }
}

impl Engine for OfficeWorker {
    fn version(&self) -> String {
        self.version.clone()
    }

    fn handle(&mut self, op: Op) -> Handled {
        match op {
            Op::Convert(args) => {
                let mut times = EngineTimes::default();
                let result = self.convert(&args, &mut times);
                Handled {
                    result,
                    engine: times,
                }
            }
            other => Handled::err(WorkerError::new(
                WorkerErrorKind::Unsupported,
                format!("the office worker does not answer {}", other.name()),
            )),
        }
    }
}

fn domain_family(family: Family) -> DomainFamily {
    match family {
        Family::Document => DomainFamily::Document,
        Family::Spreadsheet => DomainFamily::Spreadsheet,
        Family::Presentation => DomainFamily::Presentation,
    }
}

fn doc_url(path: &Path) -> Result<DocUrl, WorkerError> {
    let text = path.to_str().ok_or_else(|| {
        WorkerError::new(
            WorkerErrorKind::Internal,
            "failed to name the file: not UTF-8",
        )
    })?;
    DocUrl::from_absolute_path(text).map_err(|err| {
        WorkerError::new(
            WorkerErrorKind::Internal,
            format!("failed to name the file: {err}"),
        )
    })
}

/// The classification of a failed load (`docs/m0-spikes.md`): one prompt
/// without a password means a password is required; a second prompt after
/// a password means the password was wrong.
pub fn load_error(has_password: bool, calls: u32, message: &str) -> WorkerError {
    match (has_password, calls) {
        (false, 1..) => WorkerError::new(
            WorkerErrorKind::PasswordRequired,
            "the document needs a password",
        ),
        (true, 2..) => WorkerError::new(WorkerErrorKind::InvalidPassword, "the password is wrong"),
        _ => WorkerError::new(
            WorkerErrorKind::LoadFailed,
            format!("failed to load document: {message}"),
        ),
    }
}

fn limit_exceeded() -> WorkerError {
    WorkerError::new(
        WorkerErrorKind::LimitExceeded,
        "failed to export document: output exceeds limit",
    )
}

/// Why `save_as` failed: the job's own cap (the file reached it, so
/// `RLIMIT_FSIZE` stopped LibreOffice), a full disk, or an export error.
fn export_error(output: &Path, max_output_bytes: u64) -> WorkerError {
    let written = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    if written >= max_output_bytes {
        return limit_exceeded();
    }
    let dir = output.parent().unwrap_or(Path::new("/"));
    if sys::free_bytes(dir).is_some_and(|free| free < MIN_FREE_BYTES) {
        return WorkerError::new(
            WorkerErrorKind::DiskFull,
            "failed to export document: temp disk full",
        );
    }
    WorkerError::new(WorkerErrorKind::ExportFailed, "failed to export document")
}

/// The PDF export options, always explicit, so no setting comes from the
/// profile. The keys are those of LibreOffice's `writer_pdf_Export` family.
pub fn filter_options(family: Family, export: &ExportOptions) -> String {
    let flag = |v: bool| if v { "true" } else { "false" };
    let entry =
        |name: &str, v: bool| format!(r#""{name}":{{"type":"boolean","value":"{}"}}"#, flag(v));
    let entries = match family {
        Family::Presentation => vec![
            entry("ExportNotesPages", export.notes != SlideNotes::None),
            entry("ExportOnlyNotesPages", export.notes == SlideNotes::Only),
            entry("ExportHiddenSlides", export.include_hidden_slides),
        ],
        Family::Spreadsheet => vec![entry("SinglePageSheets", export.one_page_per_sheet)],
        Family::Document => vec![entry("ExportNotes", false)],
    };
    format!("{{{}}}", entries.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_errors_tell_required_from_invalid() {
        assert_eq!(
            load_error(false, 1, "x").kind,
            WorkerErrorKind::PasswordRequired
        );
        assert_eq!(
            load_error(true, 2, "x").kind,
            WorkerErrorKind::InvalidPassword
        );
        assert_eq!(load_error(true, 1, "x").kind, WorkerErrorKind::LoadFailed);
        assert_eq!(load_error(false, 0, "x").kind, WorkerErrorKind::LoadFailed);
    }

    #[test]
    fn filter_options_are_explicit_json() {
        let deck = filter_options(
            Family::Presentation,
            &ExportOptions {
                notes: SlideNotes::Only,
                include_hidden_slides: true,
                one_page_per_sheet: false,
            },
        );
        let value: serde_json::Value = serde_json::from_str(&deck).unwrap();
        assert_eq!(value["ExportNotesPages"]["value"], "true");
        assert_eq!(value["ExportOnlyNotesPages"]["value"], "true");
        assert_eq!(value["ExportHiddenSlides"]["value"], "true");
        let sheet = filter_options(
            Family::Spreadsheet,
            &ExportOptions {
                one_page_per_sheet: true,
                ..ExportOptions::default()
            },
        );
        let value: serde_json::Value = serde_json::from_str(&sheet).unwrap();
        assert_eq!(value["SinglePageSheets"]["value"], "true");
        let doc: serde_json::Value =
            serde_json::from_str(&filter_options(Family::Document, &ExportOptions::default()))
                .unwrap();
        assert_eq!(doc["ExportNotes"]["value"], "false");
    }
}
