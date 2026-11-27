//! What byte-level detection found. Detection itself is in `detect/`; the
//! domain only receives its result.

use super::document::Format;

/// The detected input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    pub format: Format,
    /// OOXML inside OLE2 (`EncryptionInfo` + `EncryptedPackage`), found by
    /// detection. Other encryption (legacy `.doc`/`.xls`, ODF, PDF) is found
    /// by the engines.
    pub encrypted: bool,
}

/// Why an upload cannot be converted, before any engine runs.
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    /// `label` names what was seen (`html`, `odb`, `zip`, `unknown`), for
    /// the 415 message. Never derived from the file name.
    #[error("unsupported format: {label}")]
    Unsupported { label: &'static str },
    /// The archive's declared entry count, expanded size or ratio is over
    /// a cap. Advisory: these are the archive's own claims.
    #[error("the archive exceeds the {limit} limit")]
    LimitExceeded { limit: &'static str },
    #[error("failed to read upload: {0}")]
    Io(#[from] std::io::Error),
}
