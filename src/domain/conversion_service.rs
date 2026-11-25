//! The conversion service: routes a detected upload through the engines.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::conversion::{ConversionRequest, ConversionResult, PdfOrigin, StageTimings};
use super::detection::Detection;
use super::document::Family;
use super::engine::{
    Admission, AssembleJob, DecryptJob, EngineError, InspectJob, Inspection, OfficeEngine,
    OfficeJob, PdfEngine,
};
use super::limits::Limits;
use super::secret::Password;
use super::verification::Warning;

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
    pub crc32: u32,
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
}

/// Routes by family: office files to the office engine, PDFs to inspect
/// (and decrypt), images to assemble.
pub struct ConversionService {
    pub(super) office: Arc<dyn OfficeEngine>,
    pub(super) pdf: Arc<dyn PdfEngine>,
    pub(super) limits: Limits,
    /// `Some(18)` with `VERIFY_PROBE_RENDER`.
    pub(super) probe_dpi: Option<u16>,
}

/// The DPI of the probe render for `BLANK_PAGE` (V3).
pub const PROBE_DPI: u16 = 18;

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
        }
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

        let (pdf, origin, inspection) = match family {
            Family::Document | Family::Presentation | Family::Spreadsheet => {
                let max_output_bytes = self.reserve(
                    job.budget,
                    self.limits.pdf_output_bytes(job.upload.size_bytes),
                )?;
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
                    .map_err(engine_error)?;
                timings.add_call(&converted.timing);
                timings.convert = Some(converted.timing.wall);
                timings.convert_engine = Some(converted.timing.engine);
                check_deadline()?;
                let inspection = self
                    .inspect(
                        &paths.document_pdf,
                        None,
                        Admission::Continue,
                        job.deadline,
                        timings,
                    )
                    .await
                    .map_err(engine_error)?;
                (paths.document_pdf.clone(), PdfOrigin::Converted, inspection)
            }
            Family::Pdf => {
                let inspection = self
                    .inspect(
                        &paths.source,
                        job.request.password.clone(),
                        Admission::Queue,
                        job.deadline,
                        timings,
                    )
                    .await
                    .map_err(engine_error)?;
                if inspection.needs_password {
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
                    let decrypted = self
                        .pdf
                        .decrypt(
                            DecryptJob {
                                pdf: paths.source.clone(),
                                password,
                                output: paths.document_pdf.clone(),
                                max_output_bytes,
                            },
                            Admission::Continue,
                            job.deadline,
                        )
                        .await
                        .map_err(engine_error)?;
                    timings.add_call(&decrypted.timing);
                    timings.decrypt = Some(decrypted.timing.wall);
                    timings.decrypt_engine = Some(decrypted.timing.engine);
                    (
                        paths.document_pdf.clone(),
                        PdfOrigin::Passthrough,
                        inspection,
                    )
                } else {
                    (paths.source.clone(), PdfOrigin::Passthrough, inspection)
                }
            }
            Family::Image => {
                let max_output_bytes = self.reserve(
                    job.budget,
                    self.limits.pdf_output_bytes(job.upload.size_bytes),
                )?;
                let assembled = self
                    .pdf
                    .assemble(
                        AssembleJob {
                            images: vec![paths.source.clone()],
                            output: paths.document_pdf.clone(),
                            max_output_bytes,
                        },
                        Admission::Queue,
                        job.deadline,
                    )
                    .await
                    .map_err(engine_error)?;
                timings.add_call(&assembled.timing);
                timings.convert = Some(assembled.timing.wall);
                timings.convert_engine = Some(assembled.timing.engine);
                check_deadline()?;
                let inspection = self
                    .inspect(
                        &paths.document_pdf,
                        None,
                        Admission::Continue,
                        job.deadline,
                        timings,
                    )
                    .await
                    .map_err(engine_error)?;
                (paths.document_pdf.clone(), PdfOrigin::Assembled, inspection)
            }
        };
        if inspection.page_count == 0 {
            return Err(ConversionError::Engine(EngineError::Failed(
                "the PDF has no pages".into(),
            )));
        }

        let started = Instant::now();
        let warnings = verify(&inspection);
        timings.verify = Some(started.elapsed());

        Ok(ConversionResult {
            pdf,
            origin,
            format: job.detection.format,
            family,
            encrypted: job.detection.encrypted || inspection.encrypted,
            page_count: inspection.page_count,
            pages: Vec::new(),
            warnings,
        })
    }

    async fn inspect(
        &self,
        pdf: &Path,
        password: Option<Password>,
        adm: Admission,
        deadline: Instant,
        timings: &mut StageTimings,
    ) -> Result<Inspection, EngineError> {
        let inspection = self
            .pdf
            .inspect(
                InspectJob {
                    pdf: pdf.to_path_buf(),
                    password,
                    probe_dpi: self.probe_dpi,
                    collect_fonts: false,
                },
                adm,
                deadline,
            )
            .await?;
        timings.add_call(&inspection.timing);
        timings.inspect = Some(inspection.timing.wall);
        let probe = self.probe_dpi.map(|_| inspection.probe_engine);
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

/// The warnings the inspection gives. The rules V1 to V4 arrive with the
/// office worker's pre-flight (step 11); until then there are none.
fn verify(_inspection: &Inspection) -> Vec<Warning> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use async_trait::async_trait;
    use futures_util::FutureExt;

    use super::*;
    use crate::domain::conversion::RawOptions;
    use crate::domain::document::{DocType, Format};
    use crate::domain::engine::{
        Assembled, Converted, Decrypted, EngineTiming, PageSize, RenderJob, Rendered, WorkerRef,
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
                return Err(err.clone());
            }
            Ok(Converted {
                pdf_bytes: 100,
                doc_type: DocType::Text,
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
            if let Some(err) = &self.inspect_error {
                return Err(err.clone());
            }
            Ok(Inspection {
                page_count: self.page_count.unwrap_or(1),
                encrypted: self.needs_password,
                needs_password: self.needs_password,
                pages: vec![PageSize {
                    width_pt: 595.0,
                    height_pt: 842.0,
                }],
                probe: Vec::new(),
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
            _: DecryptJob,
            adm: Admission,
            _: Instant,
        ) -> Result<Decrypted, EngineError> {
            self.record("decrypt", adm);
            Ok(Decrypted {
                pdf_bytes: 10,
                timing: timing(4),
                worker: WORKER,
            })
        }
        async fn render(
            &self,
            _: RenderJob,
            adm: Admission,
            _: Instant,
        ) -> Result<Rendered, EngineError> {
            self.record("render", adm);
            Err(EngineError::Unavailable)
        }
        async fn assemble(
            &self,
            _: AssembleJob,
            adm: Admission,
            _: Instant,
        ) -> Result<Assembled, EngineError> {
            self.record("assemble", adm);
            Ok(Assembled {
                page_count: 1,
                pdf_bytes: 10,
                timing: timing(6),
                worker: WORKER,
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
    }

    struct Run {
        result: Result<ConversionResult, ConversionError>,
        timings: StageTimings,
    }

    fn run(script: Arc<Script>, format: Format, raw: RawOptions, budget: u64) -> Run {
        let service = ConversionService::new(script.clone(), script, test_limits(), true);
        let paths = JobPaths {
            source: PathBuf::from(format!("/job/source.{}", format.extension())),
            document_pdf: PathBuf::from("/job/document.pdf"),
            pages_dir: PathBuf::from("/job/pages"),
        };
        let upload = UploadFacts {
            size_bytes: 1000,
            sha256: [0; 32],
            crc32: 0,
            file_name: None,
        };
        let (request, _) = ConversionRequest::new(raw, &test_limits()).unwrap();
        let budget = Budget {
            left: AtomicU64::new(budget),
        };
        let mut timings = StageTimings::default();
        let job = ConversionJob {
            paths: &paths,
            upload: &upload,
            detection: Detection {
                format,
                encrypted: false,
            },
            request: &request,
            budget: &budget,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        let result = service.convert(job, &mut timings).now_or_never().unwrap();
        Run { result, timings }
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
    fn image_is_assembled() {
        let script = Arc::new(Script::default());
        let result = run(script.clone(), Format::Png, RawOptions::default(), u64::MAX)
            .result
            .unwrap();
        assert_eq!(result.origin, PdfOrigin::Assembled);
        assert_eq!(
            script.calls(),
            [
                ("assemble", Admission::Queue),
                ("inspect", Admission::Continue)
            ]
        );
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
