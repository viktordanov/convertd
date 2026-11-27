//! Process control for the supervisor: spawning, the child registry, the
//! reaper, group kills, the stderr drain and one-off children.
//!
//! **The reaper owns every wait.** No other supervisor code may wait on a
//! child (`clippy.toml` bans the std methods); a one-off child goes through
//! [`Spawner::run_to_completion`].
//!
//! - **Spawning** happens on one process-wide thread, `convertd-spawner`,
//!   which lives until the process exits. On Linux the worker's
//!   parent-death signal fires when the *thread* that forked it dies, so a
//!   short-lived spawning thread would kill healthy workers.
//! - **Reaping** is scoped to the PIDs a `Spawner` registered. Each wake
//!   peeks every registered child with `waitid(P_PID, NOWAIT)`, kills its
//!   group while the zombie still holds the PGID, then reaps it. Groups of
//!   dead workers are swept with `waitid(P_PGID)` until `ECHILD`. Only
//!   [`ReaperMode::ProcessWide`] (in `serve`) also reaps unknown children.
//!   It peeks them with `waitid(P_ALL, NOWAIT)` too, so a worker that
//!   exits between its own peek and that pass still has its group killed.
//! - **Locking:** the spawner thread holds the registry lock across
//!   `spawn()` and the insert, so an exit is never peeked before it is
//!   registered; `kill_group` takes the same lock and does nothing once
//!   the child is reaped, so a recycled PID is never signalled.

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic::AssertUnwindSafe;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak, mpsc};
use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, WaitOptions, kill_process_group, waitid, waitpid,
};
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::sync::{oneshot, watch};

/// The reaper's safety net for a coalesced or missed SIGCHLD.
pub const REAP_TICK: Duration = Duration::from_secs(2);

/// Longest stderr line logged, in bytes.
pub const STDERR_LINE_MAX: usize = 512;

/// Output kept from a one-off child, per stream.
pub const CAPTURE_MAX: usize = 4 * 1024 * 1024;

/// Which children the reaper collects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaperMode {
    /// Only the children this `Spawner` registered, and their groups.
    /// Tests, the `engine` CLI and everything but `serve`.
    Owned,
    /// Also any unknown child (`waitid(P_ALL)`), logged as `stray child
    /// reaped`. Only for `serve`, which on Linux becomes a child subreaper.
    /// No other code in the process may wait on a child of its own.
    ProcessWide,
}

/// How a child ended. `status` is `None` only if something else reaped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildExit {
    pub status: Option<ExitStatus>,
}

struct Entry {
    exit: watch::Sender<Option<ChildExit>>,
    exited: Arc<AtomicBool>,
}

#[derive(Default)]
struct Registry {
    children: HashMap<i32, Entry>,
    /// Process groups of dead children that may still hold reapable members.
    dead_groups: Vec<i32>,
}

struct Inner {
    registry: Mutex<Registry>,
    mode: ReaperMode,
}

/// Spawns children, registers them and reaps them. Cheap to clone. Build it
/// inside a tokio runtime: it starts the reaper task.
#[derive(Clone)]
pub struct Spawner {
    inner: Arc<Inner>,
}

/// A spawned child and the parent's ends of its pipes (`None` for streams
/// that were not piped).
pub struct Spawned {
    pub child: ChildHandle,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
}

/// One registered child. Its PGID equals its PID.
#[derive(Clone)]
pub struct ChildHandle {
    pid: i32,
    exited: Arc<AtomicBool>,
    exit: watch::Receiver<Option<ChildExit>>,
    spawner: Weak<Inner>,
}

/// The result of [`Spawner::run_to_completion`].
#[derive(Debug)]
pub struct Completed {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ChildHandle {
    pub fn pid(&self) -> u32 {
        self.pid.unsigned_abs()
    }

    /// The reaper has collected this child.
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// The exit, once the reaper has collected it.
    pub fn exit(&self) -> Option<ChildExit> {
        *self.exit.borrow()
    }

    /// Waits until the reaper collects the child. Cancel safe.
    pub async fn wait(&self) -> ChildExit {
        let mut rx = self.exit.clone();
        match rx.wait_for(Option::is_some).await {
            Ok(exit) => exit.unwrap_or(ChildExit { status: None }),
            // The spawner is gone, and it killed and reaped its children.
            Err(_) => self.exit().unwrap_or(ChildExit { status: None }),
        }
    }

