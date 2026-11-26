//! The state every handler shares, built once at start.

use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use axum::http::HeaderValue;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::middleware::auth::ApiKeys;
use super::openapi;
use super::serialization::build_info::{BuildInfo, FontsFile};
use super::serialization::capabilities::{Capabilities, ServiceInfo};
use super::serialization::manifest::EngineInfo;
use super::throughput::MinRate;
use crate::config::Config;
use crate::domain::conversion_service::ConversionService;
use crate::domain::engine::{OfficeEngine, PdfEngine};
use crate::domain::readiness::Readiness;
use crate::domain::verification::FontCatalog;
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
    /// `MAX_CONCURRENT_REQUESTS`: the permits `inflight` holds when idle.
    pub max_inflight: usize,
    /// Set on SIGTERM: readyz answers `draining` and conversions 503.
    pub draining: AtomicBool,
    /// `TMP_MAX_BYTES`.
    pub disk_budget: DiskBudget,
    pub tmp_dir: PathBuf,
    /// `IO_IDLE_TIMEOUT_SECONDS`: upload stall and response stall.
    pub io_idle_timeout: Duration,
    /// The minimum-throughput floors, over windows of `io_idle_timeout`.
    pub upload_rate: MinRate,
    pub response_rate: MinRate,
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
        // Install the metrics recorder before anything records: metrics
        // recorded earlier go to the no-op recorder and are lost.
        crate::observability::metrics::handle();
        let build_info = load_build_info(&config.server.build_info_path);
        let service = ServiceInfo {
            name: "convert".to_owned(),
            version: SERVICE_VERSION.to_owned(),
            commit: option_env!("CONVERTD_COMMIT").map(str::to_owned),
        };
        let unknown = || "unknown".to_owned();
        // fc-list prints a face's names comma-separated; capabilities and
        // the fingerprint list each face's first name.
        let mut fonts: Vec<String> = build_info
            .fonts
            .iter()
            .filter_map(|entry| entry.split(',').next())
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .collect();
        fonts.sort();
        fonts.dedup();
        let libreoffice = build_info.libreoffice.clone().unwrap_or_else(unknown);
        let pdfium = build_info.pdfium.clone().unwrap_or_else(unknown);
        let settings = output_settings(config);
        let catalog = FontCatalog::new(
            build_info.fonts.iter().cloned(),
            build_info.font_substitutes.clone(),
            build_info.default_substitute.clone(),
            load_fonts_file(&config.server.fonts_path).0,
        )
        .with_coverage(
            build_info
                .coverage
                .iter()
                .map(|[start, end]| (*start, *end)),
        );
        let fingerprint = fingerprint(&FingerprintParts {
            version: SERVICE_VERSION,
            commit: service.commit.as_deref().unwrap_or(""),
            libreoffice: &libreoffice,
            pdfium: &pdfium,
            fonts: &fonts,
            coverage: catalog.coverage(),
            settings: &settings,
        });
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
            )
            .with_probe_max_pages(config.server.verify_probe_max_pages)
            .with_fonts(catalog),
            inflight: Arc::new(Semaphore::new(config.server.max_concurrent_requests)),
            max_inflight: config.server.max_concurrent_requests,
            draining: AtomicBool::new(false),
            disk_budget: DiskBudget::new(config.server.tmp_max_bytes),
            tmp_dir: config.server.tmp_dir.clone(),
            io_idle_timeout: config.server.io_idle_timeout,
            upload_rate: MinRate {
                min_per_second: config.server.upload_min_bytes_per_second,
                window: config.server.io_idle_timeout,
            },
            response_rate: MinRate {
                min_per_second: config.server.response_min_bytes_per_second,
                window: config.server.io_idle_timeout,
            },
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

/// Reads `fonts.json`. Missing or unreadable: no family counts as
/// metric-compatible, with one warning.
pub fn load_fonts_file(path: &Path) -> FontsFile {
    match std::fs::read(path) {
        Ok(bytes) => FontsFile::parse(&bytes).unwrap_or_else(|err| {
            tracing::warn!(path = %path.display(), error = %err, "failed to parse the fonts file");
            FontsFile::default()
        }),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "fonts file missing; no family counts as metric-compatible");
            FontsFile::default()
        }
    }
}

