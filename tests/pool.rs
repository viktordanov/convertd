//! The worker pool with the `test` role (plan 002 steps 4 and 5).
#![cfg(feature = "test-hooks")]

use std::collections::HashSet;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use convertd::engines::config::Paper;
use convertd::engines::pool::{Admission, CallError, LeaseError, Pool, PoolSpec};
use convertd::engines::process::{ReaperMode, Spawner};
use convertd::engines::slot::WorkerEnv;
use convertd::ipc::*;
use convertd::worker::Role;
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};

/// The bound of a whole test. Every lease and call also has its own
/// deadline; this catches the waits that do not (task joins, status polls).
const TEST: Duration = Duration::from_secs(60);

/// `fut`, or a test failure naming `what` once `limit` has passed.
async fn within<T>(limit: Duration, what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(limit, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {limit:?}: {what}"),
    }
}

fn base_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("convertd-002-pool-{name}-{}", std::process::id()))
}

fn spec(name: &str, size: u16) -> PoolSpec {
    PoolSpec {
        name: "test",
        role: Role::Test,
        program: PathBuf::from(env!("CARGO_BIN_EXE_convertd")),
        workers_dir: base_dir(name).join("workers"),
        profile_template: None,
        env: WorkerEnv::from_vars(Vec::new(), Paper::A4),
        init: Init {
            protocol: PROTOCOL_VERSION,
            ..Init::default()
        },
        size,
        max_jobs: 1_000_000,
        max_rss: u64::MAX,
        kill_rss: u64::MAX,
        max_age: Duration::from_secs(3600),
        age_check: Duration::from_secs(30),
        queue_capacity: 16,
        queue_wait: Duration::from_secs(10),
        start_timeout: Duration::from_secs(10),
    }
}

fn start(spec: PoolSpec) -> Pool {
    Pool::start(spec, Spawner::new(ReaperMode::Owned).unwrap())
}

async fn ready(spec: PoolSpec) -> Pool {
    let size = spec.size as usize;
    let pool = start(spec);
    assert!(
        pool.wait_idle(size, Duration::from_secs(20)).await,
        "the pool did not get ready: {:?}",
        pool.status()
    );
    pool
}

async fn finish(pool: Pool, name: &str) {
    pool.shutdown(Duration::from_secs(1)).await;
    let _ = std::fs::remove_dir_all(base_dir(name));
}

fn after(ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(ms)
}

fn test_op(op: TestOp) -> Op {
    Op::Test(op)
}

fn gone(pid: u32) -> bool {
    test_kill_process(Pid::from_raw(pid as i32).unwrap()) == Err(Errno::SRCH)
}

async fn wait_gone(pid: u32, within: Duration) -> bool {
    let until = Instant::now() + within;
    while !gone(pid) && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    gone(pid)
}

