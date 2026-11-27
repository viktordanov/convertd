//! Configuration from environment variables, parsed by hand (lane 001).
//!
//! Every variable of `INTERFACES.md` §4 and plan 001's configuration table
//! is parsed here, used or not, so a bad value stops start-up instead of
//! running half-configured. The engine variables are parsed by
//! [`EngineConfig`], which lane 002 owns. A variable set to the empty
//! string counts as unset.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::domain::limits::Limits;
use crate::engines::EngineConfig;

/// The cap on `TIMEOUT_MAX_SECONDS`.
pub const TIMEOUT_MAX_CAP_SECONDS: u64 = 210;

/// The upper bound on `max_output_bytes`: 4 GiB − 1 MiB.
const MAX_OUTPUT_BYTES_CAP: u64 = 4 * 1024 * 1024 * 1024 - 1024 * 1024;

/// A configuration value that stopped start-up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("failed to parse {name}: {reason}")]
pub struct ConfigError {
    /// The environment variable.
    pub name: &'static str,
    /// What is wrong with it.
    pub reason: String,
}

impl ConfigError {
    pub fn new(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            reason: reason.into(),
        }
    }
}

/// A snapshot of the environment, and the CPU count the defaults use.
///
/// Tests build one from pairs, so they never touch the process
/// environment.
#[derive(Clone)]
pub struct Env {
    vars: HashMap<OsString, OsString>,
    cpus: usize,
}

impl Env {
    /// The process environment and `available_parallelism()`.
    pub fn from_process() -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self {
            vars: std::env::vars_os().collect(),
            cpus,
        }
    }

    /// An environment made of `pairs`, with `cpus` CPUs.
    pub fn from_pairs<I, K, V>(pairs: I, cpus: usize) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        Self {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            cpus: cpus.max(1),
        }
    }

    /// The CPU count for the pool-size defaults, at least 1.
    pub fn cpus(&self) -> usize {
        self.cpus
    }

    /// The value of `name`, or `None` when it is unset or empty.
    pub fn get(&self, name: &'static str) -> Result<Option<&str>, ConfigError> {
        match self.vars.get(std::ffi::OsStr::new(name)) {
            None => Ok(None),
            Some(value) => match value.to_str() {
                Some("") => Ok(None),
                Some(value) => Ok(Some(value)),
                None => Err(ConfigError::new(name, "the value is not valid UTF-8")),
            },
        }
    }

    /// `name` parsed with `FromStr`, or `None` when unset.
    pub fn parse_opt<T>(&self, name: &'static str) -> Result<Option<T>, ConfigError>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        let Some(value) = self.get(name)? else {
            return Ok(None);
        };
        value
            .parse()
            .map(Some)
            .map_err(|err| ConfigError::new(name, format!("invalid value {value:?}: {err}")))
    }

    /// `name` parsed with `FromStr`, or `default` when unset.
    pub fn parse<T>(&self, name: &'static str, default: T) -> Result<T, ConfigError>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        Ok(self.parse_opt(name)?.unwrap_or(default))
    }

    /// An unsigned integer that must be at least 1.
    pub fn positive<T>(&self, name: &'static str, default: T) -> Result<T, ConfigError>
    where
        T: FromStr + PartialEq + Default,
        T::Err: fmt::Display,
    {
        let value = self.parse(name, default)?;
        if value == T::default() {
            return Err(ConfigError::new(name, "must be at least 1, got 0"));
        }
        Ok(value)
    }

    /// Whole seconds, zero allowed.
    pub fn seconds(&self, name: &'static str, default: u64) -> Result<Duration, ConfigError> {
        self.parse(name, default).map(Duration::from_secs)
    }

    /// Whole seconds, at least 1.
    pub fn positive_seconds(
        &self,
        name: &'static str,
        default: u64,
    ) -> Result<Duration, ConfigError> {
        self.positive(name, default).map(Duration::from_secs)
    }

    /// A path, or `default` when unset.
    pub fn path(&self, name: &'static str, default: &str) -> Result<PathBuf, ConfigError> {
        Ok(PathBuf::from(self.get(name)?.unwrap_or(default)))
    }
}

