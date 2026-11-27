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
        oom_events: None,
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

/// Two workers that start together age together. Their replacements start
/// one at a time, so the memory budget holds one overlap per pool, and the
/// pool keeps at least `size - 1` live workers throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workers_due_together_are_replaced_one_at_a_time() {
    within(
        TEST,
        "workers_due_together_are_replaced_one_at_a_time",
        async {
            let mut spec = spec("age-two", 2);
            spec.max_age = Duration::from_millis(600);
            spec.age_check = Duration::from_millis(50);
            // A slow start keeps each replacement visible as `starting`.
            spec.init.test.ready_delay_ms = 300;
            let pool = ready(spec).await;
            let first: Vec<u32> = {
                let a = pool.lease(after(5000), Admission::Queue).await.unwrap();
                let b = pool.lease(after(5000), Admission::Queue).await.unwrap();
                vec![a.pid().unwrap(), b.pid().unwrap()]
            };
            let stop = Arc::new(AtomicBool::new(false));
            let most_starting = Arc::new(AtomicUsize::new(0));
            let fewest_live = Arc::new(AtomicUsize::new(usize::MAX));
            let sampler = {
                let pool = pool.clone();
                let (stop, most, fewest) = (
                    Arc::clone(&stop),
                    Arc::clone(&most_starting),
                    Arc::clone(&fewest_live),
                );
                tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let s = pool.status();
                        most.fetch_max(s.starting, Ordering::Relaxed);
                        fewest.fetch_min(s.idle + s.busy, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                })
            };
            for pid in &first {
                assert!(
                    wait_gone(*pid, Duration::from_secs(10)).await,
                    "the aged worker {pid} stayed"
                );
            }
            stop.store(true, Ordering::Relaxed);
            sampler.await.unwrap();
            assert_eq!(
                most_starting.load(Ordering::Relaxed),
                1,
                "the two replacements started together"
            );
            assert!(
                fewest_live.load(Ordering::Relaxed) >= 1,
                "the pool fell below size - 1"
            );
            finish(pool, "age-two").await;
        },
    )
    .await;
}

