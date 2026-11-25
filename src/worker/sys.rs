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
    if let Some(expected) = std::env::var(ENV_SUPERVISOR_PID)
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    {
        if getppid().map(|p| p.as_raw_nonzero().get()) != Some(expected) {
            exit(0);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn die_with_supervisor() -> io::Result<()> {
    Ok(())
}

fn ignore_sigxfsz() {
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