/// A secret value whose `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// `LOG_MODE`: `prod` logs JSON, `debug` logs pretty text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMode {
    Prod,
    Debug,
}

impl FromStr for LogMode {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "prod" => Ok(LogMode::Prod),
            "debug" => Ok(LogMode::Debug),
            _ => Err("expected prod or debug"),
        }
    }
}

/// `TEST_ENGINE`, accepted only in builds with cargo feature `test-hooks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestEngine {
    /// Level A: lane 001's fake engines, no real queue.
    Fake,
    /// Level B: both pools run `convertd worker test`.
    Worker,
}

impl FromStr for TestEngine {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fake" => Ok(TestEngine::Fake),
            "worker" => Ok(TestEngine::Worker),
            _ => Err("expected fake or worker"),
        }
    }
}

/// Everything `convertd serve` reads.
#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub limits: Limits,
    pub engines: EngineConfig,
    /// `None` means the real engines.
    pub test_engine: Option<TestEngine>,
}

/// The HTTP server, the job directories and the probes.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
    pub log_mode: LogMode,
    pub api_key: Secret,
    pub api_key_previous: Option<Secret>,
    /// Admission before the body is read; 429 beyond it.
    pub max_concurrent_requests: usize,
    /// Upload idle timeout and the response pump's per-chunk send timeout.
    pub io_idle_timeout: Duration,
    /// The upload's floor, averaged over each `io_idle_timeout`; 0 is off.
    pub upload_min_bytes_per_second: u64,
    /// The response body's floor, the same way; 0 is off.
    pub response_min_bytes_per_second: u64,
    pub shutdown_grace: Duration,
    /// `HEADER_READ_TIMEOUT_SECONDS`: the longest a connection may take to
    /// send a request's headers, on a new or an idle keep-alive connection.
    pub header_read_timeout: Duration,
    /// `MAX_CONNECTIONS`: open connections that serve every route; a few
    /// more serve only the probes.
    pub max_connections: usize,
    /// Root of the job directories and the worker slots.
    pub tmp_dir: PathBuf,
    /// The disk budget for everything under `tmp_dir`.
    pub tmp_max_bytes: u64,
    pub verify_probe_render: bool,
    /// Without a page selection, the blank-page probe looks at no more
    /// than this many pages.
    pub verify_probe_max_pages: u32,
    pub metrics_enabled: bool,
    /// Serves `/swagger/doc.json` only.
    pub swagger_enabled: bool,
    pub build_info_path: PathBuf,
    /// `FONTS_PATH`: the `{family: metric_compatible}` table for V2.
    pub fonts_path: PathBuf,
    /// `TOKIO_WORKER_THREADS`; `None` means 2.
    pub tokio_worker_threads: Option<usize>,
}

impl ServerConfig {
    /// The output cap per engine job: `min(TMP_MAX_BYTES / 4, 4 GiB − 1 MiB)`.
    pub fn max_output_bytes(&self) -> u64 {
        (self.tmp_max_bytes / 4).min(MAX_OUTPUT_BYTES_CAP)
    }
}

impl Config {
    /// Parses the process environment.
    pub fn from_process() -> Result<Self, ConfigError> {
        Self::from_env(&Env::from_process())
    }

    /// Parses `env`. `API_KEY` is required.
    pub fn from_env(env: &Env) -> Result<Self, ConfigError> {
        let server = ServerConfig::from_env(env)?;
        let limits = limits_from_env(env, server.max_output_bytes())?;
        Ok(Self {
            server,
            limits,
            engines: EngineConfig::from_env(env)?,
            test_engine: test_engine(env)?,
        })
    }
}

/// `PORT`, default 8080. `convertd healthcheck` reads only this.
pub fn port(env: &Env) -> Result<u16, ConfigError> {
    env.parse("PORT", 8080)
}

