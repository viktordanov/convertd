//! `POST /v1/convert`.

use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::body::{Pump, file_body, zip_body};
use super::middleware::request_id::RequestId;
use super::response::{ApiError, content_disposition};
use super::serialization::manifest::{Manifest, ManifestParts, Timings, page_path};
use super::timing::{RequestTimings, micros};
use super::upload::{self, Received, UploadError};
use crate::bundle::{PlanError, ZipPlan};
use crate::domain::conversion::{ConversionResult, Output};
use crate::domain::conversion_service::{ConversionError, ConversionJob};
use crate::jobs::JobDir;

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
/// Images are wrapped into a PDF with one page per image frame.
///
/// **Options.** Send them as a JSON object in the `options` part. Leave
/// the part out to get a PDF with default settings. Unknown properties
/// are rejected with `VALIDATION_ERROR` so that typos never pass silently.
///
/// **Timing.** The call is synchronous. `timeout_seconds` is the whole
/// budget for queueing, conversion, rendering and packaging. When it runs
/// out the engine process is killed and the call returns
/// `CONVERSION_TIMEOUT`.
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
            description = "The conversion succeeded. The content type follows the `output`\noption: `application/pdf` for `pdf`, `application/zip` for\n`images` and `bundle`.\n\nThe ZIP is laid out as:\n\n```\nmanifest.json        described by the Manifest schema\ndocument.pdf         only for output=bundle\npages/0001.jpg       one file per rendered page, 1-based, zero-padded\npages/0002.jpg\n```\n\nImages and the PDF are stored without further compression.\n",
            headers(
                ("X-Request-ID" = String, description = "The correlation ID of this request."),
                ("Content-Disposition" = String, description = "`attachment` with a file name derived from the upload, for example `deck.pdf` or `deck.zip`."),
                ("Convert-Source-Format" = String, description = "The detected input format, as listed by `/v1/capabilities`."),
                ("Convert-Page-Count" = u32, description = "Number of pages in the resulting PDF."),
                ("Convert-Warning-Count" = u32, description = "Number of warnings raised for this conversion. The warnings\nthemselves are in `manifest.json`; ask for `bundle` to read them.\n"),
                ("Convert-Engine-Fingerprint" = String, description = "Identifies the exact engine build: service version, LibreOffice\nversion, PDFium version and installed font set. Use it in cache keys.\n"),
                ("Server-Timing" = String, description = "Where the time of this request went, in the W3C Server-Timing form\n`name;dur=<milliseconds>`, comma-separated, with 3 decimals. It is\nsent on every response produced after the file type was detected,\nincluding the error responses that follow detection. It is meant for\ndiagnosis and benchmarks.\n\n| Name | Stage |\n| --- | --- |\n| `recv` | Receiving the upload. Outside `total`. |\n| `detect` | Detecting the file type. |\n| `preflight` | Scanning the source before conversion. Runs alongside `convert`. |\n| `queue` | Waiting for a free worker. |\n| `lease` | Taking the worker from its pool. |\n| `spawn` | Starting a worker on this request's path. 0 when a warm worker was used. |\n| `bind` | Loading the engine library in that worker. 0 when a warm worker was used. |\n| `convert`, `convert_engine` | Producing the PDF: wall time, then time inside the engine. |\n| `inspect`, `inspect_engine` | Reading the PDF's facts: wall time, then engine time. |\n| `probe`, `probe_engine` | The blank-page probe: wall time, then engine time. |\n| `render`, `render_engine` | Rendering page images: wall time, then engine time (the longest chunk). |\n| `verify` | The quality checks that raise warnings. |\n| `package` | Building the manifest and the ZIP plan before the headers are sent. |\n| `total` | From the end of the upload to the start of the response. |\n\nStages that did not run are left out, except `spawn` and `bind`.\n"),
            ),
            content(
                (inline(Binary) = "application/pdf"),
                (inline(Binary) = "application/zip"),
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
    // Admission before the body is read: a 429 never reads the upload.
    let Ok(permit) = Arc::clone(&state.inflight).try_acquire_owned() else {
        return ApiError::busy().into_response();
    };
    let job = match JobDir::create(&state.tmp_dir, &state.disk_budget) {
        Ok(job) => job,
        Err(err) => {
            return ApiError::from_upload(&upload::map_io(err), state.queue_wait_seconds)
                .into_response();
        }
    };
    let mut detected = None;
    let received = match upload::receive(request, &job, state.service.limits(), &mut detected).await
    {
        Ok(received) => received,
        Err(err) => {
            let mut response =
                ApiError::from_upload(&err, state.queue_wait_seconds).into_response();
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

    let mut timings = RequestTimings {
        recv: Some(received.upload_done - started),
        detect: Some(received.detect),
        ..Default::default()
    };
    let deadline = received.upload_done + received.request.timeout;
    let paths = job.paths(received.detection.format);
    // The supervisor creates pages/; the render engine writes into it.
    if received.request.output != Output::Pdf
        && let Err(err) = std::fs::create_dir(&paths.pages_dir)
    {
        let api = ApiError::from_upload(&upload::map_io(err), state.queue_wait_seconds);
        let mut response = api.into_response();
        response
            .headers_mut()
            .insert(SERVER_TIMING, timings.header());
        return response;
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
        Err(err) => {
            let api = ApiError::from_conversion(&err, state.queue_wait_seconds);
            timings.total = Some(received.upload_done.elapsed());
            log_finished(
                &request_id,
                &received,
                None,
                &timings,
                Some(api.code.as_str()),
            );
            let mut response = api.into_response();
            response
                .headers_mut()
                .insert(SERVER_TIMING, timings.header());
            response
        }
    }
}

/// How long after the deadline the backstop fires, so that the engine's
/// own `Timeout` (with its kill logged) wins in the normal case.
pub const BACKSTOP_GRACE: Duration = Duration::from_millis(250);

const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");
const SOURCE_FORMAT: HeaderName = HeaderName::from_static("convert-source-format");
const PAGE_COUNT: HeaderName = HeaderName::from_static("convert-page-count");
const WARNING_COUNT: HeaderName = HeaderName::from_static("convert-warning-count");
const FINGERPRINT: HeaderName = HeaderName::from_static("convert-engine-fingerprint");

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
    let prepared = match output {
        Output::Pdf => File::open(&result.pdf)
            .and_then(|file| Ok((file.metadata()?.len(), file)))
            .map(|(size, file)| Prepared::Pdf { file, size })
            .map_err(|err| ApiError::internal(format!("failed to open the result: {err}"))),
        Output::Images | Output::Bundle => {
            let manifest = ManifestInput::new(state, received, request_id, timings);
            let job = Arc::clone(&job);
            let result = result.clone();
            tokio::task::spawn_blocking(move || plan_zip(&job, &manifest, &result))
                .await
                .unwrap_or_else(|err| {
                    Err(ApiError::internal(format!("failed to plan the ZIP: {err}")))
                })
        }
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(api) => {
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
        request_id: request_id.to_owned(),
    };
    let (body, content_type, extension) = match prepared {
        Prepared::Pdf { file, size } => (file_body(file, size, pump), "application/pdf", "pdf"),
        Prepared::Zip { plan, .. } => (zip_body(plan, pump), "application/zip", "zip"),
    };
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
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
    Pdf { file: File, size: u64 },
    Zip { plan: ZipPlan, size: u64 },
}

impl Prepared {
    fn size(&self) -> u64 {
        match self {
            Prepared::Pdf { size, .. } | Prepared::Zip { size, .. } => *size,
        }
    }
}

/// What the blocking ZIP planner needs from the request, owned.
struct ManifestInput {
    request_id: String,
    upload: crate::domain::conversion_service::UploadFacts,
    request: crate::domain::conversion::ConversionRequest,
    source: std::path::PathBuf,
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
            source: received.source.clone(),
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

/// Builds the stored ZIP: `manifest.json`, then `document.pdf` for a
/// bundle, then `pages/NNNN.<ext>`. Sizes come from `fstat`; the page
/// paths come from the page numbers, never from the engine's file names.
fn plan_zip(
    job: &JobDir,
    input: &ManifestInput,
    result: &ConversionResult,
) -> Result<Prepared, ApiError> {
    let format = input.request.images.format;
    let io = |err: std::io::Error| ApiError::internal(format!("failed to plan the ZIP: {err}"));
    let page_files: Vec<std::path::PathBuf> = result
        .pages
        .iter()
        .map(|page| {
            job.pages_dir()
                .join(page_path(page.number, format).trim_start_matches("pages/"))
        })
        .collect();
    let page_sizes = page_files
        .iter()
        .map(|path| std::fs::metadata(path).map(|m| m.len()))
        .collect::<Result<Vec<u64>, _>>()
        .map_err(io)?;
    let bundle = input.request.output == Output::Bundle;
    let pdf_size = if bundle {
        Some(std::fs::metadata(&result.pdf).map_err(io)?.len())
    } else {
        None
    };
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
        PlanError::Io { .. } => ApiError::internal(err.to_string()),
    };
    let mut plan = ZipPlan::new();
    plan.add_bytes("manifest.json", bytes::Bytes::from(manifest));
    if bundle {
        // The upload's CRC holds only for a PDF passed through unchanged.
        let crc = (result.pdf == input.source).then_some(input.upload.crc32);
        plan.add_file("document.pdf", result.pdf.clone(), crc)
            .map_err(plan_error)?;
    }
    for ((page, path), name) in result
        .pages
        .iter()
        .zip(page_files)
        .zip(result.pages.iter().map(|p| page_path(p.number, format)))
    {
        plan.add_file(name, path, page.crc32).map_err(plan_error)?;
    }
    plan.check().map_err(plan_error)?;
    let size = plan.content_length();
    Ok(Prepared::Zip { plan, size })
}

/// The `conversion finished` line: PLAN's fields plus the §11 overheads,
/// in microseconds.
fn log_finished(
    request_id: &str,
    received: &Received,
    result: Option<(&ConversionResult, u64)>,
    timings: &RequestTimings,
    error_code: Option<&str>,
) {
    let s = &timings.stages;
    tracing::info!(
        request_id,
        source_format = received.detection.format.id(),
        family = received.detection.format.family().as_str(),
        output = received.request.output.as_str(),
        page_count = result.map(|(r, _)| r.page_count),
        input_bytes = received.upload.size_bytes,
        output_bytes = result.map(|(_, size)| size),
        warning_count = result.map(|(r, _)| r.warnings.len()),
        render_us = s.render.map(micros),
        recv_us = timings.recv.map(micros),
        queue_us = micros(s.queue),
        convert_us = s.convert.map(micros),
        decrypt_us = s.decrypt.map(micros),
        inspect_us = s.inspect.map(micros),
        engine_us = micros(s.engine_total()),
        total_us = timings.total.map(micros),
        overhead_us = micros(timings.overhead()),
        overhead_strict_us = micros(timings.overhead_strict()),
        error_code,
        "conversion finished"
    );
}

// A binary response body, for the spec only.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
#[allow(dead_code)]
pub struct Binary(Vec<u8>);
