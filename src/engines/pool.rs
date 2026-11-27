//! One generic worker pool, used for both office and render workers.
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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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

/// How often a call samples its worker's RSS against `kill_rss`.
pub const KILL_RSS_TICK: Duration = Duration::from_millis(200);

/// A SIGKILL at an RSS within this share of `kill_rss` (or at `max_rss`
/// and above) is taken for the kernel's OOM killer: the call ran out of
/// memory, it did not crash.
pub const OOM_MARGIN_PERCENT: u64 = 10;

/// Samples process `pid`'s RSS every [`KILL_RSS_TICK`] into `peak`, and
/// resolves once a sample reaches `limit` (never, when the limit is off:
/// `u64::MAX`). A failed read is retried on the next tick: the watchdog
/// never parks, so it cannot stop sampling after one bad read.
async fn rss_above(pid: u32, limit: u64, peak: &AtomicU64) -> u64 {
    let start = tokio::time::Instant::now() + KILL_RSS_TICK;
    let mut tick = tokio::time::interval_at(start, KILL_RSS_TICK);
    let mut logged = false;
    loop {
        tick.tick().await;
        match crate::worker::sys::rss_of(pid) {
            Some(rss) => {
                record_peak(peak, rss);
                if rss >= limit {
                    return rss;
                }
            }
            None if !logged => {
                logged = true;
                tracing::debug!(pid, "failed to read the worker's RSS; retrying each tick");
            }
            None => {}
        }
    }
}

/// Keeps the highest RSS of a call. A worker that the kernel kills tears
/// down its memory before its exit is seen, so a late sample can read 0:
/// the peak, not the last sample, tells whether it died of memory.
pub fn record_peak(peak: &AtomicU64, rss: u64) {
    peak.fetch_max(rss, Ordering::Relaxed);
}

/// The `oom_kill` count of a cgroup v2 `memory.events` file, or `None` when
/// the file is absent (cgroup v1, macOS) or has no such line.
pub fn read_oom_kills(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill ")?.trim().parse().ok())
}

/// Where the manager is in `place_ready`, for a test hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceStage {
    /// Before the idle lock is taken.
    BeforeLock,
    /// Under the idle lock, just after the incumbent is retired. A hook
    /// here must not take the idle lock.
    Retired,
}

