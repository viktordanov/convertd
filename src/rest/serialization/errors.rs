//! The error body `{error, code}`.

use std::borrow::Cow;

use serde::Serialize;
use utoipa::ToSchema;

/// Stable machine-readable error code. Branch on this, not on the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    BadRequest,
    Unauthorized,
    NotFound,
    FileTooLarge,
    UnsupportedFormat,
    ValidationError,
    PasswordRequired,
    InvalidPassword,
    LimitExceeded,
    ConversionFailed,
    TooManyRequests,
    InternalError,
    ServiceUnavailable,
    ConversionTimeout,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "BAD_REQUEST",
            ErrorCode::Unauthorized => "UNAUTHORIZED",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::FileTooLarge => "FILE_TOO_LARGE",
            ErrorCode::UnsupportedFormat => "UNSUPPORTED_FORMAT",
            ErrorCode::ValidationError => "VALIDATION_ERROR",
            ErrorCode::PasswordRequired => "PASSWORD_REQUIRED",
            ErrorCode::InvalidPassword => "INVALID_PASSWORD",
            ErrorCode::LimitExceeded => "LIMIT_EXCEEDED",
            ErrorCode::ConversionFailed => "CONVERSION_FAILED",
            ErrorCode::TooManyRequests => "TOO_MANY_REQUESTS",
            ErrorCode::InternalError => "INTERNAL_ERROR",
            ErrorCode::ServiceUnavailable => "SERVICE_UNAVAILABLE",
            ErrorCode::ConversionTimeout => "CONVERSION_TIMEOUT",
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[schema(as = Error)]
pub struct ErrorBody {
    /// A human-readable message. Safe to log; never contains document content.
    #[schema(value_type = String, example = "unsupported format: html")]
    pub error: Cow<'static, str>,
    pub code: ErrorCode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_serialise_as_their_names() {
        for code in [
            ErrorCode::BadRequest,
            ErrorCode::NotFound,
            ErrorCode::ConversionTimeout,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), code.as_str());
        }
        let body = ErrorBody {
            error: "x".into(),
            code: ErrorCode::NotFound,
        };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"error":"x","code":"NOT_FOUND"}"#
        );
    }
}