impl ServerConfig {
    fn from_env(env: &Env) -> Result<Self, ConfigError> {
        let api_key = env
            .get("API_KEY")?
            .map(Secret::new)
            .ok_or_else(|| ConfigError::new("API_KEY", "required, and must not be empty"))?;
        Ok(Self {
            host: env.parse("HOST", IpAddr::from([0, 0, 0, 0]))?,
            port: port(env)?,
            log_mode: env.parse("LOG_MODE", LogMode::Prod)?,
            api_key,
            api_key_previous: env.get("API_KEY_PREVIOUS")?.map(Secret::new),
            max_concurrent_requests: env.positive("MAX_CONCURRENT_REQUESTS", 16)?,
            io_idle_timeout: env.positive_seconds("IO_IDLE_TIMEOUT_SECONDS", 30)?,
            upload_min_bytes_per_second: env.parse("UPLOAD_MIN_BYTES_PER_SECOND", 16_384)?,
            response_min_bytes_per_second: env.parse("RESPONSE_MIN_BYTES_PER_SECOND", 16_384)?,
            shutdown_grace: env.seconds("SHUTDOWN_GRACE_SECONDS", 30)?,
            header_read_timeout: env.positive_seconds("HEADER_READ_TIMEOUT_SECONDS", 10)?,
            max_connections: env.positive("MAX_CONNECTIONS", 1024)?,
            tmp_dir: env.path("TMP_DIR", "/tmp/convert")?,
            tmp_max_bytes: env.positive("TMP_MAX_BYTES", 3_221_225_472)?,
            verify_probe_render: env.parse("VERIFY_PROBE_RENDER", true)?,
            verify_probe_max_pages: env.positive("VERIFY_PROBE_MAX_PAGES", 200)?,
            metrics_enabled: env.parse("METRICS_ENABLED", true)?,
            swagger_enabled: env.parse("SWAGGER_ENABLED", true)?,
            build_info_path: env.path("BUILD_INFO_PATH", "/opt/convertd/build-info.json")?,
            fonts_path: env.path("FONTS_PATH", "/etc/convertd/fonts.json")?,
            tokio_worker_threads: match env.parse_opt::<usize>("TOKIO_WORKER_THREADS")? {
                Some(0) => {
                    return Err(ConfigError::new(
                        "TOKIO_WORKER_THREADS",
                        "must be at least 1, got 0",
                    ));
                }
                other => other,
            },
        })
    }
}

/// The request caps; `max_output_bytes` comes from `TMP_MAX_BYTES`.
fn limits_from_env(env: &Env, max_output_bytes: u64) -> Result<Limits, ConfigError> {
    let timeout_max: u64 = env.positive("TIMEOUT_MAX_SECONDS", TIMEOUT_MAX_CAP_SECONDS)?;
    if timeout_max > TIMEOUT_MAX_CAP_SECONDS {
        return Err(ConfigError::new(
            "TIMEOUT_MAX_SECONDS",
            format!("must be at most {TIMEOUT_MAX_CAP_SECONDS}, got {timeout_max}"),
        ));
    }
    let timeout_default: u64 = env.positive("TIMEOUT_DEFAULT_SECONDS", 120)?;
    if timeout_default > timeout_max {
        return Err(ConfigError::new(
            "TIMEOUT_DEFAULT_SECONDS",
            format!("must be at most TIMEOUT_MAX_SECONDS ({timeout_max}), got {timeout_default}"),
        ));
    }
    Ok(Limits {
        max_upload_bytes: env.positive("MAX_UPLOAD_BYTES", 125_829_120)?,
        timeout_default: Duration::from_secs(timeout_default),
        timeout_max: Duration::from_secs(timeout_max),
        max_render_pages: env.positive("MAX_RENDER_PAGES", 500)?,
        max_dpi: env.positive("MAX_DPI", 300)?,
        max_edge_px: env.positive("MAX_EDGE_PX", 4096)?,
        max_output_bytes,
    })
}

