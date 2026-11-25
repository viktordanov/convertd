//! One generic worker pool, used for both office and render workers (plan
//! 002 steps 4 and 5).
//!
//! - **Leasing.** Idle workers sit in a list guarded by one mutex, next to
//!   a FIFO-fair semaphore whose permits equal the idle count. `lease`
//!   takes a permit (waiting in FIFO order, bounded by the admission mode
//!   and the deadline) and pops a worker.
//! - **Calling.** `Lease::call` writes one frame and waits for the reply,
//!   the worker's exit or the deadline. On the deadline the process group
//!   is killed and the call is `Timeout`.
//! - **Returning.** Dropping a `Lease` returns the worker, or kills it when
//!   the call was cancelled, it died, or the recycle policy says so.
//!   Discarding a worker is dropping it: `Drop for Worker` kills its group
//!   and tells the manager the slot is free.
//! - **The manager** (one task per pool) owns the slots: it spawns workers
//!   to `Ready` within `start_timeout`, replaces vacated ones with a
//!   back-off after failures, retires workers replaced first, and checks
//!   idle workers for age.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, Semaphore, TryAcquireError, mpsc};

use crate::engines::conn::Conn;
use crate::engines::process::{ChildHandle, Spawner, drain_stderr};
use crate::engines::slot::{SlotDir, WorkerEnv};
use crate::ipc::{self, Call, FrameError, Op, PROTOCOL_VERSION, Reply, Request, Response};
use crate::worker::{Role, micros};

/// The first back-off after a failed start; it doubles up to [`BACKOFF_MAX`].
pub const BACKOFF_START: Duration = Duration::from_millis(500);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How long a crashed worker's exit status is waited for.
pub const CRASH_STATUS_WAIT: Duration = Duration::from_millis(100);

/// Everything a pool needs.
#[derive(Debug, Clone)]
pub struct PoolSpec {
    /// `office` or `render`: the label in logs, metrics and slot names.
    pub name: &'static str,
    pub role: Role,
    /// The `convertd` binary.
    pub program: PathBuf,
    /// `$TMP_DIR/workers`.
    pub workers_dir: PathBuf,
    pub profile_template: Option<PathBuf>,
    pub env: WorkerEnv,
    pub init: ipc::Init,
    pub size: u16,
    pub max_jobs: u32,
    pub max_rss: u64,
    /// Kill during a job above this RSS (plan step 10, not yet enforced).
    pub kill_rss: u64,
    pub max_age: Duration,
    /// How often idle workers are checked for `max_age`.
    pub age_check: Duration,
    /// `Queue`-mode waiters allowed before `QueueFull`.
    pub queue_capacity: usize,
    /// The longest `Queue`-mode wait.
    pub queue_wait: Duration,
    pub start_timeout: Duration,
}

/// How a lease waits for a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The first engine call of a request: bounded by the queue capacity
    /// and `queue_wait`, so it may give 429.
    Queue,
    /// A later call of a request that was already admitted: waits up to
    /// the deadline and never counts against the capacity.
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    #[error("failed to lease a worker: the queue is full")]
    QueueFull,
    #[error("failed to lease a worker: the queue wait ran out")]
    QueueTimeout,
    #[error("failed to lease a worker: the deadline passed")]
    DeadlineExpired,
    #[error("failed to lease a worker: the pool is unavailable")]
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    #[error("failed to finish the call: the deadline passed")]
    Timeout,
    #[error("failed to finish the call: the worker died ({status:?})")]
    Crashed { status: Option<ExitStatus> },
    #[error("failed to finish the call: {0}")]
    Protocol(String),
}

/// Why a worker left. The labels of `convert_worker_restarts_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Jobs,
    Rss,
    Age,
    Timeout,
    Crash,
    Cancelled,
    Error,
    StartFailed,
    /// Not a restart: the pool is shutting down.
    Shutdown,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Jobs => "jobs",
            Reason::Rss => "rss",
            Reason::Age => "age",
            Reason::Timeout => "timeout",
            Reason::Crash => "crash",
            Reason::Cancelled => "cancelled",
            Reason::Error => "error",
            Reason::StartFailed => "start_failed",
            Reason::Shutdown => "shutdown",
        }
    }
}

/// What to do with a worker when its lease ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recycle {
    Keep,
    /// Spawn the replacement first; retire this one once it is `Ready`.
    ReplaceFirst(Reason),
    /// Kill now, then replace.
    KillNow(Reason),
}

/// The recycle policy, from the numbers the worker reported. A job limit
/// of 1 is kill-first, so a worker never serves two calls.
pub fn recycle_decision(
    jobs: u32,
    age: Duration,
    rss: u64,
    recycle_after: bool,
    max_jobs: u32,
    max_age: Duration,
    max_rss: u64,
) -> Recycle {
    if recycle_after {
        Recycle::KillNow(Reason::Error)
    } else if rss >= max_rss {
        Recycle::KillNow(Reason::Rss)
    } else if max_jobs <= 1 && jobs >= 1 {
        Recycle::KillNow(Reason::Jobs)
    } else if jobs >= max_jobs {
        Recycle::ReplaceFirst(Reason::Jobs)
    } else if age >= max_age {
        Recycle::ReplaceFirst(Reason::Age)
    } else {
        Recycle::Keep
    }
}

