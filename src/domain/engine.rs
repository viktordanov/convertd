//! The engine seam between the request path (`rest/`, `domain/`) and the
//! engine workers (`engines/`, `worker/`).
//!
//! # Contract for every async method (the engines guarantee it)
//!
//! 1. **Deadline.** `deadline` is absolute. The engine enforces it itself
//!    (the domain has no timer): at the deadline it kills the worker's
//!    process group and returns `Timeout`. It also bounds the queue wait.
//!    `rest/` adds a backstop of `deadline + 250 ms` around the whole
//!    service call, which drops the future (rule 2).
//! 2. **Cancel on drop.** If the returned future is dropped, the worker
//!    running the job is killed and its slot recycled. No worker writes
//!    into the job directory after the future is gone.
//! 3. **Files only.** Engines read and write the paths they are given,
//!    inside the job directory. Document bytes never cross IPC.
//! 4. **Timing.** `EngineTiming::engine` is measured inside the worker,
//!    from just before the document is opened to just after its last
//!    engine call. For a call split into chunks, `engine` is the longest
//!    chunk and `wall` the whole call.
//! 5. **Output budget.** Every job that writes carries `max_output_bytes`.
//!    The engine stops at that many bytes and returns `LimitExceeded`.
//! 6. **Admission.** `Admission::Queue` may fail with `QueueFull` or
//!    `QueueTimeout`. `Admission::Continue` waits up to the deadline and
//!    never returns them. The first engine call of a request uses
//!    `Queue`, every later one `Continue`.
//!
//! An engine that crashes is retried once inside the engine, only with at
//! least 1 s left, never after a timeout or an engine-reported error.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::conversion::{ExportOptions, ImageFormat};
use super::document::{DocType, Family};
use super::secret::Password;
use super::verification::SourceFacts;

/// Whether an engine call may be refused for a full queue (rule 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The first engine call of a request: may give `QueueFull` or
    /// `QueueTimeout`.
    Queue,
    /// Every later call: waits up to the deadline.
    Continue,
}

/// Engine failures. Mapped to statuses only in `rest/`:
///
/// | Variant | Status, code |
/// |---|---|
/// | `QueueFull` | 429 `TOO_MANY_REQUESTS`, `Retry-After: 1` |
/// | `QueueTimeout` | 429, `Retry-After: QUEUE_WAIT_SECONDS` |
/// | `Unavailable` | 503 `SERVICE_UNAVAILABLE`, `Retry-After: 5` |
/// | `DiskFull` | 503, `Retry-After: 5` |
/// | `Timeout` | 504 `CONVERSION_TIMEOUT` |
/// | `PasswordRequired`, `InvalidPassword` | 422 |
/// | `LimitExceeded` | 422 `LIMIT_EXCEEDED` |
/// | `Failed`, `Crashed`, `Poisoned` | 422 `CONVERSION_FAILED` |
/// | `Internal` | 500 |
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("engine queue is full")]
    QueueFull,
    #[error("engine queue wait expired")]
    QueueTimeout,
    /// Draining, or no worker can start.
    #[error("engine is unavailable")]
    Unavailable,
    /// ENOSPC or EDQUOT, or statvfs below 1 MiB free: caused by other jobs
    /// and worth a retry. Not this job's own cap, which is `LimitExceeded`.
    #[error("no space left for the job")]
    DiskFull,
    /// The deadline passed; the worker's process group is already killed.
    #[error("engine deadline expired")]
    Timeout,
    #[error("document needs a password")]
    PasswordRequired,
    #[error("document password is wrong")]
    InvalidPassword,
    /// Decrypt with `check_owner`: the password opened the file but did not
    /// unlock the owner, and the owner restricts it. Nothing was written;
    /// the service passes the original encrypted PDF on.
    #[error("the document keeps its owner restrictions")]
    Restricted,
    /// This job's own cap: `max_output_bytes`, EFBIG from its own
    /// `RLIMIT_FSIZE`, the RSS kill, a pixel or zip bomb. The message names
    /// the limit and never holds a path.
    #[error("{0}")]
    LimitExceeded(String),
    /// Load or export failed, empty document, no pages.
    #[error("{0}")]
    Failed(String),
    /// The worker crashed twice (the engine retries once on a fresh worker).
    #[error("engine crashed")]
    Crashed,
    /// The file's hash recently crashed or timed out a worker. `queued` is
    /// the time the request waited for workers before the refusal, measured
    /// where it was refused: the service charges it to `queue`.
    #[error("document is known to crash the engine")]
    Poisoned { queued: Duration },
    #[error("{0}")]
    Internal(String),
}

