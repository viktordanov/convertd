//! The conversion service: routes a detected upload through the engines.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::conversion::{
    ConversionRequest, ConversionResult, ExportOptions, Output, PdfOrigin, StageTimings,
};
use super::detection::Detection;
use super::document::{DocType, Family, Format};
use super::engine::{
    Admission, DecryptJob, EngineError, InspectJob, Inspection, OfficeEngine, OfficeJob, PageSize,
    PdfEngine, RenderJob, RenderedPage,
};
use super::limits::Limits;
use super::secret::Password;
use super::verification::{self, FontCatalog, VerifyInput};

/// Why a conversion failed. Statuses are mapped only in `rest/`.
#[derive(Debug, thiserror::Error)]
pub enum ConversionError {
    /// An option is out of range or `pages` selects nothing (422).
    #[error("{0}")]
    Validation(String),
    /// A server limit, such as `MAX_RENDER_PAGES` (422).
    #[error("{0}")]
    LimitExceeded(String),
    /// The deadline passed (504).
    #[error("the conversion did not finish within {seconds} seconds")]
    Timeout { seconds: u64 },
    /// The disk budget is spent by other jobs (503).
    #[error("not enough temporary storage")]
    DiskFull,
    #[error(transparent)]
    Engine(EngineError),
}

/// The paths of one job directory the engines read and write.
#[derive(Debug, Clone)]
pub struct JobPaths {
    /// `<job>/source.<ext>`.
    pub source: PathBuf,
    /// `<job>/document.pdf`.
    pub document_pdf: PathBuf,
    /// `<job>/pages/`.
    pub pages_dir: PathBuf,
}

/// What the upload sink measured.
#[derive(Debug, Clone)]
pub struct UploadFacts {
    pub size_bytes: u64,
    pub sha256: [u8; 32],
    /// The sanitised base name of the part's file name, if any.
    pub file_name: Option<String>,
}

/// The job's share of the disk budget (`TMP_MAX_BYTES`). The service
/// reserves what each writing engine call may write before the call, and
/// passes that amount as `max_output_bytes`. Implemented by
/// `jobs::JobBudget`; everything is returned when the job ends.
pub trait OutputBudget: Send + Sync {
    /// `false` when other jobs hold the budget.
    fn reserve(&self, bytes: u64) -> bool;

    /// Gives back up to `bytes` of what this job reserved, for a file that
    /// is already gone.
    fn release(&self, bytes: u64);
}

/// Routes by family: office files to the office engine, PDFs to inspect
/// (and decrypt). Image inputs never reach it (415 at detection).
pub struct ConversionService {
    pub(super) office: Arc<dyn OfficeEngine>,
    pub(super) pdf: Arc<dyn PdfEngine>,
    pub(super) limits: Limits,
    /// `Some(18)` with `VERIFY_PROBE_RENDER`.
    pub(super) probe_dpi: Option<u16>,
    /// `VERIFY_PROBE_MAX_PAGES`: the probed pages that count.
    pub(super) probe_max_pages: u32,
    /// For `FONT_SUBSTITUTED`; empty until [`ConversionService::with_fonts`].
    pub(super) fonts: FontCatalog,
    /// The office worker writes a copy of a Word package with page sizes
    /// (`DEFAULT_PAPER=A4`); see [`ConversionService::with_paper_copy`].
    pub(super) paper_copy: bool,
}

/// The DPI of the probe render for `BLANK_PAGE` (V3).
pub const PROBE_DPI: u16 = 18;

/// `VERIFY_PROBE_MAX_PAGES` by default.
pub const DEFAULT_PROBE_MAX_PAGES: u32 = 200;

impl ConversionService {
    pub fn new(
        office: Arc<dyn OfficeEngine>,
        pdf: Arc<dyn PdfEngine>,
        limits: Limits,
        verify_probe_render: bool,
    ) -> Self {
        Self {
            office,
            pdf,
            limits,
            probe_dpi: verify_probe_render.then_some(PROBE_DPI),
            probe_max_pages: DEFAULT_PROBE_MAX_PAGES,
            fonts: FontCatalog::default(),
            paper_copy: false,
        }
    }

    /// The office worker writes a copy of a DOCX, DOCM or DOTX that names
    /// its page sizes, so its office conversion reserves the copy too. Only
    /// with `DEFAULT_PAPER=A4`: Letter is the importer's own default.
    pub fn with_paper_copy(mut self, paper_copy: bool) -> Self {
        self.paper_copy = paper_copy;
        self
    }

    /// At most `pages` probed pages count for V3 (`VERIFY_PROBE_MAX_PAGES`).
    pub fn with_probe_max_pages(mut self, pages: u32) -> Self {
        self.probe_max_pages = pages;
        self
    }

    /// The installed families and their substitutes, for V2.
    pub fn with_fonts(mut self, fonts: FontCatalog) -> Self {
        self.fonts = fonts;
        self
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }
}

/// One request's inputs to [`ConversionService::convert`].
pub struct ConversionJob<'a> {
    pub paths: &'a JobPaths,
    pub upload: &'a UploadFacts,
    pub detection: Detection,
    pub request: &'a ConversionRequest,
    pub budget: &'a dyn OutputBudget,
    /// `upload_done + timeout`, computed once by the caller.
    pub deadline: Instant,
}

