//! `POST /v1/convert`.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::body::{Pump, file_body, tar_body};
use super::middleware::request_id::RequestId;
use super::response::{ApiError, content_disposition};
use super::serialization::errors::ErrorCode;
use super::serialization::manifest::{Manifest, ManifestParts, Timings, page_path};
use super::timing::{RequestTimings, micros};
use super::upload::{self, Received, UploadError};
use crate::bundle::{PlanError, TarPlan};
use crate::domain::conversion::{ConversionResult, Output};
use crate::domain::conversion_service::{ConversionError, ConversionJob};
use crate::jobs::JobDir;
use crate::observability::metrics;

use super::openapi::{
    BadRequest, ConversionTimeout, FileTooLarge, InternalError, RequestIdParam, ServiceUnavailable,
    TooManyRequests, Unauthorized, Unprocessable, UnsupportedFormat,
};
use super::serialization::options::ConvertRequestDoc;
use super::state::AppState;

/// Convert one document to PDF, page images, or both
///
/// Uploads one file and returns it as a PDF, as page images, or as a
/// bundle of both.
///
/// **Routing.** The file type is detected from the bytes, never from the
/// file name or the part's content type. Office and OpenDocument files go
/// through LibreOffice. PDFs skip conversion and go straight to rendering.
/// Image files (PNG, JPEG, TIFF, GIF, BMP, WebP) are not accepted as
/// input and answer 415 `UNSUPPORTED_FORMAT`.
///
/// **Options.** Send them as a JSON object in the `options` part. Leave
/// the part out to get a PDF with default settings. Unknown properties
/// are rejected with `VALIDATION_ERROR` so that typos never pass silently.
///
/// **Timing.** The call is synchronous. `timeout_seconds` is the whole
/// budget from the end of the upload to the response headers: queueing,
/// conversion, rendering and packaging. When it runs out the engine
/// process is killed and the call returns `CONVERSION_TIMEOUT`. Streaming
/// the body comes after it and is not part of that budget. The server cuts
/// a body that the client stops reading, or reads below the deployment's
/// minimum rate; the same holds for an upload that stalls or trickles.
///
/// **Safety.** Macros never run, external links are never fetched and the
/// engine has no network access. A document that relies on any of those
/// still converts, without that content.
#[utoipa::path(
    post,
    path = "/v1/convert",
    operation_id = "convertDocument",
    tag = "conversion",
    params(RequestIdParam),
    request_body(
        content = ConvertRequestDoc,
        content_type = "multipart/form-data",
        encoding(
            ("file" = (content_type = "application/octet-stream")),
            ("options" = (content_type = "application/json")),
        ),
    ),
    responses(
        (status = 200,
            description = "The conversion succeeded. The content type follows the `output`\noption: `application/pdf` for `pdf`, `application/zstd` for\n`images` and `bundle`.\n\nThe `application/zstd` body is a ustar archive in one zstd frame\n(`<stem>.tar.zst`), sent chunked without `Content-Length`. Its entries,\nin this order:\n\n```\nmanifest.json        described by the Manifest schema\ndocument.pdf         only for output=bundle\npages/0001.jpg       one file per rendered page, 1-based, zero-padded\npages/0002.jpg\n```\n\nEvery entry is a regular file, mode 0644, mtime 0, uid and gid 0.\n\nRead the body to the end before you use it: a stream that ends early is not a valid archive.\n",
            headers(
                ("X-Request-ID" = String, description = "The correlation ID of this request."),
                ("Content-Disposition" = String, description = "`attachment` with a file name derived from the upload, for example `deck.pdf` or `deck.tar.zst`."),
                ("Convert-Source-Format" = String, description = "The detected input format, as listed by `/v1/capabilities`."),
                ("Convert-Page-Count" = u32, description = "Number of pages in the resulting PDF."),
                ("Convert-Warning-Count" = u32, description = "Number of warnings raised for this conversion. The warnings\nthemselves are in `manifest.json`; ask for `bundle` to read them.\n"),
                ("Convert-Engine-Fingerprint" = String, description = "Identifies everything that decides the output bytes: service version,\nLibreOffice version, PDFium version, installed font set, and the\ndeployment settings that change the output (render caps, default paper\nsize, the blank-page probe). Use it in cache keys.\n"),
                ("Server-Timing" = String, description = "Where the time of this request went, in the W3C Server-Timing form\n`name;dur=<milliseconds>`, comma-separated, with 3 decimals. It is\nsent on every response produced after the file type was detected,\nincluding the error responses that follow detection. It is meant for\ndiagnosis and benchmarks.\n\n| Name | Stage |\n| --- | --- |\n| `recv` | Receiving the upload. Outside `total`. |\n| `detect` | Detecting the file type. |\n| `preflight` | The office worker's scan of an office source, inside `convert` before the engine opens the file. Counted as overhead. |\n| `queue` | Waiting for a free worker. |\n| `lease` | Taking the worker from its pool. |\n| `spawn` | Starting a worker on this request's path. 0 when a warm worker was used. |\n| `bind` | Loading the engine library in that worker. 0 when a warm worker was used. |\n| `convert`, `convert_engine` | Producing the PDF: wall time, then time inside the engine. |\n| `inspect`, `inspect_engine` | Reading the PDF's facts: wall time, then engine time. |\n| `probe`, `probe_engine` | The blank-page probe: wall time, then engine time. |\n| `render`, `render_engine` | Rendering page images: wall time, then engine time (the longest chunk). |\n| `verify` | The quality checks that raise warnings. |\n| `package` | Building the manifest and the bundle plan before the headers are sent. |\n| `total` | From the end of the upload to the start of the response. |\n\nStages that did not run are left out, except `spawn` and `bind`.\n"),
            ),
            content(
                (inline(Binary) = "application/pdf"),
                (inline(Binary) = "application/zstd"),
            ),
        ),
        (status = 400, response = BadRequest),
        (status = 401, response = Unauthorized),
        (status = 413, response = FileTooLarge),
        (status = 415, response = UnsupportedFormat),
        (status = 422, response = Unprocessable),
        (status = 429, response = TooManyRequests),
        (status = 500, response = InternalError),
        (status = 503, response = ServiceUnavailable),
        (status = 504, response = ConversionTimeout),
    ),
)]
pub async fn convert(State(state): State<AppState>, request: Request) -> Response {
    let started = Instant::now();
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.as_str().to_owned())
        .unwrap_or_default();
    // Admission before the body is read: a 503 or 429 never reads it.
    // The permit comes first and the drain flag second, so a request that
    // the drain does not refuse holds a permit that `wait_idle` sees.
    let permit = Arc::clone(&state.inflight).try_acquire_owned();
    if state.draining.load(Ordering::SeqCst) {
        drop(permit);
        metrics::request("unknown", "unknown", ErrorCode::ServiceUnavailable.as_str());
        return ApiError::unavailable("the service is shutting down").into_response();
    }
    let Ok(permit) = permit else {
        metrics::request("unknown", "unknown", ErrorCode::TooManyRequests.as_str());
        return ApiError::busy().into_response();
    };
    // mkdir can block on a slow disk: off the runtime, as every file call.
    // The task holds the permit, so a client that leaves meanwhile does
    // not free its slot while the mkdir still runs.
    let (tmp_dir, disk_budget) = (state.tmp_dir.clone(), state.disk_budget.clone());
    let created = tokio::task::spawn_blocking(move || {
        JobDir::create(&tmp_dir, &disk_budget).map(|job| (job, permit))
    })
    .await
    .unwrap_or_else(|err| Err(std::io::Error::other(err)));
    let (mut job, permit) = match created {
        Ok(created) => created,
        Err(err) => {
            return ApiError::from_upload(&upload::map_io(err), state.queue_wait_seconds)
                .into_response();
        }
    };
    let mut detected = None;
    let shared = Arc::new(permit);
    let received = match upload::receive(
        request,
        &job,
        Arc::clone(&shared),
        state.service.limits(),
        state.upload_rate,
        &mut detected,
    )
    .await
    {
        Ok(received) => received,
        Err(err) => {
            let api = ApiError::from_upload(&err, state.queue_wait_seconds);
            metrics::request("unknown", "unknown", api.code.as_str());
            let mut response = api.into_response();
            // Every response after detection carries Server-Timing: a
            // detection error, and any error that follows it (an options
            // part after the file, a spent budget at the true-up).
            if detected.is_some() || matches!(err, UploadError::Detect(_)) {
                let timings = RequestTimings {
                    recv: Some(started.elapsed()),
                    detect: detected,
                    ..Default::default()
                };
                response
                    .headers_mut()
                    .insert(SERVER_TIMING, timings.header());
            }
            return response;
        }
    };
    // receive awaited its sink, so the permit is the handler's alone again.
    let Ok(mut permit) = Arc::try_unwrap(shared) else {
        return ApiError::internal("the upload sink still holds the permit".to_owned())
            .into_response();
    };

    let mut timings = RequestTimings {
        recv: Some(received.upload_done - started),
        detect: Some(received.detect),
        ..Default::default()
    };
    let deadline = received.upload_done + received.request.timeout;
    let paths = job.paths(received.detection.format);
    // The supervisor creates pages/; the render engine writes into it.
    if received.request.output != Output::Pdf {
        let pages_dir = paths.pages_dir.clone();
        #[cfg(feature = "test-hooks")]
        let gate_name = received.upload.file_name.clone();
        let made = off_runtime((permit, job), move |_| {
            #[cfg(feature = "test-hooks")]
            while test_hooks::pages_gated(gate_name.as_deref()) {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::fs::create_dir(pages_dir)
        });
        // The deadline runs from the end of the upload: a wait for a
        // blocking thread counts too.
        let until = tokio::time::Instant::from_std(deadline);
        match tokio::time::timeout_at(until, made).await {
            Ok(Some((Ok(()), held))) => (permit, job) = held,
            Ok(Some((Err(err), _))) => {
                return pages_failed(state.queue_wait_seconds, &timings, err);
            }
            Ok(None) => {
                let err = std::io::Error::other("the task that makes pages/ panicked");
                return pages_failed(state.queue_wait_seconds, &timings, err);
            }
            Err(_) => {
                let timeout = ConversionError::Timeout {
                    seconds: received.request.timeout.as_secs(),
                };
                return conversion_failed(&state, &received, &request_id, &mut timings, &timeout);
            }
        }
    }
    let outcome = {
        let conversion = state.service.convert(
            ConversionJob {
                paths: &paths,
                upload: &received.upload,
                detection: received.detection,
                request: &received.request,
                budget: job.budget(),
                deadline,
            },
            &mut timings.stages,
        );
        // The engines enforce the deadline; this backstop turns a hung
        // engine into a 504. Dropping the future cancels the engine job.
        match tokio::time::timeout_at((deadline + BACKSTOP_GRACE).into(), conversion).await {
            Ok(outcome) => outcome,
            Err(_) => Err(ConversionError::Timeout {
                seconds: received.request.timeout.as_secs(),
            }),
        }
    };
    let package_started = Instant::now();
    match outcome {
        Ok(result) => {
            success(
                &state,
                &received,
                result,
                job,
                permit,
                &request_id,
                &mut timings,
                package_started,
            )
            .await
        }
        Err(err) => conversion_failed(&state, &received, &request_id, &mut timings, &err),
    }
}