/// Test hooks: a function the manager calls in `place_ready` for the pool of
/// that name, at each [`PlaceStage`] (to drive a race). Its second argument
/// tells whether the pool's idle lock is held at that moment, probed with
/// `try_lock` by the caller of the hook, not reported by `place_ready`.
#[cfg(feature = "test-hooks")]
type ReadyHook = Arc<dyn Fn(PlaceStage, bool) + Send + Sync>;
#[cfg(feature = "test-hooks")]
static PLACE_READY_HOOKS: Mutex<Vec<(&'static str, u64, ReadyHook)>> = Mutex::new(Vec::new());
#[cfg(feature = "test-hooks")]
static PLACE_READY_HOOK_IDS: AtomicU64 = AtomicU64::new(0);

/// Registers `hook` for the pool named `pool`, in place of any earlier one
/// (test hooks only). It stays registered while the returned guard lives.
#[cfg(feature = "test-hooks")]
#[must_use = "the hook is removed when the guard is dropped"]
pub fn set_place_ready_hook(
    pool: &'static str,
    hook: impl Fn(PlaceStage, bool) + Send + Sync + 'static,
) -> PlaceReadyHook {
    let id = PLACE_READY_HOOK_IDS.fetch_add(1, Ordering::Relaxed);
    let mut hooks = lock(&PLACE_READY_HOOKS);
    hooks.retain(|(name, _, _)| *name != pool);
    hooks.push((pool, id, Arc::new(hook)));
    PlaceReadyHook { pool, id }
}

/// Removes its own place_ready hook when dropped, not a later one.
#[cfg(feature = "test-hooks")]
pub struct PlaceReadyHook {
    pool: &'static str,
    id: u64,
}

#[cfg(feature = "test-hooks")]
impl Drop for PlaceReadyHook {
    fn drop(&mut self) {
        lock(&PLACE_READY_HOOKS).retain(|(name, id, _)| !(*name == self.pool && *id == self.id));
    }
}

#[cfg(feature = "test-hooks")]
type LeaveHook = Arc<dyn Fn() + Send + Sync>;
#[cfg(feature = "test-hooks")]
static QUEUE_LEAVE_HOOKS: Mutex<Vec<(&'static str, u64, LeaveHook)>> = Mutex::new(Vec::new());

/// Registers `hook` for the pool named `pool` (test hooks only). A lease
/// that leaves the queue calls it after the waiting count drops and before
/// the queue gauge changes. It stays registered while the guard lives.
#[cfg(feature = "test-hooks")]
#[must_use = "the hook is removed when the guard is dropped"]
pub fn set_queue_leave_hook(
    pool: &'static str,
    hook: impl Fn() + Send + Sync + 'static,
) -> QueueLeaveHook {
    let id = PLACE_READY_HOOK_IDS.fetch_add(1, Ordering::Relaxed);
    let mut hooks = lock(&QUEUE_LEAVE_HOOKS);
    hooks.retain(|(name, _, _)| *name != pool);
    hooks.push((pool, id, Arc::new(hook)));
    QueueLeaveHook { pool, id }
}

/// Removes its own queue-leave hook when dropped, not a later one.
#[cfg(feature = "test-hooks")]
pub struct QueueLeaveHook {
    pool: &'static str,
    id: u64,
}

#[cfg(feature = "test-hooks")]
impl Drop for QueueLeaveHook {
    fn drop(&mut self) {
        lock(&QUEUE_LEAVE_HOOKS).retain(|(name, id, _)| !(*name == self.pool && *id == self.id));
    }
}

fn queue_leave_hook(_pool: &str) {
    #[cfg(feature = "test-hooks")]
    {
        let hook = lock(&QUEUE_LEAVE_HOOKS)
            .iter()
            .find(|(name, _, _)| *name == _pool)
            .map(|(_, _, hook)| Arc::clone(hook));
        if let Some(hook) = hook {
            hook();
        }
    }
}

fn place_ready_hook(_pool: &str, _stage: PlaceStage, _idle: &Mutex<Idle>) {
    #[cfg(feature = "test-hooks")]
    {
        let hook = lock(&PLACE_READY_HOOKS)
            .iter()
            .find(|(name, _, _)| *name == _pool)
            .map(|(_, _, hook)| Arc::clone(hook));
        if let Some(hook) = hook {
            // Held by this thread or another: try_lock fails either way.
            let locked = _idle.try_lock().is_err();
            hook(_stage, locked);
        }
    }
}

/// The cgroup v2 file whose `oom_kill` count tells a kernel OOM kill, as
/// seen inside the container's cgroup namespace.
pub const CGROUP_MEMORY_EVENTS: &str = "/sys/fs/cgroup/memory.events";

/// Whether a worker that died at `rss` died of memory: at or above its
/// recycle threshold, or within [`OOM_MARGIN_PERCENT`] of its kill limit.
pub fn near_memory_limit(rss: u64, max_rss: u64, kill_rss: u64) -> bool {
    let margin = kill_rss / 100 * OOM_MARGIN_PERCENT;
    rss >= max_rss || rss >= kill_rss.saturating_sub(margin)
}

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
    /// Kill the worker during a call when its RSS reaches this many bytes
    /// (`WORKER_KILL_RSS_MB`); `u64::MAX` turns the watchdog off.
    pub kill_rss: u64,
    pub max_age: Duration,
    /// How often idle workers are checked for `max_age`.
    pub age_check: Duration,
    /// `Queue`-mode waiters allowed before `QueueFull`.
    pub queue_capacity: usize,
    /// The longest `Queue`-mode wait.
    pub queue_wait: Duration,
    pub start_timeout: Duration,
    /// The cgroup's `memory.events`: a SIGKILL during a call in which its
    /// `oom_kill` count rose is a memory failure. `None` without cgroup v2.
    pub oom_events: Option<PathBuf>,
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
    /// The worker passed `kill_rss` during the call and was killed.
    #[error("failed to finish the call: the worker used {rss} bytes")]
    TooBig { rss: u64 },
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
    /// Killed during a call above `kill_rss` (counted as `rss`).
    RssKill,
    /// Not a restart: the pool is shutting down.
    Shutdown,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Jobs => "jobs",
            Reason::Rss | Reason::RssKill => "rss",
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
    /// Conversions this process has served, the current one included. A
    /// `Ping` or an `Inspect` does not count.
    pub job: u32,
}

const FATE_NORMAL: u8 = 0;
const FATE_REPLACING: u8 = 1;
const FATE_RETIRE: u8 = 2;

/// A worker's future, shared by the worker and the manager's entry for it:
/// normal, being replaced (still leasable), or retired (killed when next
/// seen idle). `aged` tells an age recycle from a job-count one.
#[derive(Default)]
struct Fate {
    state: AtomicU8,
    aged: AtomicBool,
}

impl Fate {
    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    /// Normal to being replaced, for `reason`. True if this call did it.
    fn replace(&self, reason: Reason) -> bool {
        let changed = self
            .state
            .compare_exchange(
                FATE_NORMAL,
                FATE_REPLACING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if changed {
            self.aged.store(reason == Reason::Age, Ordering::Release);
        }
        changed
    }

    fn retire(&self) {
        self.state.store(FATE_RETIRE, Ordering::Release);
    }

    /// Why the worker is being replaced.
    fn reason(&self) -> Reason {
        if self.aged.load(Ordering::Acquire) {
            Reason::Age
        } else {
            Reason::Jobs
        }
    }
}

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
        /// The killed process: its memory stays until it exits.
        child: ChildHandle,
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
    fate: Arc<Fate>,
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
        self.fate.state() == FATE_RETIRE
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // A LibreOffice worker gets no teardown, only SIGKILL of the group.
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
            Reason::Timeout | Reason::Cancelled | Reason::RssKill => {
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
            child: self.child.clone(),
        });
    }
}

