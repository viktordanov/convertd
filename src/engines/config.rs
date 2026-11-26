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

/// `OFFICE_MAX_JOBS` when neither it nor `WORKER_MAX_JOBS` is set. An office
/// worker lived 20 s at 50 jobs in the soak, and its RSS at recycle did not
/// grow with the job count, so 200 cuts the restarts by 4.
/// `OFFICE_MAX_RSS_MB` still recycles a worker that grows.
pub const OFFICE_DEFAULT_MAX_JOBS: u32 = 200;

/// The recycle and kill limits of one pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolLimits {
    /// Replace a worker after this many jobs.
    pub max_jobs: u32,
    /// Replace a worker whose RSS is at least this after a job.
    pub max_rss_mb: u64,
    /// Kill a worker whose RSS passes this during a job.
    pub kill_rss_mb: u64,
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
    /// The office pool's limits: `OFFICE_MAX_JOBS`, `OFFICE_MAX_RSS_MB` and
    /// `OFFICE_KILL_RSS_MB`, else the shared `WORKER_*` values. The job limit
    /// falls back to [`OFFICE_DEFAULT_MAX_JOBS`] when `WORKER_MAX_JOBS` is
    /// not set either.
    pub office: PoolLimits,
    /// The render pool's limits: `RENDER_MAX_JOBS`, `RENDER_MAX_RSS_MB` and
    /// `RENDER_KILL_RSS_MB`, else the shared `WORKER_*` values.
    pub render: PoolLimits,
    pub lo_program_dir: PathBuf,
    pub lo_profile_template: Option<PathBuf>,
    pub pdfium_lib_dir: PathBuf,
    /// Zero turns poison memory off.
    pub poison_ttl: Duration,
    pub default_paper: Paper,
}

impl EngineConfig {
    /// Parses the engine variables from `env`.
    pub fn from_env(env: &Env) -> Result<Self, ConfigError> {
        let cpus = env.cpus();
        let office_workers = env.positive("OFFICE_WORKERS", (cpus / 2).max(1))?;
        let worker_max_jobs = env.positive("WORKER_MAX_JOBS", 50)?;
        let worker_max_rss_mb = env.positive("WORKER_MAX_RSS_MB", 1024)?;
        let worker_kill_rss_mb = env.positive("WORKER_KILL_RSS_MB", 2048)?;
        // The office pool's own default unless WORKER_MAX_JOBS is set.
        let office_jobs = if env.get("WORKER_MAX_JOBS")?.is_some() {
            worker_max_jobs
        } else {
            OFFICE_DEFAULT_MAX_JOBS
        };
        let limits = |names: [&'static str; 3], jobs: u32| -> Result<PoolLimits, ConfigError> {
            let [max_jobs, max_rss, kill_rss] = names;
            Ok(PoolLimits {
                max_jobs: env.positive(max_jobs, jobs)?,
                max_rss_mb: env.positive(max_rss, worker_max_rss_mb)?,
                kill_rss_mb: env.positive(kill_rss, worker_kill_rss_mb)?,
            })
        };
        Ok(Self {
            office: limits(
                ["OFFICE_MAX_JOBS", "OFFICE_MAX_RSS_MB", "OFFICE_KILL_RSS_MB"],
                office_jobs,
            )?,
            render: limits(
                ["RENDER_MAX_JOBS", "RENDER_MAX_RSS_MB", "RENDER_KILL_RSS_MB"],
                worker_max_jobs,
            )?,
            office_workers,
            render_workers: env.positive("RENDER_WORKERS", cpus)?,
            queue_capacity: env.parse("QUEUE_CAPACITY", 2 * office_workers)?,
            queue_wait: env.seconds("QUEUE_WAIT_SECONDS", 10)?,
            worker_max_jobs,
            worker_max_rss_mb,
            worker_max_age: env.positive_seconds("WORKER_MAX_AGE_SECONDS", 3600)?,
            worker_kill_rss_mb,
            lo_program_dir: env.path("LO_PROGRAM_DIR", "/opt/libreoffice/program")?,
            lo_profile_template: env.get("LO_PROFILE_TEMPLATE")?.map(PathBuf::from),
            pdfium_lib_dir: env.path("PDFIUM_LIB_DIR", "/opt/pdfium/lib")?,
            poison_ttl: env.seconds("POISON_TTL_SECONDS", 600)?,
            default_paper: env.parse("DEFAULT_PAPER", Paper::A4)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engines(pairs: &[(&str, &str)]) -> EngineConfig {
        EngineConfig::from_env(&Env::from_pairs(pairs.iter().copied(), 2)).unwrap()
    }

    #[test]
    fn each_pool_falls_back_to_the_shared_limits() {
        let e = engines(&[
            ("WORKER_KILL_RSS_MB", "1536"),
            ("WORKER_MAX_RSS_MB", "1024"),
            ("WORKER_MAX_JOBS", "40"),
        ]);
        for limits in [e.office, e.render] {
            assert_eq!(limits.kill_rss_mb, 1536);
            assert_eq!(limits.max_rss_mb, 1024);
            assert_eq!(limits.max_jobs, 40);
        }
    }

    #[test]
    fn the_office_pool_defaults_to_200_jobs_and_the_render_pool_to_50() {
        let e = engines(&[]);
        assert_eq!((e.office.max_jobs, e.render.max_jobs), (200, 50));
        // An explicit shared value applies to both pools.
        let e = engines(&[("WORKER_MAX_JOBS", "40")]);
        assert_eq!((e.office.max_jobs, e.render.max_jobs), (40, 40));
        let e = engines(&[("WORKER_MAX_JOBS", "40"), ("OFFICE_MAX_JOBS", "300")]);
        assert_eq!((e.office.max_jobs, e.render.max_jobs), (300, 40));
    }

    #[test]
    fn each_pool_takes_its_own_limits() {
        let e = engines(&[
            ("OFFICE_KILL_RSS_MB", "1280"),
            ("OFFICE_MAX_RSS_MB", "960"),
            ("RENDER_KILL_RSS_MB", "256"),
            ("RENDER_MAX_RSS_MB", "192"),
            ("OFFICE_MAX_JOBS", "200"),
        ]);
        assert_eq!((e.office.max_jobs, e.render.max_jobs), (200, 50));
        assert_eq!((e.office.kill_rss_mb, e.office.max_rss_mb), (1280, 960));
        assert_eq!((e.render.kill_rss_mb, e.render.max_rss_mb), (256, 192));
        assert!(
            EngineConfig::from_env(&Env::from_pairs([("RENDER_KILL_RSS_MB", "0")], 2)).is_err(),
            "a limit of 0 is refused"
        );
    }
}