impl ConversionService {
    /// Produces the PDF and inspects it. Stage times are written into
    /// `timings` as each stage ends, so they survive an error or a dropped
    /// future.
    ///
    /// The first engine call uses `Admission::Queue`, every later one
    /// `Continue`. The deadline is checked between calls; the engines
    /// enforce it inside a call.
    pub async fn convert(
        &self,
        job: ConversionJob<'_>,
        timings: &mut StageTimings,
    ) -> Result<ConversionResult, ConversionError> {
        let seconds = job.request.timeout.as_secs();
        let engine_error = |err: EngineError| match err {
            EngineError::Timeout => ConversionError::Timeout { seconds },
            other => ConversionError::Engine(other),
        };
        let check_deadline = || {
            if Instant::now() >= job.deadline {
                Err(ConversionError::Timeout { seconds })
            } else {
                Ok(())
            }
        };
        let paths = job.paths;
        let family = job.detection.format.family();
        // Encrypted OOXML: provisional until the engine reports its type.
        let mut format = job.detection.format;

        // The blank-page probe, for output=pdf only. With page images every
        // requested page is rendered, and a rendered page decides
        // BLANK_PAGE itself (verification.rs), so a probe would be thrown
        // away; and a request over MAX_RENDER_PAGES is refused without one.
        let probe = self.probe_dpi.is_some() && job.request.output == Output::Pdf;
        // Set when the original encrypted PDF goes on (decrypt answered
        // Restricted): the render opens it with the password.
        let mut render_password: Option<Password> = None;
        let (pdf, origin, inspection, facts) = match family {
            Family::Document | Family::Presentation | Family::Spreadsheet => {
                // Fail fast: an encrypted package cannot open without one.
                if job.detection.encrypted && job.request.password.is_none() {
                    return Err(ConversionError::Engine(EngineError::PasswordRequired));
                }
                // The office worker may write a copy of a Word package that
                // gives its sections a page size (DEFAULT_PAPER=A4): reserved
                // too, but not part of the PDF's cap. An encrypted package
                // is not a ZIP, and gets no copy.
                let copy = if self.paper_copy
                    && !job.detection.encrypted
                    && matches!(format, Format::Docx | Format::Docm | Format::Dotx)
                {
                    self.limits.paper_copy_bytes(job.upload.size_bytes)
                } else {
                    0
                };
                let max_output_bytes = self
                    .reserve(
                        job.budget,
                        self.limits
                            .pdf_output_bytes(job.upload.size_bytes)
                            .saturating_add(copy),
                    )?
                    .saturating_sub(copy);
                let called = Instant::now();
                let converted = self
                    .office
                    .convert(
                        OfficeJob {
                            input: paths.source.clone(),
                            output: paths.document_pdf.clone(),
                            family,
                            password: job.request.password.clone(),
                            export: job.request.export,
                            sha256: job.upload.sha256,
                            max_output_bytes,
                        },
                        Admission::Queue,
                        job.deadline,
                    )
                    .await
                    .inspect_err(|err| charge_queue(timings, called, err))
                    .map_err(engine_error)?;
                // The worker removed its copy before it answered: the render
                // must not be charged for it.
                job.budget.release(copy);
                timings.add_call(&converted.timing);
                if job.detection.encrypted {
                    format = match converted.doc_type {
                        DocType::Spreadsheet => Format::Xlsx,
                        DocType::Presentation => Format::Pptx,
                        DocType::Text | DocType::Drawing | DocType::Other => Format::Docx,
                    };
                }
                timings.convert = Some(converted.timing.wall);
                timings.convert_engine = Some(converted.timing.engine);
                timings.preflight = Some(converted.timing.preflight);
                check_deadline()?;
                let inspection = self
                    .inspect(
                        &paths.document_pdf,
                        None,
                        Admission::Continue,
                        job.deadline,
                        probe,
                        job.upload.sha256,
                        // The family the worker exported with: the loaded
                        // type, else the requested one (export_family).
                        Some(effective_export(
                            Some(converted.doc_type.family().unwrap_or(family)),
                            job.request.export,
                        )),
                        timings,
                    )
                    .await
                    .map_err(engine_error)?;
                (
                    paths.document_pdf.clone(),
                    PdfOrigin::Converted,
                    inspection,
                    converted.facts,
                )
            }
            Family::Pdf => {
                let inspection = self
                    .inspect(
                        &paths.source,
                        job.request.password.clone(),
                        Admission::Queue,
                        job.deadline,
                        probe,
                        job.upload.sha256,
                        None,
                        timings,
                    )
                    .await
                    .map_err(engine_error)?;
                // A user-password PDF needs its password. An owner-password
                // PDF opens without one and passes through, but a password
                // given for it is checked: decrypt verifies it as the owner
                // password, and a wrong one is InvalidPassword.
                let owner_password_given = inspection.encrypted && job.request.password.is_some();
                if inspection.needs_password || owner_password_given {
                    let password = job
                        .request
                        .password
                        .clone()
                        .ok_or(ConversionError::Engine(EngineError::PasswordRequired))?;
                    check_deadline()?;
                    let max_output_bytes = self.reserve(
                        job.budget,
                        self.limits.decrypt_output_bytes(job.upload.size_bytes),
                    )?;
                    let called = Instant::now();
                    let decrypted = self
                        .pdf
                        .decrypt(
                            DecryptJob {
                                pdf: paths.source.clone(),
                                password: password.clone(),
                                output: paths.document_pdf.clone(),
                                max_output_bytes,
                                check_owner: true,
                                sha256: job.upload.sha256,
                            },
                            Admission::Continue,
                            job.deadline,
                        )
                        .await;
                    match decrypted {
                        Ok(decrypted) => {
                            timings.add_call(&decrypted.timing);
                            timings.decrypt = Some(decrypted.timing.wall);
                            timings.decrypt_engine = Some(decrypted.timing.engine);
                            (
                                paths.document_pdf.clone(),
                                PdfOrigin::Passthrough,
                                inspection,
                                None,
                            )
                        }
                        // Only the user password was given and the owner
                        // restricts the file: no copy without the
                        // restrictions. The original goes on, and the
                        // render opens it with the password.
                        Err(EngineError::Restricted) => {
                            render_password = Some(password);
                            (
                                paths.source.clone(),
                                PdfOrigin::Passthrough,
                                inspection,
                                None,
                            )
                        }
                        Err(err) => {
                            charge_queue(timings, called, &err);
                            return Err(engine_error(err));
                        }
                    }
                } else {
                    (
                        paths.source.clone(),
                        PdfOrigin::Passthrough,
                        inspection,
                        None,
                    )
                }
            }
            // Image inputs are out of v1: detection refuses them with 415,
            // so the service never sees one.
            Family::Image => {
                return Err(ConversionError::Engine(EngineError::Failed(
                    "image inputs are not supported".into(),
                )));
            }
        };
        if inspection.page_count == 0 {
            return Err(ConversionError::Engine(EngineError::Failed(
                "the PDF has no pages".into(),
            )));
        }
        // The worker probes at most VERIFY_PROBE_MAX_PAGES pages and says
        // how many it left out.
        let probe_skipped_pages = if probe { inspection.probe_skipped } else { 0 };

        let pages = if job.request.output == Output::Pdf {
            Vec::new()
        } else {
            check_deadline()?;
            self.render(&pdf, &job, &inspection, render_password, timings)
                .await
                .map_err(|err| match err {
                    ConversionError::Engine(engine) => engine_error(engine),
                    other => other,
                })?
        };

        let started = Instant::now();
        let verdict = verification::verify(&VerifyInput {
            format,
            request: job.request,
            facts: facts.as_ref(),
            page_count: inspection.page_count,
            probe: &inspection.probe,
            rendered: &pages,
            max_edge_px: self.limits.max_edge_px,
            fonts: &self.fonts,
        });
        timings.verify = Some(started.elapsed());

        Ok(ConversionResult {
            pdf,
            origin,
            format,
            family: format.family(),
            encrypted: job.detection.encrypted || inspection.encrypted,
            page_count: inspection.page_count,
            hidden_pages: super::pages::hidden_pages(
                facts.as_ref().and_then(|f| f.slides.as_ref()),
                &job.request.export,
                inspection.page_count,
            ),
            pages,
            warnings: verdict.warnings,
            skipped: verdict.skipped,
            probe_skipped_pages,
        })
    }