/// A snapshot of the pool for readiness and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoolStatus {
    pub target: u16,
    /// `Ready` workers not leased.
    pub idle: usize,
    /// Leased workers.
    pub busy: usize,
    /// Spawned, not yet `Ready`.
    pub starting: usize,
    /// Leases waiting for a worker.
    pub waiting: usize,
    /// Some worker of this pool has been `Ready`.
    pub ready_once: bool,
    /// The last start failed and no start has succeeded since.
    pub backoff: bool,
    pub draining: bool,
}

/// Which worker served a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerRef {
    pub pool: &'static str,
    pub slot: u16,
    pub generation: u32,
    /// Calls this process has served, the current one included.
    pub job: u32,
}

const FATE_NORMAL: u8 = 0;
const FATE_REPLACING: u8 = 1;
const FATE_RETIRE: u8 = 2;

enum Event {
    Started {
        slot: u16,
        generation: u32,
        result: Result<Box<Worker>, String>,
    },
    Vacated {
        slot: u16,
        generation: u32,
        reason: Reason,
    },
    Exited {
        slot: u16,
        generation: u32,
    },
    Replace,
    Shutdown,
}

type Handles = Arc<Mutex<HashMap<(u16, u32), ChildHandle>>>;

/// The supervisor's view of one worker process.
struct Worker {
    pool: &'static str,
    slot: u16,
    generation: u32,
    child: ChildHandle,
    conn: Conn,
    ready_at: Instant,
    spawn: Duration,
    bind: Duration,
    jobs: u32,
    rss: u64,
    next_id: u64,
    fate: Arc<AtomicU8>,
    /// Why it is being discarded, for the manager and the log.
    reason: Reason,
    events: mpsc::UnboundedSender<Event>,
    handles: Handles,
}

impl Worker {
    fn id(&self) -> String {
        format!("{}-{}", self.pool, self.slot)
    }

    fn retiring(&self) -> bool {
        self.fate.load(Ordering::Acquire) == FATE_RETIRE
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Rule 9: no teardown, only SIGKILL of the group.
        self.child.kill_group();
        lock(&self.handles).remove(&(self.slot, self.generation));
        match self.reason {
            Reason::Jobs | Reason::Rss | Reason::Age | Reason::Error => {
                tracing::info!(
                    pool = self.pool,
                    worker_id = %self.id(),
                    generation = self.generation,
                    reason = self.reason.as_str(),
                    jobs = self.jobs,
                    rss_bytes = self.rss,
                    "worker recycled"
                );
            }
            Reason::Timeout | Reason::Cancelled => {
                tracing::warn!(
                    pool = self.pool,
                    worker_id = %self.id(),
                    generation = self.generation,
                    reason = self.reason.as_str(),
                    "worker killed"
                );
            }
            Reason::Crash | Reason::StartFailed | Reason::Shutdown => {}
        }
        let _ = self.events.send(Event::Vacated {
            slot: self.slot,
            generation: self.generation,
            reason: self.reason,
        });
    }
}

#[derive(Default)]
struct Idle {
    workers: Vec<Worker>,
    busy: usize,
}

struct PoolInner {
    spec: Arc<PoolSpec>,
    idle: Mutex<Idle>,
    permits: Semaphore,
    waiting: AtomicUsize,
    starting: AtomicUsize,
    /// `Ready` workers alive (idle or busy), as the manager counts them.
    live: AtomicUsize,
    ready_once: AtomicBool,
    backoff: AtomicBool,
    draining: AtomicBool,
    /// Wakes waiters when the pool becomes unavailable.
    unavailable: Notify,
    events: mpsc::UnboundedSender<Event>,
    handles: Handles,
    /// Highest slot number ever used, for clean-up.
    max_slot: AtomicU32,
}

/// A pool of workers of one role. Cheap to clone.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<PoolInner>,
    spawner: Spawner,
}