/// What the fingerprint covers: the build, the engines, the fonts, and every
/// setting that changes the output bytes for the same input.
pub struct FingerprintParts<'a> {
    pub version: &'a str,
    pub commit: &'a str,
    pub libreoffice: &'a str,
    pub pdfium: &'a str,
    pub fonts: &'a [String],
    /// The merged glyph coverage, which decides `GLYPHS_MISSING`: a face
    /// can gain characters under the same family name.
    pub coverage: &'a [(u32, u32)],
    pub settings: &'a [(&'static str, String)],
}

/// The settings that change the output bytes, as `NAME` and value: the
/// render caps, the default page size, and the blank-page probe that
/// decides `BLANK_PAGE` warnings.
pub fn output_settings(config: &Config) -> Vec<(&'static str, String)> {
    vec![
        ("MAX_DPI", config.limits.max_dpi.to_string()),
        ("MAX_EDGE_PX", config.limits.max_edge_px.to_string()),
        (
            "DEFAULT_PAPER",
            format!("{:?}", config.engines.default_paper),
        ),
        (
            "VERIFY_PROBE_RENDER",
            config.server.verify_probe_render.to_string(),
        ),
        (
            "VERIFY_PROBE_MAX_PAGES",
            config.server.verify_probe_max_pages.to_string(),
        ),
    ]
}

/// `sha256:` plus the first 12 hex characters of SHA-256 over the service
/// version, commit, LibreOffice version, PDFium version, the sorted fonts
/// and each [`output_settings`] entry as `NAME=value`, each followed by
/// `\n`. A known coverage adds `coverage=` and its ranges as `start-end`
/// separated by `,`; without one the value is what it was before.
pub fn fingerprint(parts: &FingerprintParts<'_>) -> String {
    let mut hasher = Sha256::new();
    for part in [parts.version, parts.commit, parts.libreoffice, parts.pdfium] {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    for font in parts.fonts {
        hasher.update(font.as_bytes());
        hasher.update(b"\n");
    }
    if !parts.coverage.is_empty() {
        hasher.update(b"coverage=");
        for (n, (start, end)) in parts.coverage.iter().enumerate() {
            if n > 0 {
                hasher.update(b",");
            }
            hasher.update(format!("{start}-{end}").as_bytes());
        }
        hasher.update(b"\n");
    }
    for (name, value) in parts.settings {
        hasher.update(name.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
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
    use crate::config::Env;

    fn parts<'a>(
        fonts: &'a [String],
        settings: &'a [(&'static str, String)],
    ) -> FingerprintParts<'a> {
        FingerprintParts {
            version: "1.0.0",
            commit: "",
            libreoffice: "unknown",
            pdfium: "unknown",
            fonts,
            coverage: &[],
            settings,
        }
    }

    #[test]
    fn fingerprint_shape_and_inputs() {
        let defaults =
            output_settings(&Config::from_env(&Env::from_pairs([("API_KEY", "k")], 2)).unwrap());
        let a = fingerprint(&parts(&[], &defaults));
        assert!(a.starts_with("sha256:") && a.len() == 19, "{a}");
        assert!(a[7..].bytes().all(|b| b.is_ascii_hexdigit()));
        let b = fingerprint(&parts(&["Carlito".into()], &defaults));
        assert_ne!(a, b);
        assert_eq!(a, fingerprint(&parts(&[], &defaults)));
        // So does the glyph coverage.
        let covered = FingerprintParts {
            coverage: &[(32, 126)],
            ..parts(&[], &defaults)
        };
        let wider = FingerprintParts {
            coverage: &[(32, 591)],
            ..parts(&[], &defaults)
        };
        assert_ne!(a, fingerprint(&covered));
        assert_ne!(fingerprint(&covered), fingerprint(&wider));
        // Every setting that changes the output bytes changes it too.
        for (name, value) in [
            ("MAX_DPI", "200"),
            ("MAX_EDGE_PX", "2048"),
            ("DEFAULT_PAPER", "Letter"),
            ("VERIFY_PROBE_RENDER", "false"),
            ("VERIFY_PROBE_MAX_PAGES", "50"),
        ] {
            let config =
                Config::from_env(&Env::from_pairs([("API_KEY", "k"), (name, value)], 2)).unwrap();
            let changed = output_settings(&config);
            assert_ne!(a, fingerprint(&parts(&[], &changed)), "{name}");
        }
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