    /// Renders the selected pages into `pages/`: the selection is resolved
    /// against the page count, capped by `MAX_RENDER_PAGES`, and reserves
    /// the per-job output cap of the disk budget.
    async fn render(
        &self,
        pdf: &Path,
        job: &ConversionJob<'_>,
        inspection: &Inspection,
        password: Option<Password>,
        timings: &mut StageTimings,
    ) -> Result<Vec<RenderedPage>, ConversionError> {
        let request = job.request;
        let numbers = request
            .pages
            .resolve(inspection.page_count, self.limits.max_render_pages)?;
        let max_edge = request
            .images
            .max_edge
            .unwrap_or(self.limits.max_edge_px)
            .min(self.limits.max_edge_px);
        let max_output_bytes = self.reserve(
            job.budget,
            render_reservation(
                &inspection.pages,
                &numbers,
                request.images.dpi,
                max_edge,
                self.limits.max_output_bytes,
            ),
        )?;
        let wanted = numbers.clone();
        let called = Instant::now();
        let rendered = self
            .pdf
            .render(
                RenderJob {
                    pdf: pdf.to_path_buf(),
                    password,
                    pages: numbers,
                    format: request.images.format,
                    dpi: request.images.dpi,
                    quality: request.images.quality,
                    max_edge,
                    out_dir: job.paths.pages_dir.clone(),
                    sha256: job.upload.sha256,
                    max_output_bytes,
                },
                Admission::Continue,
                job.deadline,
            )
            .await
            .inspect_err(|err| charge_queue(timings, called, err))
            .map_err(ConversionError::Engine)?;
        timings.add_call(&rendered.timing);
        timings.render = Some(rendered.timing.wall);
        timings.render_engine = Some(rendered.timing.engine);
        let got: Vec<u32> = rendered.pages.iter().map(|page| page.number).collect();
        if got != wanted {
            return Err(ConversionError::Engine(EngineError::Internal(format!(
                "the engine rendered pages {got:?}, not {wanted:?}"
            ))));
        }
        Ok(rendered.pages)
    }

    // Two more arguments than clippy's default: the upload's hash and the
    // export, for the poison memory. A struct would only move the list.
    #[allow(clippy::too_many_arguments)]
    async fn inspect(
        &self,
        pdf: &Path,
        password: Option<Password>,
        adm: Admission,
        deadline: Instant,
        probe: bool,
        sha256: [u8; 32],
        export: Option<ExportOptions>,
        timings: &mut StageTimings,
    ) -> Result<Inspection, EngineError> {
        let probe_dpi = if probe { self.probe_dpi } else { None };
        let called = Instant::now();
        let inspection = self
            .pdf
            .inspect(
                InspectJob {
                    pdf: pdf.to_path_buf(),
                    password,
                    probe_dpi,
                    collect_fonts: false,
                    probe_max_pages: self.probe_max_pages,
                    sha256,
                    export,
                },
                adm,
                deadline,
            )
            .await
            .inspect_err(|err| charge_queue(timings, called, err))?;
        timings.add_call(&inspection.timing);
        timings.inspect = Some(inspection.timing.wall);
        let probe = probe_dpi.map(|_| inspection.probe_engine);
        timings.inspect_engine = Some(
            inspection
                .timing
                .engine
                .saturating_sub(probe.unwrap_or(Duration::ZERO)),
        );
        timings.probe_engine = probe;
        Ok(inspection)
    }