/// Where one engine call's time went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineTiming {
    /// Waiting in the bounded queue (`Admission::Queue` only).
    pub queue: Duration,
    /// From a free slot to a worker holding the job.
    pub lease: Duration,
    /// fork/exec to the worker's first "ready", when a worker started on
    /// this request's critical path; `None` when a pooled worker was leased.
    pub spawn: Option<Duration>,
    /// Worker-reported library bind or LibreOfficeKit init; `None` when
    /// pooled.
    pub bind: Option<Duration>,
    /// The whole call as the supervisor saw it.
    pub wall: Duration,
    /// Inside the worker (rule 4); the longest chunk for split calls.
    pub engine: Duration,
    /// The office worker's pre-flight scan before the load. Worker time
    /// but not engine time, so it counts as overhead;
    /// zero for calls without pre-flight.
    pub preflight: Duration,
    /// The worker CPU time, user plus system, of the calls that answered:
    /// summed over the chunks of a split render, not the longest.
    pub cpu: Duration,
}

/// Which worker ran a job, for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerRef {
    pub pool: &'static str,
    pub slot: u16,
    pub generation: u32,
    pub job: u32,
}

// ------------------------------------------------------------- office ----

/// An office document to PDF.
#[derive(Debug, Clone)]
pub struct OfficeJob {
    /// `<job>/source.<ext>`, with the detected format's canonical extension.
    pub input: PathBuf,
    /// `<job>/document.pdf`, created by the worker.
    pub output: PathBuf,
    pub family: Family,
    pub password: Option<Password>,
    pub export: ExportOptions,
    /// For poison memory.
    pub sha256: [u8; 32],
    /// Rule 5.
    pub max_output_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Converted {
    pub pdf_bytes: u64,
    /// Gives the family and `Convert-Source-Format` for encrypted OOXML.
    pub doc_type: DocType,
    /// The office worker's pre-flight scan of the source;
    /// `None` when it did not run.
    pub facts: Option<SourceFacts>,
    pub timing: EngineTiming,
    pub worker: WorkerRef,
}

// ---------------------------------------------------------------- pdf ----

#[derive(Debug, Clone)]
pub struct InspectJob {
    pub pdf: PathBuf,
    /// Only for PDF uploads; converted PDFs are never encrypted.
    pub password: Option<Password>,
    /// `Some(18)` when `VERIFY_PROBE_RENDER` is on: a grayscale probe
    /// render for V3 at this DPI.
    pub probe_dpi: Option<u16>,
    /// Unused in v1.
    pub collect_fonts: bool,
    /// With `probe_dpi`: probe pages `1..=min(page_count, probe_max_pages)`
    /// only (`VERIFY_PROBE_MAX_PAGES`); `u32::MAX` probes every page.
    pub probe_max_pages: u32,
    /// The upload's SHA-256, also for a converted or decrypted PDF: the
    /// poison memory's key, one entry per document across all operations.
    pub sha256: [u8; 32],
    /// The office export that made this PDF, `None` for a PDF upload. Two
    /// exports of one upload are different PDFs for an inspect timeout.
    pub export: Option<crate::domain::conversion::ExportOptions>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSize {
    pub width_pt: f32,
    pub height_pt: f32,
}

/// Facts, not verdicts: the `BLANK_PAGE` rule lives in the domain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbedPage {
    pub number: u32,
    pub objects: u32,
    pub flat: bool,
    pub colour: [u8; 3],
}

/// An encrypted PDF without a password gives `Err(PasswordRequired)`;
/// with a wrong one `Err(InvalidPassword)`; with the right one `Ok` with
/// `encrypted` and `needs_password`. When `needs_password`, the service
/// calls `decrypt` before anything else reads the file.
///
/// An owner-password-only PDF (`encrypted`, not `needs_password`) opens
/// without a password. With none given it passes through unchanged. With
/// one given, the service calls `decrypt`, which verifies it as the owner
/// password: verified, it writes the decrypted copy; not verified,
/// `Err(InvalidPassword)`.
///
/// Every decrypt carries `check_owner`. A user-password PDF opened with
/// its user password, whose owner restricts it, gives `Err(Restricted)`:
/// the service then passes the original encrypted PDF on, and inspect and
/// render keep opening it with the password.
#[derive(Debug, Clone)]
pub struct Inspection {
    pub page_count: u32,
    pub encrypted: bool,
    pub needs_password: bool,
    /// All pages, in order.
    pub pages: Vec<PageSize>,
    /// Only with `probe_dpi`.
    pub probe: Vec<ProbedPage>,
    /// The pages that `probe_max_pages` left unprobed.
    pub probe_skipped: u32,
    /// Probe render time inside the worker, part of `timing.engine`;
    /// reported apart as `Server-Timing` `probe_engine`.
    pub probe_engine: Duration,
    /// Only with `collect_fonts`.
    pub fonts: Vec<String>,
    pub timing: EngineTiming,
    pub worker: WorkerRef,
}

/// An encrypted PDF upload to an unencrypted copy.
#[derive(Debug, Clone)]
pub struct DecryptJob {
    /// `<job>/source.pdf`.
    pub pdf: PathBuf,
    pub password: Password,
    /// `<job>/document.pdf`.
    pub output: PathBuf,
    pub max_output_bytes: u64,
    /// Keep owner restrictions the password did not unlock: answer
    /// `Restricted` instead of writing a copy without them. The service
    /// sets it on every decrypt.
    pub check_owner: bool,
    /// The upload's SHA-256: the poison memory's key.
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct Decrypted {
    pub pdf_bytes: u64,
    pub timing: EngineTiming,
    pub worker: WorkerRef,
}

#[derive(Debug, Clone)]
pub struct RenderJob {
    /// Needs a password only when decrypt answered `Restricted` and the
    /// original encrypted PDF goes on; otherwise the service decrypted first.
    pub pdf: PathBuf,
    pub password: Option<Password>,
    /// Resolved, sorted, de-duplicated, 1-based, at most `page_count` and
    /// `MAX_RENDER_PAGES` long. The engine may split it across workers.
    pub pages: Vec<u32>,
    pub format: ImageFormat,
    /// After `MAX_DPI`.
    pub dpi: u16,
    pub quality: u8,
    /// `min(request, MAX_EDGE_PX)`.
    pub max_edge: u32,
    /// `<job>/pages/`, created by the supervisor; the engine writes
    /// `NNNN.<ext>` (at least 4 digits, 1-based) into it.
    pub out_dir: PathBuf,
    pub sha256: [u8; 32],
    /// Rule 5: total bytes of all page files of this job.
    pub max_output_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderedPage {
    pub number: u32,
    /// `"0001.jpg"`, inside `out_dir`.
    pub file_name: String,
    pub width_px: u32,
    pub height_px: u32,
    pub width_pt: f32,
    pub height_pt: f32,
    /// Informational: the bundle plan takes sizes from its own `fstat`.
    pub size_bytes: u64,
    pub objects: u32,
    pub flat: bool,
    pub colour: [u8; 3],
    /// Below the requested DPI when `max_edge` reduced the page; the domain
    /// raises `RENDER_CLAMPED` from it.
    pub effective_dpi: f32,
}

#[derive(Debug, Clone)]
pub struct Rendered {
    /// In page order.
    pub pages: Vec<RenderedPage>,
    pub timing: EngineTiming,
    pub workers: Vec<WorkerRef>,
}

// ------------------------------------------------------------- traits ----
// Used as Arc<dyn …>, so TEST_ENGINE picks real or fake engines at run
// time. The cost is one boxed future per call.

#[async_trait]
pub trait OfficeEngine: Send + Sync {
    async fn convert(
        &self,
        job: OfficeJob,
        adm: Admission,
        deadline: Instant,
    ) -> Result<Converted, EngineError>;
}

#[async_trait]
pub trait PdfEngine: Send + Sync {
    async fn inspect(
        &self,
        job: InspectJob,
        adm: Admission,
        deadline: Instant,
    ) -> Result<Inspection, EngineError>;

