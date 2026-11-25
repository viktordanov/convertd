//! The streamed upload: multipart to the job directory, never whole in
//! memory.
//!
//! ```text
//! hyper Incoming
//!   -> RequestBodyTimeout(IO_IDLE_TIMEOUT)   idle timer, reset per frame
//!   -> multer with Constraints               allowed parts, per-part limits
//!   -> field chunks (Bytes, refcounted)
//!   -> mpsc(4)
//!   -> ONE blocking sink: SHA-256 + CRC-32 + write, then detection
//! ```
//!
//! multer is used directly because axum's `Multipart` extractor builds it
//! without `Constraints`. Parts may come in either order; `options` is
//! parsed and validated as soon as it arrives, so a bad option fails
//! before the upload is stored.

use std::error::Error as StdError;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::header;
use bytes::Bytes;
use multer::{Constraints, Multipart, SizeLimit};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use super::serialization::options::{ConvertOptions, OptionsError};
use crate::detect;
use crate::domain::conversion::{ConversionRequest, Output, RawOptions};
use crate::domain::conversion_service::{ConversionError, UploadFacts};
use crate::domain::detection::{DetectError, Detection};
use crate::domain::limits::Limits;
use crate::domain::verification::Warning;
use crate::jobs::JobDir;

/// Chunks queued between the handler and the sink.
const UPLOAD_QUEUE_CHUNKS: usize = 4;
/// The largest accepted `options` part.
pub const OPTIONS_PART_MAX: u64 = 64 * 1024;
/// Room for the multipart framing around the parts.
const FRAMING_MAX: u64 = 16 * 1024;
/// The sink's write buffer.
const WRITE_BUFFER: usize = 64 * 1024;

/// A stored, detected upload with its validated options.
#[derive(Debug)]
pub struct Received {
    /// `<job>/source.<ext>`, renamed at the sink's tail.
    pub source: PathBuf,
    pub upload: UploadFacts,
    pub detection: Detection,
    pub request: ConversionRequest,
    /// Raised by validation, such as `RENDER_CLAMPED`.
    pub warnings: Vec<Warning>,
    /// The deadline and `total` start here.
    pub upload_done: Instant,
    /// Detection time inside the sink.
    pub detect: Duration,
}

/// Why an upload was refused.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("the request must be multipart/form-data with a boundary")]
    NotMultipart,
    #[error("malformed multipart body: {0}")]
    Malformed(String),
    #[error("unknown multipart part {0:?}")]
    UnknownPart(String),
    #[error("duplicate multipart part {0:?}")]
    DuplicatePart(&'static str),
    #[error("missing multipart part \"file\"")]
    MissingFile,
    #[error("the file part is empty")]
    EmptyFile,
    #[error("file exceeds the {limit} byte limit")]
    TooLarge { limit: u64 },
    #[error("the options part exceeds {OPTIONS_PART_MAX} bytes")]
    OptionsTooLarge,
    /// No data for `IO_IDLE_TIMEOUT_SECONDS`.
    #[error("upload stalled")]
    Stalled,
    /// The body ended or failed before the multipart did.
    #[error("the upload ended early: {0}")]
    Incomplete(String),
    #[error("{}", match .0 { OptionsError::Syntax(m) | OptionsError::Invalid(m) => m })]
    Options(OptionsError),
    #[error(transparent)]
    Request(ConversionError),
    /// `output` values that later steps of plan 001 serve (step 8).
    #[error("output {} is not available yet", .0.as_str())]
    OutputNotYet(Output),
    #[error(transparent)]
    Detect(DetectError),
    /// ENOSPC or EDQUOT, or the disk budget is spent.
    #[error("no space left for the upload")]
    DiskFull,
    #[error("failed to store upload: {0}")]
    Io(io::Error),
    #[error("failed to run the upload sink: {0}")]
    Sink(String),
}

