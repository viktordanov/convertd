//! Process set-up for a worker: descriptor moves, signals, `_exit`, RSS.
//!
//! The `unsafe` FFI calls of the worker live here (AGENT-RULES.md rule 13).

use std::fs::File;
use std::io;
use std::os::fd::AsFd;

/// The supervisor's PID, set by the spawner, to close the parent-death race.
pub const ENV_SUPERVISOR_PID: &str = "CONVERTD_SUPERVISOR_PID";
/// `<pool>-<slot>`, set by the spawner, for logs.
pub const ENV_WORKER_ID: &str = "CONVERTD_WORKER_ID";

/// The protocol streams, moved off fds 0 and 1.
pub struct ProtocolIo {
    pub input: File,
    pub output: File,
}

/// Sets the process up before any engine loads:
///
/// - the parent-death signal (Linux) and the check that the supervisor is
///   still the parent;
/// - `SIGXFSZ` ignored, so a write past `RLIMIT_FSIZE` fails with `EFBIG`
///   instead of killing the worker;
/// - the protocol pipes moved to new close-on-exec descriptors, fd 1
///   pointed at stderr (engines print to stdout) and fd 0 at `/dev/null`.
pub fn setup() -> io::Result<ProtocolIo> {
    die_with_supervisor()?;
    ignore_sigxfsz();
    let input = rustix::io::fcntl_dupfd_cloexec(io::stdin().as_fd(), 3)?;
    let output = rustix::io::fcntl_dupfd_cloexec(io::stdout().as_fd(), 3)?;
    rustix::stdio::dup2_stdout(io::stderr().as_fd())?;
    let null = File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(null.as_fd())?;
    Ok(ProtocolIo {
        input: File::from(input),
        output: File::from(output),
    })
}

/// Linux: SIGKILL when the spawning thread (the supervisor's long-lived
/// `convertd-spawner`) dies. Elsewhere only pipe EOF ends a worker.
#[cfg(target_os = "linux")]
fn die_with_supervisor() -> io::Result<()> {
    use rustix::process::{Signal, getppid, set_parent_process_death_signal};

    set_parent_process_death_signal(Some(Signal::KILL))?;
    // The supervisor may have died before the signal was armed.
    let expected = std::env::var(ENV_SUPERVISOR_PID)
        .ok()
        .and_then(|v| v.parse::<i32>().ok());
    if let Some(expected) = expected
        && getppid().map(|p| p.as_raw_nonzero().get()) != Some(expected)
    {
        exit(0);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn die_with_supervisor() -> io::Result<()> {
    Ok(())
}

/// Sets SIGXFSZ to ignored. LibreOffice installs its own handler for it
/// at start-up, so the office worker calls this again after `Office::new`.
pub fn ignore_sigxfsz() {
    // SAFETY: SIG_IGN is a valid disposition and SIGXFSZ a valid signal; no
    // handler code runs.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
}

/// Leaves without unwinding, flushing or running destructors (rule 9: an
/// `Office` must never be dropped).
pub fn exit(code: i32) -> ! {
    // SAFETY: `_exit` takes any status and does not return.
    unsafe { libc::_exit(code) }
}

/// A panic writes its message to stderr and ends the process with 101,
/// with no unwinding through engine state.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("convertd worker panicked: {info}");
        exit(101);
    }));
}

/// Reads this process's resident set size after each call; 0 where it
/// cannot be read.
pub struct RssReader {
    #[cfg(target_os = "linux")]
    statm: Option<File>,
}

impl Default for RssReader {
    fn default() -> Self {
        Self::new()
    }
}

impl RssReader {
    #[cfg(target_os = "linux")]
    pub fn new() -> Self {
        Self {
            statm: File::open("/proc/self/statm").ok(),
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new() -> Self {
        Self {}
    }

    /// Linux: `/proc/self/statm`, kept open and read from offset 0.
    #[cfg(target_os = "linux")]
    pub fn read(&mut self) -> u64 {
        use std::os::unix::fs::FileExt;

        let Some(statm) = &self.statm else {
            return 0;
        };
        let mut buf = [0u8; 128];
        let Ok(n) = statm.read_at(&mut buf, 0) else {
            return 0;
        };
        // size resident shared text lib data dt, in pages.
        let pages: u64 = std::str::from_utf8(&buf[..n])
            .ok()
            .and_then(|s| s.split_ascii_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        pages * page_size()
    }

    /// macOS, for host tests: the task's resident size.
    #[cfg(target_os = "macos")]
    pub fn read(&mut self) -> u64 {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: the buffer is a `proc_taskinfo` of `size` bytes, which is
        // what PROC_PIDTASKINFO writes; it is read only if all of it was written.
        let written = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return 0;
        }
        // SAFETY: `proc_pidinfo` filled the whole struct.
        unsafe { info.assume_init() }.pti_resident_size
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn read(&mut self) -> u64 {
        0
    }
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).unwrap_or(4096)
}

/// Free bytes for unprivileged users on the file system that holds `path`,
/// or `None` when `statvfs` fails.
pub fn free_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c_path` is a valid NUL-terminated string and `stat` points to
    // a `statvfs` that the call fills; it is read only on success.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: `statvfs` returned 0, so it filled the struct.
    let stat = unsafe { stat.assume_init() };
    #[allow(clippy::unnecessary_cast)] // the field types differ by platform
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// Caps the size of any file this process writes (`RLIMIT_FSIZE`), or lifts
/// the cap with `None`. With SIGXFSZ ignored, a write past the cap fails
/// with `EFBIG`.
pub fn set_file_size_limit(limit: Option<u64>) -> io::Result<()> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    let hard = getrlimit(Resource::Fsize).maximum;
    let current = match (limit, hard) {
        (Some(limit), Some(hard)) => Some(limit.min(hard)),
        (Some(limit), None) => Some(limit),
        (None, hard) => hard,
    };
    setrlimit(
        Resource::Fsize,
        Rlimit {
            current,
            maximum: hard,
        },
    )
    .map_err(io::Error::from)
}

/// Set by the SIGXFSZ handler: a write passed `RLIMIT_FSIZE`.
static FILE_SIZE_HIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_sigxfsz(_: libc::c_int) {
    // Async-signal-safe: one atomic store.
    FILE_SIZE_HIT.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Catches SIGXFSZ with a handler that records the hit, so the write that
/// passed `RLIMIT_FSIZE` fails with `EFBIG` and [`take_file_size_hit`]
/// tells the cap apart from other export errors. The office worker calls
/// it after `Office::new`, which installs a SIGXFSZ handler of its own that
/// crashes the process.
pub fn catch_sigxfsz() -> io::Result<()> {
    // SAFETY: `action` is zeroed and then filled; the handler only stores
    // to an atomic, which is async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigxfsz as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        if libc::sigaction(libc::SIGXFSZ, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Whether a write passed `RLIMIT_FSIZE` since the last call.
pub fn take_file_size_hit() -> bool {
    FILE_SIZE_HIT.swap(false, std::sync::atomic::Ordering::SeqCst)
}