    /// Reserves `bytes` of the disk budget and returns it as the job's
    /// `max_output_bytes`.
    fn reserve(&self, budget: &dyn OutputBudget, bytes: u64) -> Result<u64, ConversionError> {
        if budget.reserve(bytes) {
            Ok(bytes)
        } else {
            Err(ConversionError::DiskFull)
        }
    }
}

/// A call that failed before it reached a worker spent its time waiting.
/// Charge it to `queue`, so the error's `Server-Timing` shows the wait. An
/// engine error carries no timing of its own, with one exception.
/// `Poisoned` carries the time the request waited for workers, measured
/// where it was refused: before the first lease (none), after a lease
/// wait, before or after a crash retry's wait, or in a render rerun. That
/// is charged exactly, for every operation. `QueueTimeout` and `QueueFull`
/// charge the call's whole time. A crash whose retry gets no worker in
/// time (`QueueTimeout`) thus also counts the first attempt's time on the
/// worker.
fn charge_queue(timings: &mut StageTimings, called: Instant, err: &EngineError) {
    match err {
        EngineError::Poisoned { queued } => timings.queue += *queued,
        EngineError::QueueTimeout | EngineError::QueueFull => timings.queue += called.elapsed(),
        _ => {}
    }
}

/// Per-page slack on top of the raw bitmap: format headers, PNG filter
/// bytes and the odd image that encodes larger than its pixels.
const RENDER_PAGE_SLACK: u64 = 64 * 1024;
/// Slack per render.
const RENDER_SLACK: u64 = 1024 * 1024;

/// What a render of the pages `numbers` may write, so the job reserves that
/// much of the disk budget rather than the whole per-job cap: per page, the
/// raw 4-byte-per-pixel bitmap at `dpi` and under `max_edge`, plus an
/// eighth and [`RENDER_PAGE_SLACK`]; plus [`RENDER_SLACK`]; at most `cap`
/// (`max_output_bytes`). The render job gets this as its `max_output_bytes`,
/// so it is the engine's hard limit: an encoder that writes more fails the
/// request with `LimitExceeded`. A page the inspection did not size counts
/// as `max_edge` square.
pub fn render_reservation(
    pages: &[PageSize],
    numbers: &[u32],
    dpi: u16,
    max_edge: u32,
    cap: u64,
) -> u64 {
    let edge = f64::from(max_edge.max(1));
    let mut total = RENDER_SLACK;
    for &number in numbers {
        let (width, height) = match usize::try_from(number)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| pages.get(i))
        {
            Some(page) => {
                let scale = f64::from(dpi) / 72.0;
                let (w, h) = (
                    f64::from(page.width_pt.max(0.0)) * scale,
                    f64::from(page.height_pt.max(0.0)) * scale,
                );
                let longest = w.max(h);
                let fit = if longest > edge { edge / longest } else { 1.0 };
                ((w * fit).ceil().max(1.0), (h * fit).ceil().max(1.0))
            }
            None => (edge, edge),
        };
        let raw = (width * height * 4.0) as u64;
        total = total.saturating_add(raw + raw / 8 + RENDER_PAGE_SLACK);
        if total >= cap {
            return cap;
        }
    }
    total.min(cap)
}

