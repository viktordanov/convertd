//! `POST /v1/convert`.

use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::body::{Pump, file_body};
use super::middleware::request_id::RequestId;
use super::response::{ApiError, content_disposition};
use super::timing::{RequestTimings, micros};
use super::upload::{self, Received, UploadError};
use crate::domain::conversion::ConversionResult;
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
                ("Server-Timing" = String, description = "Where the time went inside the service (INTERFACES §9), as `name;dur=<ms>` entries with 3 decimals, in this order: recv, detect, preflight, queue, lease, spawn, bind, convert, convert_engine, inspect, inspect_engine, probe, probe_engine, render, render_engine, verify, package, total. Stages that did not run are left out; spawn and bind are always sent. `total` runs from the end of the upload to the start of the response; `*_engine` is time inside the engine worker."),
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
    let received = match upload::receive(request, &job, state.service.limits()).await {
        Ok(received) => received,
        Err(err) => {
            let mut response =
                ApiError::from_upload(&err, state.queue_wait_seconds).into_response();
            if let UploadError::Detect(_) = err {
                let timings = RequestTimings {
                    recv: Some(started.elapsed()),
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
        Ok(result) => success(
            &state,
            &received,
            result,
            job,
            permit,
            &request_id,
            &mut timings,
            package_started,
        ),
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
/// directory and the permit from here on.
#[allow(clippy::too_many_arguments)]
fn success(
    state: &AppState,
    received: &Received,
    result: ConversionResult,
    job: Arc<JobDir>,
    permit: tokio::sync::OwnedSemaphorePermit,
    request_id: &str,
    timings: &mut RequestTimings,
    package_started: Instant,
) -> Response {
    let file = match File::open(&result.pdf).and_then(|file| Ok((file.metadata()?.len(), file))) {
        Ok((size, file)) => (file, size),
        Err(err) => {
            let api = ApiError::internal(format!("failed to open the result: {err}"));
            timings.total = Some(received.upload_done.elapsed());
            log_finished(request_id, received, None, timings, Some(api.code.as_str()));
            return api.into_response();
        }
    };
    let (file, size) = file;
    let warning_count = received.warnings.len() + result.warnings.len();
    timings.package = Some(package_started.elapsed());
    timings.total = Some(received.upload_done.elapsed());
    log_finished(request_id, received, Some((&result, size)), timings, None);

    let body = file_body(
        file,
        size,
        Pump {
            job,
            permit,
            idle: state.io_idle_timeout,
            request_id: request_id.to_owned(),
        },
    );
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/pdf"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(received.upload.file_name.as_deref(), "pdf"),
    );
    headers.insert(SOURCE_FORMAT, HeaderValue::from_static(result.format.id()));
    headers.insert(PAGE_COUNT, HeaderValue::from(result.page_count));
    headers.insert(WARNING_COUNT, HeaderValue::from(warning_count));
    headers.insert(FINGERPRINT, state.fingerprint.clone());
    headers.insert(SERVER_TIMING, timings.header());
    response
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
        warning_count = result.map(|(r, _)| r.warnings.len() + received.warnings.len()),
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