/// One leased worker. Dropping it returns the worker to the pool (or kills it).
pub struct Lease {
    pool: Arc<PoolInner>,
    worker: Option<Worker>,
    queued: Duration,
    leased: Duration,
    spawned_on_path: Option<(Duration, Duration)>,
    in_flight: bool,
    broken: Option<Reason>,
    recycle_after: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Pool {
    /// Starts the pool: removes this pool's stale slot directories, then
    /// spawns every slot in the background. Returns at once. Must run
    /// inside a tokio runtime.
    pub fn start(spec: PoolSpec, spawner: Spawner) -> Pool {
        remove_stale_slots(&spec);
        let (events, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(PoolInner {
            spec: Arc::new(spec),
            idle: Mutex::new(Idle::default()),
            permits: Semaphore::new(0),
            waiting: AtomicUsize::new(0),
            starting: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            ready_once: AtomicBool::new(false),
            backoff: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            unavailable: Notify::new(),
            events,
            handles: Arc::default(),
            max_slot: AtomicU32::new(0),
        });
        let manager = Manager {
            pool: Arc::downgrade(&inner),
            spec: Arc::clone(&inner.spec),
            spawner: spawner.clone(),
            members: BTreeMap::new(),
            next_generation: 1,
            failures: 0,
            backoff_until: None,
        };
        tokio::spawn(manager.run(rx));
        Pool { inner, spawner }
    }

    pub fn name(&self) -> &'static str {
        self.inner.spec.name
    }

    /// Leases a worker. Under `Queue`, fails at once with `QueueFull` when
    /// `queue_capacity` leases already wait, and waits at most
    /// `queue_wait`; under `Continue`, waits until `deadline`. While the
    /// pool has no live worker and its last start failed, or after
    /// `drain`, it is `Unavailable` at once.
    pub async fn lease(&self, deadline: Instant, adm: Admission) -> Result<Lease, LeaseError> {
        let inner = &self.inner;
        let asked = Instant::now();
        let mut queued = Duration::ZERO;
        loop {
            if inner.draining.load(Ordering::Acquire) {
                return Err(LeaseError::Unavailable);
            }
            let permit = match inner.permits.try_acquire() {
                Ok(permit) => permit,
                Err(TryAcquireError::Closed) => return Err(LeaseError::Unavailable),
                Err(TryAcquireError::NoPermits) => {
                    let waited = Instant::now();
                    let permit = self.wait_for_permit(deadline, adm).await?;
                    queued += waited.elapsed();
                    permit
                }
            };
            permit.forget();
            let worker = {
                let mut idle = lock(&inner.idle);
                let worker = idle.workers.pop();
                if worker.is_some() {
                    idle.busy += 1;
                }
                worker
            };
            let Some(worker) = worker else {
                // A permit without a worker cannot happen; try again.
                continue;
            };
            if worker.child.has_exited() || worker.retiring() {
                let reason = if worker.retiring() {
                    Reason::Jobs
                } else {
                    Reason::Crash
                };
                inner.discard(worker, reason);
                continue;
            }
            let spawned_on_path = (worker.jobs == 0 && worker.ready_at > asked)
                .then_some((worker.spawn, worker.bind));
            return Ok(Lease {
                pool: Arc::clone(inner),
                worker: Some(worker),
                queued,
                leased: asked.elapsed(),
                spawned_on_path,
                in_flight: false,
                broken: None,
                recycle_after: false,
            });
        }
    }

    async fn wait_for_permit(
        &self,
        deadline: Instant,
        adm: Admission,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, LeaseError> {
        let inner = &self.inner;
        if inner.unavailable_now() {
            return Err(LeaseError::Unavailable);
        }
        let queue_limit = Instant::now() + inner.spec.queue_wait;
        let limit = match adm {
            Admission::Queue => {
                if inner.waiting.load(Ordering::Acquire) >= inner.spec.queue_capacity {
                    return Err(LeaseError::QueueFull);
                }
                deadline.min(queue_limit)
            }
            Admission::Continue => deadline,
        };
        let _waiting = WaitingGuard::new(inner);
        let limit_at = tokio::time::Instant::from_std(limit);
        loop {
            let unavailable = inner.unavailable.notified();
            tokio::select! {
                permit = inner.permits.acquire() => {
                    return permit.map_err(|_| LeaseError::Unavailable);
                }
                () = unavailable => {
                    if inner.unavailable_now() || inner.draining.load(Ordering::Acquire) {
                        return Err(LeaseError::Unavailable);
                    }
                }
                () = tokio::time::sleep_until(limit_at) => {
                    return Err(if adm == Admission::Queue && queue_limit < deadline {
                        LeaseError::QueueTimeout
                    } else {
                        LeaseError::DeadlineExpired
                    });
                }
            }
        }
    }

    /// An idle worker, or nothing; never waits.
    pub fn try_lease(&self) -> Option<Lease> {
        let inner = &self.inner;
        loop {
            if inner.draining.load(Ordering::Acquire) {
                return None;
            }
            inner.permits.try_acquire().ok()?.forget();
            let worker = {
                let mut idle = lock(&inner.idle);
                let worker = idle.workers.pop();
                if worker.is_some() {
                    idle.busy += 1;
                }
                worker
            }?;
            if worker.child.has_exited() || worker.retiring() {
                let reason = if worker.retiring() {
                    Reason::Jobs
                } else {
                    Reason::Crash
                };
                inner.discard(worker, reason);
                continue;
            }
            return Some(Lease {
                pool: Arc::clone(inner),
                worker: Some(worker),
                queued: Duration::ZERO,
                leased: Duration::ZERO,
                spawned_on_path: None,
                in_flight: false,
                broken: None,
                recycle_after: false,
            });
        }
    }

    pub fn status(&self) -> PoolStatus {
        let inner = &self.inner;
        let (idle, busy) = {
            let idle = lock(&inner.idle);
            (idle.workers.len(), idle.busy)
        };
        PoolStatus {
            target: inner.spec.size,
            idle,
            busy,
            starting: inner.starting.load(Ordering::Acquire),
            waiting: inner.waiting.load(Ordering::Acquire),
            ready_once: inner.ready_once.load(Ordering::Acquire),
            backoff: inner.backoff.load(Ordering::Acquire),
            draining: inner.draining.load(Ordering::Acquire),
        }
    }

    /// Waits until at least `n` workers are idle, or `timeout` passes.
    pub async fn wait_idle(&self, n: usize, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        loop {
            if self.status().idle >= n {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// New leases are `Unavailable`; waiters are released.
    pub fn drain(&self) {
        let inner = &self.inner;
        inner.draining.store(true, Ordering::Release);
        inner.permits.close();
        inner.unavailable.notify_waiters();
    }

    /// Drains, waits up to `grace` for leases to end, kills every worker of
    /// this pool and removes its slot directories (and `workers_dir` once
    /// it is empty).
    pub async fn shutdown(&self, grace: Duration) {
        self.drain();
        let inner = &self.inner;
        let until = Instant::now() + grace;
        while lock(&inner.idle).busy > 0 && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let idle: Vec<Worker> = std::mem::take(&mut lock(&inner.idle).workers);
        for mut worker in idle {
            worker.reason = Reason::Shutdown;
            drop(worker);
        }
        let _ = inner.events.send(Event::Shutdown);
        let handles: Vec<ChildHandle> = lock(&inner.handles).values().cloned().collect();
        for child in &handles {
            child.kill_group();
        }
        for child in &handles {
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
        let spec = Arc::clone(&inner.spec);
        let max_slot = inner.max_slot.load(Ordering::Acquire);
        let _ = tokio::task::spawn_blocking(move || {
            for slot in 0..=max_slot {
                let _ = SlotDir::new(&spec.workers_dir, spec.name, slot as u16).remove();
            }
            remove_stale_slots(&spec);
            // Only removes it once the other pool's slots are gone too.
            let _ = std::fs::remove_dir(&spec.workers_dir);
        })
        .await;
    }

    /// The spawner this pool starts its workers with.
    pub fn spawner(&self) -> &Spawner {
        &self.spawner
    }
}

struct WaitingGuard<'a>(&'a PoolInner);

impl<'a> WaitingGuard<'a> {
    fn new(inner: &'a PoolInner) -> Self {
        let depth = inner.waiting.fetch_add(1, Ordering::AcqRel) + 1;
        metrics::gauge!("convert_queue_depth", "pool" => inner.spec.name).set(depth as f64);
        Self(inner)
    }
}

impl Drop for WaitingGuard<'_> {
    fn drop(&mut self) {
        let depth = self.0.waiting.fetch_sub(1, Ordering::AcqRel) - 1;
        metrics::gauge!("convert_queue_depth", "pool" => self.0.spec.name).set(depth as f64);
    }
}

impl PoolInner {
    fn unavailable_now(&self) -> bool {
        self.backoff.load(Ordering::Acquire) && self.live.load(Ordering::Acquire) == 0
    }

    /// Discards a leased (busy) worker.
    fn discard(&self, mut worker: Worker, reason: Reason) {
        worker.reason = reason;
        lock(&self.idle).busy -= 1;
        drop(worker);
    }

    /// Returns a leased worker to the idle list, unless it was retired
    /// while it was out.
    fn give_back(&self, worker: Worker) {
        let retire = {
            let mut idle = lock(&self.idle);
            idle.busy -= 1;
            if worker.retiring() {
                Some(worker)
            } else {
                idle.workers.push(worker);
                None
            }
        };
        match retire {
            Some(mut worker) => {
                worker.reason = Reason::Jobs;
                drop(worker);
            }
            None => self.permits.add_permits(1),
        }
    }

    /// Removes one idle worker matching `pred`, taking its permit. Best
    /// effort: when every permit is taken, a lease pops it and checks it.
    fn take_idle(&self, pred: impl Fn(&Worker) -> bool) -> Option<Worker> {
        let permit = self.permits.try_acquire().ok()?;
        let mut idle = lock(&self.idle);
        let at = idle.workers.iter().position(pred)?;
        permit.forget();
        Some(idle.workers.remove(at))
    }
}

impl Lease {
    /// Sends `op` and waits for the reply until `deadline`. A timeout kills
    /// the worker's process group. After any error the worker is discarded
    /// when the lease ends.
    pub async fn call(&mut self, op: Op, deadline: Instant) -> Result<Reply, CallError> {
        let Some(worker) = self.worker.as_mut() else {
            return Err(CallError::Protocol("the lease has no worker".into()));
        };
        if self.broken.is_some() || worker.child.has_exited() {
            self.broken = Some(Reason::Crash);
            return Err(CallError::Crashed {
                status: worker.child.exit().and_then(|e| e.status),
            });
        }
        let id = worker.next_id;
        worker.next_id += 1;
        self.in_flight = true;
        let request = Request::Call(Call { id, op });
        let at = tokio::time::Instant::from_std(deadline);
        let outcome = tokio::time::timeout_at(at, async {
            if let Err(err) = worker.conn.send(&request).await {
                return Exchange::Frame(err);
            }
            tokio::select! {
                // A reply that arrived before the exit still counts.
                biased;
                frame = worker.conn.recv() => match frame {
                    Ok(Some(Response::Reply(reply))) if reply.id == id => Exchange::Reply(reply),
                    Ok(Some(other)) => Exchange::Unexpected(format!("{other:?}")),
                    Ok(None) => Exchange::Eof,
                    Err(err) => Exchange::Frame(err),
                },
                _ = worker.child.wait() => Exchange::Eof,
            }
        })
        .await;
        self.in_flight = false;
        match outcome {
            Ok(Exchange::Reply(reply)) => {
                worker.jobs = reply.job;
                worker.rss = reply.rss_bytes;
                Ok(reply)
            }
            Err(_) => {
                worker.child.kill_group();
                self.broken = Some(Reason::Timeout);
                Err(CallError::Timeout)
            }
            Ok(Exchange::Unexpected(what)) => {
                worker.child.kill_group();
                self.broken = Some(Reason::Error);
                Err(CallError::Protocol(format!("unexpected frame {what}")))
            }
            Ok(Exchange::Frame(FrameError::Json(err))) => {
                worker.child.kill_group();
                self.broken = Some(Reason::Error);
                Err(CallError::Protocol(format!("bad frame: {err}")))
            }
            Ok(Exchange::Frame(FrameError::TooLarge { len })) => {
                worker.child.kill_group();
                self.broken = Some(Reason::Error);
                Err(CallError::Protocol(format!(
                    "oversize frame of {len} bytes"
                )))
            }
            Ok(Exchange::Eof | Exchange::Frame(_)) => {
                worker.child.kill_group();
                self.broken = Some(Reason::Crash);
                let status = tokio::time::timeout(CRASH_STATUS_WAIT, worker.child.wait())
                    .await
                    .ok()
                    .and_then(|exit| exit.status);
                tracing::warn!(
                    pool = worker.pool,
                    worker_id = %worker.id(),
                    generation = worker.generation,
                    signal = status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s)),
                    exit_code = status.and_then(|s| s.code()),
                    jobs = worker.jobs,
                    "worker crashed"
                );
                Err(CallError::Crashed { status })
            }
        }
    }

    /// Which worker this lease holds.
    pub fn worker_ref(&self) -> Option<WorkerRef> {
        self.worker.as_ref().map(|w| WorkerRef {
            pool: w.pool,
            slot: w.slot,
            generation: w.generation,
            job: w.jobs,
        })
    }

    /// The worker's PID.
    pub fn pid(&self) -> Option<u32> {
        self.worker.as_ref().map(|w| w.child.pid())
    }

    /// Time spent waiting for a free worker.
    pub fn queued(&self) -> Duration {
        self.queued
    }

    /// The whole `lease` call, the queue included.
    pub fn leased(&self) -> Duration {
        self.leased
    }

    /// Spawn (to `Ready`) and bind times, when this worker started while
    /// the lease waited for it, so its start was on the request's path.
    pub fn spawned_on_path(&self) -> Option<(Duration, Duration)> {
        self.spawned_on_path
    }

    /// Recycle the worker when the lease ends (after an engine-reported
    /// error that leaves the engine suspect).
    pub fn recycle_after(&mut self) {
        self.recycle_after = true;
    }
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("worker", &self.worker_ref())
            .field("queued", &self.queued)
            .field("in_flight", &self.in_flight)
            .finish()
    }
}

enum Exchange {
    Reply(Reply),
    Unexpected(String),
    Eof,
    Frame(FrameError),
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        let pool = &self.pool;
        if self.in_flight {
            // The call future was dropped mid-call: its reply would reach
            // the next lease, so the worker goes.
            pool.discard(worker, Reason::Cancelled);
            return;
        }
        if let Some(reason) = self.broken {
            pool.discard(worker, reason);
            return;
        }
        if worker.child.has_exited() {
            pool.discard(worker, Reason::Crash);
            return;
        }
        let spec = &pool.spec;
        match recycle_decision(
            worker.jobs,
            worker.ready_at.elapsed(),
            worker.rss,
            self.recycle_after,
            spec.max_jobs,
            spec.max_age,
            spec.max_rss,
        ) {
            Recycle::Keep => pool.give_back(worker),
            Recycle::KillNow(reason) => pool.discard(worker, reason),
            Recycle::ReplaceFirst(_) => {
                if worker
                    .fate
                    .compare_exchange(
                        FATE_NORMAL,
                        FATE_REPLACING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    let _ = pool.events.send(Event::Replace);
                }
                pool.give_back(worker);
            }
        }
    }
}

