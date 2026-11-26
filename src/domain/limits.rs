//! The server's caps on one request.

use std::time::Duration;

/// Bytes in one MiB.
pub const MIB: u64 = 1024 * 1024;

/// Request caps, from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_upload_bytes: u64,
    pub timeout_default: Duration,
    /// Requested timeouts above it are lowered to it without an error.
    pub timeout_max: Duration,
    pub max_render_pages: u32,
    /// Requested DPI above it is clamped, with a `RENDER_CLAMPED` warning.
    pub max_dpi: u16,
    pub max_edge_px: u32,
    /// The cap on what one engine job may write:
    /// `min(TMP_MAX_BYTES / 4, 4 GiB − 1 MiB)`.
    pub max_output_bytes: u64,
}

impl Limits {
    /// What an office conversion may write:
    /// `min(max_output_bytes, max(64 MiB, 8 × source size))`.
    pub fn pdf_output_bytes(&self, source_bytes: u64) -> u64 {
        source_bytes
            .saturating_mul(8)
            .max(64 * MIB)
            .min(self.max_output_bytes)
    }

    /// What decrypting a PDF may write: the source size plus 1 MiB.
    pub fn decrypt_output_bytes(&self, source_bytes: u64) -> u64 {
        source_bytes.saturating_add(MIB).min(self.max_output_bytes)
    }
}

#[cfg(test)]
pub(crate) fn test_limits() -> Limits {
    Limits {
        max_upload_bytes: 125_829_120,
        timeout_default: Duration::from_secs(120),
        timeout_max: Duration::from_secs(210),
        max_render_pages: 500,
        max_dpi: 300,
        max_edge_px: 4096,
        max_output_bytes: 768 * MIB,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_reservations() {
        let limits = test_limits();
        assert_eq!(limits.pdf_output_bytes(1000), 64 * MIB);
        assert_eq!(limits.pdf_output_bytes(10 * MIB), 80 * MIB);
        assert_eq!(limits.pdf_output_bytes(200 * MIB), 768 * MIB);
        assert_eq!(limits.decrypt_output_bytes(10), MIB + 10);
        assert_eq!(limits.decrypt_output_bytes(u64::MAX), 768 * MIB);
    }
}