/// The answer to a conversion that failed after detection: logged as
/// `conversion finished`, counted in the metrics, with Server-Timing.
fn conversion_failed(
    state: &AppState,
    received: &Received,
    request_id: &str,
    timings: &mut RequestTimings,
    err: &ConversionError,
) -> Response {
    let api = ApiError::from_conversion(err, state.queue_wait_seconds);
    timings.total = Some(received.upload_done.elapsed());
    log_finished(request_id, received, None, timings, Some(api.code.as_str()));
    let mut response = api.into_response();
    response
        .headers_mut()
        .insert(SERVER_TIMING, timings.header());
    response
}

/// How long after the deadline the backstop fires, so that the engine's
/// own `Timeout` (with its kill logged) wins in the normal case.
pub const BACKSTOP_GRACE: Duration = Duration::from_millis(250);

const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");
const SOURCE_FORMAT: HeaderName = HeaderName::from_static("convert-source-format");
const PAGE_COUNT: HeaderName = HeaderName::from_static("convert-page-count");
const WARNING_COUNT: HeaderName = HeaderName::from_static("convert-warning-count");
const FINGERPRINT: HeaderName = HeaderName::from_static("convert-engine-fingerprint");

/// The answer when `pages/` cannot be made.
fn pages_failed(
    queue_wait_seconds: u32,
    timings: &RequestTimings,
    err: std::io::Error,
) -> Response {
    let api = ApiError::from_upload(&upload::map_io(err), queue_wait_seconds);
    let mut response = api.into_response();
    response
        .headers_mut()
        .insert(SERVER_TIMING, timings.header());
    response
}