/// The export settings that LibreOffice applies to a document of `family`,
/// as `worker::office::filter_options` reads them: the others make no
/// difference to the PDF, so they must not make another poison key.
fn effective_export(family: Option<Family>, export: ExportOptions) -> ExportOptions {
    match family {
        Some(Family::Presentation) => ExportOptions {
            one_page_per_sheet: false,
            ..export
        },
        Some(Family::Spreadsheet) => ExportOptions {
            one_page_per_sheet: export.one_page_per_sheet,
            ..ExportOptions::default()
        },
        _ => ExportOptions::default(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_settings_an_export_uses_make_its_poison_key() {
        use super::super::conversion::SlideNotes;
        let all = ExportOptions {
            notes: SlideNotes::Only,
            include_hidden_slides: true,
            one_page_per_sheet: true,
        };
        assert_eq!(
            effective_export(Some(Family::Document), all),
            ExportOptions::default()
        );
        assert_eq!(
            effective_export(Some(Family::Spreadsheet), all),
            ExportOptions {
                one_page_per_sheet: true,
                ..ExportOptions::default()
            }
        );
        assert_eq!(
            effective_export(Some(Family::Presentation), all),
            ExportOptions {
                one_page_per_sheet: false,
                ..all
            }
        );
    }

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use async_trait::async_trait;
    use futures_util::FutureExt;

    use super::*;
    use crate::domain::conversion::{Output, RawOptions};
    use crate::domain::document::{DocType, Format};
    use crate::domain::engine::{
        Converted, Decrypted, EngineTiming, PageSize, RenderJob, Rendered, WorkerRef,
    };
    use crate::domain::limits::test_limits;

    const WORKER: WorkerRef = WorkerRef {
        pool: "script",
        slot: 0,
        generation: 0,
        job: 0,
    };

    fn timing(engine_ms: u64) -> EngineTiming {
        EngineTiming {
            queue: Duration::from_millis(1),
            lease: Duration::from_millis(1),
            spawn: None,
            bind: Some(Duration::from_millis(2)),
            wall: Duration::from_millis(engine_ms + 3),
            engine: Duration::from_millis(engine_ms),
            preflight: Duration::ZERO,
            cpu: Duration::ZERO,
        }
    }

    /// Records each call and answers from its settings.
    #[derive(Default)]
    struct Script {
        calls: Mutex<Vec<(&'static str, Admission)>>,
        convert_error: Option<EngineError>,
        inspect_error: Option<EngineError>,
        needs_password: bool,
        page_count: Option<u32>,
        /// Render one page fewer than asked.
        drop_a_page: bool,
        doc_type: Option<DocType>,
        rendered: Mutex<Option<RenderJob>>,
        inspected: Mutex<Option<InspectJob>>,
        decrypted: Mutex<Option<DecryptJob>>,
        /// decrypt answers `Restricted`: only the user password was given.
        restricted: bool,
        /// How long `convert` waits before it returns `convert_error`, and
        /// `render` before `render_error`, as a pool that refuses after a
        /// lease wait does.
        error_after: Duration,
        render_error: Option<EngineError>,
    }

    impl Script {
        fn record(&self, op: &'static str, adm: Admission) {
            self.calls.lock().unwrap().push((op, adm));
        }
        fn calls(&self) -> Vec<(&'static str, Admission)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl OfficeEngine for Script {
        async fn convert(
            &self,
            _: OfficeJob,
            adm: Admission,
            _: Instant,
        ) -> Result<Converted, EngineError> {
            self.record("convert", adm);
            if let Some(err) = &self.convert_error {
                std::thread::sleep(self.error_after);
                return Err(err.clone());
            }
            Ok(Converted {
                pdf_bytes: 100,
                doc_type: self.doc_type.unwrap_or(DocType::Text),
                facts: None,
                timing: timing(30),
                worker: WORKER,
            })
        }
    }

    #[async_trait]
    impl PdfEngine for Script {
        async fn inspect(
            &self,
            job: InspectJob,
            adm: Admission,
            _: Instant,
        ) -> Result<Inspection, EngineError> {
            self.record("inspect", adm);
            *self.inspected.lock().unwrap() = Some(job.clone());
            if let Some(err) = &self.inspect_error {
                return Err(err.clone());
            }
            let page_count = self.page_count.unwrap_or(1);
            Ok(Inspection {
                page_count,
                encrypted: self.needs_password,
                needs_password: self.needs_password,
                pages: vec![PageSize {
                    width_pt: 595.0,
                    height_pt: 842.0,
                }],
                // As the worker does: the first probe_max_pages pages.
                probe_skipped: match job.probe_dpi {
                    Some(_) => page_count.saturating_sub(job.probe_max_pages),
                    None => 0,
                },
                probe: match job.probe_dpi {
                    Some(_) => (1..=page_count.min(job.probe_max_pages))
                        .map(|number| crate::domain::engine::ProbedPage {
                            number,
                            objects: 1,
                            flat: false,
                            colour: [255, 255, 255],
                        })
                        .collect(),
                    None => Vec::new(),
                },
                probe_engine: job
                    .probe_dpi
                    .map_or(Duration::ZERO, |_| Duration::from_millis(2)),
                fonts: Vec::new(),
                timing: timing(5),
                worker: WORKER,
            })
        }
        async fn decrypt(
            &self,
            job: DecryptJob,
            adm: Admission,
            _: Instant,
        ) -> Result<Decrypted, EngineError> {
            self.record("decrypt", adm);
            *self.decrypted.lock().unwrap() = Some(job);
            if self.restricted {
                return Err(EngineError::Restricted);
            }
            Ok(Decrypted {
                pdf_bytes: 10,
                timing: timing(4),
                worker: WORKER,
            })
        }
        async fn render(
            &self,
            job: RenderJob,
            adm: Admission,
            _: Instant,
        ) -> Result<Rendered, EngineError> {
            self.record("render", adm);
            if let Some(err) = &self.render_error {
                std::thread::sleep(self.error_after);
                return Err(err.clone());
            }
            let mut numbers = job.pages.clone();
            if self.drop_a_page {
                numbers.pop();
            }
            let pages = numbers
                .iter()
                .map(|&number| RenderedPage {
                    number,
                    file_name: format!("{number:04}.jpg"),
                    width_px: 10,
                    height_px: 10,
                    width_pt: 595.0,
                    height_pt: 842.0,
                    size_bytes: 1,
                    objects: 1,
                    flat: false,
                    colour: [0; 3],
                    effective_dpi: f32::from(job.dpi),
                })
                .collect();
            *self.rendered.lock().unwrap() = Some(job);
            Ok(Rendered {
                pages,
                timing: timing(7),
                workers: vec![WORKER],
            })
        }
    }

    struct Budget {
        left: AtomicU64,
    }

    impl OutputBudget for Budget {
        fn reserve(&self, bytes: u64) -> bool {
            self.left
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(bytes)
                })
                .is_ok()
        }

        fn release(&self, bytes: u64) {
            self.left.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    struct Run {
        result: Result<ConversionResult, ConversionError>,
        timings: StageTimings,
    }

    fn run(script: Arc<Script>, format: Format, raw: RawOptions, budget: u64) -> Run {
        run_detected(script, format, false, raw, budget)
    }

    fn run_detected(
        script: Arc<Script>,
        format: Format,
        encrypted: bool,
        raw: RawOptions,
        budget: u64,
    ) -> Run {
        let service = ConversionService::new(script.clone(), script, test_limits(), true);
        let paths = JobPaths {
            source: PathBuf::from(format!("/job/source.{}", format.extension())),
            document_pdf: PathBuf::from("/job/document.pdf"),
            pages_dir: PathBuf::from("/job/pages"),
        };
        let upload = UploadFacts {
            size_bytes: 1000,
            sha256: [0; 32],
            file_name: None,
        };
        let request = ConversionRequest::new(raw, &test_limits()).unwrap();
        let budget = Budget {
            left: AtomicU64::new(budget),
        };
        let mut timings = StageTimings::default();
        let job = ConversionJob {
            paths: &paths,
            upload: &upload,
            detection: Detection { format, encrypted },
            request: &request,
            budget: &budget,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        let result = service.convert(job, &mut timings).now_or_never().unwrap();
        Run { result, timings }
    }

    /// Without a page selection the probe covers at most
    /// VERIFY_PROBE_MAX_PAGES pages, and the rest are counted as skipped.
    /// With a selection and page images, the rendered pages decide
    /// BLANK_PAGE, so nothing is probed (review, lane 001 item 9).
    /// Every decrypt asks the worker to keep owner restrictions. When it
    /// answers `Restricted` (only the user password was given), the
    /// original encrypted PDF goes on: it is the PDF output, and the render
    /// opens it with the password (review, lane 001 item 9).
    #[test]
    fn a_restricted_pdf_passes_through_and_renders_with_the_password() {
        let script = Arc::new(Script {
            needs_password: true,
            restricted: true,
            page_count: Some(2),
            ..Default::default()
        });
        let raw = RawOptions {
            output: Some(Output::Images),
            password: Some("user".into()),
            ..Default::default()
        };
        let result = run(script.clone(), Format::Pdf, raw, u64::MAX)
            .result
            .unwrap();
        let decrypt = script.decrypted.lock().unwrap().clone().unwrap();
        assert!(
            decrypt.check_owner,
            "every decrypt keeps owner restrictions"
        );
        assert_eq!(result.pdf, PathBuf::from("/job/source.pdf"), "the original");
        let render = script.rendered.lock().unwrap().clone().unwrap();
        assert_eq!(render.pdf, PathBuf::from("/job/source.pdf"));
        assert_eq!(
            render.password.map(|p| p.expose().to_owned()).as_deref(),
            Some("user")
        );
    }

    #[test]
    fn the_probe_is_bounded_and_the_rest_counted() {
        let script = Arc::new(Script {
            page_count: Some(300),
            ..Default::default()
        });
        let result = run(script.clone(), Format::Pdf, RawOptions::default(), u64::MAX)
            .result
            .unwrap();
        let job = script.inspected.lock().unwrap().clone().unwrap();
        assert_eq!(job.probe_dpi, Some(PROBE_DPI));
        assert_eq!(result.probe_skipped_pages, 100);

        let script = Arc::new(Script {
            page_count: Some(300),
            ..Default::default()
        });
        let raw = RawOptions {
            output: Some(Output::Images),
            pages: Some("1-3".into()),
            ..Default::default()
        };
        let result = run(script.clone(), Format::Pdf, raw, u64::MAX)
            .result
            .unwrap();
        let job = script.inspected.lock().unwrap().clone().unwrap();
        assert_eq!(job.probe_dpi, None, "the rendered pages decide");
        assert_eq!(result.probe_skipped_pages, 0);
    }

    /// Page images decide BLANK_PAGE for every page they show, so with
    /// output images or bundle the probe never runs, with or without a
    /// selection. A request over MAX_RENDER_PAGES is then refused without
    /// a probe of any of its pages (performance review, lane 001 item 2).
    #[test]
    fn page_images_need_no_probe_and_are_capped_first() {
        for output in [Output::Images, Output::Bundle] {
            let script = Arc::new(Script {
                page_count: Some(300),
                ..Default::default()
            });
            let raw = RawOptions {
                output: Some(output),
                ..Default::default()
            };
            let result = run(script.clone(), Format::Pdf, raw, u64::MAX)
                .result
                .unwrap();
            let job = script.inspected.lock().unwrap().clone().unwrap();
            assert_eq!(job.probe_dpi, None, "{output:?}: every page is rendered");
            assert_eq!(result.probe_skipped_pages, 0);
        }

        let script = Arc::new(Script {
            page_count: Some(600),
            ..Default::default()
        });
        let raw = RawOptions {
            output: Some(Output::Images),
            ..Default::default()
        };
        let result = run(script.clone(), Format::Pdf, raw, u64::MAX).result;
        assert!(
            matches!(result, Err(ConversionError::LimitExceeded(_))),
            "{result:?}"
        );
        let job = script.inspected.lock().unwrap().clone().unwrap();
        assert_eq!(job.probe_dpi, None, "refused without a probe");
        assert!(
            !script.calls().iter().any(|(op, _)| *op == "render"),
            "{:?}",
            script.calls()
        );
    }

    #[test]
    fn render_reservation_follows_the_pages() {
        let a4 = PageSize {
            width_pt: 595.0,
            height_pt: 842.0,
        };
        let cap = 768 << 20;
        // A4 at 150 dpi: 1240 x 1755 px, 8.7 MB raw.
        let one = render_reservation(&[a4], &[1], 150, 4096, cap);
        assert!(one > 8_700_000 && one < 12_000_000, "{one}");
        // Three pages, three times as much, less the per-render slack.
        let three = render_reservation(&[a4; 3], &[1, 2, 3], 150, 4096, cap);
        assert_eq!(three - (1 << 20), 3 * (one - (1 << 20)));
        // max_edge shrinks the page: 600 px long edge.
        let small = render_reservation(&[a4], &[1], 150, 600, cap);
        assert!(small < one / 4, "{small}");
        // Never more than the per-job cap; an unknown page counts as max_edge square.
        assert_eq!(
            render_reservation(&[a4; 3], &[1, 2, 3], 600, 4096, 10 << 20),
            10 << 20
        );
        let unknown = render_reservation(&[], &[1], 150, 100, cap);
        assert_eq!(unknown, (1 << 20) + 40_000 + 5_000 + 64 * 1024);
    }

    #[test]
    fn office_converts_then_inspects() {
        let script = Arc::new(Script::default());
        let run = run(
            script.clone(),
            Format::Docx,
            RawOptions::default(),
            u64::MAX,
        );
        let result = run.result.unwrap();
        assert_eq!(result.origin, PdfOrigin::Converted);
        assert_eq!(result.pdf, PathBuf::from("/job/document.pdf"));
        assert_eq!(result.page_count, 1);
        assert_eq!(
            script.calls(),
            [
                ("convert", Admission::Queue),
                ("inspect", Admission::Continue)
            ]
        );
        let t = run.timings;
        assert_eq!(t.convert_engine, Some(Duration::from_millis(30)));
        assert_eq!(t.inspect_engine, Some(Duration::from_millis(3)));
        assert_eq!(t.probe_engine, Some(Duration::from_millis(2)));
        assert_eq!(t.queue, Duration::from_millis(2));
        assert_eq!(t.bind, Duration::from_millis(4));
        assert_eq!(t.engine_total(), Duration::from_millis(35));
        assert!(t.verify.is_some());
    }

    #[test]
    fn pdf_passes_through() {
        let script = Arc::new(Script::default());
        let result = run(script.clone(), Format::Pdf, RawOptions::default(), u64::MAX)
            .result
            .unwrap();
        assert_eq!(result.origin, PdfOrigin::Passthrough);
        assert_eq!(result.pdf, PathBuf::from("/job/source.pdf"));
        assert_eq!(script.calls(), [("inspect", Admission::Queue)]);
    }

    #[test]
    fn encrypted_pdf_is_decrypted() {
        let script = Arc::new(Script {
            needs_password: true,
            ..Default::default()
        });
        let raw = RawOptions {
            password: Some("secret".into()),
            ..Default::default()
        };
        let run = run(script.clone(), Format::Pdf, raw, u64::MAX);
        let result = run.result.unwrap();
        assert_eq!(result.origin, PdfOrigin::Passthrough);
        assert_eq!(result.pdf, PathBuf::from("/job/document.pdf"));
        assert!(result.encrypted);
        assert_eq!(
            script.calls(),
            [
                ("inspect", Admission::Queue),
                ("decrypt", Admission::Continue)
            ]
        );
        assert_eq!(run.timings.decrypt_engine, Some(Duration::from_millis(4)));
    }

    #[test]
    fn an_image_never_reaches_an_engine() {
        let script = Arc::new(Script::default());
        let result = run(script.clone(), Format::Png, RawOptions::default(), u64::MAX).result;
        assert!(matches!(
            result,
            Err(ConversionError::Engine(EngineError::Failed(_)))
        ));
        assert!(script.calls().is_empty());
    }

    #[test]
    fn spent_budget_is_disk_full() {
        let script = Arc::new(Script::default());
        let run = run(script.clone(), Format::Docx, RawOptions::default(), 1024);
        assert!(matches!(run.result, Err(ConversionError::DiskFull)));
        assert!(
            script.calls().is_empty(),
            "no engine call without a reservation"
        );
    }

    /// A refusal that comes after a wait for a worker shows the wait as
    /// `queue`. `QueueTimeout` and `QueueFull` charge the call's time.
    /// `Poisoned` charges the wait it carries, exactly, for a render too.
    /// An error from the worker itself charges nothing to `queue`.
    #[test]
    fn a_refusal_after_the_lease_wait_charges_the_queue() {
        let wait = Duration::from_millis(30);
        let queued = Duration::from_millis(7);
        for (error, charged) in [
            (EngineError::QueueTimeout, true),
            (EngineError::Failed("bad".into()), false),
        ] {
            let script = Arc::new(Script {
                convert_error: Some(error.clone()),
                error_after: wait,
                ..Default::default()
            });
            let run = run(script, Format::Docx, RawOptions::default(), u64::MAX);
            assert!(run.result.is_err(), "{error:?}");
            if charged {
                assert!(run.timings.queue >= wait, "{error:?}: {:?}", run.timings);
            } else {
                assert_eq!(run.timings.queue, Duration::ZERO, "{error:?}");
            }
        }
        // The call's 30 ms include work on a worker: only the carried
        // 7 ms of waiting count.
        let script = Arc::new(Script {
            convert_error: Some(EngineError::Poisoned { queued }),
            error_after: wait,
            ..Default::default()
        });
        let convert = run(script, Format::Docx, RawOptions::default(), u64::MAX);
        assert!(convert.result.is_err());
        assert_eq!(convert.timings.queue, queued, "convert Poisoned");
        let images = || RawOptions {
            output: Some(Output::Images),
            ..Default::default()
        };
        let script = Arc::new(Script {
            render_error: Some(EngineError::Poisoned { queued }),
            error_after: wait,
            ..Default::default()
        });
        let run_poisoned = run(script, Format::Pdf, images(), u64::MAX);
        assert!(run_poisoned.result.is_err());
        // The inspection's own queue time (1 ms), plus the render's carried
        // wait.
        assert_eq!(
            run_poisoned.timings.queue,
            Duration::from_millis(1) + queued,
            "render Poisoned"
        );
        for (error, charged) in [
            (EngineError::QueueFull, true),
            (EngineError::Failed("bad".into()), false),
        ] {
            let script = Arc::new(Script {
                render_error: Some(error.clone()),
                error_after: wait,
                ..Default::default()
            });
            let raw = RawOptions {
                output: Some(Output::Images),
                ..Default::default()
            };
            let run = run(script.clone(), Format::Pdf, raw, u64::MAX);
            assert!(run.result.is_err(), "render {error:?}");
            assert!(script.calls().iter().any(|(op, _)| *op == "render"));
            if charged {
                assert!(
                    run.timings.queue >= wait,
                    "render {error:?}: {:?}",
                    run.timings
                );
            } else {
                // The inspection's own queue time (1 ms) stays.
                assert!(
                    run.timings.queue < wait,
                    "render {error:?}: {:?}",
                    run.timings
                );
            }
        }
    }

    #[test]
    fn engine_errors_return_at_once() {
        let script = Arc::new(Script {
            convert_error: Some(EngineError::Failed("bad".into())),
            ..Default::default()
        });
        let run1 = run(
            script.clone(),
            Format::Docx,
            RawOptions::default(),
            u64::MAX,
        );
        assert!(matches!(
            run1.result,
            Err(ConversionError::Engine(EngineError::Failed(_)))
        ));
        assert_eq!(script.calls(), [("convert", Admission::Queue)]);

        let script = Arc::new(Script {
            inspect_error: Some(EngineError::Timeout),
            ..Default::default()
        });
        let raw = RawOptions {
            timeout_seconds: Some(30),
            ..Default::default()
        };
        let run2 = run(script, Format::Pdf, raw, u64::MAX);
        assert!(matches!(
            run2.result,
            Err(ConversionError::Timeout { seconds: 30 })
        ));
    }

    #[test]
    fn images_render_the_selected_pages() {
        let script = Arc::new(Script {
            page_count: Some(12),
            ..Default::default()
        });
        let raw = RawOptions {
            output: Some(Output::Images),
            pages: Some("2-3,10-".into()),
            dpi: Some(600),
            max_edge: Some(8000),
            ..Default::default()
        };
        let run = run(script.clone(), Format::Pdf, raw, u64::MAX);
        let result = run.result.unwrap();
        assert_eq!(
            result.pages.iter().map(|p| p.number).collect::<Vec<_>>(),
            [2, 3, 10, 11, 12]
        );
        assert_eq!(
            script.calls(),
            [
                ("inspect", Admission::Queue),
                ("render", Admission::Continue)
            ]
        );
        let job = script.rendered.lock().unwrap().clone().unwrap();
        assert_eq!((job.dpi, job.max_edge), (300, 4096));
        assert_eq!(job.out_dir, PathBuf::from("/job/pages"));
        // The reservation follows the pages: the script sizes only page 1,
        // so the five selected pages count as max_edge square.
        let a4 = PageSize {
            width_pt: 595.0,
            height_pt: 842.0,
        };
        assert_eq!(
            job.max_output_bytes,
            render_reservation(
                &[a4],
                &[2, 3, 10, 11, 12],
                300,
                4096,
                test_limits().max_output_bytes
            )
        );
        assert_eq!(run.timings.render_engine, Some(Duration::from_millis(7)));
        // MAX_DPI lowered 600 to 300.
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn render_limits_and_a_short_render() {
        let script = Arc::new(Script {
            page_count: Some(501),
            ..Default::default()
        });
        let raw = RawOptions {
            output: Some(Output::Bundle),
            ..Default::default()
        };
        let err = run(script.clone(), Format::Pdf, raw.clone(), u64::MAX)
            .result
            .unwrap_err();
        assert!(matches!(err, ConversionError::LimitExceeded(_)), "{err:?}");
        assert_eq!(script.calls(), [("inspect", Admission::Queue)]);

        let script = Arc::new(Script {
            page_count: Some(3),
            drop_a_page: true,
            ..Default::default()
        });
        let err = run(script, Format::Pdf, raw, u64::MAX).result.unwrap_err();
        assert!(
            matches!(err, ConversionError::Engine(EngineError::Internal(_))),
            "{err:?}"
        );
    }

    #[test]
    fn encrypted_ooxml_needs_a_password_and_takes_the_engines_type() {
        let script = Arc::new(Script::default());
        let run1 = run_detected(
            script.clone(),
            Format::Docx,
            true,
            RawOptions::default(),
            u64::MAX,
        );
        assert!(matches!(
            run1.result,
            Err(ConversionError::Engine(EngineError::PasswordRequired))
        ));
        assert!(
            script.calls().is_empty(),
            "no engine call without a password"
        );

        let script = Arc::new(Script {
            doc_type: Some(DocType::Spreadsheet),
            ..Default::default()
        });
        let raw = RawOptions {
            password: Some("secret".into()),
            ..Default::default()
        };
        let result = run_detected(script, Format::Docx, true, raw, u64::MAX)
            .result
            .unwrap();
        assert_eq!(result.format, Format::Xlsx);
        assert_eq!(result.family, Family::Spreadsheet);
        assert!(result.encrypted);
    }

    #[test]
    fn zero_pages_fail() {
        let script = Arc::new(Script {
            page_count: Some(0),
            ..Default::default()
        });
        let run = run(script, Format::Pdf, RawOptions::default(), u64::MAX);
        assert!(matches!(
            run.result,
            Err(ConversionError::Engine(EngineError::Failed(_)))
        ));
    }
}