#[derive(Default)]
struct Idle {
    workers: Vec<Worker>,
    busy: usize,
    /// Ready replacements without a permit, out of `workers` so no lease
    /// can take them: each waits until a busy incumbent it replaces has
    /// left.
    waiting: Vec<Worker>,
    /// `(slot, generation)` of every leased worker. A worker that is in
    /// neither `workers` nor here is gone (discarded or exited).
    leased: Vec<(u16, u32)>,
}

impl Idle {
    /// Workers a lease can take now.
    fn leasable(&self) -> usize {
        self.workers.len()
    }

    fn lease_one(&mut self, worker: &Worker) {
        self.busy += 1;
        self.leased.push((worker.slot, worker.generation));
    }

    fn unlease(&mut self, worker: &Worker) {
        let id = (worker.slot, worker.generation);
        self.leased.retain(|&leased| leased != id);
    }

    /// A retired incumbent left: one waiting replacement becomes leasable.
    /// True when the caller must add its permit.
    fn release_one(&mut self) -> bool {
        match self.waiting.pop() {
            Some(worker) => {
                self.workers.push(worker);
                true
            }
            None => false,
        }
    }
}

struct PoolInner {
    spec: Arc<PoolSpec>,
    idle: Mutex<Idle>,
    permits: Semaphore,
    /// Every waiting lease, for status and metrics.
    waiting: AtomicUsize,
    /// `Queue`-mode waiters only: the ones `queue_capacity` bounds.
    queued: AtomicUsize,
    starting: AtomicUsize,
    /// `Ready` workers alive (idle or busy), as the manager counts them.
    live: AtomicUsize,
    ready_once: AtomicBool,
    backoff: AtomicBool,
    draining: Arc<AtomicBool>,
    /// Worker starts that have not finished, so shutdown can wait for them.
    starts: Arc<AtomicUsize>,
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
            queued: AtomicUsize::new(0),
            starting: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            ready_once: AtomicBool::new(false),
            backoff: AtomicBool::new(false),
            draining: Arc::new(AtomicBool::new(false)),
            starts: Arc::new(AtomicUsize::new(0)),
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
            exiting: Vec::new(),
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
                if let Some(worker) = &worker {
                    idle.lease_one(worker);
                    inner.publish_counts(&idle);
                }
                worker
            };
            let Some(worker) = worker else {
                // A permit without a worker cannot happen; try again.
                continue;
            };
            if worker.child.has_exited() || worker.retiring() {
                let reason = if worker.retiring() {
                    worker.fate.reason()
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
        // Take the place first, then check it, so concurrent leases cannot
        // both pass the bound. Only `Queue` waiters count against it.
        let _queued = match adm {
            Admission::Queue => {
                let place = QueuePlace::take(inner);
                if place.depth > inner.spec.queue_capacity {
                    return Err(LeaseError::QueueFull);
                }
                Some(place)
            }
            Admission::Continue => None,
        };
        let limit = match adm {
            Admission::Queue => deadline.min(queue_limit),
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
                if let Some(worker) = &worker {
                    idle.lease_one(worker);
                    inner.publish_counts(&idle);
                }
                worker
            }?;
            if worker.child.has_exited() || worker.retiring() {
                let reason = if worker.retiring() {
                    worker.fate.reason()
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
            (idle.leasable(), idle.busy)
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
        let idle: Vec<Worker> = {
            let mut idle = lock(&inner.idle);
            let mut all = std::mem::take(&mut idle.workers);
            all.append(&mut idle.waiting);
            all
        };
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
        // A start already under way would recreate its slot after the
        // clean-up; it sees `draining` and gives up, so wait for it.
        let until = Instant::now() + Duration::from_secs(5);
        while inner.starts.load(Ordering::Acquire) > 0 && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(2)).await;
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

/// A place in the bounded `Queue`-mode queue, given back on drop.
struct QueuePlace<'a> {
    inner: &'a PoolInner,
    /// Queue-mode waiters, this one included.
    depth: usize,
}

impl<'a> QueuePlace<'a> {
    fn take(inner: &'a PoolInner) -> Self {
        let depth = inner.queued.fetch_add(1, Ordering::AcqRel) + 1;
        Self { inner, depth }
    }
}

impl Drop for QueuePlace<'_> {
    fn drop(&mut self) {
        self.inner.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

struct WaitingGuard<'a>(&'a PoolInner);

/// The gauge moves by one with each waiter, never to a value read from the
/// counter: two leaves that overtake each other would publish a stale depth
/// (4 after 0). Increments and decrements commute, so the gauge ends where
/// the counter does.
impl<'a> WaitingGuard<'a> {
    fn new(inner: &'a PoolInner) -> Self {
        inner.waiting.fetch_add(1, Ordering::AcqRel);
        metrics::gauge!("convert_queue_depth", "pool" => inner.spec.name).increment(1.0);
        Self(inner)
    }
}

impl Drop for WaitingGuard<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::AcqRel);
        queue_leave_hook(self.0.spec.name);
        metrics::gauge!("convert_queue_depth", "pool" => self.0.spec.name).decrement(1.0);
    }
}