/// A request's admission permit and job directory.
type Held = (tokio::sync::OwnedSemaphorePermit, Arc<JobDir>);

/// Runs `work` on the blocking pool with the request's permit and job
/// directory, and hands both back. A client that leaves, or a deadline
/// that passes, drops this future, not the task: work that has started
/// keeps the slot taken and the directory until it ends. Work that no
/// thread has taken up yet is skipped then, so a full pool does not
/// spend threads on answers nobody receives. `None` if the task panicked
/// or was skipped; the permit and the directory went with it.
async fn off_runtime<T: Send + 'static>(
    held: Held,
    work: impl FnOnce(&JobDir) -> T + Send + 'static,
) -> Option<(T, Held)> {
    let ended = Ended(Arc::new(AtomicBool::new(false)));
    let skip = Arc::clone(&ended.0);
    let task = tokio::task::spawn_blocking(move || {
        if skip.load(Ordering::SeqCst) {
            return None;
        }
        Some((work(&held.1), held))
    });
    let out = task.await.ok().flatten();
    drop(ended);
    out
}

/// Set when [`off_runtime`]'s future is dropped or done.
struct Ended(Arc<AtomicBool>);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The 200: headers first, then the body pump, which owns the job
/// directory and the permit from here on. Anything that fails before the
/// headers still becomes an error status.
#[allow(clippy::too_many_arguments)]
async fn success(
    state: &AppState,
    received: &Received,
    result: ConversionResult,
    job: Arc<JobDir>,
    permit: tokio::sync::OwnedSemaphorePermit,
    request_id: &str,
    timings: &mut RequestTimings,
    package_started: Instant,
) -> Response {
    let output = received.request.output;
    // The deadline covers everything up to the headers, packaging
    // included. It is checked before planning, and again after it, because
    // planning a bundle reads every page's size and can take long.
    let deadline = received.upload_done + received.request.timeout;
    let timed_out = || {
        ApiError::from_conversion(
            &ConversionError::Timeout {
                seconds: received.request.timeout.as_secs(),
            },
            state.queue_wait_seconds,
        )
    };
    let held = (permit, job);
    // The wait for a blocking thread and the work both end at the deadline.
    // A task still running then keeps the permit and the job until it ends.
    let until = tokio::time::Instant::from_std(deadline);
    let done = if Instant::now() >= deadline {
        Ok(Some((Err(timed_out()), held)))
    } else if output == Output::Pdf {
        let pdf = result.pdf.clone();
        let opened = off_runtime(held, move |job| {
            settle(job);
            File::open(pdf)
                .and_then(|file| Ok((file.metadata()?.len(), file)))
                .map(|(size, file)| Prepared::Pdf { file, size })
                .map_err(|err| ApiError::internal(format!("failed to open the result: {err}")))
        });
        tokio::time::timeout_at(until, opened).await
    } else {
        let manifest = ManifestInput::new(state, received, request_id, timings);
        let result = result.clone();
        let planned = off_runtime(held, move |job| plan_tar(job, &manifest, &result));
        tokio::time::timeout_at(until, planned).await
    };
    let (prepared, held) = match done {
        // The second check, just before the 200.
        Ok(Some((prepared, held))) => (
            prepared.and_then(|prepared| {
                if Instant::now() >= deadline {
                    Err(timed_out())
                } else {
                    Ok(prepared)
                }
            }),
            Some(held),
        ),
        Ok(None) => (
            Err(ApiError::internal(
                "failed to prepare the response: the task panicked".to_owned(),
            )),
            None,
        ),
        Err(_) => (Err(timed_out()), None),
    };
    let (prepared, (permit, job)) = match (prepared, held) {
        (Ok(prepared), Some(held)) => (prepared, held),
        (prepared, _) => {
            let api = prepared
                .err()
                .unwrap_or_else(|| ApiError::internal("failed to prepare the response".to_owned()));
            timings.total = Some(received.upload_done.elapsed());
            log_finished(request_id, received, None, timings, Some(api.code.as_str()));
            let mut response = api.into_response();
            response
                .headers_mut()
                .insert(SERVER_TIMING, timings.header());
            return response;
        }
    };
    let size = prepared.size();
    timings.package = Some(package_started.elapsed());
    timings.total = Some(received.upload_done.elapsed());
    log_finished(request_id, received, Some((&result, size)), timings, None);

    let pump = Pump {
        job,
        permit,
        idle: state.io_idle_timeout,
        rate: state.response_rate,
        request_id: request_id.to_owned(),
    };
    let (body, content_type, extension) = match prepared {
        Prepared::Pdf { file, size } => (file_body(file, size, pump), "application/pdf", "pdf"),
        Prepared::Tar { plan, .. } => (tar_body(plan, pump), "application/zstd", "tar.zst"),
    };
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // A tar.zst is chunked: its compressed length is not known in advance.
    if matches!(output, Output::Pdf) {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    }
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(received.upload.file_name.as_deref(), extension),
    );
    headers.insert(SOURCE_FORMAT, HeaderValue::from_static(result.format.id()));
    headers.insert(PAGE_COUNT, HeaderValue::from(result.page_count));
    headers.insert(WARNING_COUNT, HeaderValue::from(result.warnings.len()));
    headers.insert(FINGERPRINT, state.fingerprint.clone());
    headers.insert(SERVER_TIMING, timings.header());
    response
}

