//! The state every handler shares, built once at start.

use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderValue;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::middleware::auth::ApiKeys;
use super::openapi;
use super::serialization::build_info::BuildInfo;
use super::serialization::capabilities::{Capabilities, ServiceInfo};
use super::serialization::manifest::EngineInfo;
use crate::config::Config;
use crate::domain::conversion_service::ConversionService;
use crate::domain::engine::{OfficeEngine, PdfEngine};
use crate::domain::readiness::Readiness;
use crate::jobs::DiskBudget;

/// The service version in capabilities, manifests and the fingerprint.
pub const SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The engines `main.rs` picked: lane 002's, or the fakes.
pub struct Engines {
    pub office: Arc<dyn OfficeEngine>,
    pub pdf: Arc<dyn PdfEngine>,
    pub readiness: Arc<dyn Readiness>,
}

/// Cheap to clone: one `Arc`.
#[derive(Clone)]
pub struct AppState(Arc<Shared>);

impl Deref for AppState {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        &self.0
    }
}

pub struct Shared {
    pub service: ConversionService,
    /// `MAX_CONCURRENT_REQUESTS`: taken before the body is read, released
    /// by the response pump.
    pub inflight: Arc<Semaphore>,
    /// `TMP_MAX_BYTES`.
    pub disk_budget: DiskBudget,
    pub tmp_dir: PathBuf,
    /// `IO_IDLE_TIMEOUT_SECONDS`: upload stall and response stall.
    pub io_idle_timeout: Duration,
    pub api_keys: ApiKeys,
    /// `QUEUE_WAIT_SECONDS`, the `Retry-After` of a queue timeout.
    pub queue_wait_seconds: u32,
    pub readiness: Arc<dyn Readiness>,
    /// `Convert-Engine-Fingerprint`, computed once.
    pub fingerprint: HeaderValue,
    pub engine_info: EngineInfo,
    /// The `/v1/capabilities` body, fixed for the deployment's lifetime.
    pub capabilities_json: Bytes,
    /// The bytes `convertd openapi` prints, served at `/swagger/doc.json`.
    pub spec_json: Bytes,
    pub swagger_enabled: bool,
    pub metrics_enabled: bool,
}

impl AppState {
    pub fn new(config: &Config, engines: Engines) -> Self {
        let build_info = load_build_info(&config.server.build_info_path);
        let service = ServiceInfo {
            name: "convert".to_owned(),
            version: SERVICE_VERSION.to_owned(),
            commit: option_env!("CONVERTD_COMMIT").map(str::to_owned),
        };
        let unknown = || "unknown".to_owned();
        let mut fonts = build_info.fonts.clone();
        fonts.sort();
        fonts.dedup();
        let libreoffice = build_info.libreoffice.clone().unwrap_or_else(unknown);
        let pdfium = build_info.pdfium.clone().unwrap_or_else(unknown);
        let fingerprint = fingerprint(
            SERVICE_VERSION,
            service.commit.as_deref().unwrap_or(""),
            &libreoffice,
            &pdfium,
            &fonts,
        );
        let engine_info = EngineInfo {
            service_version: SERVICE_VERSION.to_owned(),
            libreoffice: Some(libreoffice),
            pdfium,
            fingerprint: fingerprint.clone(),
        };
        let capabilities = Capabilities::new(service, engine_info.clone(), &config.limits, fonts);
        // Serialising plain structs of strings and integers cannot fail.
        let capabilities_json = serde_json::to_vec(&capabilities).unwrap_or_default();
        let mut spec_json = openapi::spec_json();
        spec_json.push('\n');

        Self(Arc::new(Shared {
            service: ConversionService::new(
                engines.office,
                engines.pdf,
                config.limits.clone(),
                config.server.verify_probe_render,
            ),
            inflight: Arc::new(Semaphore::new(config.server.max_concurrent_requests)),
            disk_budget: DiskBudget::new(config.server.tmp_max_bytes),
            tmp_dir: config.server.tmp_dir.clone(),
            io_idle_timeout: config.server.io_idle_timeout,
            api_keys: ApiKeys::new(
                &config.server.api_key,
                config.server.api_key_previous.as_ref(),
            ),
            queue_wait_seconds: u32::try_from(config.engines.queue_wait.as_secs())
                .unwrap_or(u32::MAX),
            readiness: engines.readiness,
            fingerprint: HeaderValue::from_str(&fingerprint)
                .unwrap_or_else(|_| HeaderValue::from_static("sha256:unknown")),
            engine_info,
            capabilities_json: Bytes::from(capabilities_json),
            spec_json: Bytes::from(spec_json),
            swagger_enabled: config.server.swagger_enabled,
            metrics_enabled: config.server.metrics_enabled,
        }))
    }
}

/// Reads `build-info.json`. Missing or unreadable (dev, fake engines): the
/// versions are `unknown` and the fonts empty, with one warning.
pub fn load_build_info(path: &Path) -> BuildInfo {
    match std::fs::read(path) {
        Ok(bytes) => BuildInfo::parse(&bytes).unwrap_or_else(|err| {
            tracing::warn!(path = %path.display(), error = %err, "failed to parse build info");
            BuildInfo::default()
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(path = %path.display(), "build info missing; engine versions are unknown");
            BuildInfo::default()
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "failed to read build info");
            BuildInfo::default()
        }
    }
}

/// `sha256:` plus the first 12 hex characters of SHA-256 over the service
/// version, commit, LibreOffice version, PDFium version and the sorted
/// fonts, each separated by `\n`.
pub fn fingerprint(
    version: &str,
    commit: &str,
    libreoffice: &str,
    pdfium: &str,
    fonts: &[String],
) -> String {
    let mut hasher = Sha256::new();
    for part in [version, commit, libreoffice, pdfium] {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    for font in fonts {
        hasher.update(font.as_bytes());
        hasher.update(b"\n");
    }
    let digest: [u8; 32] = hasher.finalize().into();
    format!("sha256:{}", &hex(&digest)[..12])
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_shape_and_inputs() {
        let a = fingerprint("1.0.0", "", "unknown", "unknown", &[]);
        assert!(a.starts_with("sha256:") && a.len() == 19, "{a}");
        assert!(a[7..].bytes().all(|b| b.is_ascii_hexdigit()));
        let b = fingerprint("1.0.0", "", "unknown", "unknown", &["Carlito".into()]);
        assert_ne!(a, b);
        assert_eq!(a, fingerprint("1.0.0", "", "unknown", "unknown", &[]));
    }

    #[test]
    fn hex_is_lowercase() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn missing_build_info_is_empty() {
        let info = load_build_info(Path::new("/nonexistent/build-info.json"));
        assert!(info.libreoffice.is_none() && info.fonts.is_empty());
    }
}
