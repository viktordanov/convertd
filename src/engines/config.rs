//! Engine and pool configuration (lane 002), parsed through `crate::config::Env`.
//!
//! Workers read no configuration: the supervisor passes them what they need.
//! `TMP_DIR` is parsed once, in `config::ServerConfig`.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::config::{ConfigError, Env};

/// `DEFAULT_PAPER`, passed to workers as `LC_PAPER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paper {
    A4,
    Letter,
}

impl FromStr for Paper {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "A4" => Ok(Paper::A4),
            "Letter" => Ok(Paper::Letter),
            _ => Err("expected A4 or Letter"),
        }
    }
}

/// The engine variables of `INTERFACES.md` §4.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Default: half of the CPUs, at least 1.
    pub office_workers: usize,
    /// Default: all of the CPUs.
    pub render_workers: usize,
    /// Waiters allowed on the office pool. Default: 2 × `office_workers`.
    pub queue_capacity: usize,
    pub queue_wait: Duration,
    pub worker_max_jobs: u32,
    pub worker_max_rss_mb: u64,
    pub worker_max_age: Duration,
    /// Kill during a job above this RSS.
    pub worker_kill_rss_mb: u64,
    pub lo_program_dir: PathBuf,
    pub lo_profile_template: Option<PathBuf>,
    pub pdfium_lib_dir: PathBuf,
    /// Set in the image from the pinned build; `serve` with the real
    /// engines needs it (not enforced in the skeleton).
    pub pdfium_build: Option<String>,
    /// Zero turns poison memory off.
    pub poison_ttl: Duration,
    pub default_paper: Paper,
}

impl EngineConfig {
    /// Parses the engine variables from `env`.
    pub fn from_env(env: &Env) -> Result<Self, ConfigError> {
        let cpus = env.cpus();
        let office_workers = env.positive("OFFICE_WORKERS", (cpus / 2).max(1))?;
        Ok(Self {
            office_workers,
            render_workers: env.positive("RENDER_WORKERS", cpus)?,
            queue_capacity: env.parse("QUEUE_CAPACITY", 2 * office_workers)?,
            queue_wait: env.seconds("QUEUE_WAIT_SECONDS", 10)?,
            worker_max_jobs: env.positive("WORKER_MAX_JOBS", 50)?,
            worker_max_rss_mb: env.positive("WORKER_MAX_RSS_MB", 1024)?,
            worker_max_age: env.positive_seconds("WORKER_MAX_AGE_SECONDS", 3600)?,
            worker_kill_rss_mb: env.positive("WORKER_KILL_RSS_MB", 2048)?,
            lo_program_dir: env.path("LO_PROGRAM_DIR", "/opt/libreoffice/program")?,
            lo_profile_template: env.get("LO_PROFILE_TEMPLATE")?.map(PathBuf::from),
            pdfium_lib_dir: env.path("PDFIUM_LIB_DIR", "/opt/pdfium/lib")?,
            pdfium_build: env.get("PDFIUM_BUILD")?.map(str::to_owned),
            poison_ttl: env.seconds("POISON_TTL_SECONDS", 600)?,
            default_paper: env.parse("DEFAULT_PAPER", Paper::A4)?,
        })
    }
}
