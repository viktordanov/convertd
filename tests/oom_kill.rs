//! A real kernel OOM kill of a worker (review of the ingestion, F).
//!
//! Runs where the container has a memory limit and cgroup v2: set
//! `CONVERTD_MEMORY_LIMIT_MB` to the limit, as in the dev image with
//! `--memory`. Elsewhere it prints `skipped` and passes. A file of its own,
//! so the kernel's victim is the worker that allocates, never another
//! test's process.
#![cfg(all(target_os = "linux", feature = "test-hooks"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use convertd::domain::engine::EngineError;
use convertd::engines::call::call_with_poison;
use convertd::engines::config::Paper;
use convertd::engines::poison::{Poison, PoisonList, Work};
use convertd::engines::pool::{Admission, CGROUP_MEMORY_EVENTS, Pool, PoolSpec, read_oom_kills};
use convertd::engines::process::{ReaperMode, Spawner};
use convertd::engines::slot::WorkerEnv;
use convertd::ipc::*;
use convertd::worker::Role;

/// The kernel kills the worker for the cgroup, at an RSS under both of its
/// own limits: the call is `LimitExceeded` (422) at once, with no crash
/// retry and no poison strike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kernel_oom_kill_is_limit_exceeded_with_no_retry_and_no_strike() {
    let Some(limit_mb) = std::env::var("CONVERTD_MEMORY_LIMIT_MB")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    else {
        eprintln!("skipped: no memory limit (CONVERTD_MEMORY_LIMIT_MB is not set)");
        return;
    };
    let events = Path::new(CGROUP_MEMORY_EVENTS);
    let Some(before) = read_oom_kills(events) else {
        eprintln!("skipped: no cgroup v2 ({CGROUP_MEMORY_EVENTS} has no oom_kill)");
        return;
    };
    let dir = std::env::temp_dir().join(format!("convertd-oom-kill-{}", std::process::id()));
    let spec = PoolSpec {
        name: "oom",
        role: Role::Test,
        program: PathBuf::from(env!("CARGO_BIN_EXE_convertd")),
        workers_dir: dir.join("workers"),
        profile_template: None,
        env: WorkerEnv::from_vars(Vec::new(), Paper::A4),
        init: Init {
            protocol: PROTOCOL_VERSION,
            ..Init::default()
        },
        size: 1,
        max_jobs: 1_000,
        // Both RSS rules off: only the cgroup's count can tell memory.
        max_rss: u64::MAX,
        kill_rss: u64::MAX,
        max_age: Duration::from_secs(3600),
        age_check: Duration::from_secs(30),
        queue_capacity: 4,
        queue_wait: Duration::from_secs(10),
        start_timeout: Duration::from_secs(10),
        oom_events: Some(events.to_path_buf()),
    };
    let pool = Pool::start(spec, Spawner::new(ReaperMode::Owned).unwrap());
    assert!(pool.wait_idle(1, Duration::from_secs(20)).await);
    let list = PoisonList::new(Duration::from_secs(60)).unwrap();
    let poison = Poison {
        list: Arc::clone(&list),
        key: [5; 32],
        work: Work::Convert,
    };
    let started = Instant::now();
    let result = call_with_poison(
        &pool,
        Op::Test(TestOp::Allocate { mb: limit_mb + 512 }),
        Admission::Queue,
        Instant::now() + Duration::from_secs(60),
        |_| false,
        Some(&poison),
    )
    .await;
    let took = started.elapsed();
    let after = read_oom_kills(events).unwrap();
    eprintln!("oom_kill {before} -> {after}, answered after {took:?}");
    assert!(
        after > before,
        "the kernel did not OOM-kill: raise the allocation"
    );
    assert!(
        matches!(result, Err(EngineError::LimitExceeded(_))),
        "{:?}",
        result.map(|c| c.reply.rss_bytes)
    );
    assert!(took < Duration::from_secs(20), "not fast: {took:?}");
    assert!(
        !list.is_poisoned(&poison.key),
        "a memory failure is no strike"
    );
    assert!(list.is_empty(), "no strike at all");
    // One death and one replacement: a crash retry would have started a
    // third worker.
    assert!(pool.wait_idle(1, Duration::from_secs(20)).await);
    let lease = pool
        .lease(Instant::now() + Duration::from_secs(10), Admission::Queue)
        .await
        .unwrap();
    assert_eq!(lease.worker_ref().unwrap().generation, 2, "no retry");
    drop(lease);
    pool.shutdown(Duration::from_secs(1)).await;
    let _ = std::fs::remove_dir_all(&dir);
}