/// A response body, ready to stream.
enum Prepared {
    Pdf {
        file: File,
        size: u64,
    },
    /// `size` is the uncompressed tar length, for the log only.
    Tar {
        plan: TarPlan,
        size: u64,
    },
}

impl Prepared {
    fn size(&self) -> u64 {
        match self {
            Prepared::Pdf { size, .. } | Prepared::Tar { size, .. } => *size,
        }
    }
}

/// What the blocking bundle planner needs from the request, owned.
struct ManifestInput {
    request_id: String,
    upload: crate::domain::conversion_service::UploadFacts,
    request: crate::domain::conversion::ConversionRequest,
    engine: super::serialization::manifest::EngineInfo,
    timings: Timings,
}

impl ManifestInput {
    fn new(
        state: &AppState,
        received: &Received,
        request_id: &str,
        timings: &RequestTimings,
    ) -> Self {
        let s = &timings.stages;
        let ms = |d: Option<Duration>| {
            u64::try_from(d.unwrap_or_default().as_millis()).unwrap_or(u64::MAX)
        };
        Self {
            request_id: request_id.to_owned(),
            upload: received.upload.clone(),
            request: received.request.clone(),
            engine: state.engine_info.clone(),
            timings: Timings {
                queue: ms(Some(s.queue)),
                convert: ms(s.convert),
                render: ms(s.render),
                total: ms(Some(received.upload_done.elapsed())),
            },
        }
    }
}