/// ENOSPC and EDQUOT become `DiskFull`; other errors stay I/O errors.
pub fn map_io(err: io::Error) -> UploadError {
    if is_disk_full(&err) {
        UploadError::DiskFull
    } else {
        UploadError::Io(err)
    }
}

pub fn is_disk_full(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
}

/// Reads the multipart body into `job`. Called after auth and the
/// concurrency permit; rejects an oversized `Content-Length` before a byte
/// is read.
pub async fn receive(
    request: Request,
    job: &Arc<JobDir>,
    limits: &Limits,
) -> Result<Received, UploadError> {
    let max_upload = limits.max_upload_bytes;
    let whole_stream = max_upload + OPTIONS_PART_MAX + FRAMING_MAX;
    let boundary = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| multer::parse_boundary(value).ok())
        .ok_or(UploadError::NotMultipart)?;
    let content_length = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if content_length.is_some_and(|length| length > whole_stream) {
        return Err(UploadError::TooLarge { limit: max_upload });
    }
    let reserved = content_length.unwrap_or(whole_stream);
    if !job.budget().try_reserve(reserved) {
        return Err(UploadError::DiskFull);
    }

    let constraints = Constraints::new()
        .allowed_fields(vec!["file", "options"])
        .size_limit(
            SizeLimit::new()
                .whole_stream(whole_stream)
                .for_field("file", max_upload)
                .for_field("options", OPTIONS_PART_MAX),
        );
    let body = request.into_body().into_data_stream();
    let mut multipart = Multipart::with_constraints(body, boundary, constraints);

    let mut options: Option<(ConversionRequest, Vec<Warning>)> = None;
    let mut stored: Option<(SinkResult, Option<String>)> = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| map_multer(e, max_upload))?
    {
        match field.name() {
            Some("options") => {
                if options.is_some() {
                    return Err(UploadError::DuplicatePart("options"));
                }
                let bytes = field.bytes().await.map_err(|e| map_multer(e, max_upload))?;
                options = Some(validate_options(&bytes, limits)?);
            }
            Some("file") => {
                if stored.is_some() {
                    return Err(UploadError::DuplicatePart("file"));
                }
                let file_name = field.file_name().and_then(sanitize_file_name);
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(job.upload_part())
                    .map_err(map_io)?;
                let (tx, rx) = mpsc::channel(UPLOAD_QUEUE_CHUNKS);
                let sink_job = Arc::clone(job);
                let sink = tokio::task::spawn_blocking(move || sink(file, &sink_job, rx));
                let forwarded = forward(&mut field, &tx, max_upload).await;
                drop(tx);
                let result = sink
                    .await
                    .map_err(|err| UploadError::Sink(err.to_string()))?;
                // A multer error explains a sink that stopped early.
                forwarded?;
                stored = Some((result?, file_name));
            }
            other => {
                return Err(UploadError::UnknownPart(
                    other.unwrap_or_default().to_owned(),
                ));
            }
        }
    }
    let (result, file_name) = stored.ok_or(UploadError::MissingFile)?;
    let (request, warnings) = match options {
        Some(options) => options,
        None => validate_options(b"{}", limits)?,
    };
    job.budget().release(reserved.saturating_sub(result.size));
    Ok(Received {
        source: result.source,
        upload: UploadFacts {
            size_bytes: result.size,
            sha256: result.sha256,
            crc32: result.crc32,
            file_name,
        },
        detection: result.detection,
        request,
        warnings,
        upload_done: Instant::now(),
        detect: result.detect,
    })
}

/// Sends the file part's chunks to the sink, then the end marker. Stops
/// early when the sink is gone (it failed; its error is the answer).
async fn forward(
    field: &mut multer::Field<'_>,
    tx: &mpsc::Sender<SinkMessage>,
    max_upload: u64,
) -> Result<(), UploadError> {
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if tx.send(SinkMessage::Chunk(chunk)).await.is_err() {
                    return Ok(());
                }
            }
            Ok(None) => {
                let _ = tx.send(SinkMessage::Done).await;
                return Ok(());
            }
            Err(err) => return Err(map_multer(err, max_upload)),
        }
    }
}

