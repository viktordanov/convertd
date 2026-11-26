//! Which engines `convertd serve` runs, and the handle that stops them (lane 002).
//!
//! `main.rs` calls [`start`] once and [`EngineHandle::shutdown`] after the
//! server stops.
//!
//! - `TEST_ENGINE` unset: the office pool runs `convertd worker office`
//!   (LibreOfficeKit) and the render pool `convertd worker render`
//!   (PDFium).
//! - `TEST_ENGINE=worker` (level B, feature `test-hooks`): both pools run
//!   `convertd worker test` behind the same pools, queue and kills.
//! - `TEST_ENGINE=fake` (level A): lane 001's in-process fakes, no pools.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::domain::engine::{OfficeEngine, PdfEngine};
use crate::domain::readiness::{PoolStatus, Readiness, ReadinessSnapshot, ReadinessStatus};
use crate::engines::office_lok::OfficeLok;
use crate::engines::pdf_pdfium::PdfPdfium;
use crate::engines::poison::PoisonList;
use crate::engines::pool::{Pool, PoolSpec};
use crate::engines::process::{ReaperMode, Spawner};
use crate::engines::slot::WorkerEnv;
use crate::ipc::{Init, PROTOCOL_VERSION};
use crate::worker::Role;

/// How long a worker may take from spawn to `Ready`, warm-up included.
pub const START_TIMEOUT: Duration = Duration::from_secs(60);

/// How often idle workers are checked for `WORKER_MAX_AGE_SECONDS`.
pub const AGE_CHECK: Duration = Duration::from_secs(30);

/// The engines that `serve` hands to the HTTP layer.
pub struct Selected {
    pub office: Arc<dyn OfficeEngine>,
    pub pdf: Arc<dyn PdfEngine>,
    pub readiness: Arc<dyn Readiness>,
    pub handle: EngineHandle,
}

/// Stops the engines at shutdown.
pub struct EngineHandle {
    pools: Vec<Pool>,
    workers_dir: Option<PathBuf>,
}

impl EngineHandle {
    #[cfg(feature = "test-hooks")]
    fn none() -> Self {
        Self {
            pools: Vec::new(),
            workers_dir: None,
        }
    }

    /// Stops new leases; in-flight calls finish. Called on SIGTERM.
    pub async fn drain(&self) {
        for pool in &self.pools {
            pool.drain();
        }
    }

    /// Waits up to `grace` for in-flight calls, then kills every worker and
    /// removes `$TMP_DIR/workers`.
    pub async fn shutdown(self, grace: Duration) {
        let stops = self.pools.iter().map(|pool| pool.shutdown(grace));
        futures_util::future::join_all(stops).await;
        if let Some(dir) = self.workers_dir {
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        }
    }
}

/// Readiness over the two pools (the spec's `Readiness`):
///
/// - `draining` after `drain`;
/// - `ready` when every pool has a live `Ready` worker, idle or busy;
/// - `starting` while some pool has never been ready;
/// - `degraded` when a pool was ready once and has no live worker now.
pub struct PoolReadiness {
    office: Pool,
    render: Pool,
}

impl PoolReadiness {
    pub fn new(office: Pool, render: Pool) -> Self {
        Self { office, render }
    }
}

fn pool_status(pool: &Pool) -> (PoolStatus, crate::engines::pool::PoolStatus) {
    let status = pool.status();
    let counts = PoolStatus {
        target: u32::from(status.target),
        idle: status.idle as u32,
        busy: status.busy as u32,
    };
    (counts, status)
}

/// The readiness of a set of pool states (pure, for tests).
pub fn readiness_of(pools: &[crate::engines::pool::PoolStatus]) -> ReadinessStatus {
    if pools.iter().any(|p| p.draining) {
        ReadinessStatus::Draining
    } else if pools.iter().all(|p| p.idle + p.busy > 0) {
        ReadinessStatus::Ready
    } else if pools.iter().any(|p| !p.ready_once) {
        ReadinessStatus::Starting
    } else {
        ReadinessStatus::Degraded
    }
}

impl Readiness for PoolReadiness {
    fn readiness(&self) -> ReadinessSnapshot {
        let (office, office_raw) = pool_status(&self.office);
        let (render, render_raw) = pool_status(&self.render);
        ReadinessSnapshot {
            status: readiness_of(&[office_raw, render_raw]),
            office,
            render,
        }
    }
}