/// The overlap the memory budget covers: a replacement that is ready while
/// its incumbent is still leased waits beside it. Until that lease returns,
/// no second replacement starts. Then the second swap completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_replacement_holds_back_the_next_one() {
    within(
        TEST,
        "a_waiting_replacement_holds_back_the_next_one",
        async {
            let mut spec = spec("age-held", 2);
            spec.max_age = Duration::from_millis(600);
            spec.age_check = Duration::from_millis(50);
            spec.init.test.ready_delay_ms = 300;
            let pool = ready(spec).await;
            // Both age while idle; the first replacement starts.
            let until = Instant::now() + Duration::from_secs(10);
            while pool.status().starting == 0 {
                assert!(Instant::now() < until, "no replacement started");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            // Hold both incumbents across the replacement's Ready.
            let a = pool.lease(after(10_000), Admission::Queue).await.unwrap();
            let b = pool.lease(after(10_000), Admission::Queue).await.unwrap();
            let (pid_a, pid_b) = (a.pid().unwrap(), b.pid().unwrap());
            let until = Instant::now() + Duration::from_secs(10);
            while pool.status().starting != 0 {
                assert!(Instant::now() < until, "the replacement never got ready");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            // Ready, and waiting beside a leased incumbent: nothing else
            // starts while both leases are held.
            let hold = Instant::now() + Duration::from_millis(500);
            while Instant::now() < hold {
                assert_eq!(pool.status().starting, 0, "a second replacement started");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            assert!(!gone(pid_a) && !gone(pid_b));
            // Returning the incumbents lets each swap complete in turn.
            drop(a);
            drop(b);
            for pid in [pid_a, pid_b] {
                assert!(
                    wait_gone(pid, Duration::from_secs(10)).await,
                    "the incumbent {pid} stayed"
                );
            }
            finish(pool, "age-held").await;
        },
    )
    .await;
}

/// A crash while replacements wait: the worker started for the hole takes
/// the free lease slot, so the pool serves at its size again, rather than
/// waiting beside another leased incumbent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hole_is_filled_while_replacements_wait() {
    within(TEST, "a_hole_is_filled_while_replacements_wait", async {
        let mut spec = spec("age-hole", 3);
        spec.max_age = Duration::from_millis(600);
        spec.age_check = Duration::from_millis(50);
        spec.init.test.ready_delay_ms = 300;
        let pool = ready(spec).await;
        let until = Instant::now() + Duration::from_secs(10);
        while pool.status().starting == 0 {
            assert!(Instant::now() < until, "no replacement started");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // All three age together; hold two, and crash the third.
        let a = pool.lease(after(10_000), Admission::Queue).await.unwrap();
        let b = pool.lease(after(10_000), Admission::Queue).await.unwrap();
        let mut c = pool.lease(after(10_000), Admission::Queue).await.unwrap();
        let _ = c.call(test_op(TestOp::Exit { code: 3 }), after(5000)).await;
        drop(c);
        // The hole's worker must become leasable while a and b stay held.
        let third = pool.lease(after(5000), Admission::Queue).await;
        assert!(third.is_ok(), "the free lease slot stayed empty: {third:?}");
        drop(third);
        drop(a);
        drop(b);
        finish(pool, "age-hole").await;
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

/// `convert_queue_depth{pool}` as the recorder renders it.
fn queue_depth(pool: &str) -> Option<f64> {
    let prefix = format!("convert_queue_depth{{pool=\"{pool}\"}} ");
    prometheus()
        .render()
        .lines()
        .find_map(|line| line.strip_prefix(&prefix)?.parse().ok())
}

/// Many waiters give up at the same instant on several threads. Their
/// updates of the queue gauge must not publish a stale depth: once every
/// waiter has left, the gauge reads 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_queue_gauge_reads_zero_after_concurrent_waiters_leave() {
    prometheus();
    within(
        TEST,
        "the_queue_gauge_reads_zero_after_concurrent_waiters_leave",
        async {
            let mut spec = spec("queue-gauge", 1);
            spec.name = "queue-gauge";
            spec.queue_capacity = 1024;
            let pool = ready(spec).await;
            let busy = pool.lease(after(30_000), Admission::Queue).await.unwrap();
            for round in 0..8 {
                let until = after(200);
                let waiters: Vec<_> = (0..256)
                    .map(|_| {
                        let pool = pool.clone();
                        tokio::spawn(async move { pool.lease(until, Admission::Queue).await })
                    })
                    .collect();
                for waiter in waiters {
                    let err = waiter.await.unwrap().unwrap_err();
                    assert!(matches!(err, LeaseError::DeadlineExpired), "{err:?}");
                }
                assert_eq!(queue_depth("queue-gauge"), Some(0.0), "round {round}");
            }
            drop(busy);
            finish(pool, "queue-gauge").await;
        },
    )
    .await;
}

/// The race behind the stale gauge, made certain. Two waiters queue. The
/// first leaves on a thread of its own and stops in the hook, between its
/// count and its gauge update. Only then does the second leave, all the
/// way. The gauge must still read 0 once the first finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_queue_gauge_survives_a_leave_that_is_overtaken() {
    prometheus();
    within(
        TEST,
        "the_queue_gauge_survives_a_leave_that_is_overtaken",
        async {
            let mut spec = spec("queue-overtaken", 1);
            spec.name = "queue-overtaken";
            let pool = ready(spec).await;
            let busy = pool.lease(after(30_000), Admission::Queue).await.unwrap();
            // The hook stops only the marked thread, and says when it does.
            let marked = Arc::new(std::sync::OnceLock::new());
            let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let hook_state = Mutex::new((entered_tx, released));
            let hook_marked = Arc::clone(&marked);
            let _hook = convertd::engines::pool::set_queue_leave_hook("queue-overtaken", move || {
                if hook_marked.get() == Some(&std::thread::current().id()) {
                    let state = hook_state.lock().unwrap();
                    state.0.send(()).unwrap();
                    let _ = state.1.recv_timeout(Duration::from_secs(10));
                }
            });
            // The first waiter, on a runtime of its own, leaves when told:
            // dropping its lease future runs the guard on that thread.
            let (leave_first, first_leaves) = tokio::sync::oneshot::channel::<()>();
            let first = {
                let pool = pool.clone();
                let marked = Arc::clone(&marked);
                std::thread::spawn(move || {
                    marked.set(std::thread::current().id()).unwrap();
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async {
                            tokio::select! {
                                lease = pool.lease(after(30_000), Admission::Queue) => lease.is_ok(),
                                _ = first_leaves => false,
                            }
                        })
                })
            };
            let (leave_second, second_leaves) = tokio::sync::oneshot::channel::<()>();
            let second = {
                let pool = pool.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        lease = pool.lease(after(30_000), Admission::Queue) => lease.is_ok(),
                        _ = second_leaves => false,
                    }
                })
            };
            let until = Instant::now() + Duration::from_secs(10);
            while pool.status().waiting < 2 {
                assert!(Instant::now() < until, "the waiters did not queue");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(queue_depth("queue-overtaken"), Some(2.0));
            leave_first.send(()).unwrap();
            tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(10)))
                .await
                .unwrap()
                .expect("the first waiter reached the hook");
            leave_second.send(()).unwrap();
            assert!(!second.await.unwrap(), "the second waiter left");
            release.send(()).unwrap();
            assert!(!first.join().unwrap(), "the first waiter left");
            assert_eq!(queue_depth("queue-overtaken"), Some(0.0));
            drop(busy);
            finish(pool, "queue-overtaken").await;
        },
    )
    .await;
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