fn test_engine(env: &Env) -> Result<Option<TestEngine>, ConfigError> {
    if env.get("TEST_ENGINE")?.is_none() {
        return Ok(None);
    }
    if !cfg!(feature = "test-hooks") {
        return Err(ConfigError::new(
            "TEST_ENGINE",
            "test engines need a build with cargo feature test-hooks",
        ));
    }
    env.parse_opt("TEST_ENGINE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::config::Paper;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env::from_pairs(pairs.iter().copied(), 8)
    }

    fn parse(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_env(&env(pairs))
    }

    #[test]
    fn defaults() {
        let config = parse(&[("API_KEY", "k")]).unwrap();

        let s = &config.server;
        assert_eq!(s.host, IpAddr::from([0, 0, 0, 0]));
        assert_eq!(s.port, 8080);
        assert_eq!(s.log_mode, LogMode::Prod);
        assert_eq!(s.api_key.expose(), "k");
        assert_eq!(s.api_key_previous, None);
        assert_eq!(s.max_concurrent_requests, 16);
        assert_eq!(s.io_idle_timeout, Duration::from_secs(30));
        assert_eq!(s.upload_min_bytes_per_second, 16_384);
        assert_eq!(s.response_min_bytes_per_second, 16_384);
        assert_eq!(s.shutdown_grace, Duration::from_secs(30));
        assert_eq!(s.tmp_dir, PathBuf::from("/tmp/convert"));
        assert_eq!(s.tmp_max_bytes, 3_221_225_472);
        assert_eq!(s.max_output_bytes(), 768 * 1024 * 1024);
        assert!(s.verify_probe_render);
        assert_eq!(s.verify_probe_max_pages, 200);
        assert!(s.metrics_enabled);
        assert!(s.swagger_enabled);
        assert_eq!(
            s.build_info_path,
            PathBuf::from("/opt/convertd/build-info.json")
        );
        assert_eq!(s.fonts_path, PathBuf::from("/etc/convertd/fonts.json"));
        assert_eq!(s.tokio_worker_threads, None);

        let l = &config.limits;
        assert_eq!(l.max_upload_bytes, 125_829_120);
        assert_eq!(l.timeout_default, Duration::from_secs(120));
        assert_eq!(l.timeout_max, Duration::from_secs(210));
        assert_eq!(l.max_render_pages, 500);
        assert_eq!(l.max_dpi, 300);
        assert_eq!(l.max_edge_px, 4096);

        let e = &config.engines;
        assert_eq!(e.office_workers, 4);
        assert_eq!(e.render_workers, 8);
        assert_eq!(e.queue_capacity, 8);
        assert_eq!(e.queue_wait, Duration::from_secs(10));
        assert_eq!(e.worker_max_jobs, 50);
        assert_eq!(e.worker_max_rss_mb, 1024);
        assert_eq!(e.worker_max_age, Duration::from_secs(3600));
        assert_eq!(e.worker_kill_rss_mb, 2048);
        assert_eq!(e.lo_program_dir, PathBuf::from("/opt/libreoffice/program"));
        assert_eq!(e.lo_profile_template, None);
        assert_eq!(e.pdfium_lib_dir, PathBuf::from("/opt/pdfium/lib"));
        assert_eq!(e.poison_ttl, Duration::from_secs(600));
        assert_eq!(e.default_paper, Paper::A4);

        assert_eq!(config.test_engine, None);
    }

    #[test]
    fn pool_defaults_follow_the_cpu_count() {
        let one = Config::from_env(&Env::from_pairs([("API_KEY", "k")], 1)).unwrap();
        assert_eq!(one.engines.office_workers, 1);
        assert_eq!(one.engines.render_workers, 1);
        assert_eq!(one.engines.queue_capacity, 2);

        let three = parse(&[("API_KEY", "k"), ("OFFICE_WORKERS", "3")]).unwrap();
        assert_eq!(three.engines.office_workers, 3);
        assert_eq!(three.engines.queue_capacity, 6);
    }

    #[test]
    fn values_are_read() {
        let config = parse(&[
            ("API_KEY", "k"),
            ("API_KEY_PREVIOUS", "old"),
            ("HOST", "127.0.0.1"),
            ("PORT", "18080"),
            ("LOG_MODE", "debug"),
            ("TMP_MAX_BYTES", "68719476736"),
            ("DEFAULT_PAPER", "Letter"),
        ])
        .unwrap();
        assert_eq!(config.server.host, IpAddr::from([127, 0, 0, 1]));
        assert_eq!(config.server.port, 18080);
        assert_eq!(config.server.log_mode, LogMode::Debug);
        assert_eq!(
            config.server.api_key_previous.as_ref().map(Secret::expose),
            Some("old")
        );
        assert_eq!(config.server.max_output_bytes(), MAX_OUTPUT_BYTES_CAP);
        assert_eq!(config.engines.default_paper, Paper::Letter);
    }

    #[test]
    fn empty_counts_as_unset() {
        let config = parse(&[("API_KEY", "k"), ("PORT", ""), ("API_KEY_PREVIOUS", "")]).unwrap();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.api_key_previous, None);
    }

    #[test]
    fn bad_value_names_the_variable() {
        let err = parse(&[("API_KEY", "k"), ("PORT", "eighty")]).unwrap_err();
        assert_eq!(err.name, "PORT");
        assert!(
            err.to_string()
                .starts_with("failed to parse PORT: invalid value \"eighty\""),
            "{err}"
        );
    }

    #[test]
    fn ranges_are_enforced() {
        for (name, value) in [
            ("TIMEOUT_MAX_SECONDS", "211"),
            ("TIMEOUT_DEFAULT_SECONDS", "0"),
            ("MAX_CONCURRENT_REQUESTS", "0"),
            ("OFFICE_WORKERS", "0"),
            ("TOKIO_WORKER_THREADS", "0"),
            ("METRICS_ENABLED", "yes"),
            ("LOG_MODE", "verbose"),
            ("DEFAULT_PAPER", "A3"),
        ] {
            let err = parse(&[("API_KEY", "k"), (name, value)]).unwrap_err();
            assert_eq!(err.name, name, "{err}");
        }
        let err = parse(&[
            ("API_KEY", "k"),
            ("TIMEOUT_MAX_SECONDS", "60"),
            ("TIMEOUT_DEFAULT_SECONDS", "120"),
        ])
        .unwrap_err();
        assert_eq!(err.name, "TIMEOUT_DEFAULT_SECONDS");
    }

    #[test]
    fn missing_api_key_stops_start_up() {
        for pairs in [&[][..], &[("API_KEY", "")][..]] {
            let err = parse(pairs).unwrap_err();
            assert_eq!(err.name, "API_KEY");
            assert_eq!(
                err.to_string(),
                "failed to parse API_KEY: required, and must not be empty"
            );
        }
    }

    #[test]
    fn api_key_is_redacted_in_debug() {
        let config = parse(&[("API_KEY", "hunter2"), ("API_KEY_PREVIOUS", "hunter1")]).unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter"), "{debug}");
    }

    #[test]
    fn port_alone_needs_no_api_key() {
        assert_eq!(port(&env(&[])).unwrap(), 8080);
        assert_eq!(port(&env(&[("PORT", "9000")])).unwrap(), 9000);
    }

    #[cfg(not(feature = "test-hooks"))]
    #[test]
    fn test_engine_needs_the_feature() {
        let err = parse(&[("API_KEY", "k"), ("TEST_ENGINE", "fake")]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "failed to parse TEST_ENGINE: test engines need a build with cargo feature test-hooks"
        );
    }

    #[cfg(feature = "test-hooks")]
    #[test]
    fn test_engine_with_the_feature() {
        let fake = parse(&[("API_KEY", "k"), ("TEST_ENGINE", "fake")]).unwrap();
        assert_eq!(fake.test_engine, Some(TestEngine::Fake));
        let worker = parse(&[("API_KEY", "k"), ("TEST_ENGINE", "worker")]).unwrap();
        assert_eq!(worker.test_engine, Some(TestEngine::Worker));
        let err = parse(&[("API_KEY", "k"), ("TEST_ENGINE", "real")]).unwrap_err();
        assert_eq!(err.name, "TEST_ENGINE");
    }
}