/// Gives back the disk reservation that the finished engine calls did not
/// use, so a long download holds only what is on disk. On an error the
/// whole reservation stays until the job ends, which is safe.
fn settle(job: &JobDir) {
    if let Err(err) = job.settle_budget() {
        settle_failed(&err);
    }
}

/// [`settle`], which also returns the size of each of `wanted` that the
/// walk measured. The others, and all on an error, get their own stat.
fn settle_with_sizes(job: &JobDir, wanted: &[&Path]) -> std::io::Result<Vec<u64>> {
    let measured = match job.settle_budget_with_sizes(wanted) {
        Ok(settled) => settled.sizes,
        Err(err) => {
            settle_failed(&err);
            vec![None; wanted.len()]
        }
    };
    wanted
        .iter()
        .zip(measured)
        .map(|(path, size)| match size {
            Some(len) => Ok(len),
            None => std::fs::metadata(path).map(|m| m.len()),
        })
        .collect()
}

fn settle_failed(err: &std::io::Error) {
    tracing::warn!(error = %err, "failed to measure the job directory; its reservation stays");
}

/// Builds the tar.zst plan: `manifest.json`, then `document.pdf` for a
/// bundle, then `pages/NNNN.<ext>` in page order. Sizes come from the
/// `fstat` of the settle walk, one per file (a file it did not count gets
/// its own); the page paths come from the page numbers, never from the
/// engine's file names.
fn plan_tar(
    job: &JobDir,
    input: &ManifestInput,
    result: &ConversionResult,
) -> Result<Prepared, ApiError> {
    #[cfg(feature = "test-hooks")]
    {
        use std::sync::atomic::Ordering::SeqCst;
        let name = input.upload.file_name.as_deref();
        if test_hooks::gated(name) {
            test_hooks::PLAN_ENTERED.store(true, SeqCst);
            while test_hooks::gated(name) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let ms = test_hooks::PLAN_DELAY_MS.load(SeqCst);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
    let format = input.request.images.format;
    let io = |err: std::io::Error| ApiError::internal(format!("failed to plan the bundle: {err}"));
    let pages: Vec<(String, std::path::PathBuf)> = result
        .pages
        .iter()
        .map(|page| {
            let name = page_path(page.number, format);
            let path = job.pages_dir().join(name.trim_start_matches("pages/"));
            (name, path)
        })
        .collect();
    let bundle = input.request.output == Output::Bundle;
    let mut wanted: Vec<&Path> = pages.iter().map(|(_, path)| path.as_path()).collect();
    if bundle {
        wanted.push(&result.pdf);
    }
    let mut page_sizes = settle_with_sizes(job, &wanted).map_err(io)?;
    let pdf_size = if bundle { page_sizes.pop() } else { None };
    let manifest = Manifest::new(ManifestParts {
        request_id: &input.request_id,
        upload: &input.upload,
        request: &input.request,
        result,
        pdf_size,
        page_sizes: &page_sizes,
        engine: &input.engine,
        timings: input.timings.clone(),
    });
    let manifest = serde_json::to_vec(&manifest)
        .map_err(|err| ApiError::internal(format!("failed to serialise the manifest: {err}")))?;

    let plan_error = |err: PlanError| match err {
        PlanError::TooLarge => {
            ApiError::from_conversion(&ConversionError::LimitExceeded(err.to_string()), 0)
        }
        PlanError::NameTooLong(_) | PlanError::Io { .. } => ApiError::internal(err.to_string()),
    };
    let mut plan = TarPlan::new();
    plan.add_bytes("manifest.json", bytes::Bytes::from(manifest))
        .map_err(plan_error)?;
    if let Some(size) = pdf_size {
        plan.add_file_sized("document.pdf", result.pdf.clone(), size)
            .map_err(plan_error)?;
    }
    for ((name, path), size) in pages.into_iter().zip(page_sizes) {
        plan.add_file_sized(name, path, size).map_err(plan_error)?;
    }
    let size = plan.tar_length();
    Ok(Prepared::Tar { plan, size })
}

/// The `conversion finished` line: PLAN's fields plus the §11 overheads,
/// in microseconds, and the workers' CPU time of the calls that succeeded,
/// in milliseconds with microsecond resolution.
fn log_finished(
    request_id: &str,
    received: &Received,
    result: Option<(&ConversionResult, u64)>,
    timings: &RequestTimings,
    error_code: Option<&'static str>,
) {
    let s = &timings.stages;
    // Encrypted OOXML has no known format until the engine reports it.
    let (source_format, family) = match result {
        Some((result, _)) => (result.format.id(), result.family.as_str()),
        None if received.detection.encrypted => ("encrypted-ooxml", "unknown"),
        None => (
            received.detection.format.id(),
            received.detection.format.family().as_str(),
        ),
    };
    tracing::info!(
        request_id,
        source_format,
        family,
        output = received.request.output.as_str(),
        page_count = result.map(|(r, _)| r.page_count),
        input_bytes = received.upload.size_bytes,
        output_bytes = result.map(|(_, size)| size),
        warning_count = result.map(|(r, _)| r.warnings.len()),
        render_us = s.render.map(micros),
        recv_us = timings.recv.map(micros),
        queue_us = micros(s.queue),
        preflight_us = s.preflight.map(micros),
        convert_us = s.convert.map(micros),
        decrypt_us = s.decrypt.map(micros),
        inspect_us = s.inspect.map(micros),
        engine_us = micros(s.engine_total()),
        total_us = timings.total.map(micros),
        overhead_us = micros(timings.overhead()),
        overhead_strict_us = micros(timings.overhead_strict()),
        cpu_ms = micros(s.cpu) as f64 / 1000.0,
        error_code,
        "conversion finished"
    );
    record_metrics(
        received,
        result.map(|(r, _)| r),
        timings,
        error_code,
        family,
    );
}

/// The request's metrics: its outcome, its stages, its input size, its
/// warnings and skipped checks, and its rendered pages.
fn record_metrics(
    received: &Received,
    result: Option<&ConversionResult>,
    timings: &RequestTimings,
    error_code: Option<&'static str>,
    family: &'static str,
) {
    metrics::request(
        received.request.output.as_str(),
        family,
        error_code.unwrap_or("OK"),
    );
    metrics::input_bytes(family, received.upload.size_bytes);
    let s = &timings.stages;
    let stages = [
        ("receive", timings.recv),
        ("detect", timings.detect),
        ("queue", Some(s.queue)),
        ("preflight", s.preflight),
        ("convert", s.convert),
        ("inspect", s.inspect),
        ("render", s.render),
        ("respond", timings.package),
        ("engine", Some(s.engine_total())),
        ("total", timings.total),
        ("overhead", timings.total.map(|_| timings.overhead())),
    ];
    for (stage, duration) in stages {
        if let Some(duration) = duration {
            metrics::stage(stage, family, duration);
        }
    }
    let Some(result) = result else { return };
    for warning in &result.warnings {
        metrics::warning(warning.code.as_str());
    }
    if result.probe_skipped_pages > 0 {
        metrics::verify_skipped_by("blank_page", "pages", u64::from(result.probe_skipped_pages));
    }
    for (check, reason) in &result.skipped {
        metrics::verify_skipped(check.as_str(), reason);
    }
    if !result.pages.is_empty() {
        metrics::pages_rendered(
            received.request.images.format.as_str(),
            result.pages.len() as u64,
        );
    }
}

// A binary response body, for the spec only.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
#[allow(dead_code)]
pub struct Binary(Vec<u8>);

/// Hooks for tests (feature `test-hooks`).
#[cfg(feature = "test-hooks")]
pub mod test_hooks {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    /// Milliseconds that planning a bundle sleeps first: a slow plan, to
    /// test the deadline check after it. Zero in every other test.
    pub static PLAN_DELAY_MS: AtomicU64 = AtomicU64::new(0);
    /// The upload file name whose bundle plan the gate holds. The plan of
    /// that upload sets `PLAN_ENTERED`, then waits while the name stays;
    /// every other request's plan passes. `None`: no gate.
    pub static PLAN_GATE: Mutex<Option<String>> = Mutex::new(None);
    /// Set when the gated plan starts.
    pub static PLAN_ENTERED: AtomicBool = AtomicBool::new(false);
    /// The upload file name whose mkdir of `pages/` waits while the name
    /// stays. `None`: no gate.
    pub static PAGES_GATE: Mutex<Option<String>> = Mutex::new(None);

    /// Whether the plan of the upload named `name` is held.
    pub fn gated(name: Option<&str>) -> bool {
        holds(&PLAN_GATE, name)
    }

    /// Whether the mkdir of `pages/` for the upload named `name` is held.
    pub fn pages_gated(name: Option<&str>) -> bool {
        holds(&PAGES_GATE, name)
    }

    fn holds(gate: &Mutex<Option<String>>, name: Option<&str>) -> bool {
        let Some(name) = name else {
            return false;
        };
        gate.lock()
            .map(|gate| gate.as_deref() == Some(name))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::Semaphore;

    use super::*;
    use crate::jobs::DiskBudget;

    /// Work still queued for a blocking thread when the request ends is
    /// skipped, and its permit and job go back; work that started runs.
    #[test]
    fn queued_work_is_skipped_once_the_request_ends() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tmp = std::env::temp_dir().join(format!("convertd-off-runtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        runtime.block_on(async {
            let semaphore = Arc::new(Semaphore::new(1));
            let held = (
                Arc::clone(&semaphore).try_acquire_owned().unwrap(),
                JobDir::create(&tmp, &DiskBudget::new(u64::MAX)).unwrap(),
            );
            // The only blocking thread is busy for 300 ms.
            let busy =
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(300)));
            let ran = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&ran);
            let waited = tokio::time::timeout(
                Duration::from_millis(50),
                off_runtime(held, move |_| flag.store(true, Ordering::SeqCst)),
            )
            .await;
            assert!(waited.is_err(), "the work got a thread at once");
            busy.await.unwrap();
            // The skipped task gives the permit back without running.
            let permit = tokio::time::timeout(Duration::from_secs(2), semaphore.acquire())
                .await
                .expect("the permit never came back");
            drop(permit);
            assert!(!ran.load(Ordering::SeqCst), "the work ran for nobody");
            // Work that is not dropped runs.
            let held = (
                Arc::clone(&semaphore).try_acquire_owned().unwrap(),
                JobDir::create(&tmp, &DiskBudget::new(u64::MAX)).unwrap(),
            );
            let done = off_runtime(held, |_| 7).await;
            assert_eq!(done.map(|(value, _)| value), Some(7));
        });
        drop(runtime);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
