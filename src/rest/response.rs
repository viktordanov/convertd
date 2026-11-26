//! Errors as HTTP responses: the only place that maps errors to statuses.
//!
//! | Source | Status | Code |
//! |---|---|---|
//! | auth failure | 401 | `UNAUTHORIZED` |
//! | unknown path / wrong method | 404 / 405 | `NOT_FOUND` / `BAD_REQUEST` |
//! | malformed multipart, bad parts, options syntax, upload stalled | 400 | `BAD_REQUEST` |
//! | upload over the cap | 413 | `FILE_TOO_LARGE` |
//! | unsupported format | 415 | `UNSUPPORTED_FORMAT` |
//! | options data error, `Validation` | 422 | `VALIDATION_ERROR` |
//! | `PasswordRequired`, `InvalidPassword`, `LimitExceeded` | 422 | same names |
//! | `Failed`, `Crashed`, `Poisoned` | 422 | `CONVERSION_FAILED` |
//! | `QueueFull`, the concurrency permit | 429 | `TOO_MANY_REQUESTS`, `Retry-After: 1` |
//! | `QueueTimeout` | 429 | `TOO_MANY_REQUESTS`, `Retry-After: QUEUE_WAIT_SECONDS` |
//! | `Unavailable`, `DiskFull` | 503 | `SERVICE_UNAVAILABLE`, `Retry-After: 5` |
//! | `Timeout` | 504 | `CONVERSION_TIMEOUT` |
//! | `Internal`, I/O, panic | 500 | `INTERNAL_ERROR` |

use std::borrow::Cow;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::serialization::errors::{ErrorBody, ErrorCode};
use super::serialization::options::OptionsError;
use super::upload::UploadError;
use crate::domain::conversion_service::ConversionError;
use crate::domain::detection::DetectError;
use crate::domain::engine::EngineError;

/// `Retry-After` for a full queue or the concurrency permit.
pub const RETRY_AFTER_BUSY: u32 = 1;
/// `Retry-After` for `SERVICE_UNAVAILABLE`.
pub const RETRY_AFTER_UNAVAILABLE: u32 = 5;

/// An error response: `{error, code}`, the status and `Retry-After`.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: ErrorCode,
    pub message: Cow<'static, str>,
    pub retry_after: Option<u32>,
    /// Logged with the request ID, never returned.
    pub detail: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: ErrorCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
            detail: None,
        }
    }

    pub fn with_retry_after(mut self, seconds: u32) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn bad_request(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::BadRequest, message)
    }

    pub fn validation(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::ValidationError,
            message,
        )
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "missing or invalid API key",
        )
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, "no such route")
    }

    pub fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::BadRequest,
            "method not allowed",
        )
    }

    pub fn too_large(limit: u64) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::FileTooLarge,
            format!("file exceeds the {limit} byte limit"),
        )
    }

    /// The concurrency permit is taken (`MAX_CONCURRENT_REQUESTS`).
    pub fn busy() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::TooManyRequests,
            "too many requests in flight",
        )
        .with_retry_after(RETRY_AFTER_BUSY)
    }

    pub fn unavailable(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ServiceUnavailable,
            message,
        )
        .with_retry_after(RETRY_AFTER_UNAVAILABLE)
    }

    /// A spent disk budget or ENOSPC: other jobs hold the disk.
    pub fn disk_full() -> Self {
        Self::unavailable("not enough temporary storage; retry later").with_detail("disk_full")
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::InternalError,
            "internal server error",
        )
        .with_detail(detail)
    }

    fn unprocessable(code: ErrorCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    }

    /// An engine error. `queue_wait` is `QUEUE_WAIT_SECONDS`, the
    /// `Retry-After` of a queue timeout.
    pub fn from_engine(err: &EngineError, queue_wait: u32) -> Self {
        match err {
            EngineError::QueueFull => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                ErrorCode::TooManyRequests,
                "the conversion queue is full",
            )
            .with_retry_after(RETRY_AFTER_BUSY),
            EngineError::QueueTimeout => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                ErrorCode::TooManyRequests,
                "no worker became free in time",
            )
            .with_retry_after(queue_wait.max(1)),
            EngineError::Unavailable => Self::unavailable("the service is not ready"),
            EngineError::DiskFull => Self::disk_full(),
            EngineError::Timeout => Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                ErrorCode::ConversionTimeout,
                "the conversion did not finish in time",
            ),
            EngineError::PasswordRequired => Self::unprocessable(
                ErrorCode::PasswordRequired,
                "the document is encrypted and no password was provided",
            ),
            EngineError::InvalidPassword => Self::unprocessable(
                ErrorCode::InvalidPassword,
                "the password does not open the document",
            ),
            EngineError::LimitExceeded(message) => {
                Self::unprocessable(ErrorCode::LimitExceeded, message.clone())
            }
            EngineError::Failed(_) | EngineError::Crashed | EngineError::Poisoned => {
                Self::unprocessable(
                    ErrorCode::ConversionFailed,
                    "the document could not be converted",
                )
                .with_detail(err.to_string())
            }
            EngineError::Internal(message) => Self::internal(message.clone()),
        }
    }

    pub fn from_conversion(err: &ConversionError, queue_wait: u32) -> Self {
        match err {
            ConversionError::Validation(message) => Self::validation(message.clone()),
            ConversionError::LimitExceeded(message) => {
                Self::unprocessable(ErrorCode::LimitExceeded, message.clone())
            }
            ConversionError::Timeout { seconds } => Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                ErrorCode::ConversionTimeout,
                format!("the conversion did not finish within {seconds} seconds"),
            ),
            ConversionError::DiskFull => Self::disk_full(),
            ConversionError::Engine(engine) => Self::from_engine(engine, queue_wait),
        }
    }

    pub fn from_detect(err: &DetectError) -> Self {
        match err {
            DetectError::Unsupported { label } => Self::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorCode::UnsupportedFormat,
                format!("unsupported format: {label}"),
            ),
            DetectError::LimitExceeded { .. } => {
                Self::unprocessable(ErrorCode::LimitExceeded, err.to_string())
            }
            DetectError::Io(io) => Self::internal(format!("failed to read upload: {io}")),
        }
    }
}