fn validate_options(
    bytes: &[u8],
    limits: &Limits,
) -> Result<(ConversionRequest, Vec<Warning>), UploadError> {
    let dto = ConvertOptions::parse(bytes).map_err(UploadError::Options)?;
    let (request, warnings) =
        ConversionRequest::new(RawOptions::from(dto), limits).map_err(UploadError::Request)?;
    if request.output != Output::Pdf {
        return Err(UploadError::OutputNotYet(request.output));
    }
    Ok((request, warnings))
}

fn map_multer(err: multer::Error, max_upload: u64) -> UploadError {
    match err {
        multer::Error::UnknownField { field_name } => {
            UploadError::UnknownPart(field_name.unwrap_or_default())
        }
        multer::Error::FieldSizeExceeded { field_name, .. }
            if field_name.as_deref() == Some("options") =>
        {
            UploadError::OptionsTooLarge
        }
        multer::Error::FieldSizeExceeded { .. } | multer::Error::StreamSizeExceeded { .. } => {
            UploadError::TooLarge { limit: max_upload }
        }
        multer::Error::StreamReadFailed(inner) => {
            if is_stall(inner.as_ref()) {
                UploadError::Stalled
            } else {
                UploadError::Incomplete(inner.to_string())
            }
        }
        multer::Error::IncompleteStream | multer::Error::IncompleteFieldData { .. } => {
            UploadError::Incomplete(err.to_string())
        }
        other => UploadError::Malformed(other.to_string()),
    }
}

/// Whether the body failed because `RequestBodyTimeoutLayer` fired.
fn is_stall(err: &(dyn StdError + Send + Sync + 'static)) -> bool {
    let mut current: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(err) = current {
        if err.is::<tower_http::timeout::TimeoutError>() {
            return true;
        }
        current = err.source();
    }
    false
}

/// The base name of the part's file name, without control characters, at
/// most 255 bytes. Used only to name the response.
pub fn sanitize_file_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    let mut end = clean.len().min(255);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    let clean = &clean[..end];
    (!clean.is_empty() && clean != "." && clean != "..").then(|| clean.to_owned())
}

enum SinkMessage {
    Chunk(Bytes),
    /// The file part ended normally. Without it, a closed channel means the
    /// handler went away.
    Done,
}

#[derive(Debug)]
struct SinkResult {
    source: PathBuf,
    size: u64,
    sha256: [u8; 32],
    crc32: u32,
    detection: Detection,
    detect: Duration,
}

/// Digests of what was written.
#[derive(Debug, PartialEq, Eq)]
pub struct Written {
    pub size: u64,
    pub sha256: [u8; 32],
    pub crc32: u32,
}

/// Runs on one blocking thread. Owns the file and a clone of the
/// `Arc<JobDir>`, so the directory outlives it even if the handler future
/// is dropped (hyper drops it on peer EOF mid-request).
fn sink(
    mut file: File,
    job: &JobDir,
    mut rx: mpsc::Receiver<SinkMessage>,
) -> Result<SinkResult, UploadError> {
    let written = write_chunks(&mut file, || rx.blocking_recv())?;
    let Some(written) = written else {
        return Err(UploadError::Incomplete("the client went away".into()));
    };
    if written.size == 0 {
        return Err(UploadError::EmptyFile);
    }
    let started = Instant::now();
    let detection = detect::detect_file(&mut file, written.size).map_err(|err| match err {
        DetectError::Io(io) => map_io(io),
        other => UploadError::Detect(other),
    })?;
    let detect = started.elapsed();
    drop(file);
    let source = job.source_path(detection.format);
    std::fs::rename(job.upload_part(), &source).map_err(map_io)?;
    Ok(SinkResult {
        source,
        size: written.size,
        sha256: written.sha256,
        crc32: written.crc32,
        detection,
        detect,
    })
}

