//! Level-A fake engines (lane 001): [`FakeOffice`] and [`FakePdf`] behind
//! the domain traits, selected with `TEST_ENGINE=fake` in `test-hooks`
//! builds. Also [`Unwired`], which stands in until lane 002's engines are
//! wired.
//!
//! No real pool, queue, deadline kill or process: a 429 comes from
//! `FAKE:TOO_MANY_REQUESTS` or from the service's own permit, a 504 from
//! the fake's own deadline check or from the handler's backstop
//! (`FAKE:HANG`). Tests that mean to prove queue or kill behaviour belong
//! at level B (lane 002's `convertd worker test`).
//!
//! In-process tests set a [`FakeBehavior`] directly. With `test-hooks`,
//! `FakeOffice::from_markers` and `FakePdf::from_markers` read the `FAKE:`
//! markers of INTERFACES.md §10 from the first 1 MiB of each input
//! instead. Markers are never read without the feature.

use std::io::Cursor;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use crate::domain::conversion::ImageFormat;
use crate::domain::document::{DocType, Family};
use crate::domain::engine::{
    Admission, AssembleJob, Assembled, Converted, DecryptJob, Decrypted, EngineError, EngineTiming,
    InspectJob, Inspection, OfficeEngine, OfficeJob, PageSize, PdfEngine, ProbedPage, RenderJob,
    Rendered, RenderedPage, WorkerRef,
};
use crate::domain::readiness::{PoolStatus, Readiness, ReadinessSnapshot, ReadinessStatus};

/// A valid one-page A4 PDF (601 bytes; checked with `qpdf --check`).
pub const ONE_PAGE_PDF: &[u8] = b"\
%PDF-1.4\n\
%\xe2\xe3\xcf\xd3\n\
1 0 obj\n\
<< /Type /Catalog /Pages 2 0 R >>\n\
endobj\n\
2 0 obj\n\
<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n\
endobj\n\
3 0 obj\n\
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\n\
endobj\n\
4 0 obj\n\
<< /Length 51 >>\n\
stream\n\
BT /F1 24 Tf 72 760 Td (convertd fake engine) Tj ET\n\
endstream\n\
endobj\n\
5 0 obj\n\
<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\n\
endobj\n\
xref\n\
0 6\n\
0000000000 65535 f \n\
0000000015 00000 n \n\
0000000064 00000 n \n\
0000000121 00000 n \n\
0000000247 00000 n \n\
0000000348 00000 n \n\
trailer\n\
<< /Size 6 /Root 1 0 R >>\n\
startxref\n\
418\n\
%%EOF\n\
";

/// How many leading bytes of an input are scanned for markers and for
/// `/Encrypt`.
const SCAN_BYTES: u64 = 1024 * 1024;

/// What a fake does on its next calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeBehavior {
    /// Answer at once (after `engine_time`).
    Succeed,
    /// Answer after this long, but never past the deadline: at the deadline
    /// return `Timeout`, as a real engine must.
    Delay(Duration),
    /// Never answer, ignoring the deadline: the handler's backstop must
    /// turn it into a 504.
    Hang,
    /// Fail with this error.
    Fail(EngineError),
    /// Succeed; the PDF has this many pages.
    Pages(u32),
    /// Panic inside the engine call (`FAKE:PANIC`).
    Panic,
}

/// The settings both fakes share.
#[derive(Debug)]
struct Settings {
    behavior: Mutex<FakeBehavior>,
    /// Read `FAKE:` markers from the input (test-hooks builds only).
    markers: bool,
    /// Slept and reported as `EngineTiming::engine`.
    engine_time: Duration,
    jobs: AtomicU32,
}

impl Settings {
    fn new(behavior: FakeBehavior, markers: bool) -> Self {
        Self {
            behavior: Mutex::new(behavior),
            markers,
            engine_time: Duration::ZERO,
            jobs: AtomicU32::new(0),
        }
    }