    async fn decrypt(
        &self,
        job: DecryptJob,
        adm: Admission,
        deadline: Instant,
    ) -> Result<Decrypted, EngineError>;

    async fn render(
        &self,
        job: RenderJob,
        adm: Admission,
        deadline: Instant,
    ) -> Result<Rendered, EngineError>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Proves the traits are object safe and complete without the real engines.
    struct Null;

    const WORKER: WorkerRef = WorkerRef {
        pool: "null",
        slot: 0,
        generation: 0,
        job: 0,
    };

    #[async_trait]
    impl OfficeEngine for Null {
        async fn convert(
            &self,
            _: OfficeJob,
            _: Admission,
            _: Instant,
        ) -> Result<Converted, EngineError> {
            Err(EngineError::Unavailable)
        }
    }

    #[async_trait]
    impl PdfEngine for Null {
        async fn inspect(
            &self,
            _: InspectJob,
            _: Admission,
            _: Instant,
        ) -> Result<Inspection, EngineError> {
            Ok(Inspection {
                page_count: 1,
                encrypted: false,
                needs_password: false,
                pages: vec![PageSize {
                    width_pt: 595.0,
                    height_pt: 842.0,
                }],
                probe: Vec::new(),
                probe_skipped: 0,
                probe_engine: Duration::ZERO,
                fonts: Vec::new(),
                timing: EngineTiming::default(),
                worker: WORKER,
            })
        }
        async fn decrypt(
            &self,
            _: DecryptJob,
            _: Admission,
            _: Instant,
        ) -> Result<Decrypted, EngineError> {
            Err(EngineError::Unavailable)
        }
        async fn render(
            &self,
            _: RenderJob,
            _: Admission,
            _: Instant,
        ) -> Result<Rendered, EngineError> {
            Err(EngineError::Unavailable)
        }
    }