    /// SIGKILLs the child's process group. Returns whether a signal was
    /// sent: nothing is sent once the child is reaped.
    pub fn kill_group(&self) -> bool {
        match self.spawner.upgrade() {
            Some(inner) => inner.kill_group(self.pid),
            None => false,
        }
    }
}

struct SpawnJob {
    cmd: Command,
    inner: Arc<Inner>,
    reply: oneshot::Sender<io::Result<Spawned>>,
}

/// The one spawner thread of the process. It is started on first use and
/// never exits: the sender lives in a static, so its loop never ends.
fn spawner_thread() -> io::Result<&'static Mutex<mpsc::Sender<SpawnJob>>> {
    static SENDER: OnceLock<Mutex<mpsc::Sender<SpawnJob>>> = OnceLock::new();
    static START: Mutex<()> = Mutex::new(());
    if let Some(tx) = SENDER.get() {
        return Ok(tx);
    }
    let _guard = lock(&START);
    if let Some(tx) = SENDER.get() {
        return Ok(tx);
    }
    let (tx, rx) = mpsc::channel::<SpawnJob>();
    std::thread::Builder::new()
        .name("convertd-spawner".into())
        .spawn(move || {
            for job in rx {
                let SpawnJob { cmd, inner, reply } = job;
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| inner.spawn_now(cmd)))
                    .unwrap_or_else(|_| Err(io::Error::other("the spawn panicked")));
                let _ = reply.send(result);
            }
            // Unreachable while the static sender exists; never exit anyway.
            loop {
                std::thread::park();
            }
        })?;
    Ok(SENDER.get_or_init(|| Mutex::new(tx)))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn exit_status(raw: i32) -> ExitStatus {
    ExitStatus::from_raw(raw)
}

impl Spawner {
    /// A spawner and its reaper task. Must run inside a tokio runtime.
    pub fn new(mode: ReaperMode) -> io::Result<Spawner> {
        if mode == ReaperMode::ProcessWide {
            become_subreaper()?;
        }
        let inner = Arc::new(Inner {
            registry: Mutex::new(Registry::default()),
            mode,
        });
        // Listen before the first spawn, so no SIGCHLD is missed.
        let mut sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REAP_TICK);
            loop {
                tokio::select! {
                    _ = sigchld.recv() => {}
                    _ = tick.tick() => {}
                }
                let Some(inner) = weak.upgrade() else { break };
                inner.reap();
            }
        });
        Ok(Spawner { inner })
    }

    pub fn mode(&self) -> ReaperMode {
        self.inner.mode
    }

    /// Spawns `cmd` in its own process group (PGID = PID) on the
    /// `convertd-spawner` thread and registers it. The caller sets the
    /// environment, the directory and the stdio.
    pub async fn spawn(&self, mut cmd: Command) -> io::Result<Spawned> {
        cmd.process_group(0);
        let (reply, answer) = oneshot::channel();
        let job = SpawnJob {
            cmd,
            inner: Arc::clone(&self.inner),
            reply,
        };
        lock(spawner_thread()?)
            .send(job)
            .map_err(|_| io::Error::other("the spawner thread is gone"))?;
        answer
            .await
            .map_err(|_| io::Error::other("the spawner thread dropped the request"))?
    }

    /// A spawner with no reaper task, for tests that drive the reaper by
    /// hand. It does not become a subreaper.
    #[cfg(feature = "test-hooks")]
    pub fn without_reaper_task(mode: ReaperMode) -> Spawner {
        Spawner {
            inner: Arc::new(Inner {
                registry: Mutex::new(Registry::default()),
                mode,
            }),
        }
    }

    /// Runs only the stray pass of the reaper: the pass that collects a
    /// worker that exited after its own peek.
    #[cfg(feature = "test-hooks")]
    pub fn reap_strays_now(&self) {
        let mut registry = lock(&self.inner.registry);
        self.inner.reap_strays(&mut registry);
    }

    /// The registered children that have not been reaped.
    pub fn live_children(&self) -> usize {
        lock(&self.inner.registry).children.len()
    }

    /// Runs a one-off child to the end: stdin from `/dev/null`, stdout and
    /// stderr captured (at most [`CAPTURE_MAX`] each). Past `timeout` the
    /// group is killed and the error is `TimedOut`. The only way to run a
    /// helper program in the supervisor.
    pub async fn run_to_completion(
        &self,
        mut cmd: Command,
        timeout: Duration,
    ) -> io::Result<Completed> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let spawned = self.spawn(cmd).await?;
        let child = spawned.child;
        let out = capture(spawned.stdout);
        let err = capture(spawned.stderr);
        let run = async {
            let (stdout, stderr, exit) = tokio::join!(out, err, child.wait());
            (stdout, stderr, exit)
        };
        match tokio::time::timeout(timeout, run).await {
            Ok((stdout, stderr, exit)) => Ok(Completed {
                status: exit.status,
                stdout: stdout?,
                stderr: stderr?,
            }),
            Err(_) => {
                child.kill_group();
                child.wait().await;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "failed to run the helper: it timed out",
                ))
            }
        }
    }
}

