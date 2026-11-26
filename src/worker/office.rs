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
//! - A DOCX, DOCM or DOTX whose sections name no page size is loaded from
//!   a copy that names the `DEFAULT_PAPER` size (`docx_paper`), swapped
//!   into the source's place for the load. The worker learns the paper
//!   from its `LC_PAPER`, which the supervisor sets.

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use libreofficekit::{CallbackType, DocUrl, DocumentType, Office, OfficeOptionalFeatures};

use crate::domain::document::Family as DomainFamily;
use crate::engines::config::Paper;
use crate::ipc::{
    ConvertArgs, ConvertedOut, EngineTimes, ExportOptions, Family, Init, Op, Output, SlideNotes,
    SourceFactsWire, WorkerError, WorkerErrorKind,
};
use crate::worker::docx_paper;
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
    /// The URL of the current load, to answer a prompt whose payload does
    /// not parse as a URL.
    url: Option<DocUrl>,
}

/// A soft `RLIMIT_FSIZE` for the load, which writes LibreOffice's own temp
/// files: generous, so real documents never hit it, but finite, so a
/// hostile one cannot fill the disk while it loads.
pub fn load_cap(max_output_bytes: u64) -> u64 {
    max_output_bytes.saturating_mul(4).max(LOAD_CAP_MIN)
}

/// The smallest load cap.
pub const LOAD_CAP_MIN: u64 = 1 << 30;

/// A password that cannot reach LibreOffice: the crate builds a C string
/// from it, and a NUL byte would make that fail and leave the prompt
/// unanswered, so LibreOffice would wait for ever.
pub fn usable_password(password: &str) -> Result<(), WorkerError> {
    if password.contains('\0') {
        return Err(WorkerError::new(
            WorkerErrorKind::InvalidPassword,
            "the password is wrong",
        ));
    }
    Ok(())
}