/// Starts the engines that `config` selects.
pub async fn start(config: &Config) -> Result<Selected, String> {
    #[cfg(feature = "test-hooks")]
    if config.test_engine == Some(crate::config::TestEngine::Fake) {
        use super::fake::{FakeOffice, FakePdf, FakeReadiness};
        tracing::warn!("using the level-A fake engines (TEST_ENGINE=fake)");
        return Ok(Selected {
            office: Arc::new(FakeOffice::from_markers()),
            pdf: Arc::new(FakePdf::from_markers()),
            readiness: Arc::new(FakeReadiness::new(true)),
            handle: EngineHandle::none(),
        });
    }
    let level_b = config.test_engine.is_some();
    let (office_role, render_role) = roles(level_b)?;

    let workers_dir = config.server.tmp_dir.join("workers");
    let stale = workers_dir.clone();
    tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&stale) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    })
    .await
    .map_err(|err| format!("failed to clear the worker slots: {err}"))?
    .map_err(|err| format!("failed to clear the worker slots: {err}"))?;

    let spawner = Spawner::new(ReaperMode::ProcessWide)
        .map_err(|err| format!("failed to start the process spawner: {err}"))?;
    let program = std::env::current_exe()
        .map_err(|err| format!("failed to find the convertd binary: {err}"))?;
    let env = WorkerEnv::from_process(config.engines.default_paper)
        .with_path_prefix(&config.engines.lo_program_dir);
    if env.lc_all_dropped {
        tracing::warn!("LC_ALL is set; it is not passed to the workers, so LC_PAPER applies");
    }
    let engines = &config.engines;
    let init = Init {
        protocol: PROTOCOL_VERSION,
        lo_program_dir: Some(engines.lo_program_dir.clone()),
        pdfium_lib_dir: Some(engines.pdfium_lib_dir.clone()),
        warmup: !level_b,
        test: Default::default(),
    };
    let spec = |name: &'static str,
                role: Role,
                size: usize,
                queue_capacity: usize,
                limits: crate::engines::config::PoolLimits| PoolSpec {
        name,
        role,
        program: program.clone(),
        workers_dir: workers_dir.clone(),
        profile_template: engines.lo_profile_template.clone(),
        env: env.clone(),
        init: init.clone(),
        size: u16::try_from(size).unwrap_or(u16::MAX),
        max_jobs: limits.max_jobs,
        max_rss: limits.max_rss_mb.saturating_mul(1 << 20),
        kill_rss: limits.kill_rss_mb.saturating_mul(1 << 20),
        max_age: engines.worker_max_age,
        age_check: AGE_CHECK,
        queue_capacity,
        queue_wait: engines.queue_wait,
        start_timeout: START_TIMEOUT,
    };
    let office = Pool::start(
        spec(
            "office",
            office_role,
            engines.office_workers,
            engines.queue_capacity,
            engines.office,
        ),
        spawner.clone(),
    );
    let render = Pool::start(
        spec(
            "render",
            render_role,
            engines.render_workers,
            engines.render_workers.saturating_mul(2),
            engines.render,
        ),
        spawner,
    );
    tracing::info!(
        office_workers = engines.office_workers,
        render_workers = engines.render_workers,
        office_kill_rss_mb = engines.office.kill_rss_mb,
        render_kill_rss_mb = engines.render.kill_rss_mb,
        level_b,
        "engine pools starting"
    );
    // One poison memory for both pools; None when POISON_TTL_SECONDS is 0.
    let poison = PoisonList::new(engines.poison_ttl);
    Ok(Selected {
        office: Arc::new(OfficeLok::new(office.clone()).with_poison(poison.clone())),
        pdf: Arc::new(PdfPdfium::new(render.clone()).with_poison(poison)),
        readiness: Arc::new(PoolReadiness::new(office.clone(), render.clone())),
        handle: EngineHandle {
            pools: vec![office, render],
            workers_dir: Some(workers_dir),
        },
    })
}

#[cfg(feature = "test-hooks")]
fn roles(level_b: bool) -> Result<(Role, Role), String> {
    Ok(if level_b {
        tracing::warn!("using level B: both pools run convertd worker test (TEST_ENGINE=worker)");
        (Role::Test, Role::Test)
    } else {
        (Role::Office, Role::Render)
    })
}

#[cfg(not(feature = "test-hooks"))]
fn roles(level_b: bool) -> Result<(Role, Role), String> {
    if level_b {
        // config.rs refuses TEST_ENGINE without the feature; never reached.
        return Err("failed to start the engines: TEST_ENGINE needs feature test-hooks".into());
    }
    Ok((Role::Office, Role::Render))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::pool::PoolStatus as Raw;

    fn pool(idle: usize, busy: usize, ready_once: bool, draining: bool) -> Raw {
        Raw {
            target: 1,
            idle,
            busy,
            ready_once,
            draining,
            ..Raw::default()
        }
    }

    #[test]
    fn readiness_states() {
        let ready = pool(1, 0, true, false);
        let busy = pool(0, 1, true, false);
        let new = pool(0, 0, false, false);
        let lost = pool(0, 0, true, false);
        let draining = pool(1, 0, true, true);
        assert_eq!(readiness_of(&[ready, busy]), ReadinessStatus::Ready);
        assert_eq!(readiness_of(&[ready, new]), ReadinessStatus::Starting);
        assert_eq!(readiness_of(&[ready, lost]), ReadinessStatus::Degraded);
        assert_eq!(readiness_of(&[draining, ready]), ReadinessStatus::Draining);
    }
}