async fn capture(fd: Option<OwnedFd>) -> io::Result<Vec<u8>> {
    let Some(fd) = fd else {
        return Ok(Vec::new());
    };
    let mut rx = pipe::Receiver::from_owned_fd(fd)?;
    let mut kept = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let n = rx.read(&mut chunk).await?;
        if n == 0 {
            return Ok(kept);
        }
        let room = CAPTURE_MAX.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..n.min(room)]);
    }
}

fn signal_group(pid: i32) {
    if let Some(pid) = Pid::from_raw(pid) {
        // ESRCH or EPERM: the group has no live member left.
        let _ = kill_process_group(pid, Signal::KILL);
    }
}

impl Inner {
    /// On the spawner thread, under the registry lock.
    fn spawn_now(self: &Arc<Self>, mut cmd: Command) -> io::Result<Spawned> {
        let mut registry = lock(&self.registry);
        let mut child = cmd.spawn()?;
        let pid = i32::try_from(child.id()).map_err(io::Error::other)?;
        let stdin = child.stdin.take().map(OwnedFd::from);
        let stdout = child.stdout.take().map(OwnedFd::from);
        let stderr = child.stderr.take().map(OwnedFd::from);
        // Dropping a `Child` does not wait; the reaper owns the exit.
        drop(child);
        let (exit, exit_rx) = watch::channel(None);
        let exited = Arc::new(AtomicBool::new(false));
        registry.children.insert(
            pid,
            Entry {
                exit,
                exited: Arc::clone(&exited),
            },
        );
        Ok(Spawned {
            child: ChildHandle {
                pid,
                exited,
                exit: exit_rx,
                spawner: Arc::downgrade(self),
            },
            stdin,
            stdout,
            stderr,
        })
    }

    fn kill_group(&self, pid: i32) -> bool {
        let registry = lock(&self.registry);
        if !registry.children.contains_key(&pid) {
            return false;
        }
        signal_group(pid);
        true
    }

    fn reap(&self) {
        let mut registry = lock(&self.registry);
        let pids: Vec<i32> = registry.children.keys().copied().collect();
        for pid in pids {
            let Some(id) = Pid::from_raw(pid) else {
                continue;
            };
            let peek = waitid(
                WaitId::Pid(id),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            );
            if matches!(peek, Ok(None)) {
                continue;
            }
            // Exited (or, with ECHILD, reaped by someone else). The zombie
            // holds the PGID, so killing the group cannot hit a stranger.
            signal_group(pid);
            let status = match waitpid(Some(id), WaitOptions::NOHANG) {
                Ok(Some((_, status))) => Some(exit_status(status.as_raw())),
                _ => None,
            };
            registry.finish(pid, status);
        }
        registry.dead_groups.retain(|&pgid| sweep_group(pgid));
        if self.mode == ReaperMode::ProcessWide {
            self.reap_strays(&mut registry);
        }
    }

    /// Reaps every exited child that no `reap` pass took: unknown ones
    /// (`stray child reaped`) and registered workers that exited after
    /// their own peek. Each exit is peeked first, so a worker's group is
    /// killed while its zombie still holds the PGID.
    fn reap_strays(&self, registry: &mut Registry) {
        while let Some(pid) = peek_exited_child() {
            let Some(id) = Pid::from_raw(pid) else {
                break;
            };
            let registered = registry.children.contains_key(&pid);
            if registered {
                signal_group(pid);
            }
            let status = match waitpid(Some(id), WaitOptions::NOHANG) {
                Ok(Some((_, status))) => Some(exit_status(status.as_raw())),
                // The peeked child is gone: never loop on it.
                _ => break,
            };
            if registered {
                registry.finish(pid, status);
            } else {
                tracing::info!(pid, "stray child reaped");
            }
        }
    }
}