/// The office engine of one worker process.
pub struct OfficeWorker {
    office: ManuallyDrop<Office>,
    state: Rc<RefCell<PasswordState>>,
    version: String,
    /// `DEFAULT_PAPER`, from `LC_PAPER`; `None` leaves Word packages as
    /// they came.
    paper: Option<Paper>,
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
        // LibreOffice's start-up installs a SIGXFSZ handler that turns a
        // write past RLIMIT_FSIZE into a crash ("Fatal exception: Signal 25").
        // Ours records the hit, and the write fails with EFBIG instead.
        sys::catch_sigxfsz().map_err(|err| format!("failed to catch SIGXFSZ: {err}"))?;
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
                let (answer, current) = {
                    let mut state = shared.borrow_mut();
                    state.calls += 1;
                    let answer = if state.calls == 1 {
                        state.password.clone()
                    } else {
                        None
                    };
                    (answer, state.url.clone())
                };
                // SAFETY: for DocumentPassword, LibreOfficeKit passes the
                // document URL as a NUL-terminated string that lives for the
                // duration of the callback.
                let payload = unsafe { std::ffi::CStr::from_ptr(payload) }.to_string_lossy();
                // LibreOffice waits until the prompt is answered, so it is
                // always answered: with the load's own URL when the payload
                // does not parse, and with no password when the answer fails.
                let Some(url) = DocUrl::from_remote_uri(&*payload).ok().or(current) else {
                    return;
                };
                if callback
                    .set_document_password(&url, answer.as_deref())
                    .is_err()
                {
                    let _ = callback.set_document_password(&url, None);
                }
            })
            .map_err(|err| format!("failed to register the password callback: {err}"))?;
        // The crate frees the version string that LibreOffice allocated with
        // CString::from_raw, which is right only under the system allocator,
        // the only one convertd uses (the mimalloc option was dropped).
        let version = office
            .get_version_info()
            .map(|v| {
                format!(
                    "{}.{} ({})",
                    v.product_version.major, v.product_version.minor, v.build_id
                )
            })
            .unwrap_or_default();
        let paper = std::env::var("LC_PAPER")
            .ok()
            .and_then(|value| Paper::from_lc_paper(&value));
        let mut worker = Self {
            office,
            state,
            version,
            paper,
        };
        if init.warmup {
            worker.warm_up();
        }
        Ok(worker)
    }

    /// Converts the warm-up documents of `warmup::files` (one per family,
    /// shaped like real ones), so the first real job does not load the
    /// import filters, the substitute fonts and the PDF export. A failure
    /// is reported on stderr and does not stop the worker.
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
        // A worker that died during a swap may have left the source set
        // aside: it goes back before anything reads it.
        if docx_paper::is_word_package(&args.input)
            && let Some(swap) = Swap::new(&args.input)
        {
            swap.recover()?;
        }
        let facts = crate::preflight::scan(&args.input, domain_family(args.family));
        // The copy with page sizes counts as pre-flight: it reads the source
        // before LibreOffice does.
        let swap = self.paper_swap(args);
        times.preflight_us = micros(preflight.elapsed());
        let swap = swap?;
        let result = self.convert_from(&args.input, args, times);
        // The source goes back before the job answers: the supervisor then
        // stops reserving the copy's bytes.
        if let Some(swap) = &swap {
            swap.restore().inspect_err(|_| {
                let _ = std::fs::remove_file(&args.output);
            })?;
        }
        let doc_type = result?;
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

    /// For a Word package whose sections name no page size, swaps a copy
    /// that names the `DEFAULT_PAPER` size into the source's place, so
    /// LibreOffice loads it under the source's own name and folder: a
    /// FILENAME field and relative links stay as they were. `None` loads
    /// the source as it came. Only A4 needs a copy: the importer's own
    /// default is Letter. A failed write or rename (a full disk) fails the
    /// job, so that the paper never depends on the disk.
    fn paper_swap(&self, args: &ConvertArgs) -> Result<Option<Swap>, WorkerError> {
        let Some(paper) = self.paper.filter(|paper| *paper != Paper::Letter) else {
            return Ok(None);
        };
        if args.family != Family::Document || !docx_paper::is_word_package(&args.input) {
            return Ok(None);
        }
        let Some(swap) = Swap::new(&args.input) else {
            return Ok(None);
        };
        // convert() has already put a set-aside source back.
        match docx_paper::repack(&args.input, &swap.fresh, paper) {
            Ok(()) => {}
            Err(skip) => {
                swap.discard()?;
                return match skip {
                    docx_paper::Skip::Io(err) => Err(io_error("write the paper copy", &err)),
                    _ => Ok(None),
                };
            }
        }
        if let Err(err) = std::fs::rename(&args.input, &swap.backup) {
            swap.discard()?;
            return Err(io_error("set the source aside", &err));
        }
        if let Err(err) = std::fs::rename(&swap.fresh, &args.input) {
            swap.restore()?;
            return Err(io_error("put the paper copy in place", &err));
        }
        Ok(Some(swap))
    }

    /// Loads `source` (the job's input or its copy) and exports the PDF.
    fn convert_from(
        &mut self,
        source: &Path,
        args: &ConvertArgs,
        times: &mut EngineTimes,
    ) -> Result<&'static str, WorkerError> {
        let input = doc_url(source)?;
        let output = doc_url(&args.output)?;
        let password = args
            .password
            .as_ref()
            .map(|p| p.expose().to_owned())
            .filter(|p| !p.is_empty());
        if let Some(password) = &password {
            usable_password(password)?;
        }
        let has_password = password.is_some();
        {
            let mut state = self.state.borrow_mut();
            state.password = password;
            state.calls = 0;
            state.url = Some(input.clone());
        }
        let started = Instant::now();
        let result = self.load_and_export(&input, &output, args, has_password);
        times.engine_us = micros(started.elapsed());
        times.convert_us = times.engine_us;
        {
            let mut state = self.state.borrow_mut();
            state.password = None;
            state.url = None;
        }

        result.inspect_err(|_| {
            let _ = std::fs::remove_file(&args.output);
        })
    }

    fn load_and_export(
        &mut self,
        input: &DocUrl,
        output: &DocUrl,
        args: &ConvertArgs,
        has_password: bool,
    ) -> Result<&'static str, WorkerError> {
        // LibreOffice may reinstall its SIGXFSZ handler; ours goes back
        // before every capped section.
        sys::catch_sigxfsz().map_err(|err| io_error("catch SIGXFSZ", &err))?;
        sys::take_file_size_hit();
        sys::set_file_size_limit(Some(load_cap(args.max_output_bytes)))
            .map_err(|err| io_error("cap the load", &err))?;
        let loaded = self.office.document_load(input);
        let load_capped = sys::take_file_size_hit();
        let _ = sys::set_file_size_limit(None);
        let mut document = match loaded {
            Ok(_) if load_capped => return Err(limit_exceeded()),
            Ok(document) => document,
            Err(_) if load_capped => return Err(limit_exceeded()),
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
        // The loaded type decides the export keys, not the requested family:
        // an encrypted OOXML upload arrives as a document of unknown type
        // (named source.docx, family document) and may be a deck or a sheet.
        let filter = filter_options(export_family(doc_type, args.family), &args.export);
        // The export cap is the job's own: max_output_bytes. The load above
        // ran under the more generous load cap, because it writes
        // LibreOffice's own temp files, which are no part of the output.
        sys::catch_sigxfsz().map_err(|err| io_error("catch SIGXFSZ", &err))?;
        sys::take_file_size_hit();
        sys::set_file_size_limit(Some(args.max_output_bytes))
            .map_err(|err| io_error("cap the output size", &err))?;
        let saved = document.save_as(output, "pdf", Some(&filter));
        let capped = sys::take_file_size_hit();
        // Lift the cap again, so the profile and the next load are not capped.
        let _ = sys::set_file_size_limit(None);
        // The document is closed here; the instance stays (rule 9).
        drop(document);
        match saved {
            Ok(true) if !capped => Ok(doc_type),
            Ok(_) | Err(_) if capped => Err(limit_exceeded()),
            Ok(_) | Err(_) => Err(export_error(&args.output, args.max_output_bytes)),
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

/// The files of a swap: the copy is written to `fresh`, the source is set
/// aside as `backup`, and `fresh` takes the source's place. Both live in
/// `docx_paper::COPY_DIR` next to the source.
struct Swap {
    source: std::path::PathBuf,
    dir: std::path::PathBuf,
    fresh: std::path::PathBuf,
    backup: std::path::PathBuf,
}

impl Swap {
    fn new(source: &Path) -> Option<Self> {
        let (fresh, backup) = docx_paper::swap_paths(source)?;
        Some(Self {
            source: source.to_path_buf(),
            dir: backup.parent()?.to_path_buf(),
            fresh,
            backup,
        })
    }

    /// Whether the copy folder is a real folder: anything else at its path
    /// is not ours and is left alone.
    fn dir_is_ours(&self) -> Result<bool, WorkerError> {
        match std::fs::symlink_metadata(&self.dir) {
            Ok(meta) => Ok(meta.file_type().is_dir()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(io_error("look for the paper copy", &err)),
        }
    }

    /// After a crash: puts a set-aside source back, and drops a copy.
    fn recover(&self) -> Result<(), WorkerError> {
        if !self.dir_is_ours()? {
            return Ok(());
        }
        match std::fs::symlink_metadata(&self.backup) {
            Ok(_) => std::fs::rename(&self.backup, &self.source)
                .map_err(|err| io_error("put the source back", &err))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error("look for the set-aside source", &err)),
        }
        self.discard()
    }

    /// Puts the source back over the copy, then drops what is left.
    fn restore(&self) -> Result<(), WorkerError> {
        std::fs::rename(&self.backup, &self.source)
            .map_err(|err| io_error("put the source back", &err))?;
        self.discard()
    }

    /// Removes an unused copy, then the folder if it is empty. A copy that
    /// is gone already is fine; any other failure is the job's I/O error,
    /// because the supervisor stops reserving the copy's bytes when the job
    /// answers.
    fn discard(&self) -> Result<(), WorkerError> {
        if !self.dir_is_ours()? {
            return Ok(());
        }
        match std::fs::remove_file(&self.fresh) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error("remove the paper copy", &err)),
        }
        let _ = std::fs::remove_dir(&self.dir);
        Ok(())
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

/// Why `save_as` failed when no write passed the cap: the output file
/// reached it anyway, a full disk, or an export error.
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

/// The family whose export keys apply: LibreOffice's document type when it
/// names an office family, else the requested family.
pub fn export_family(doc_type: &str, requested: Family) -> Family {
    match doc_type {
        "text" => Family::Document,
        "spreadsheet" => Family::Spreadsheet,
        "presentation" => Family::Presentation,
        _ => requested,
    }
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

    /// A swap in a fresh folder, with `source` holding `original`.
    fn swap_in(name: &str, original: &[u8]) -> (std::path::PathBuf, Swap) {
        let root = std::env::temp_dir().join(format!(
            "convertd-office-swap-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.docx");
        std::fs::write(&source, original).unwrap();
        let swap = Swap::new(&source).unwrap();
        (root, swap)
    }

    /// Sets the source aside and puts `copy` in its place, as paper_swap
    /// does after a successful repack.
    fn swap_into_place(swap: &Swap, copy: &[u8]) {
        std::fs::create_dir(&swap.dir).unwrap();
        std::fs::write(&swap.fresh, copy).unwrap();
        std::fs::rename(&swap.source, &swap.backup).unwrap();
        std::fs::rename(&swap.fresh, &swap.source).unwrap();
    }

    #[test]
    fn a_swap_puts_the_source_back_and_leaves_nothing() {
        let (root, swap) = swap_in("restore", b"original");
        swap_into_place(&swap, b"copy");
        assert_eq!(std::fs::read(&swap.source).unwrap(), b"copy");
        swap.restore().unwrap();
        assert_eq!(std::fs::read(&swap.source).unwrap(), b"original");
        assert!(!swap.dir.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_retry_recovers_from_every_crash_point() {
        // After the copy was written, before the swap.
        let (root, swap) = swap_in("fresh", b"original");
        std::fs::create_dir(&swap.dir).unwrap();
        std::fs::write(&swap.fresh, b"partial").unwrap();
        swap.recover().unwrap();
        assert_eq!(std::fs::read(&swap.source).unwrap(), b"original");
        assert!(!swap.dir.exists());
        let _ = std::fs::remove_dir_all(&root);
        // Between the two renames: the source is set aside.
        let (root, swap) = swap_in("between", b"original");
        std::fs::create_dir(&swap.dir).unwrap();
        std::fs::write(&swap.fresh, b"copy").unwrap();
        std::fs::rename(&swap.source, &swap.backup).unwrap();
        swap.recover().unwrap();
        assert_eq!(std::fs::read(&swap.source).unwrap(), b"original");
        assert!(!swap.dir.exists());
        let _ = std::fs::remove_dir_all(&root);
        // With the copy in place.
        let (root, swap) = swap_in("placed", b"original");
        swap_into_place(&swap, b"copy");
        swap.recover().unwrap();
        assert_eq!(std::fs::read(&swap.source).unwrap(), b"original");
        assert!(!swap.dir.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn something_else_at_the_copy_folder_is_left_alone() {
        let (root, swap) = swap_in("file", b"original");
        std::fs::write(&swap.dir, b"not ours").unwrap();
        swap.recover().unwrap();
        swap.discard().unwrap();
        assert_eq!(std::fs::read(&swap.dir).unwrap(), b"not ours");
        assert_eq!(std::fs::read(&swap.source).unwrap(), b"original");
        let _ = std::fs::remove_dir_all(root);
    }

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
    fn a_password_with_a_nul_byte_is_refused_before_the_engine() {
        assert_eq!(
            usable_password("se\0cret").unwrap_err().kind,
            WorkerErrorKind::InvalidPassword
        );
        assert!(usable_password("secret").is_ok());
    }

    #[test]
    fn the_load_cap_is_generous_but_finite() {
        assert_eq!(load_cap(4096), LOAD_CAP_MIN);
        assert_eq!(load_cap(768 << 20), 3 * (1 << 30));
        assert_eq!(load_cap(u64::MAX), u64::MAX);
    }

    #[test]
    fn the_loaded_type_decides_the_export_keys() {
        assert_eq!(
            export_family("presentation", Family::Document),
            Family::Presentation
        );
        assert_eq!(
            export_family("spreadsheet", Family::Document),
            Family::Spreadsheet
        );
        assert_eq!(
            export_family("text", Family::Presentation),
            Family::Document
        );
        assert_eq!(
            export_family("drawing", Family::Presentation),
            Family::Presentation
        );
        assert_eq!(
            export_family("other", Family::Spreadsheet),
            Family::Spreadsheet
        );
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