    fn behavior(&self) -> FakeBehavior {
        self.behavior
            .lock()
            .map(|b| b.clone())
            .unwrap_or(FakeBehavior::Succeed)
    }

    fn worker(&self, pool: &'static str) -> WorkerRef {
        WorkerRef {
            pool,
            slot: 0,
            generation: 0,
            job: self.jobs.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// The behaviour for one call: the configured one, or the input's
    /// marker when markers are on.
    async fn behavior_for(&self, input: &Path) -> Result<FakeBehavior, EngineError> {
        if !self.markers {
            return Ok(self.behavior());
        }
        let head = read_head(input).await?;
        Ok(marker_behavior(&head).unwrap_or(FakeBehavior::Succeed))
    }

    /// Plays `behavior` against the deadline. `Ok(pages)` means "go on and
    /// do the work", with the page count a `Pages` behaviour asks for.
    async fn play(&self, behavior: FakeBehavior, deadline: Instant) -> Result<Played, EngineError> {
        if Instant::now() >= deadline {
            return Err(EngineError::Timeout);
        }
        let (pages, delay) = match behavior {
            FakeBehavior::Succeed => (None, Duration::ZERO),
            FakeBehavior::Pages(pages) => (Some(pages), Duration::ZERO),
            FakeBehavior::Fail(err) => return Err(err),
            FakeBehavior::Hang => std::future::pending().await,
            FakeBehavior::Panic => panic!("FAKE:PANIC"),
            FakeBehavior::Delay(delay) => (None, delay),
        };
        let engine = delay + self.engine_time;
        if !engine.is_zero() {
            let until = Instant::now() + engine;
            if until > deadline {
                tokio::time::sleep_until(deadline.into()).await;
                return Err(EngineError::Timeout);
            }
            tokio::time::sleep_until(until.into()).await;
        }
        Ok(Played { pages, engine })
    }

    fn timing(&self, started: Instant, engine: Duration) -> EngineTiming {
        EngineTiming {
            queue: Duration::ZERO,
            lease: Duration::ZERO,
            spawn: None,
            bind: None,
            wall: started.elapsed(),
            engine,
        }
    }
}

/// What [`Settings::play`] did.
struct Played {
    pages: Option<u32>,
    engine: Duration,
}

/// The marker's behaviour, when the feature that reads markers is on.
#[cfg(feature = "test-hooks")]
fn marker_behavior(head: &[u8]) -> Option<FakeBehavior> {
    use super::fake_markers::{self, Marker, MarkerCode};
    Some(match fake_markers::parse(head)? {
        Marker::DelayMs(ms) => FakeBehavior::Delay(Duration::from_millis(ms)),
        Marker::Pages(pages) => FakeBehavior::Pages(pages),
        Marker::Hang => FakeBehavior::Hang,
        Marker::Panic => FakeBehavior::Panic,
        Marker::Code(code) => FakeBehavior::Fail(match code {
            MarkerCode::PasswordRequired => EngineError::PasswordRequired,
            MarkerCode::InvalidPassword => EngineError::InvalidPassword,
            MarkerCode::LimitExceeded => {
                EngineError::LimitExceeded("the fake engine's limit".into())
            }
            MarkerCode::ConversionFailed => EngineError::Failed("the fake engine failed".into()),
            MarkerCode::TooManyRequests => EngineError::QueueFull,
            MarkerCode::ServiceUnavailable => EngineError::Unavailable,
            MarkerCode::InternalError => {
                EngineError::Internal("the fake engine's internal error".into())
            }
        }),
    })
}

#[cfg(not(feature = "test-hooks"))]
fn marker_behavior(_head: &[u8]) -> Option<FakeBehavior> {
    None
}

/// The first [`SCAN_BYTES`] of `path`.
async fn read_head(path: &Path) -> Result<Vec<u8>, EngineError> {
    let file = tokio::fs::File::open(path).await.map_err(io_error)?;
    let mut head = Vec::new();
    file.take(SCAN_BYTES)
        .read_to_end(&mut head)
        .await
        .map_err(io_error)?;
    Ok(head)
}

fn io_error(err: std::io::Error) -> EngineError {
    if matches!(
        err.kind(),
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
    ) {
        EngineError::DiskFull
    } else {
        EngineError::Internal(format!("fake engine I/O: {err}"))
    }
}

/// Writes `bytes` to `path`, stopping at `max_output_bytes` (rule 5).
async fn write_capped(
    path: &Path,
    bytes: &[u8],
    max_output_bytes: u64,
) -> Result<u64, EngineError> {
    if bytes.len() as u64 > max_output_bytes {
        return Err(EngineError::LimitExceeded(format!(
            "output exceeds the {max_output_bytes} byte limit"
        )));
    }
    tokio::fs::write(path, bytes).await.map_err(io_error)?;
    Ok(bytes.len() as u64)
}

/// The fake PDF, with a `%FAKE:PAGES=n` comment after `%%EOF` so the fake
/// inspect of a converted document reports `n` pages.
fn pdf_with_pages(pages: Option<u32>) -> Vec<u8> {
    let mut pdf = ONE_PAGE_PDF.to_vec();
    if let Some(pages) = pages {
        pdf.extend_from_slice(format!("%FAKE:PAGES={pages}\n").as_bytes());
    }
    pdf
}

/// Office documents to the fake PDF.
#[derive(Debug)]
pub struct FakeOffice {
    settings: Settings,
}

impl FakeOffice {
    pub fn new(behavior: FakeBehavior) -> Self {
        Self {
            settings: Settings::new(behavior, false),
        }
    }

    /// Reads `FAKE:` markers from each input (test-hooks builds only).
    #[cfg(feature = "test-hooks")]
    pub fn from_markers() -> Self {
        Self {
            settings: Settings::new(FakeBehavior::Succeed, true),
        }
    }

    /// Engine time to sleep and report on every call.
    pub fn with_engine_time(mut self, engine_time: Duration) -> Self {
        self.settings.engine_time = engine_time;
        self
    }

    pub fn set_behavior(&self, behavior: FakeBehavior) {
        if let Ok(mut current) = self.settings.behavior.lock() {
            *current = behavior;
        }
    }
}

#[async_trait]
impl OfficeEngine for FakeOffice {
    async fn convert(
        &self,
        job: OfficeJob,
        _adm: Admission,
        deadline: Instant,
    ) -> Result<Converted, EngineError> {
        let started = Instant::now();
        let behavior = self.settings.behavior_for(&job.input).await?;
        let played = self.settings.play(behavior, deadline).await?;
        let pdf_bytes = write_capped(
            &job.output,
            &pdf_with_pages(played.pages),
            job.max_output_bytes,
        )
        .await?;
        Ok(Converted {
            pdf_bytes,
            doc_type: match job.family {
                Family::Presentation => DocType::Presentation,
                Family::Spreadsheet => DocType::Spreadsheet,
                _ => DocType::Text,
            },
            facts: None,
            timing: self.settings.timing(started, played.engine),
            worker: self.settings.worker("fake-office"),
        })
    }
}

/// Inspect, decrypt, render and assemble on fake PDFs.
#[derive(Debug)]
pub struct FakePdf {
    settings: Settings,
    /// The password an encrypted PDF (one containing `/Encrypt`) opens
    /// with; `None` accepts any non-empty password.
    password: Option<String>,
}

impl FakePdf {
    pub fn new(behavior: FakeBehavior) -> Self {
        Self {
            settings: Settings::new(behavior, false),
            password: None,
        }
    }

    /// Reads `FAKE:` markers from each input (test-hooks builds only).
    #[cfg(feature = "test-hooks")]
    pub fn from_markers() -> Self {
        Self {
            settings: Settings::new(FakeBehavior::Succeed, true),
            password: None,
        }
    }

    pub fn with_engine_time(mut self, engine_time: Duration) -> Self {
        self.settings.engine_time = engine_time;
        self
    }

    /// The only password that opens an encrypted PDF.
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    pub fn set_behavior(&self, behavior: FakeBehavior) {
        if let Ok(mut current) = self.settings.behavior.lock() {
            *current = behavior;
        }
    }

    fn opens(&self, password: &str) -> bool {
        match &self.password {
            Some(expected) => expected == password,
            None => !password.is_empty(),
        }
    }
}

const A4: PageSize = PageSize {
    width_pt: 595.0,
    height_pt: 842.0,
};

#[async_trait]
impl PdfEngine for FakePdf {
    async fn inspect(
        &self,
        job: InspectJob,
        _adm: Admission,
        deadline: Instant,
    ) -> Result<Inspection, EngineError> {
        let started = Instant::now();
        let head = read_head(&job.pdf).await?;
        let behavior = if self.settings.markers {
            marker_behavior(&head).unwrap_or(FakeBehavior::Succeed)
        } else {
            self.settings.behavior()
        };
        let played = self.settings.play(behavior, deadline).await?;
        let pages = played.pages.unwrap_or(1);
        let encrypted = memchr::memmem::find(&head, b"/Encrypt").is_some();
        if encrypted {
            match &job.password {
                None => return Err(EngineError::PasswordRequired),
                Some(password) if !self.opens(password.expose()) => {
                    return Err(EngineError::InvalidPassword);
                }
                Some(_) => {}
            }
        }
        Ok(Inspection {
            page_count: pages,
            encrypted,
            needs_password: encrypted,
            pages: vec![A4; pages as usize],
            probe: match job.probe_dpi {
                Some(_) => (1..=pages)
                    .map(|number| ProbedPage {
                        number,
                        objects: 1,
                        flat: false,
                        colour: [255, 255, 255],
                    })
                    .collect(),
                None => Vec::new(),
            },
            probe_engine: Duration::ZERO,
            fonts: Vec::new(),
            timing: self.settings.timing(started, played.engine),
            worker: self.settings.worker("fake-render"),
        })
    }

    async fn decrypt(
        &self,
        job: DecryptJob,
        _adm: Admission,
        deadline: Instant,
    ) -> Result<Decrypted, EngineError> {
        let started = Instant::now();
        if Instant::now() >= deadline {
            return Err(EngineError::Timeout);
        }
        if !self.opens(job.password.expose()) {
            return Err(EngineError::InvalidPassword);
        }
        // A copy without the /Encrypt marker stands in for PDFium's
        // save-as-copy without security.
        let bytes = tokio::fs::read(&job.pdf).await.map_err(io_error)?;
        let plain: Vec<u8> = match memchr::memmem::find(&bytes, b"/Encrypt") {
            Some(at) => [&bytes[..at], b"/Decrypt", &bytes[at + 8..]].concat(),
            None => bytes,
        };
        let pdf_bytes = write_capped(&job.output, &plain, job.max_output_bytes).await?;
        Ok(Decrypted {
            pdf_bytes,
            timing: self.settings.timing(started, Duration::ZERO),
            worker: self.settings.worker("fake-render"),
        })
    }

    async fn render(
        &self,
        job: RenderJob,
        _adm: Admission,
        deadline: Instant,
    ) -> Result<Rendered, EngineError> {
        let started = Instant::now();
        let behavior = self.settings.behavior_for(&job.pdf).await?;
        let played = self.settings.play(behavior, deadline).await?;
        let image = tiny_image(job.format, job.quality)?;
        let mut written = 0u64;
        let mut pages = Vec::with_capacity(job.pages.len());
        for &number in &job.pages {
            written += image.len() as u64;
            if written > job.max_output_bytes {
                return Err(EngineError::LimitExceeded(format!(
                    "rendered pages exceed the {} byte limit",
                    job.max_output_bytes
                )));
            }
            let file_name = format!("{number:04}.{}", job.format.extension());
            tokio::fs::write(job.out_dir.join(&file_name), &image)
                .await
                .map_err(io_error)?;
            pages.push(RenderedPage {
                number,
                file_name,
                width_px: 1,
                height_px: 1,
                width_pt: A4.width_pt,
                height_pt: A4.height_pt,
                size_bytes: image.len() as u64,
                objects: 1,
                flat: false,
                colour: [255, 255, 255],
                effective_dpi: f32::from(job.dpi),
                crc32: Some(crc32fast::hash(&image)),
            });
        }
        Ok(Rendered {
            pages,
            timing: self.settings.timing(started, played.engine),
            workers: vec![self.settings.worker("fake-render")],
        })
    }

    async fn assemble(
        &self,
        job: AssembleJob,
        _adm: Admission,
        deadline: Instant,
    ) -> Result<Assembled, EngineError> {
        let started = Instant::now();
        let input = job
            .images
            .first()
            .ok_or_else(|| EngineError::Failed("no image to assemble".into()))?;
        let behavior = self.settings.behavior_for(input).await?;
        let played = self.settings.play(behavior, deadline).await?;
        let pdf_bytes = write_capped(
            &job.output,
            &pdf_with_pages(played.pages),
            job.max_output_bytes,
        )
        .await?;
        Ok(Assembled {
            page_count: played.pages.unwrap_or(1),
            pdf_bytes,
            timing: self.settings.timing(started, played.engine),
            worker: self.settings.worker("fake-render"),
        })
    }
}

/// A white 1×1 image in `format`.
fn tiny_image(format: ImageFormat, quality: u8) -> Result<Vec<u8>, EngineError> {
    let pixel = image::RgbImage::from_pixel(1, 1, image::Rgb([255, 255, 255]));
    let mut out = Cursor::new(Vec::new());
    let result = match format {
        ImageFormat::Png => pixel.write_to(&mut out, image::ImageFormat::Png),
        ImageFormat::Webp => pixel.write_to(&mut out, image::ImageFormat::WebP),
        ImageFormat::Jpeg => {
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.max(1))
                .encode_image(&pixel)
        }
    };
    result.map_err(|err| EngineError::Internal(format!("fake image encoding: {err}")))?;
    Ok(out.into_inner())
}

/// Readiness for the fakes: ready unless switched off.
#[derive(Debug)]
pub struct FakeReadiness {
    healthy: AtomicBool,
}

impl FakeReadiness {
    pub fn new(healthy: bool) -> Self {
        Self {
            healthy: AtomicBool::new(healthy),
        }
    }

    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Relaxed);
    }
}