/// One exited child of this process, left a zombie (`waitid(P_ALL,
/// WNOWAIT)`), or `None` when no child has exited.
fn peek_exited_child() -> Option<i32> {
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: waitid fills the siginfo_t it is given. WNOWAIT leaves
        // the child waitable, and WNOHANG returns at once.
        let rc = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // ECHILD: no children at all.
            return None;
        }
        // SAFETY: zeroed, then written by a successful waitid. With
        // WNOHANG and no exited child, si_pid stays 0.
        let pid = unsafe { info.assume_init().si_pid() };
        return (pid > 0).then_some(pid);
    }
}

impl Registry {
    fn finish(&mut self, pid: i32, status: Option<ExitStatus>) {
        if let Some(entry) = self.children.remove(&pid) {
            entry.exited.store(true, Ordering::Release);
            entry.exit.send_replace(Some(ChildExit { status }));
        }
        self.dead_groups.push(pid);
    }
}

/// Reaps what is left of a dead worker's group. Keeps the group while it
/// may still have members; drops it on `ECHILD`.
fn sweep_group(pgid: i32) -> bool {
    let Some(id) = Pid::from_raw(pgid) else {
        return false;
    };
    loop {
        match waitid(
            WaitId::Pgid(Some(id)),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
        ) {
            Ok(Some(_)) => continue,
            Ok(None) => return true,
            Err(Errno::INTR) => continue,
            Err(_) => return false,
        }
    }
}

impl Drop for Inner {
    /// The last handle is gone: kill and reap every child still
    /// registered, synchronously, so none outlives the spawner.
    fn drop(&mut self) {
        let registry = self
            .registry
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pids: Vec<i32> = registry.children.keys().copied().collect();
        for pid in pids {
            signal_group(pid);
            let status = Pid::from_raw(pid)
                .and_then(|id| waitpid(Some(id), WaitOptions::empty()).ok().flatten())
                .map(|(_, status)| exit_status(status.as_raw()));
            registry.finish(pid, status);
        }
    }
}

/// Linux: become the child subreaper, so strays that leave their group
/// reparent to the supervisor even when it is not PID 1.
#[cfg(target_os = "linux")]
fn become_subreaper() -> io::Result<()> {
    rustix::process::set_child_subreaper(None).map_err(io::Error::from)
}

#[cfg(not(target_os = "linux"))]
fn become_subreaper() -> io::Result<()> {
    Ok(())
}

/// Linux: mark the supervisor not dumpable, so a compromised worker with the
/// same UID cannot read `/proc/<supervisor>/environ`.
/// The flag resets on `execve`, so workers are unaffected.
#[cfg(target_os = "linux")]
pub fn set_not_dumpable() -> io::Result<()> {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(io::Error::from)
}

#[cfg(not(target_os = "linux"))]
pub fn set_not_dumpable() -> io::Result<()> {
    Ok(())
}

/// Drains a worker's stderr until EOF, so a full pipe never blocks it.
/// Each line is logged at debug as `worker stderr`, cut to
/// [`STDERR_LINE_MAX`] bytes; with debug off the bytes are read and
/// dropped. EOF does not mean the worker is gone: its children may hold
/// the pipe.
pub fn drain_stderr(fd: OwnedFd, worker_id: String) -> io::Result<()> {
    let mut rx = pipe::Receiver::from_owned_fd(fd)?;
    tokio::spawn(async move {
        let mut chunk = vec![0u8; 8 * 1024];
        let mut line = Vec::with_capacity(STDERR_LINE_MAX);
        let mut cut = false;
        loop {
            let n = match rx.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if !tracing::enabled!(tracing::Level::DEBUG) {
                continue;
            }
            for &byte in &chunk[..n] {
                if byte == b'\n' {
                    log_line(&worker_id, &line, cut);
                    line.clear();
                    cut = false;
                } else if line.len() < STDERR_LINE_MAX {
                    line.push(byte);
                } else {
                    cut = true;
                }
            }
        }
        if !line.is_empty() {
            log_line(&worker_id, &line, cut);
        }
    });
    Ok(())
}

fn log_line(worker_id: &str, line: &[u8], cut: bool) {
    let text = String::from_utf8_lossy(line);
    tracing::debug!(worker_id, line = %text, cut, "worker stderr");
}