// Step 4.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hang_times_out_and_the_slot_is_replaced() {
    within(TEST, "a_hang_times_out_and_the_slot_is_replaced", async {
        let pool = ready(spec("hang", 1)).await;
        let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
        let first = lease.worker_ref().unwrap().generation;
        let started = Instant::now();
        let err = lease
            .call(test_op(TestOp::Hang), after(200))
            .await
            .unwrap_err();
        let took = started.elapsed();
        assert_eq!(err, CallError::Timeout);
        assert!(took < Duration::from_millis(300), "took {took:?}");
        drop(lease);
        let mut lease = pool.lease(after(10_000), Admission::Queue).await.unwrap();
        assert_ne!(lease.worker_ref().unwrap().generation, first);
        assert!(lease.call(Op::Ping, after(5000)).await.is_ok());
        drop(lease);
        finish(pool, "hang").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_is_a_crash_with_sigabrt() {
    within(TEST, "an_abort_is_a_crash_with_sigabrt", async {
        let pool = ready(spec("abort", 1)).await;
        let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
        let err = lease
            .call(test_op(TestOp::Abort), after(10_000))
            .await
            .unwrap_err();
        let CallError::Crashed { status } = err else {
            panic!("expected a crash, got {err:?}");
        };
        assert_eq!(status.and_then(|s| s.signal()), Some(libc::SIGABRT));
        drop(lease);
        finish(pool, "abort").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_call_kills_the_worker() {
    within(TEST, "a_dropped_call_kills_the_worker", async {
        let pool = ready(spec("cancel", 1)).await;
        let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
        let first = lease.worker_ref().unwrap().generation;
        let pid = lease.pid().unwrap();
        let call = lease.call(test_op(TestOp::Sleep { ms: 5000 }), after(10_000));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), call)
                .await
                .is_err()
        );
        drop(lease);
        assert!(wait_gone(pid, Duration::from_secs(2)).await, "not killed");
        let mut lease = pool.lease(after(10_000), Admission::Queue).await.unwrap();
        assert_ne!(lease.worker_ref().unwrap().generation, first);
        assert!(lease.call(Op::Ping, after(5000)).await.is_ok());
        drop(lease);
        finish(pool, "cancel").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eight_leases_on_two_workers_complete_in_fifo_order() {
    within(
        TEST,
        "eight_leases_on_two_workers_complete_in_fifo_order",
        async {
            let pool = ready(spec("fifo", 2)).await;
            // Hold both workers, queue eight leases in a known order, then let go.
            // Each holder keeps its worker 10 ms longer than the one before, so no
            // two grants happen at the same instant and the recorded order is the
            // grant order.
            let a = pool.lease(after(5000), Admission::Queue).await.unwrap();
            let b = pool.lease(after(5000), Admission::Queue).await.unwrap();
            let order = Arc::new(Mutex::new(Vec::new()));
            let mut tasks = Vec::new();
            for i in 0..8u64 {
                let (p, order) = (pool.clone(), Arc::clone(&order));
                tasks.push(tokio::spawn(async move {
                    let mut lease = p.lease(after(20_000), Admission::Queue).await.unwrap();
                    order.lock().unwrap().push(i);
                    lease
                        .call(test_op(TestOp::Sleep { ms: 20 + 10 * i }), after(5000))
                        .await
                        .unwrap();
                }));
                while pool.status().waiting < i as usize + 1 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            drop(a);
            while order.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            drop(b);
            for task in tasks {
                task.await.unwrap();
            }
            assert_eq!(*order.lock().unwrap(), (0..8).collect::<Vec<_>>());
            finish(pool, "fifo").await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fatal_at_init_backs_off_and_leases_are_unavailable() {
    within(
        TEST,
        "a_fatal_at_init_backs_off_and_leases_are_unavailable",
        async {
            let mut spec = spec("fatal", 1);
            spec.init.test.fatal = true;
            let pool = start(spec);
            let until = Instant::now() + Duration::from_secs(10);
            while !pool.status().backoff && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let status = pool.status();
            assert!(status.backoff && !status.ready_once, "{status:?}");
            let started = Instant::now();
            let err = pool.lease(after(5000), Admission::Queue).await.unwrap_err();
            assert_eq!(err, LeaseError::Unavailable);
            assert!(started.elapsed() < Duration::from_millis(50));
            finish(pool, "fatal").await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiters_are_released_when_the_pool_becomes_unavailable() {
    within(
        TEST,
        "waiters_are_released_when_the_pool_becomes_unavailable",
        async {
            let mut spec = spec("release", 1);
            // Slow start-up, then a fatal: the waiter is queued before the failure.
            spec.init.test.fatal = true;
            spec.init.test.ready_delay_ms = 300;
            let pool = start(spec);
            let started = Instant::now();
            let err = pool.lease(after(5000), Admission::Queue).await.unwrap_err();
            assert_eq!(err, LeaseError::Unavailable);
            assert!(started.elapsed() < Duration::from_secs(2));
            finish(pool, "release").await;
        },
    )
    .await;
}

// Step 5.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capacity_one_lets_one_wait_and_refuses_the_next() {
    within(
        TEST,
        "capacity_one_lets_one_wait_and_refuses_the_next",
        async {
            let mut spec = spec("capacity", 1);
            spec.queue_capacity = 1;
            let pool = ready(spec).await;
            let busy = pool.lease(after(5000), Admission::Queue).await.unwrap();
            let waiter = {
                let pool = pool.clone();
                tokio::spawn(async move { pool.lease(after(5000), Admission::Queue).await.is_ok() })
            };
            while pool.status().waiting < 1 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let started = Instant::now();
            let err = pool.lease(after(5000), Admission::Queue).await.unwrap_err();
            assert_eq!(err, LeaseError::QueueFull);
            assert!(started.elapsed() < Duration::from_millis(50));
            // `Continue` is not bounded by the capacity: it waits.
            let cont = {
                let pool = pool.clone();
                tokio::spawn(
                    async move { pool.lease(after(5000), Admission::Continue).await.is_ok() },
                )
            };
            drop(busy);
            assert!(waiter.await.unwrap());
            assert!(cont.await.unwrap());
            finish(pool, "capacity").await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_wait_and_the_deadline_bound_a_lease() {
    within(
        TEST,
        "the_queue_wait_and_the_deadline_bound_a_lease",
        async {
            let mut spec = spec("queue-wait", 1);
            spec.queue_wait = Duration::from_millis(100);
            let pool = ready(spec).await;
            let busy = pool.lease(after(5000), Admission::Queue).await.unwrap();

            let started = Instant::now();
            let err = pool.lease(after(5000), Admission::Queue).await.unwrap_err();
            let took = started.elapsed();
            assert_eq!(err, LeaseError::QueueTimeout);
            assert!(took >= Duration::from_millis(100) && took < Duration::from_millis(300));

            // The deadline comes first: DeadlineExpired, not QueueTimeout.
            let err = pool.lease(after(30), Admission::Queue).await.unwrap_err();
            assert_eq!(err, LeaseError::DeadlineExpired);
            // `Continue` ignores the queue wait and runs to the deadline.
            let started = Instant::now();
            let err = pool
                .lease(after(250), Admission::Continue)
                .await
                .unwrap_err();
            assert_eq!(err, LeaseError::DeadlineExpired);
            assert!(started.elapsed() >= Duration::from_millis(250));
            drop(busy);
            finish(pool, "queue-wait").await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_jobs_replaces_first_and_never_empties_a_pool_of_one() {
    within(
        TEST,
        "max_jobs_replaces_first_and_never_empties_a_pool_of_one",
        async {
            let mut spec = spec("replace", 1);
            spec.max_jobs = 3;
            let pool = ready(spec).await;
            let stop = Arc::new(AtomicBool::new(false));
            let lowest = Arc::new(AtomicUsize::new(usize::MAX));
            let sampler = {
                let (pool, stop, lowest) = (pool.clone(), Arc::clone(&stop), Arc::clone(&lowest));
                tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let s = pool.status();
                        lowest.fetch_min(s.idle + s.busy, Ordering::Relaxed);
                        tokio::task::yield_now().await;
                    }
                })
            };
            let mut first_pid = None;
            for _ in 0..3 {
                let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
                assert_eq!(lease.worker_ref().unwrap().generation, 1);
                first_pid.get_or_insert(lease.pid().unwrap());
                lease
                    .call(test_op(TestOp::Sleep { ms: 0 }), after(5000))
                    .await
                    .unwrap();
            }
            let until = Instant::now() + Duration::from_secs(10);
            let mut generation = 1;
            while generation == 1 && Instant::now() < until {
                let lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
                generation = lease.worker_ref().unwrap().generation;
                drop(lease);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(generation, 2);
            assert!(wait_gone(first_pid.unwrap(), Duration::from_secs(2)).await);
            stop.store(true, Ordering::Relaxed);
            sampler.await.unwrap();
            assert!(
                lowest.load(Ordering::Relaxed) >= 1,
                "idle + busy fell to 0 during the swap"
            );
            finish(pool, "replace").await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_jobs_one_never_serves_two_calls_from_one_process() {
    within(
        TEST,
        "max_jobs_one_never_serves_two_calls_from_one_process",
        async {
            let mut spec = spec("one-shot", 2);
            spec.max_jobs = 1;
            let pool = ready(spec).await;
            let mut pids = HashSet::new();
            let mut spawned_on_path = 0;
            for _ in 0..10 {
                let mut lease = pool.lease(after(10_000), Admission::Queue).await.unwrap();
                assert!(pids.insert(lease.pid().unwrap()), "a PID served twice");
                if lease.spawned_on_path().is_some() {
                    spawned_on_path += 1;
                }
                let reply = lease
                    .call(test_op(TestOp::Sleep { ms: 0 }), after(5000))
                    .await
                    .unwrap();
                assert_eq!(reply.job, 1);
            }
            assert_eq!(pids.len(), 10);
            // Back to back, most leases wait for a spawn, which is then reported.
            assert!(spawned_on_path > 0);
            finish(pool, "one-shot").await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rss_above_the_limit_recycles_the_worker() {
    within(TEST, "rss_above_the_limit_recycles_the_worker", async {
        let mut spec = spec("rss", 1);
        spec.max_rss = 32 << 20;
        let pool = ready(spec).await;
        let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
        let first = lease.worker_ref().unwrap().generation;
        let reply = lease
            .call(test_op(TestOp::Allocate { mb: 64 }), after(10_000))
            .await
            .unwrap();
        assert!(reply.rss_bytes >= 64 << 20, "rss {}", reply.rss_bytes);
        drop(lease);
        let lease = pool.lease(after(10_000), Admission::Queue).await.unwrap();
        assert_ne!(lease.worker_ref().unwrap().generation, first);
        drop(lease);
        finish(pool, "rss").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recycle_after_replaces_the_worker() {
    within(TEST, "recycle_after_replaces_the_worker", async {
        let pool = ready(spec("recycle-after", 1)).await;
        let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
        let first = lease.worker_ref().unwrap().generation;
        lease.call(Op::Ping, after(5000)).await.unwrap();
        lease.recycle_after();
        drop(lease);
        let lease = pool.lease(after(10_000), Admission::Queue).await.unwrap();
        assert_ne!(lease.worker_ref().unwrap().generation, first);
        drop(lease);
        finish(pool, "recycle-after").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_workers_are_replaced_when_they_age() {
    within(TEST, "idle_workers_are_replaced_when_they_age", async {
        let logs = captured_logs();
        let mut spec = spec("age", 1);
        spec.name = "age";
        spec.max_age = Duration::from_millis(300);
        spec.age_check = Duration::from_millis(50);
        let pool = ready(spec).await;
        let first = {
            let lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
            lease.pid().unwrap()
        };
        assert!(
            wait_gone(first, Duration::from_secs(5)).await,
            "the aged worker stayed"
        );
        let lease = pool.lease(after(10_000), Admission::Queue).await.unwrap();
        assert_ne!(lease.pid().unwrap(), first);
        drop(lease);
        // The recycle is reported as an age recycle, not a job-count one.
        let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        let reasons: Vec<String> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["message"] == "worker recycled" && v["pool"] == "age")
            .map(|v| v["reason"].as_str().unwrap_or("").to_owned())
            .collect();
        assert!(
            !reasons.is_empty() && reasons.iter().all(|r| r == "age"),
            "{reasons:?}"
        );
        finish(pool, "age").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_leases_cannot_overrun_the_queue_bound() {
    within(
        TEST,
        "concurrent_leases_cannot_overrun_the_queue_bound",
        async {
            let mut spec = spec("bound", 1);
            spec.queue_capacity = 4;
            let pool = ready(spec).await;
            let busy = pool.lease(after(5000), Admission::Queue).await.unwrap();
            let barrier = Arc::new(tokio::sync::Barrier::new(16));
            let tasks: Vec<_> = (0..16)
                .map(|_| {
                    let (pool, barrier) = (pool.clone(), Arc::clone(&barrier));
                    tokio::spawn(async move {
                        barrier.wait().await;
                        pool.lease(after(5000), Admission::Queue).await.map(drop)
                    })
                })
                .collect();
            let until = Instant::now() + Duration::from_secs(5);
            while pool.status().waiting < 4 && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(pool.status().waiting, 4);
            drop(busy);
            let mut full = 0;
            let mut served = 0;
            for task in tasks {
                match task.await.unwrap() {
                    Ok(()) => served += 1,
                    Err(LeaseError::QueueFull) => full += 1,
                    Err(other) => panic!("unexpected {other:?}"),
                }
            }
            assert_eq!((served, full), (4, 12));
            finish(pool, "bound").await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn try_lease_takes_only_an_idle_worker() {
    within(TEST, "try_lease_takes_only_an_idle_worker", async {
        let pool = ready(spec("try", 1)).await;
        let lease = pool.try_lease().expect("an idle worker");
        assert!(pool.try_lease().is_none());
        drop(lease);
        assert!(pool.try_lease().is_some());
        finish(pool, "try").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_refuses_new_leases_and_releases_waiters() {
    within(
        TEST,
        "drain_refuses_new_leases_and_releases_waiters",
        async {
            let pool = ready(spec("drain", 1)).await;
            let busy = pool.lease(after(5000), Admission::Queue).await.unwrap();
            let waiter = {
                let pool = pool.clone();
                tokio::spawn(async move { pool.lease(after(5000), Admission::Continue).await })
            };
            while pool.status().waiting < 1 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            pool.drain();
            assert_eq!(
                waiter.await.unwrap().map(|_| ()).unwrap_err(),
                LeaseError::Unavailable
            );
            assert_eq!(
                pool.lease(after(5000), Admission::Queue)
                    .await
                    .map(|_| ())
                    .unwrap_err(),
                LeaseError::Unavailable
            );
            assert!(pool.status().draining);
            drop(busy);
            finish(pool, "drain").await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kills_every_worker_and_removes_the_slots() {
    within(
        TEST,
        "shutdown_kills_every_worker_and_removes_the_slots",
        async {
            let spec = spec("shutdown", 3);
            let workers_dir = spec.workers_dir.clone();
            let pool = ready(spec).await;
            let mut pids = Vec::new();
            let mut leases = Vec::new();
            for _ in 0..3 {
                let lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
                pids.push(lease.pid().unwrap());
                leases.push(lease);
            }
            // One lease is still out when the grace runs out.
            let held = leases.pop().unwrap();
            drop(leases);
            assert!(workers_dir.is_dir());
            pool.shutdown(Duration::from_millis(200)).await;
            for pid in pids {
                assert!(
                    wait_gone(pid, Duration::from_secs(2)).await,
                    "{pid} survived"
                );
            }
            assert!(!workers_dir.exists(), "the workers directory stayed");
            drop(held);
            let _ = std::fs::remove_dir_all(base_dir("shutdown"));
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_page_count_marker_flows_from_convert_to_inspect() {
    within(
        TEST,
        "the_page_count_marker_flows_from_convert_to_inspect",
        async {
            let dir = base_dir("markers");
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("source.docx");
            std::fs::write(&source, b"PK FAKE:PAGES=4").unwrap();
            let pdf = dir.join("document.pdf");
            let pool = ready(spec("markers", 1)).await;
            let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
            let reply = lease
                .call(
                    Op::Convert(ConvertArgs {
                        input: source,
                        output: pdf.clone(),
                        family: Family::Document,
                        password: None,
                        export: ExportOptions::default(),
                        max_output_bytes: 1 << 20,
                    }),
                    after(5000),
                )
                .await
                .unwrap();
            assert!(matches!(reply.result, Ok(Output::Converted(_))));
            drop(lease);
            let mut lease = pool.lease(after(5000), Admission::Continue).await.unwrap();
            let reply = lease
                .call(
                    Op::Inspect(InspectArgs {
                        pdf,
                        password: None,
                        probe_dpi: None,
                        probe_pages: Default::default(),
                        collect_fonts: false,
                    }),
                    after(5000),
                )
                .await
                .unwrap();
            let Ok(Output::Inspected(inspected)) = reply.result else {
                panic!("expected inspected");
            };
            assert_eq!(inspected.page_count, 4);
            drop(lease);
            finish(pool, "markers").await;
        },
    )
    .await;
}

/// Captures every log line of this test binary as JSON.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    use std::sync::OnceLock;

    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Sink(Arc::clone(&buf));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        buf
    })
    .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_ready_is_logged_with_spawn_and_bind_times() {
    within(
        TEST,
        "worker_ready_is_logged_with_spawn_and_bind_times",
        async {
            let logs = captured_logs();
            let mut spec = spec("ready-log", 1);
            // Other tests of this binary log too; this pool has its own name.
            spec.name = "ready-log";
            spec.init.test.ready_delay_ms = 20;
            let pool = ready(spec).await;
            let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
            let line: serde_json::Value = text
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|v| v["message"] == "worker ready" && v["pool"] == "ready-log")
                .expect("a worker ready line");
            assert_eq!(line["role"], "test");
            let spawn_us = line["spawn_us"].as_u64().unwrap();
            let bind_us = line["bind_us"].as_u64().unwrap();
            assert!(bind_us >= 20_000, "bind_us {bind_us}");
            assert!(spawn_us >= bind_us, "spawn {spawn_us} < bind {bind_us}");
            assert!(line["rss_bytes"].as_u64().is_some());
            finish(pool, "ready-log").await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_a_replacement_start_leaves_nothing_behind() {
    within(
        TEST,
        "shutdown_during_a_replacement_start_leaves_nothing_behind",
        async {
            let mut spec = spec("shutdown-start", 1);
            // The replacement is slow to get ready, so it is mid-start at shutdown.
            spec.init.test.ready_delay_ms = 300;
            let workers_dir = spec.workers_dir.clone();
            let pool = ready(spec).await;
            let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
            let _ = lease.call(test_op(TestOp::Abort), after(5000)).await;
            drop(lease);
            while pool.status().starting == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            pool.shutdown(Duration::from_millis(100)).await;
            assert_eq!(
                pool.spawner().live_children(),
                0,
                "a worker outlived shutdown"
            );
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(!workers_dir.exists(), "a slot came back after shutdown");
            assert_eq!(pool.spawner().live_children(), 0);
            let _ = std::fs::remove_dir_all(base_dir("shutdown-start"));
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_over_the_kill_limit_is_killed_during_its_call() {
    within(
        TEST,
        "a_worker_over_the_kill_limit_is_killed_during_its_call",
        async {
            let mut spec = spec("kill-rss", 1);
            spec.kill_rss = 128 << 20;
            let pool = ready(spec).await;
            let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
            let first = lease.worker_ref().unwrap().generation;
            // The allocation may already trip the watchdog; if it answers first,
            // the next call finds the worker over the limit.
            let started = Instant::now();
            let mut result = lease
                .call(test_op(TestOp::Allocate { mb: 256 }), after(10_000))
                .await;
            if result.is_ok() {
                let sleeping = Instant::now();
                result = lease
                    .call(test_op(TestOp::Sleep { ms: 5000 }), after(10_000))
                    .await;
                let took = sleeping.elapsed();
                assert!(took < Duration::from_millis(500), "killed after {took:?}");
            }
            let Err(CallError::TooBig { rss }) = result else {
                panic!("expected TooBig, got {result:?}");
            };
            assert!(rss >= 128 << 20, "rss {rss}");
            eprintln!(
                "killed at {rss} bytes, {:?} after the first call",
                started.elapsed()
            );
            drop(lease);
            let lease = pool.lease(after(10_000), Admission::Queue).await.unwrap();
            assert_ne!(lease.worker_ref().unwrap().generation, first);
            drop(lease);
            finish(pool, "kill-rss").await;
        },
    )
    .await;
}

/// One Prometheus recorder for this test binary.
fn prometheus() -> &'static metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    HANDLE.get_or_init(|| {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::set_global_recorder(recorder).expect("no other recorder in this binary");
        handle
    })
}

/// `convert_workers{pool="gauges",state}` as the recorder renders it.
fn gauge(state: &str) -> Option<f64> {
    let prefix = format!("convert_workers{{pool=\"gauges\",state=\"{state}\"}} ");
    prometheus()
        .render()
        .lines()
        .find_map(|line| line.strip_prefix(&prefix)?.parse().ok())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_gauges_follow_a_lease_and_its_return() {
    prometheus();
    within(
        TEST,
        "the_worker_gauges_follow_a_lease_and_its_return",
        async {
            let mut spec = spec("gauges", 1);
            spec.name = "gauges";
            let pool = ready(spec).await;
            let lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
            assert_eq!(pool.status().busy, 1);
            assert_eq!((gauge("idle"), gauge("busy")), (Some(0.0), Some(1.0)));
            drop(lease);
            assert_eq!(pool.status().idle, 1);
            assert_eq!((gauge("idle"), gauge("busy")), (Some(1.0), Some(0.0)));
            finish(pool, "gauges").await;
        },
    )
    .await;
}

/// A replacement starts beside its incumbent but is leased only after the
/// incumbent has left, so a pool never has more busy workers than its
/// size and never more than one worker's memory above its kill budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_is_leased_only_after_its_incumbent_leaves() {
    within(
        TEST,
        "a_replacement_is_leased_only_after_its_incumbent_leaves",
        async {
            let mut spec = spec("overlap", 1);
            spec.max_jobs = 2;
            spec.init.test.ready_delay_ms = 300;
            let pool = ready(spec).await;
            for _ in 0..2 {
                let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
                lease
                    .call(test_op(TestOp::Sleep { ms: 0 }), after(5000))
                    .await
                    .unwrap();
            }
            // The second return asked for a replacement; the incumbent
            // still serves while it starts.
            let mut last = pool.lease(after(5000), Admission::Queue).await.unwrap();
            assert_eq!(last.worker_ref().unwrap().generation, 1);
            let long = tokio::spawn(async move {
                last.call(test_op(TestOp::Sleep { ms: 1500 }), after(5000))
                    .await
                    .unwrap();
                last
            });
            let until = Instant::now() + Duration::from_secs(5);
            while pool.status().starting > 0 || pool.status().idle + pool.status().busy < 1 {
                assert!(
                    Instant::now() < until,
                    "no replacement: {:?}",
                    pool.status()
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            let status = pool.status();
            assert!(!long.is_finished(), "the incumbent's job ended too early");
            assert_eq!(status.busy, 1, "{status:?}");
            assert_eq!(
                status.idle, 0,
                "the replacement is not leasable yet: {status:?}"
            );
            assert!(
                pool.try_lease().is_none(),
                "a second worker was leased beside the incumbent"
            );
            drop(long.await.unwrap());
            let lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
            assert_eq!(lease.worker_ref().unwrap().generation, 2);
            drop(lease);
            finish(pool, "overlap").await;
        },
    )
    .await;
}

/// `max_jobs` counts conversions: a `Ping` or an `Inspect` is not a job, so
/// a busy render pool that inspects before each render does not recycle
/// twice as often.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pings_and_inspects_do_not_count_towards_max_jobs() {
    within(
        TEST,
        "pings_and_inspects_do_not_count_towards_max_jobs",
        async {
            let dir = base_dir("not-jobs");
            let pdf = dir.join("source.pdf");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&pdf, b"%PDF-1.4 FAKE:PAGES=1").unwrap();
            let mut spec = spec("not-jobs", 1);
            spec.max_jobs = 2;
            let pool = ready(spec).await;
            let inspect = Op::Inspect(InspectArgs {
                pdf: pdf.clone(),
                password: None,
                probe_dpi: None,
                probe_pages: Default::default(),
                collect_fonts: false,
            });
            for op in [Op::Ping, inspect.clone(), Op::Ping, inspect] {
                let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
                assert_eq!(lease.worker_ref().unwrap().generation, 1);
                let reply = lease.call(op, after(5000)).await.unwrap();
                assert_eq!(reply.job, 0, "not a job");
            }
            for job in 1..=2 {
                let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
                assert_eq!(lease.worker_ref().unwrap().generation, 1);
                let reply = lease
                    .call(test_op(TestOp::Sleep { ms: 0 }), after(5000))
                    .await
                    .unwrap();
                assert_eq!(reply.job, job);
            }
            // The second job reached max_jobs: the next lease gets the replacement.
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                let lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
                if lease.worker_ref().unwrap().generation == 2 {
                    break;
                }
                drop(lease);
                assert!(Instant::now() < until, "no replacement after two jobs");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            finish(pool, "not-jobs").await;
        },
    )
    .await;
}
