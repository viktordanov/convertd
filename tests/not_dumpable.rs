//! S1: the supervisor is not dumpable, so a worker with its UID cannot read
//! `/proc/<supervisor>/environ`, which holds `API_KEY`. A file of its own:
//! the flag is set on this whole test process, which plays the supervisor.
#![cfg(all(target_os = "linux", feature = "test-hooks"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use convertd::engines::config::Paper;
use convertd::engines::pool::{Admission, Pool, PoolSpec};
use convertd::engines::process::{ReaperMode, Spawner, set_not_dumpable};
use convertd::engines::slot::WorkerEnv;
use convertd::ipc::*;
use convertd::worker::Role;

fn spec(dir: &std::path::Path) -> PoolSpec {
    PoolSpec {
        name: "not-dumpable",
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
        max_jobs: 1_000_000,
        max_rss: u64::MAX,
        kill_rss: u64::MAX,
        max_age: Duration::from_secs(3600),
        age_check: Duration::from_secs(30),
        queue_capacity: 16,
        queue_wait: Duration::from_secs(10),
        start_timeout: Duration::from_secs(10),
        oom_events: None,
    }
}

/// Whether this process may trace any process (`CAP_SYS_PTRACE` in
/// `CapEff`): then the flag does not stop it, and the test proves nothing.
fn can_ptrace_any() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << 19) != 0)
}

/// What a worker gets when it reads `/proc/<pid>/environ`.
async fn worker_reads_environ(pool: &Pool, pid: u32) -> TestOutput {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut lease = pool.lease(deadline, Admission::Queue).await.unwrap();
    let reply = lease
        .call(Op::Test(TestOp::ReadEnviron { pid }), deadline)
        .await
        .unwrap();
    match reply.result {
        Ok(Output::Test(out)) => out,
        other => panic!("unexpected reply: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_cannot_read_the_environment_of_a_not_dumpable_supervisor() {
    if can_ptrace_any() {
        eprintln!("skipped: this process has CAP_SYS_PTRACE");
        return;
    }
    let dir = std::env::temp_dir().join(format!("convertd-011-dumpable-{}", std::process::id()));
    let pool = Pool::start(spec(&dir), Spawner::new(ReaperMode::Owned).unwrap());
    assert!(pool.wait_idle(1, Duration::from_secs(20)).await);
    let me = std::process::id();
    // The control: while this process is dumpable, its worker reads it.
    match worker_reads_environ(&pool, me).await {
        TestOutput::Read { bytes: Some(n), .. } => assert!(n > 0),
        other => panic!("the control read failed: {other:?}"),
    }
    set_not_dumpable().unwrap();
    // Denied, or hidden: with procfs mounted `hidepid=2` the entry of a
    // process the worker may not inspect is not found at all.
    match worker_reads_environ(&pool, me).await {
        TestOutput::Read {
            bytes: None,
            error: Some(kind),
        } => assert!(
            kind == "PermissionDenied" || kind == "NotFound",
            "unexpected error {kind}"
        ),
        other => panic!("the worker read the supervisor's environment: {other:?}"),
    }
    // The supervisor still reads the /proc entries that its gauges use
    // (INTERFACES §11) and its worker's RSS (the watchdog). Its own
    // `environ` is now root-owned 0400, which no gauge reads.
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let parsed = convertd::observability::metrics::parse_status(&status);
    assert!(
        parsed.rss_bytes.is_some() && parsed.threads.is_some(),
        "{parsed:?}"
    );
    assert!(std::fs::read_dir("/proc/self/fd").unwrap().count() > 0);
    let lease = pool
        .lease(Instant::now() + Duration::from_secs(10), Admission::Queue)
        .await
        .unwrap();
    let pid = lease.pid().unwrap();
    assert!(
        convertd::worker::sys::rss_of(pid).is_some_and(|rss| rss > 0),
        "the watchdog reads the worker's RSS"
    );
    drop(lease);
    pool.shutdown(Duration::from_secs(1)).await;
    let _ = std::fs::remove_dir_all(dir);
}