impl PoolInner {
    /// Sets `convert_workers` from the idle list. Called under the idle
    /// lock on every lease and return, and after every manager event, so
    /// the gauges agree with `status()` and two updates never reorder.
    fn publish_counts(&self, idle: &Idle) {
        let name = self.spec.name;
        metrics::gauge!("convert_workers", "pool" => name, "state" => "idle")
            .set(idle.leasable() as f64);
        metrics::gauge!("convert_workers", "pool" => name, "state" => "busy").set(idle.busy as f64);
        metrics::gauge!("convert_workers", "pool" => name, "state" => "starting")
            .set(self.starting.load(Ordering::Acquire) as f64);
    }

    fn unavailable_now(&self) -> bool {
        self.backoff.load(Ordering::Acquire) && self.live.load(Ordering::Acquire) == 0
    }

    /// Discards a leased (busy) worker.
    fn discard(&self, mut worker: Worker, reason: Reason) {
        worker.reason = reason;
        let release = {
            let mut idle = lock(&self.idle);
            idle.busy -= 1;
            idle.unlease(&worker);
            let release = worker.retiring() && idle.release_one();
            self.publish_counts(&idle);
            release
        };
        drop(worker);
        if release {
            self.permits.add_permits(1);
        }
    }

    /// Returns a leased worker to the idle list, unless it was retired
    /// while it was out.
    fn give_back(&self, worker: Worker) {
        let mut release = false;
        let retire = {
            let mut idle = lock(&self.idle);
            idle.busy -= 1;
            idle.unlease(&worker);
            let retire = if worker.retiring() {
                // Its replacement waited for this: it gets the permit.
                release = idle.release_one();
                Some(worker)
            } else {
                idle.workers.push(worker);
                None
            };
            self.publish_counts(&idle);
            retire
        };
        match retire {
            Some(mut worker) => {
                worker.reason = worker.fate.reason();
                drop(worker);
                if release {
                    self.permits.add_permits(1);
                }
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
        let op_name = op.name();
        let request = Request::Call(Call { id, op });
        let at = tokio::time::Instant::from_std(deadline);
        let pid = worker.child.pid();
        let kill_rss = self.pool.spec.kill_rss;
        // The call's peak RSS: the previous reply's, then each sample.
        let peak_rss = AtomicU64::new(worker.rss);
        let oom_kills = self
            .pool
            .spec
            .oom_events
            .as_deref()
            .and_then(read_oom_kills);
        let outcome = tokio::time::timeout_at(at, async {
            if let Err(err) = worker.conn.send(&request).await {
                return Exchange::Frame(err);
            }
            tokio::select! {
                // A reply that arrived before the exit still counts.
                biased;
                frame = worker.conn.recv() => match frame {
                    Ok(Some(Response::Reply(reply))) if reply.id == id => Exchange::Reply(Box::new(reply)),
                    Ok(Some(other)) => Exchange::Unexpected(format!("{other:?}")),
                    Ok(None) => Exchange::Eof,
                    Err(err) => Exchange::Frame(err),
                },
                _ = worker.child.wait() => Exchange::Eof,
                rss = rss_above(pid, kill_rss, &peak_rss) => Exchange::TooBig(rss),
            }
        })
        .await;
        self.in_flight = false;
        match outcome {
            Ok(Exchange::Reply(reply)) => {
                worker.jobs = reply.job;
                worker.rss = reply.rss_bytes;
                // Every reply, an engine error included: the CPU was spent.
                if op_name != "ping" {
                    crate::observability::metrics::worker_cpu(
                        self.pool.spec.name,
                        op_name,
                        Duration::from_micros(reply.engine.cpu_us),
                    );
                }
                Ok(*reply)
            }
            Err(_) => {
                worker.child.kill_group();
                self.broken = Some(Reason::Timeout);
                Err(CallError::Timeout)
            }
            Ok(Exchange::TooBig(rss)) => {
                worker.child.kill_group();
                self.broken = Some(Reason::RssKill);
                tracing::warn!(
                    pool = worker.pool,
                    worker_id = %worker.id(),
                    generation = worker.generation,
                    rss_bytes = rss,
                    kill_rss = kill_rss,
                    "worker over its memory limit"
                );
                Err(CallError::TooBig { rss })
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
                let signal = status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s));
                let rss = peak_rss.load(Ordering::Relaxed);
                // The kernel's own count first; the RSS rule without cgroup v2.
                let cgroup_oom = match (oom_kills, self.pool.spec.oom_events.as_deref()) {
                    (Some(before), Some(path)) => {
                        read_oom_kills(path).is_some_and(|now| now > before)
                    }
                    _ => false,
                };
                if signal == Some(9)
                    && (cgroup_oom || near_memory_limit(rss, self.pool.spec.max_rss, kill_rss))
                {
                    // The kernel's OOM killer: a memory failure like the
                    // watchdog's kill, so no crash retry and no strike.
                    self.broken = Some(Reason::RssKill);
                    tracing::warn!(
                        pool = worker.pool,
                        worker_id = %worker.id(),
                        generation = worker.generation,
                        rss_bytes = rss,
                        max_rss = self.pool.spec.max_rss,
                        kill_rss = kill_rss,
                        cgroup_oom_kill = cgroup_oom,
                        "worker killed by SIGKILL near its memory limit"
                    );
                    return Err(CallError::TooBig { rss });
                }
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
    Reply(Box<Reply>),
    TooBig(u64),
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
            Recycle::ReplaceFirst(reason) => {
                if worker.fate.replace(reason) {
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
    fate: Arc<Fate>,
}

struct Manager {
    pool: Weak<PoolInner>,
    spec: Arc<PoolSpec>,
    spawner: Spawner,
    members: BTreeMap<u16, Member>,
    /// Killed workers whose exit is not yet seen. They still hold their
    /// memory, so they count against the one member above the size.
    exiting: Vec<ChildHandle>,
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
                    child,
                } => {
                    self.exiting(&pool, child);
                    self.vacated(&pool, slot, generation, reason);
                }
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
        pool.publish_counts(&lock(&pool.idle));
    }

    /// Members that count towards the target: not being replaced.
    fn standing(&self) -> usize {
        self.members
            .values()
            .filter(|m| m.fate.state() == FATE_NORMAL)
            .count()
    }

    fn reconcile(&mut self) {
        let Some(pool) = self.pool.upgrade() else {
            return;
        };
        if pool.draining.load(Ordering::Acquire) || self.backoff_until.is_some() {
            return;
        }
        // At most one member above the size: a worker being replaced and
        // its replacement, one pair at a time. Workers due together are thus
        // replaced in turn, and the memory budget holds one overlap per pool.
        // A hole (a worker that died or was killed) is filled at once, since
        // it leaves the members below the size.
        let size = usize::from(self.spec.size);
        self.exiting.retain(|child| !child.has_exited());
        while self.standing() < size && self.members.len() + self.exiting.len() <= size {
            let slot = (0..=u16::MAX)
                .find(|s| !self.members.contains_key(s))
                .unwrap_or(u16::MAX);
            let generation = self.next_generation;
            self.next_generation += 1;
            let fate = Arc::new(Fate::default());
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
            pool.starts.fetch_add(1, Ordering::AcqRel);
            let start = Start {
                spec: Arc::clone(&self.spec),
                spawner: self.spawner.clone(),
                events: pool.events.clone(),
                handles: Arc::clone(&pool.handles),
                draining: Arc::clone(&pool.draining),
                starts: Arc::clone(&pool.starts),
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
                self.place_ready(pool, *worker);
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

    /// Puts a `Ready` worker where leases find it, in one critical section,
    /// so `idle + busy` never drops to 0 during a swap.
    ///
    /// - Nothing is being replaced: the worker joins the idle list with a
    ///   new permit.
    /// - The oldest worker being replaced is retired. If it is idle, the
    ///   new worker takes its place and its permit, and it goes. A lease
    ///   that already holds that permit takes the new worker.
    /// - If it is leased, the new worker waits outside the idle list until
    ///   it leaves (`release_one`): the pool never runs more jobs at once
    ///   than its size.
    ///
    /// The retire happens inside the critical section. Before, a lease that
    /// ended between the retire and the lock saw its worker retired with
    /// no replacement waiting yet, so nobody released the replacement.
    fn place_ready(&mut self, pool: &PoolInner, worker: Worker) {
        place_ready_hook(self.spec.name, PlaceStage::BeforeLock, &pool.idle);
        let (gone, new_permit) = {
            let mut idle = lock(&pool.idle);
            // Retire under the idle lock: `give_back` and `discard` read
            // `retiring()` under it too, so a lease that ends now either
            // returned its worker before (and it is swapped below) or sees it
            // retired after the replacement is in `waiting` (and releases it).
            // A free lease slot first: after a crash beside a replacement,
            // the new worker fills the hole instead of waiting beside
            // another leased incumbent, so the pool serves at its size.
            let vacant = idle.workers.len() + idle.leased.len() < usize::from(self.spec.size);
            let replaced = self
                .members
                .iter()
                .filter(|(_, m)| !vacant && m.fate.state() == FATE_REPLACING)
                .min_by_key(|(_, m)| m.generation)
                .map(|(&slot, member)| {
                    member.fate.retire();
                    (slot, member.generation, member.fate.reason())
                });
            if replaced.is_some() {
                place_ready_hook(self.spec.name, PlaceStage::Retired, &pool.idle);
            }
            let (gone, new_permit) = match replaced {
                None => {
                    idle.workers.push(worker);
                    (None, true)
                }
                Some((slot, generation, reason)) => {
                    match idle
                        .workers
                        .iter()
                        .position(|w| w.slot == slot && w.generation == generation)
                    {
                        Some(at) => {
                            let mut old = std::mem::replace(&mut idle.workers[at], worker);
                            old.reason = reason;
                            (Some(old), false)
                        }
                        // Leased: the replacement waits for its return.
                        None if idle.leased.contains(&(slot, generation)) => {
                            idle.waiting.push(worker);
                            (None, false)
                        }
                        // Gone (discarded, its exit not yet seen): nobody
                        // will release a waiter, so it takes a new permit.
                        None => {
                            idle.workers.push(worker);
                            (None, true)
                        }
                    }
                }
            };
            pool.publish_counts(&idle);
            (gone, new_permit)
        };
        drop(gone);
        if new_permit {
            pool.permits.add_permits(1);
        }
    }

    /// Counts a killed worker until the reaper sees it exit, and reconciles
    /// then: a replacement held back by it may start.
    fn exiting(&mut self, pool: &PoolInner, child: ChildHandle) {
        if child.has_exited() {
            return;
        }
        self.exiting.push(child.clone());
        let events = pool.events.clone();
        tokio::spawn(async move {
            child.wait().await;
            let _ = events.send(Event::Replace);
        });
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
                    && worker.fate.replace(Reason::Age)
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

/// One worker start: slot, spawn, `Init`, `Ready`.
struct Start {
    spec: Arc<PoolSpec>,
    spawner: Spawner,
    events: mpsc::UnboundedSender<Event>,
    handles: Handles,
    draining: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    slot: u16,
    generation: u32,
    fate: Arc<Fate>,
}

impl Start {
    async fn run(self) {
        let (slot, generation) = (self.slot, self.generation);
        let events = self.events.clone();
        let starts = Arc::clone(&self.starts);
        // A child killed during a failed start still holds its memory until
        // it exits: its member stays counted until then, so no other start
        // runs beside it.
        let result = match self.start().await {
            Ok(worker) => Ok(Box::new(worker)),
            Err((message, child)) => {
                if let Some(child) = child {
                    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                }
                Err(message)
            }
        };
        // After a shutdown the manager is gone and the worker is dropped
        // (killed) with the unsent event.
        let _ = events.send(Event::Started {
            slot,
            generation,
            result,
        });
        starts.fetch_sub(1, Ordering::AcqRel);
    }

    fn shutting_down(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// The worker, or the error and the child it killed, if it spawned one.
    async fn start(self) -> Result<Worker, (String, Option<ChildHandle>)> {
        let spec = &self.spec;
        let worker_id = format!("{}-{}", spec.name, self.slot);
        let slot_dir = SlotDir::new(&spec.workers_dir, spec.name, self.slot);
        let template = spec.profile_template.clone();
        let prepared = slot_dir.clone();
        tokio::task::spawn_blocking(move || prepared.prepare(template.as_deref()))
            .await
            .map_err(|err| (format!("failed to prepare the slot: {err}"), None))?
            .map_err(|err| (format!("failed to prepare the slot: {err}"), None))?;
        if self.shutting_down() {
            return Err((
                "failed to start the worker: the pool is shutting down".into(),
                None,
            ));
        }

        let cmd = spec
            .env
            .command(&spec.program, spec.role.as_str(), &slot_dir, &worker_id);
        let spawned_at = Instant::now();
        let spawned = self
            .spawner
            .spawn(cmd)
            .await
            .map_err(|err| (format!("failed to spawn the worker: {err}"), None))?;
        let child = spawned.child;
        lock(&self.handles).insert((self.slot, self.generation), child.clone());
        // Shutdown sets `draining` before it kills the registered handles,
        // under the same lock: either it saw this child or this sees it.
        if self.shutting_down() {
            child.kill_group();
            // Shutdown waits for this start, so it must also see the child
            // reaped, not only killed.
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            lock(&self.handles).remove(&(self.slot, self.generation));
            return Err((
                "failed to start the worker: the pool is shutting down".into(),
                None,
            ));
        }
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
            (message, Some(child.clone()))
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
    fn the_peak_survives_a_dying_workers_zero_sample() {
        let peak = AtomicU64::new(250 << 20);
        // Samples of a worker that the kernel kills: the last ones fall to 0
        // while it tears down its memory.
        for rss in [900u64 << 20, 3_100 << 20, 1_200 << 20, 0] {
            record_peak(&peak, rss);
        }
        assert_eq!(peak.load(Ordering::Relaxed), 3_100 << 20);
    }

    #[test]
    fn the_oom_kill_count_is_read_from_memory_events() {
        let dir = std::env::temp_dir().join(format!("convertd-oom-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.events");
        std::fs::write(
            &path,
            "low 0\nhigh 0\nmax 12\noom 3\noom_kill 2\noom_group_kill 0\n",
        )
        .unwrap();
        assert_eq!(read_oom_kills(&path), Some(2));
        std::fs::write(&path, "low 0\n").unwrap();
        assert_eq!(read_oom_kills(&path), None, "no oom_kill line");
        assert_eq!(
            read_oom_kills(&dir.join("absent")),
            None,
            "cgroup v1 or macOS"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sigkill_counts_as_memory_near_either_limit() {
        const MIB: u64 = 1 << 20;
        // At or above the recycle threshold.
        assert!(near_memory_limit(1024 * MIB, 1024 * MIB, 2048 * MIB));
        // Within 10 % of the kill limit, the recycle threshold off.
        assert!(near_memory_limit(1850 * MIB, u64::MAX, 2048 * MIB));
        assert!(!near_memory_limit(1800 * MIB, u64::MAX, 2048 * MIB));
        // A low RSS is a crash.
        assert!(!near_memory_limit(300 * MIB, 1024 * MIB, 2048 * MIB));
        // Both limits off: never memory.
        assert!(!near_memory_limit(64 * 1024 * MIB, u64::MAX, u64::MAX));
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