impl Readiness for FakeReadiness {
    fn readiness(&self) -> ReadinessSnapshot {
        let pool = PoolStatus {
            target: 1,
            idle: 1,
            busy: 0,
        };
        ReadinessSnapshot {
            status: if self.healthy.load(Ordering::Relaxed) {
                ReadinessStatus::Ready
            } else {
                ReadinessStatus::Starting
            },
            office: pool,
            render: pool,
        }
    }
}

/// No engine: every call is `Unavailable` (503) and readiness stays
/// `starting`. `serve` uses it while lane 002's engines are not wired
/// (plan 001 step 14), so the probes still answer.
pub struct Unwired;

#[async_trait]
impl OfficeEngine for Unwired {
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
impl PdfEngine for Unwired {
    async fn inspect(
        &self,
        _: InspectJob,
        _: Admission,
        _: Instant,
    ) -> Result<Inspection, EngineError> {
        Err(EngineError::Unavailable)
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
    async fn assemble(
        &self,
        _: AssembleJob,
        _: Admission,
        _: Instant,
    ) -> Result<Assembled, EngineError> {
        Err(EngineError::Unavailable)
    }
}

impl Readiness for Unwired {
    fn readiness(&self) -> ReadinessSnapshot {
        ReadinessSnapshot {
            status: ReadinessStatus::Starting,
            office: PoolStatus::default(),
            render: PoolStatus::default(),
        }
    }
}