/// SIGKILL `pid` from outside, as the kernel's OOM killer does.
fn sigkill(pid: u32) {
    let pid = Pid::from_raw(pid as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
}

/// A worker that dies by SIGKILL after its RSS reached its recycle limit
/// died of memory: the call is `TooBig` (422), not a crash to retry. A
/// SIGKILL at a low RSS stays a crash.
///
/// Only the outside SIGKILL can kill the worker here: the kill limit is off,
/// and both calls run on one lease, so no recycle runs between them. The
/// worker's own RSS only has to pass `max_rss` (128 MiB) after allocating
/// 200 MiB, and stay under it before: true on any runner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sigkill_near_the_memory_limit_is_too_big_not_a_crash() {
    within(
        TEST,
        "a_sigkill_near_the_memory_limit_is_too_big_not_a_crash",
        async {
            let mut spec = spec("oom-kill", 1);
            spec.kill_rss = u64::MAX;
            spec.max_rss = 128 << 20;
            let pool = ready(spec).await;
            let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
            let pid = lease.pid().unwrap();
            let reply = lease
                .call(test_op(TestOp::Allocate { mb: 200 }), after(5000))
                .await
                .unwrap();
            assert!(reply.rss_bytes >= 128 << 20, "rss {}", reply.rss_bytes);
            // The same lease: no recycle before the kill.
            let killer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                sigkill(pid);
            });
            let err = lease
                .call(test_op(TestOp::Sleep { ms: 3000 }), after(5000))
                .await
                .unwrap_err();
            killer.await.unwrap();
            assert!(matches!(err, CallError::TooBig { .. }), "{err:?}");
            drop(lease);

            // A fresh worker, under max_rss: the same SIGKILL is a crash.
            let mut lease = pool.lease(after(10_000), Admission::Queue).await.unwrap();
            assert_ne!(lease.pid().unwrap(), pid);
            let pid = lease.pid().unwrap();
            let killer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                sigkill(pid);
            });
            let err = lease
                .call(test_op(TestOp::Sleep { ms: 3000 }), after(5000))
                .await
                .unwrap_err();
            killer.await.unwrap();
            assert!(matches!(err, CallError::Crashed { .. }), "{err:?}");
            drop(lease);
            finish(pool, "oom-kill").await;
        },
    )
    .await;
}

/// A SIGKILL during a call in which the cgroup's `oom_kill` count rose is
/// the kernel's OOM killer, whatever the worker's own RSS: `TooBig`. The
/// same SIGKILL with no rise, at a low RSS, stays a crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sigkill_with_a_cgroup_oom_kill_is_too_big() {
    within(TEST, "a_sigkill_with_a_cgroup_oom_kill_is_too_big", async {
        let dir = base_dir("oom-events");
        std::fs::create_dir_all(&dir).unwrap();
        let events = dir.join("memory.events");
        let path = events.clone();
        let write = move |kills: u32| {
            std::fs::write(
                &path,
                format!("low 0\nhigh 0\nmax 7\noom 2\noom_kill {kills}\noom_group_kill 0\n"),
            )
            .unwrap()
        };
        write(3);
        let mut spec = spec("oom-events", 1);
        spec.oom_events = Some(events.clone());
        let pool = ready(spec).await;

        let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
        let pid = lease.pid().unwrap();
        let killer = {
            let write = write.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                write(4);
                sigkill(pid);
            })
        };
        let err = lease
            .call(test_op(TestOp::Sleep { ms: 3000 }), after(5000))
            .await
            .unwrap_err();
        killer.await.unwrap();
        assert!(
            matches!(err, CallError::TooBig { .. }),
            "the count rose: {err:?}"
        );
        drop(lease);

        let mut lease = pool.lease(after(10_000), Admission::Queue).await.unwrap();
        let pid = lease.pid().unwrap();
        let killer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            sigkill(pid);
        });
        let err = lease
            .call(test_op(TestOp::Sleep { ms: 3000 }), after(5000))
            .await
            .unwrap_err();
        killer.await.unwrap();
        assert!(matches!(err, CallError::Crashed { .. }), "no rise: {err:?}");
        drop(lease);
        finish(pool, "oom-events").await;
    })
    .await;
}