    #[test]
    fn traits_are_object_safe() {
        let office: Arc<dyn OfficeEngine> = Arc::new(Null);
        let pdf: Arc<dyn PdfEngine> = Arc::new(Null);
        let deadline = Instant::now() + Duration::from_secs(1);
        let job = InspectJob {
            pdf: PathBuf::from("x.pdf"),
            password: None,
            probe_dpi: None,
            collect_fonts: false,
            probe_max_pages: u32::MAX,
            sha256: [0; 32],
            export: None,
        };
        let inspection =
            futures_util::FutureExt::now_or_never(pdf.inspect(job, Admission::Queue, deadline))
                .unwrap()
                .unwrap();
        assert_eq!(inspection.page_count, 1);
        drop(office);
    }

    /// The domain must not depend on the HTTP, async runtime, serde, IPC or
    /// FFI crates.
    #[test]
    fn domain_has_no_forbidden_imports() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/domain");
        let forbidden = [
            "tokio",
            "axum",
            "serde",
            "hyper",
            "tower",
            "utoipa",
            "crate::ipc",
            "crate::rest",
            "crate::engines",
            "libc",
            "rustix",
            "pdfium",
            "libreofficekit",
        ];
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            // Only code before the tests module counts.
            let code = text.split("#[cfg(test)]").next().unwrap_or_default();
            for line in code.lines().map(str::trim) {
                if line.starts_with("//") {
                    continue;
                }
                for name in forbidden {
                    let used = line.contains(&format!("{name}::"))
                        || line.contains(&format!("use {name}"))
                        || line.contains(&format!("#[{name}"));
                    assert!(!used, "{} uses {name}: {line}", path.display());
                }
            }
            checked += 1;
        }
        assert!(checked >= 8, "checked {checked} files");
    }
}
