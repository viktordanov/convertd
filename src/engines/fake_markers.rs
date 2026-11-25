//! The `FAKE:` marker grammar of INTERFACES.md §10 (lane 001), shared by
//! level A (`engines::fake`) and level B (lane 002's `convertd worker test`).
//!
//! A pure function over bytes: no I/O, no runtime, no engine types, so a
//! worker without tokio can call it and map the result to its own errors.
//!
//! Grammar, anywhere in the first [`SCAN_LIMIT`] bytes:
//!
//! - `FAKE:DELAY_MS=<n>`: answer after `n` milliseconds (never past the
//!   deadline);
//! - `FAKE:PAGES=<n>`: the PDF has `n` pages (`n` ≥ 1);
//! - `FAKE:HANG`: never answer, ignoring the deadline;
//! - `FAKE:<CODE>`: fail with that code, one of [`MarkerCode`].
//!
//! The first marker by position wins. A `FAKE:` that is not followed by
//! one of these (for example `FAKE:PAGES=0` or `FAKE:HANGS`) is not a
//! marker and the scan goes on. A marker must lie entirely in the scanned
//! window.

#![cfg(feature = "test-hooks")]

/// Only this many leading bytes are scanned.
pub const SCAN_LIMIT: usize = 1024 * 1024;

const PREFIX: &[u8] = b"FAKE:";

/// One fake behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    DelayMs(u64),
    Pages(u32),
    Hang,
    Code(MarkerCode),
}

/// The error codes a marker can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerCode {
    PasswordRequired,
    InvalidPassword,
    LimitExceeded,
    ConversionFailed,
    TooManyRequests,
    ServiceUnavailable,
    InternalError,
}

impl MarkerCode {
    pub const ALL: [MarkerCode; 7] = [
        MarkerCode::PasswordRequired,
        MarkerCode::InvalidPassword,
        MarkerCode::LimitExceeded,
        MarkerCode::ConversionFailed,
        MarkerCode::TooManyRequests,
        MarkerCode::ServiceUnavailable,
        MarkerCode::InternalError,
    ];

    /// The name after `FAKE:`, which is also the API error code.
    pub fn as_str(self) -> &'static str {
        match self {
            MarkerCode::PasswordRequired => "PASSWORD_REQUIRED",
            MarkerCode::InvalidPassword => "INVALID_PASSWORD",
            MarkerCode::LimitExceeded => "LIMIT_EXCEEDED",
            MarkerCode::ConversionFailed => "CONVERSION_FAILED",
            MarkerCode::TooManyRequests => "TOO_MANY_REQUESTS",
            MarkerCode::ServiceUnavailable => "SERVICE_UNAVAILABLE",
            MarkerCode::InternalError => "INTERNAL_ERROR",
        }
    }
}

/// The first marker in the first [`SCAN_LIMIT`] bytes of `bytes`.
pub fn find(bytes: &[u8]) -> Option<Marker> {
    let window = &bytes[..bytes.len().min(SCAN_LIMIT)];
    memchr::memmem::find_iter(window, PREFIX)
        .find_map(|at| parse_after_prefix(&window[at + PREFIX.len()..]))
}

/// Parses what follows `FAKE:`.
fn parse_after_prefix(rest: &[u8]) -> Option<Marker> {
    if let Some(digits) = rest.strip_prefix(b"DELAY_MS=") {
        return number(digits).map(Marker::DelayMs);
    }
    if let Some(digits) = rest.strip_prefix(b"PAGES=") {
        return number(digits)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n >= 1)
            .map(Marker::Pages);
    }
    if word(rest, b"HANG") {
        return Some(Marker::Hang);
    }
    MarkerCode::ALL
        .into_iter()
        .find(|code| word(rest, code.as_str().as_bytes()))
        .map(Marker::Code)
}

/// `rest` starts with `name`, and the name is not the start of a longer
/// identifier.
fn word(rest: &[u8], name: &[u8]) -> bool {
    rest.starts_with(name)
        && !rest
            .get(name.len())
            .is_some_and(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// The run of ASCII digits at the start of `rest` as a `u64`; `None` when
/// there is none or it overflows.
fn number(rest: &[u8]) -> Option<u64> {
    let len = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if len == 0 {
        return None;
    }
    std::str::from_utf8(&rest[..len]).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_marker() {
        assert_eq!(find(b"FAKE:DELAY_MS=6000"), Some(Marker::DelayMs(6000)));
        assert_eq!(
            find(b"<w:t>FAKE:DELAY_MS=0</w:t>"),
            Some(Marker::DelayMs(0))
        );
        assert_eq!(find(b"FAKE:PAGES=3<"), Some(Marker::Pages(3)));
        assert_eq!(find(b"%FAKE:HANG\n"), Some(Marker::Hang));
        for code in MarkerCode::ALL {
            let text = format!("<w:t>FAKE:{}</w:t>", code.as_str());
            assert_eq!(find(text.as_bytes()), Some(Marker::Code(code)), "{text}");
        }
    }

    #[test]
    fn nothing_without_a_marker() {
        assert_eq!(find(b""), None);
        assert_eq!(find(b"%PDF-1.4 plain"), None);
        assert_eq!(find(b"FAKE:"), None);
        assert_eq!(find(b"fake:HANG"), None);
    }

    #[test]
    fn near_misses_are_skipped() {
        for text in [
            "FAKE:HANGS",
            "FAKE:HANG_UP",
            "FAKE:PAGES=0",
            "FAKE:PAGES=",
            "FAKE:PAGES=x",
            "FAKE:PAGES=4294967296",
            "FAKE:DELAY_MS=",
            "FAKE:DELAY_MS=-1",
            "FAKE:DELAY_MS=99999999999999999999",
            "FAKE:INTERNAL_ERRORS",
            "FAKE:PANIC",
            "FAKE: HANG",
        ] {
            assert_eq!(find(text.as_bytes()), None, "{text}");
        }
    }

    #[test]
    fn first_by_position_wins() {
        let text = b"FAKE:PAGES=0 FAKE:CONVERSION_FAILED FAKE:HANG";
        assert_eq!(find(text), Some(Marker::Code(MarkerCode::ConversionFailed)));
        assert_eq!(find(b"FAKE:HANG FAKE:PAGES=2"), Some(Marker::Hang));
    }

    #[test]
    fn only_the_first_mib_is_scanned() {
        let mut bytes = vec![b' '; SCAN_LIMIT - 9];
        bytes.extend_from_slice(b"FAKE:HANG");
        assert_eq!(find(&bytes), Some(Marker::Hang));
        bytes.insert(0, b' ');
        assert_eq!(find(&bytes), None);
    }
}