impl ApiError {
    pub fn from_upload(err: &UploadError, queue_wait: u32) -> Self {
        match err {
            UploadError::NotMultipart
            | UploadError::Malformed(_)
            | UploadError::UnknownPart(_)
            | UploadError::DuplicatePart(_)
            | UploadError::MissingFile
            | UploadError::EmptyFile
            | UploadError::OptionsTooLarge
            | UploadError::Stalled
            | UploadError::TooSlow { .. }
            | UploadError::Incomplete(_)
            | UploadError::Options(OptionsError::Syntax(_)) => Self::bad_request(err.to_string()),
            UploadError::Options(OptionsError::Invalid(message)) => {
                Self::validation(message.clone())
            }
            UploadError::TooLarge { limit } => Self::too_large(*limit),
            UploadError::Request(conversion) => Self::from_conversion(conversion, queue_wait),
            UploadError::Detect(detect) => Self::from_detect(detect),
            UploadError::DiskFull => Self::disk_full(),
            UploadError::Io(_) | UploadError::Sink(_) => Self::internal(err.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.message,
            code: self.code,
        };
        // A struct of two strings always serialises.
        let json = serde_json::to_vec(&body).unwrap_or_default();
        let mut response = (
            self.status,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            json,
        )
            .into_response();
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if let Some(detail) = self.detail {
            response.extensions_mut().insert(ErrorDetail(detail));
        }
        response
    }
}

/// `attachment` with the upload's stem and `extension`: an ASCII
/// `filename` fallback and an RFC 6266 `filename*=UTF-8''…`.
pub fn content_disposition(file_name: Option<&str>, extension: &str) -> HeaderValue {
    let stem = file_name
        .map(|name| match name.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem,
            _ => name,
        })
        .filter(|stem| !stem.trim().is_empty())
        .unwrap_or("document");
    let ascii: String = stem
        .chars()
        .map(|c| match c {
            ' '..='~' if c != '"' && c != '\\' && c != '%' => c,
            _ => '_',
        })
        .collect();
    let mut encoded = String::with_capacity(stem.len() * 3);
    for byte in stem.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    let value = format!(
        "attachment; filename=\"{ascii}.{extension}\"; filename*=UTF-8''{encoded}.{extension}"
    );
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// The logged detail of an error response, read by the access log.
#[derive(Debug, Clone)]
pub struct ErrorDetail(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    fn check(err: ApiError, status: u16, code: &str, retry_after: Option<u32>) {
        assert_eq!(err.status.as_u16(), status, "{err:?}");
        assert_eq!(err.code.as_str(), code, "{err:?}");
        assert_eq!(err.retry_after, retry_after, "{err:?}");
    }

    #[test]
    fn every_engine_error() {
        let cases = [
            (EngineError::QueueFull, 429, "TOO_MANY_REQUESTS", Some(1)),
            (
                EngineError::QueueTimeout,
                429,
                "TOO_MANY_REQUESTS",
                Some(10),
            ),
            (
                EngineError::Unavailable,
                503,
                "SERVICE_UNAVAILABLE",
                Some(5),
            ),
            (EngineError::DiskFull, 503, "SERVICE_UNAVAILABLE", Some(5)),
            (EngineError::Timeout, 504, "CONVERSION_TIMEOUT", None),
            (
                EngineError::PasswordRequired,
                422,
                "PASSWORD_REQUIRED",
                None,
            ),
            (EngineError::InvalidPassword, 422, "INVALID_PASSWORD", None),
            (
                EngineError::LimitExceeded("too big".into()),
                422,
                "LIMIT_EXCEEDED",
                None,
            ),
            (
                EngineError::Failed("x".into()),
                422,
                "CONVERSION_FAILED",
                None,
            ),
            (EngineError::Crashed, 422, "CONVERSION_FAILED", None),
            (EngineError::Poisoned, 422, "CONVERSION_FAILED", None),
            (
                EngineError::Internal("boom".into()),
                500,
                "INTERNAL_ERROR",
                None,
            ),
        ];
        for (err, status, code, retry) in cases {
            check(ApiError::from_engine(&err, 10), status, code, retry);
            let wrapped = ConversionError::Engine(err);
            check(ApiError::from_conversion(&wrapped, 10), status, code, retry);
        }
    }

    #[test]
    fn every_conversion_error() {
        let cases = [
            (
                ConversionError::Validation("v".into()),
                422,
                "VALIDATION_ERROR",
                None,
            ),
            (
                ConversionError::LimitExceeded("l".into()),
                422,
                "LIMIT_EXCEEDED",
                None,
            ),
            (
                ConversionError::Timeout { seconds: 120 },
                504,
                "CONVERSION_TIMEOUT",
                None,
            ),
            (
                ConversionError::DiskFull,
                503,
                "SERVICE_UNAVAILABLE",
                Some(5),
            ),
        ];
        for (err, status, code, retry) in cases {
            check(ApiError::from_conversion(&err, 10), status, code, retry);
        }
        let timeout = ApiError::from_conversion(&ConversionError::Timeout { seconds: 120 }, 10);
        assert_eq!(
            timeout.message,
            "the conversion did not finish within 120 seconds"
        );
    }

    #[test]
    fn every_detect_error() {
        let unsupported = ApiError::from_detect(&DetectError::Unsupported { label: "html" });
        check(unsupported.clone(), 415, "UNSUPPORTED_FORMAT", None);
        assert_eq!(unsupported.message, "unsupported format: html");
        let limit = DetectError::LimitExceeded {
            limit: "entry count",
        };
        check(ApiError::from_detect(&limit), 422, "LIMIT_EXCEEDED", None);
        let io = DetectError::Io(std::io::Error::other("x"));
        check(ApiError::from_detect(&io), 500, "INTERNAL_ERROR", None);
    }

    #[test]
    fn every_upload_error() {
        let cases = [
            (UploadError::NotMultipart, 400, "BAD_REQUEST", None),
            (UploadError::Malformed("m".into()), 400, "BAD_REQUEST", None),
            (
                UploadError::UnknownPart("x".into()),
                400,
                "BAD_REQUEST",
                None,
            ),
            (UploadError::DuplicatePart("file"), 400, "BAD_REQUEST", None),
            (UploadError::MissingFile, 400, "BAD_REQUEST", None),
            (UploadError::EmptyFile, 400, "BAD_REQUEST", None),
            (UploadError::OptionsTooLarge, 400, "BAD_REQUEST", None),
            (UploadError::Stalled, 400, "BAD_REQUEST", None),
            (
                UploadError::Incomplete("eof".into()),
                400,
                "BAD_REQUEST",
                None,
            ),
            (
                UploadError::Options(OptionsError::Syntax("s".into())),
                400,
                "BAD_REQUEST",
                None,
            ),
            (
                UploadError::Options(OptionsError::Invalid("i".into())),
                422,
                "VALIDATION_ERROR",
                None,
            ),
            (
                UploadError::TooLarge { limit: 5 },
                413,
                "FILE_TOO_LARGE",
                None,
            ),
            (
                UploadError::Request(ConversionError::Validation("v".into())),
                422,
                "VALIDATION_ERROR",
                None,
            ),
            (
                UploadError::Detect(DetectError::Unsupported { label: "html" }),
                415,
                "UNSUPPORTED_FORMAT",
                None,
            ),
            (
                UploadError::Detect(DetectError::LimitExceeded {
                    limit: "entry count",
                }),
                422,
                "LIMIT_EXCEEDED",
                None,
            ),
            (UploadError::DiskFull, 503, "SERVICE_UNAVAILABLE", Some(5)),
            (
                UploadError::Io(std::io::Error::other("x")),
                500,
                "INTERNAL_ERROR",
                None,
            ),
            (
                UploadError::Sink("panicked".into()),
                500,
                "INTERNAL_ERROR",
                None,
            ),
        ];
        for (err, status, code, retry) in cases {
            check(ApiError::from_upload(&err, 10), status, code, retry);
        }
        assert_eq!(
            ApiError::from_upload(&UploadError::MissingFile, 10).message,
            "missing multipart part \"file\""
        );
        assert_eq!(
            ApiError::from_upload(&UploadError::Stalled, 10).message,
            "upload stalled"
        );
        assert_eq!(
            ApiError::from_upload(&UploadError::DiskFull, 10)
                .detail
                .as_deref(),
            Some("disk_full")
        );
    }

    #[test]
    fn content_disposition_names() {
        let cases = [
            (
                Some("deck.pptx"),
                "attachment; filename=\"deck.pdf\"; filename*=UTF-8''deck.pdf",
            ),
            (
                Some("report.final.docx"),
                "attachment; filename=\"report.final.pdf\"; filename*=UTF-8''report.final.pdf",
            ),
            (
                Some("Bericht über \"Q3\".docx"),
                "attachment; filename=\"Bericht _ber _Q3_.pdf\"; filename*=UTF-8''Bericht%20%C3%BCber%20%22Q3%22.pdf",
            ),
            (
                Some(".hidden"),
                "attachment; filename=\".hidden.pdf\"; filename*=UTF-8''.hidden.pdf",
            ),
            (
                None,
                "attachment; filename=\"document.pdf\"; filename*=UTF-8''document.pdf",
            ),
        ];
        for (name, want) in cases {
            assert_eq!(content_disposition(name, "pdf"), want, "{name:?}");
        }
    }

    #[test]
    fn helpers() {
        check(ApiError::unauthorized(), 401, "UNAUTHORIZED", None);
        check(ApiError::not_found(), 404, "NOT_FOUND", None);
        check(ApiError::method_not_allowed(), 405, "BAD_REQUEST", None);
        check(ApiError::too_large(10), 413, "FILE_TOO_LARGE", None);
        check(ApiError::busy(), 429, "TOO_MANY_REQUESTS", Some(1));
        check(ApiError::disk_full(), 503, "SERVICE_UNAVAILABLE", Some(5));
        check(ApiError::bad_request("b"), 400, "BAD_REQUEST", None);
        assert_eq!(
            ApiError::too_large(10).message,
            "file exceeds the 10 byte limit"
        );
    }

    #[tokio::test]
    async fn response_body_and_headers() {
        let response = ApiError::busy().into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            &body[..],
            br#"{"error":"too many requests in flight","code":"TOO_MANY_REQUESTS"}"#
        );
    }
}