fn remove_stale_slots(spec: &PoolSpec) {
    let Ok(entries) = std::fs::read_dir(&spec.workers_dir) else {
        return;
    };
    let prefix = format!("{}-", spec.name);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_slot = name
            .to_str()
            .and_then(|n| n.strip_prefix(&prefix))
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
        if is_slot {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberState {
    Starting,
    Live,
}

struct Member {
    generation: u32,
    state: MemberState,
    fate: Arc<AtomicU8>,
}

struct Manager {
    pool: Weak<PoolInner>,
    spec: Arc<PoolSpec>,
    spawner: Spawner,
    members: BTreeMap<u16, Member>,
    next_generation: u32,
    failures: u32,
    backoff_until: Option<tokio::time::Instant>,
}

impl Manager {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Event>) {
        self.reconcile();
        let mut age_tick = tokio::time::interval(self.spec.age_check);
        age_tick.tick().await;
        loop {
            let backoff = self.backoff_until;
            let event = tokio::select! {
                event = rx.recv() => event,
                _ = age_tick.tick() => {
                    self.check_age();
                    continue;
                }
                () = async {
                    match backoff {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.backoff_until = None;
                    self.reconcile();
                    continue;
                }
            };
            let Some(event) = event else { break };
            let Some(pool) = self.pool.upgrade() else {
                break;
            };
            match event {
                Event::Started {
                    slot,
                    generation,
                    result,
                } => self.started(&pool, slot, generation, result),
                Event::Vacated {
                    slot,
                    generation,
                    reason,
                } => self.vacated(&pool, slot, generation, reason),
                Event::Exited { slot, generation } => {
                    if let Some(mut worker) =
                        pool.take_idle(|w| w.slot == slot && w.generation == generation)
                    {
                        worker.reason = Reason::Crash;
                        drop(worker);
                    }
                }
                Event::Replace => self.reconcile(),
                Event::Shutdown => break,
            }
            self.publish(&pool);
        }
    }

    fn publish(&self, pool: &PoolInner) {
        let (idle, busy) = {
            let idle = lock(&pool.idle);
            (idle.workers.len(), idle.busy)
        };
        let name = self.spec.name;
        metrics::gauge!("convert_workers", "pool" => name, "state" => "idle").set(idle as f64);
        metrics::gauge!("convert_workers", "pool" => name, "state" => "busy").set(busy as f64);
        metrics::gauge!("convert_workers", "pool" => name, "state" => "starting")
            .set(pool.starting.load(Ordering::Acquire) as f64);
    }

    /// Members that count towards the target: not being replaced.
    fn standing(&self) -> usize {
        self.members
            .values()
            .filter(|m| m.fate.load(Ordering::Acquire) == FATE_NORMAL)
            .count()
    }

    fn reconcile(&mut self) {
        let Some(pool) = self.pool.upgrade() else {
            return;
        };
        if pool.draining.load(Ordering::Acquire) || self.backoff_until.is_some() {
            return;
        }
        while self.standing() < usize::from(self.spec.size) {
            let slot = (0..=u16::MAX)
                .find(|s| !self.members.contains_key(s))
                .unwrap_or(u16::MAX);
            let generation = self.next_generation;
            self.next_generation += 1;
            let fate = Arc::new(AtomicU8::new(FATE_NORMAL));
            self.members.insert(
                slot,
                Member {
                    generation,
                    state: MemberState::Starting,
                    fate: Arc::clone(&fate),
                },
            );
            pool.starting.fetch_add(1, Ordering::AcqRel);
            pool.max_slot.fetch_max(u32::from(slot), Ordering::AcqRel);
            let start = Start {
                spec: Arc::clone(&self.spec),
                spawner: self.spawner.clone(),
                events: pool.events.clone(),
                handles: Arc::clone(&pool.handles),
                slot,
                generation,
                fate,
            };
            tokio::spawn(start.run());
        }
    }

    fn started(
        &mut self,
        pool: &Arc<PoolInner>,
        slot: u16,
        generation: u32,
        result: Result<Box<Worker>, String>,
    ) {
        pool.starting.fetch_sub(1, Ordering::AcqRel);
        let current = self.members.get(&slot).map(|m| m.generation) == Some(generation);
        match result {
            Ok(mut worker) => {
                if !current || pool.draining.load(Ordering::Acquire) {
                    worker.reason = Reason::Shutdown;
                    if current {
                        self.members.remove(&slot);
                    }
                    drop(worker);
                    return;
                }
                if let Some(member) = self.members.get_mut(&slot) {
                    member.state = MemberState::Live;
                }
                self.failures = 0;
                pool.backoff.store(false, Ordering::Release);
                pool.ready_once.store(true, Ordering::Release);
                pool.live.fetch_add(1, Ordering::AcqRel);
                tracing::info!(
                    role = self.spec.role.as_str(),
                    pool = self.spec.name,
                    worker_id = %worker.id(),
                    pid = worker.child.pid(),
                    generation,
                    spawn_us = micros(worker.spawn),
                    bind_us = micros(worker.bind),
                    rss_bytes = worker.rss,
                    "worker ready"
                );
                // Watch for a death while idle.
                let child = worker.child.clone();
                let events = pool.events.clone();
                tokio::spawn(async move {
                    child.wait().await;
                    let _ = events.send(Event::Exited { slot, generation });
                });
                lock(&pool.idle).workers.push(*worker);
                pool.permits.add_permits(1);
                self.retire_one_replaced(pool);
            }
            Err(error) => {
                if current {
                    self.members.remove(&slot);
                }
                self.failures += 1;
                let backoff = BACKOFF_START
                    .saturating_mul(1 << (self.failures - 1).min(16))
                    .min(BACKOFF_MAX);
                self.backoff_until = Some(tokio::time::Instant::now() + backoff);
                pool.backoff.store(true, Ordering::Release);
                pool.unavailable.notify_waiters();
                metrics::counter!(
                    "convert_worker_restarts_total",
                    "pool" => self.spec.name,
                    "reason" => Reason::StartFailed.as_str()
                )
                .increment(1);
                tracing::warn!(
                    pool = self.spec.name,
                    worker_id = %format!("{}-{slot}", self.spec.name),
                    generation,
                    error = %error,
                    backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
                    "worker start failed"
                );
            }
        }
    }

    /// A replacement is `Ready`: the oldest worker being replaced is
    /// retired, now if idle, else when its lease ends.
    fn retire_one_replaced(&mut self, pool: &PoolInner) {
        let Some((&slot, member)) = self
            .members
            .iter()
            .filter(|(_, m)| m.fate.load(Ordering::Acquire) == FATE_REPLACING)
            .min_by_key(|(_, m)| m.generation)
        else {
            return;
        };
        member.fate.store(FATE_RETIRE, Ordering::Release);
        let generation = member.generation;
        if let Some(mut worker) = pool.take_idle(|w| w.slot == slot && w.generation == generation) {
            worker.reason = Reason::Jobs;
            drop(worker);
        }
    }

    fn vacated(&mut self, pool: &PoolInner, slot: u16, generation: u32, reason: Reason) {
        let Some(member) = self.members.get(&slot) else {
            return;
        };
        if member.generation != generation {
            return;
        }
        if member.state == MemberState::Live {
            pool.live.fetch_sub(1, Ordering::AcqRel);
        }
        self.members.remove(&slot);
        if reason != Reason::Shutdown {
            metrics::counter!(
                "convert_worker_restarts_total",
                "pool" => self.spec.name,
                "reason" => reason.as_str()
            )
            .increment(1);
        }
        if pool.live.load(Ordering::Acquire) == 0 {
            pool.unavailable.notify_waiters();
        }
        self.reconcile();
    }

    fn check_age(&mut self) {
        let Some(pool) = self.pool.upgrade() else {
            return;
        };
        let mut aged = false;
        {
            let idle = lock(&pool.idle);
            for worker in &idle.workers {
                if worker.ready_at.elapsed() >= self.spec.max_age
                    && worker
                        .fate
                        .compare_exchange(
                            FATE_NORMAL,
                            FATE_REPLACING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                {
                    aged = true;
                }
            }
        }
        if aged {
            self.reconcile();
        }
    }
}

/// The cost of one worker start, as [`start_once`] measures it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartTimes {
    /// Recreating the slot directory.
    pub prepare: Duration,
    /// From the spawn request to `Ready`, the bind included.
    pub spawn: Duration,
    /// The engine bind inside the worker, from `Ready`.
    pub bind: Duration,
    pub rss_bytes: u64,
}

/// Starts one worker of `spec` in `slot` the way the pool does, measures
/// it and kills it. For `engine bench`.
pub async fn start_once(
    spec: &PoolSpec,
    spawner: &Spawner,
    slot: u16,
) -> Result<StartTimes, String> {
    let (events, _rx) = mpsc::unbounded_channel();
    let started = Instant::now();
    let start = Start {
        spec: Arc::new(spec.clone()),
        spawner: spawner.clone(),
        events,
        handles: Arc::default(),
        slot,
        generation: 0,
        fate: Arc::new(AtomicU8::new(FATE_NORMAL)),
    };
    let mut worker = start.start().await?;
    let times = StartTimes {
        prepare: started.elapsed().saturating_sub(worker.spawn),
        spawn: worker.spawn,
        bind: worker.bind,
        rss_bytes: worker.rss,
    };
    worker.reason = Reason::Shutdown;
    let child = worker.child.clone();
    drop(worker);
    child.wait().await;
    Ok(times)
}

/// One worker start: slot, spawn, `Init`, `Ready`.
struct Start {
    spec: Arc<PoolSpec>,
    spawner: Spawner,
    events: mpsc::UnboundedSender<Event>,
    handles: Handles,
    slot: u16,
    generation: u32,
    fate: Arc<AtomicU8>,
}

impl Start {
    async fn run(self) {
        let (slot, generation) = (self.slot, self.generation);
        let events = self.events.clone();
        let result = self.start().await.map(Box::new);
        let _ = events.send(Event::Started {
            slot,
            generation,
            result,
        });
    }

    async fn start(self) -> Result<Worker, String> {
        let spec = &self.spec;
        let worker_id = format!("{}-{}", spec.name, self.slot);
        let slot_dir = SlotDir::new(&spec.workers_dir, spec.name, self.slot);
        let template = spec.profile_template.clone();
        let prepared = slot_dir.clone();
        tokio::task::spawn_blocking(move || prepared.prepare(template.as_deref()))
            .await
            .map_err(|err| format!("failed to prepare the slot: {err}"))?
            .map_err(|err| format!("failed to prepare the slot: {err}"))?;

        let cmd = spec
            .env
            .command(&spec.program, spec.role.as_str(), &slot_dir, &worker_id);
        let spawned_at = Instant::now();
        let spawned = self
            .spawner
            .spawn(cmd)
            .await
            .map_err(|err| format!("failed to spawn the worker: {err}"))?;
        let child = spawned.child;
        lock(&self.handles).insert((self.slot, self.generation), child.clone());
        tracing::debug!(
            pool = spec.name,
            worker_id = %worker_id,
            pid = child.pid(),
            generation = self.generation,
            "worker started"
        );
        let fail = |message: String| {
            child.kill_group();
            lock(&self.handles).remove(&(self.slot, self.generation));
            message
        };
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (spawned.stdin, spawned.stdout, spawned.stderr)
        else {
            return Err(fail("failed to start the worker: missing pipes".into()));
        };
        drain_stderr(stderr, worker_id.clone())
            .map_err(|err| fail(format!("failed to drain stderr: {err}")))?;
        let mut conn = Conn::new(stdin, stdout)
            .map_err(|err| fail(format!("failed to open the pipes: {err}")))?;

        let handshake = async {
            conn.send(&Request::Init(spec.init.clone()))
                .await
                .map_err(|err| format!("failed to send init: {err}"))?;
            tokio::select! {
                frame = conn.recv() => match frame {
                    Ok(Some(Response::Ready(ready))) => Ok(ready),
                    Ok(Some(Response::Fatal(fatal))) => Err(fatal.message),
                    Ok(Some(other)) => Err(format!("failed to start the worker: unexpected {other:?}")),
                    Ok(None) => Err("failed to start the worker: it exited".to_string()),
                    Err(err) => Err(format!("failed to read ready: {err}")),
                },
                exit = child.wait() => Err(format!("failed to start the worker: it exited ({:?})", exit.status)),
            }
        };
        let ready = match tokio::time::timeout(spec.start_timeout, handshake).await {
            Ok(Ok(ready)) => ready,
            Ok(Err(message)) => return Err(fail(message)),
            Err(_) => return Err(fail("failed to start the worker: no ready in time".into())),
        };
        if ready.protocol != PROTOCOL_VERSION {
            return Err(fail(format!(
                "failed to start the worker: protocol {}",
                ready.protocol
            )));
        }
        Ok(Worker {
            pool: spec.name,
            slot: self.slot,
            generation: self.generation,
            child,
            conn,
            ready_at: Instant::now(),
            spawn: spawned_at.elapsed(),
            bind: Duration::from_micros(ready.bind_us),
            jobs: 0,
            rss: ready.rss_bytes,
            next_id: 1,
            fate: self.fate,
            reason: Reason::Shutdown,
            events: self.events,
            handles: self.handles,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: Duration = Duration::from_secs(3600);

    fn decide(jobs: u32, age: Duration, rss: u64, after: bool, max_jobs: u32) -> Recycle {
        recycle_decision(jobs, age, rss, after, max_jobs, H, 1000)
    }

    #[test]
    fn recycle_policy() {
        assert_eq!(decide(1, Duration::ZERO, 10, false, 50), Recycle::Keep);
        assert_eq!(
            decide(50, Duration::ZERO, 10, false, 50),
            Recycle::ReplaceFirst(Reason::Jobs)
        );
        assert_eq!(
            decide(3, H, 10, false, 50),
            Recycle::ReplaceFirst(Reason::Age)
        );
        assert_eq!(
            decide(3, Duration::ZERO, 1000, false, 50),
            Recycle::KillNow(Reason::Rss)
        );
        assert_eq!(
            decide(3, Duration::ZERO, 10, true, 50),
            Recycle::KillNow(Reason::Error)
        );
        // One call per process is kill-first, never replace-first.
        assert_eq!(
            decide(1, Duration::ZERO, 10, false, 1),
            Recycle::KillNow(Reason::Jobs)
        );
        // RSS and errors win over replace-first.
        assert_eq!(
            decide(50, H, 1000, false, 50),
            Recycle::KillNow(Reason::Rss)
        );
    }
}