type Held = Arc<Mutex<Option<convertd::engines::pool::Lease>>>;

/// A pool of one named `name` whose worker has asked for a replacement, and
/// its incumbent (generation 1) leased into `held`. Then `setup` is sent:
/// the caller's hook holds the replacement's placement until it arrives.
async fn incumbent_leased(
    name: &'static str,
    held: &Held,
    recycle_after: bool,
    setup: std::sync::mpsc::Sender<()>,
) -> Pool {
    let mut spec = spec(name, 1);
    spec.name = name;
    spec.max_jobs = 2;
    spec.init.test.ready_delay_ms = 100;
    let pool = ready(spec).await;
    for _ in 0..2 {
        let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
        lease
            .call(test_op(TestOp::Sleep { ms: 0 }), after(5000))
            .await
            .unwrap();
    }
    let mut incumbent = pool.lease(after(5000), Admission::Queue).await.unwrap();
    assert_eq!(incumbent.worker_ref().unwrap().generation, 1);
    if recycle_after {
        incumbent.recycle_after();
    }
    *held.lock().unwrap() = Some(incumbent);
    setup.send(()).unwrap();
    pool
}

/// A place_ready hook gate: the pool's first placement (its first worker)
/// passes, and the second (the replacement) waits up to 10 s for `setup`.
/// True when this call is the replacement's placement.
fn hold_replacement(
    placements: &AtomicUsize,
    setup: &Mutex<std::sync::mpsc::Receiver<()>>,
) -> bool {
    if placements.fetch_add(1, Ordering::AcqRel) != 1 {
        return false;
    }
    setup
        .lock()
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .expect("the incumbent was never stored");
    true
}

/// The next lease must get the replacement (generation 2) within 3 s.
async fn replacement_is_leasable(pool: &Pool) {
    let lease = pool
        .lease(after(3000), Admission::Queue)
        .await
        .expect("the replacement was stranded: no worker is leasable");
    assert_eq!(lease.worker_ref().unwrap().generation, 2);
}

/// The handoff race: the incumbent's lease returns while the manager
/// retires it and places its replacement. A thread returns the lease at the
/// moment of the retire; the manager waits up to 300 ms for it. A handoff
/// that is atomic under the idle lock makes the return wait for the lock,
/// see the replacement in `waiting` and release it. A handoff that retires
/// outside the lock lets the return see a retired worker with nobody
/// waiting, which strands the replacement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_that_returns_during_the_handoff_does_not_strand_the_replacement() {
    within(
        TEST,
        "a_lease_that_returns_during_the_handoff_does_not_strand_the_replacement",
        async {
            let held: Held = Arc::new(Mutex::new(None));
            let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
            let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            let returner = {
                let held = Arc::clone(&held);
                std::thread::spawn(move || {
                    if go_rx.recv_timeout(Duration::from_secs(30)).is_ok() {
                        drop(held.lock().unwrap().take());
                        let _ = ack_tx.send(());
                        let _ = done_tx.send(());
                    }
                })
            };
            let fired = Arc::new(AtomicBool::new(false));
            let (verdict_tx, verdict_rx) = std::sync::mpsc::channel::<bool>();
            let verdict_tx = Mutex::new(verdict_tx);
            let placements = Arc::new(AtomicUsize::new(0));
            let (setup_tx, setup_rx) = std::sync::mpsc::channel::<()>();
            let (go_tx, ack_rx, setup_rx) =
                (Mutex::new(go_tx), Mutex::new(ack_rx), Mutex::new(setup_rx));
            let hook_fired = Arc::clone(&fired);
            let hook_placements = Arc::clone(&placements);
            let (locked_tx, locked_rx) = std::sync::mpsc::channel::<bool>();
            let locked_tx = Mutex::new(locked_tx);
            let _hook = convertd::engines::pool::set_place_ready_hook(
                "race-return",
                move |stage, locked| {
                    match stage {
                        convertd::engines::pool::PlaceStage::BeforeLock => {
                            hold_replacement(&hook_placements, &setup_rx);
                        }
                        convertd::engines::pool::PlaceStage::Retired => {
                            if !hook_fired.swap(true, Ordering::AcqRel) {
                                // The retire must happen under the idle lock.
                                let _ = locked_tx.lock().unwrap().send(locked);
                                let _ = go_tx.lock().unwrap().send(());
                                // Under the idle lock the return must block: an
                                // acknowledgement now means it saw the retire
                                // before the replacement was placed.
                                let acked = ack_rx
                                    .lock()
                                    .unwrap()
                                    .recv_timeout(Duration::from_millis(300));
                                let _ = verdict_tx.lock().unwrap().send(acked.is_ok());
                            }
                        }
                    }
                },
            );
            let pool = incumbent_leased("race-return", &held, false, setup_tx).await;
            let until = Instant::now() + Duration::from_secs(10);
            while done_rx.try_recv().is_err() {
                assert!(Instant::now() < until, "the lease never returned");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(fired.load(Ordering::Acquire), "the hook did not run");
            let early = verdict_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the hook gave no verdict");
            assert!(
                !early,
                "the lease returned between the retire and the placement"
            );
            let locked = locked_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the hook gave no lock state");
            assert!(locked, "the incumbent was retired outside the idle lock");
            returner.join().unwrap();
            replacement_is_leasable(&pool).await;
            finish(pool, "race-return").await;
        },
    )
    .await;
}