/// Writes every chunk `next` yields through a 64 KiB buffer, hashing as it
/// goes. `Ok(None)` when the channel closed without the end marker.
fn write_chunks<W: Write>(
    writer: W,
    mut next: impl FnMut() -> Option<SinkMessage>,
) -> Result<Option<Written>, UploadError> {
    let mut out = BufWriter::with_capacity(WRITE_BUFFER, writer);
    let mut sha = Sha256::new();
    let mut crc = crc32fast::Hasher::new();
    let mut size = 0u64;
    loop {
        match next() {
            Some(SinkMessage::Chunk(chunk)) => {
                sha.update(&chunk);
                crc.update(&chunk);
                out.write_all(&chunk).map_err(map_io)?;
                size += chunk.len() as u64;
            }
            Some(SinkMessage::Done) => break,
            None => return Ok(None),
        }
    }
    out.flush().map_err(map_io)?;
    Ok(Some(Written {
        size,
        sha256: sha.finalize().into(),
        crc32: crc.finalize(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(chunks: &[&'static [u8]], done: bool) -> impl FnMut() -> Option<SinkMessage> {
        let mut queue: Vec<SinkMessage> = chunks
            .iter()
            .map(|c| SinkMessage::Chunk(Bytes::from_static(c)))
            .collect();
        if done {
            queue.push(SinkMessage::Done);
        }
        queue.reverse();
        move || queue.pop()
    }

    #[test]
    fn digests_match_the_bytes() {
        let mut out = Vec::new();
        let written = write_chunks(&mut out, messages(&[b"hello ", b"world"], true))
            .unwrap()
            .unwrap();
        assert_eq!(out, b"hello world");
        assert_eq!(written.size, 11);
        assert_eq!(
            written.sha256,
            <[u8; 32]>::from(Sha256::digest(b"hello world"))
        );
        assert_eq!(written.crc32, crc32fast::hash(b"hello world"));
    }

    #[test]
    fn a_closed_channel_without_the_end_is_not_an_upload() {
        let mut out = Vec::new();
        assert!(
            write_chunks(&mut out, messages(&[b"half"], false))
                .unwrap()
                .is_none()
        );
    }

    /// A writer that fails like a full disk.
    struct Full;

    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::StorageFull))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::StorageFull))
        }
    }

    #[test]
    fn enospc_is_disk_full() {
        let big: &'static [u8] = Box::leak(vec![7u8; 200 * 1024].into_boxed_slice());
        let err = write_chunks(Full, messages(&[big], true)).unwrap_err();
        assert!(matches!(err, UploadError::DiskFull), "{err:?}");
        // Also when the error only shows at the final flush.
        let err = write_chunks(Full, messages(&[b"tiny"], true)).unwrap_err();
        assert!(matches!(err, UploadError::DiskFull), "{err:?}");
        assert!(matches!(
            map_io(io::Error::from(io::ErrorKind::QuotaExceeded)),
            UploadError::DiskFull
        ));
        assert!(matches!(map_io(io::Error::other("x")), UploadError::Io(_)));
    }

    #[test]
    fn file_names_are_sanitised() {
        assert_eq!(
            sanitize_file_name("deck.pptx").as_deref(),
            Some("deck.pptx")
        );
        assert_eq!(
            sanitize_file_name("C:\\Users\\me\\deck.pptx").as_deref(),
            Some("deck.pptx")
        );
        assert_eq!(
            sanitize_file_name("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_file_name("a\u{0}b\nc.pdf").as_deref(),
            Some("abc.pdf")
        );
        assert_eq!(sanitize_file_name("  "), None);
        assert_eq!(sanitize_file_name("dir/"), None);
        assert_eq!(sanitize_file_name(".."), None);
        let long = "é".repeat(200);
        let cut = sanitize_file_name(&long).unwrap();
        assert!(cut.len() <= 255 && cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn other_body_errors_are_not_stalls() {
        assert!(!is_stall(&io::Error::other("connection reset")));
        let wrapped = axum::Error::new(io::Error::other("reset"));
        assert!(!is_stall(&wrapped));
    }
}