/// The incumbent's lease is discarded (a failed or cancelled call) just
/// before the manager places its replacement, while it is still only being
/// replaced: nobody waits to be released, and its exit reaches the manager
/// after the placement. The replacement must then take a new permit, not
/// wait for an incumbent that is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_discarded_incumbent_does_not_strand_its_replacement() {
    within(
        TEST,
        "a_discarded_incumbent_does_not_strand_its_replacement",
        async {
            let held: Held = Arc::new(Mutex::new(None));
            let fired = Arc::new(AtomicBool::new(false));
            let placements = Arc::new(AtomicUsize::new(0));
            let (setup_tx, setup_rx) = std::sync::mpsc::channel::<()>();
            let setup_rx = Mutex::new(setup_rx);
            let (hook_held, hook_fired) = (Arc::clone(&held), Arc::clone(&fired));
            let hook_placements = Arc::clone(&placements);
            let _hook = convertd::engines::pool::set_place_ready_hook(
                "race-discard",
                move |stage, _locked| {
                    if stage == convertd::engines::pool::PlaceStage::BeforeLock
                        && hold_replacement(&hook_placements, &setup_rx)
                    {
                        // recycle_after: the lease ends through discard.
                        drop(hook_held.lock().unwrap().take());
                        hook_fired.store(true, Ordering::Release);
                    }
                },
            );
            let pool = incumbent_leased("race-discard", &held, true, setup_tx).await;
            let until = Instant::now() + Duration::from_secs(10);
            while !fired.load(Ordering::Acquire) {
                assert!(Instant::now() < until, "the hook did not run");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            replacement_is_leasable(&pool).await;
            finish(pool, "race-discard").await;
        },
    )
    .await;
}

/// Item 010: the reply carries the worker's CPU time for the call. The
/// worker spins for 200 ms of CPU; the reply must show at least 150 ms,
/// and no more than the call's wall time, since the test worker has one
/// thread (this catches a unit slip). A 200 ms sleep on the same worker
/// then reports little CPU: the field is CPU time, not wall time, and the
/// change over one call, not the process total.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cpu_burn_reports_its_cpu_time() {
    within(TEST, "a_cpu_burn_reports_its_cpu_time", async {
        let pool = ready(spec("burn", 1)).await;
        let mut lease = pool.lease(after(5000), Admission::Queue).await.unwrap();
        let started = Instant::now();
        let reply = lease
            .call(test_op(TestOp::Burn { ms: 200 }), after(10_000))
            .await
            .unwrap();
        let wall = started.elapsed();
        assert!(reply.result.is_ok(), "{:?}", reply.result);
        let cpu = Duration::from_micros(reply.engine.cpu_us);
        eprintln!("burn: cpu {cpu:?}, wall {wall:?}");
        assert!(cpu >= Duration::from_millis(150), "cpu {cpu:?}");
        assert!(
            cpu <= wall + Duration::from_millis(10),
            "cpu {cpu:?} above wall {wall:?}"
        );
        let slept = lease
            .call(test_op(TestOp::Sleep { ms: 200 }), after(10_000))
            .await
            .unwrap();
        let idle = Duration::from_micros(slept.engine.cpu_us);
        eprintln!("sleep: cpu {idle:?}");
        assert!(idle < Duration::from_millis(50), "sleep cpu {idle:?}");
        drop(lease);
        finish(pool, "burn").await;
    })
    .await;
}
