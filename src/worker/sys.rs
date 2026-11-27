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

/// Reads this process's private resident memory after each call; 0 where it
/// cannot be read. On Linux that is [`private_rss`]: what the worker holds
/// alone, without the shared library pages that every worker maps.
pub struct RssReader {
    #[cfg(target_os = "linux")]
    status: Option<File>,
    /// The last read of `status`, reused so a read allocates nothing.
    #[cfg(target_os = "linux")]
    buf: Vec<u8>,
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
            status: File::open("/proc/self/status").ok(),
            buf: Vec::with_capacity(4096),
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new() -> Self {
        Self {}
    }

    /// Linux: `/proc/self/status`, kept open and read whole from offset 0. It
    /// is about 1.5 KiB, but a long `Groups:` line can push the memory lines
    /// past any fixed size.
    #[cfg(target_os = "linux")]
    pub fn read(&mut self) -> u64 {
        let Some(status) = &self.status else {
            return 0;
        };
        if read_whole_at(status, &mut self.buf).is_err() {
            return 0;
        }
        private_rss(&self.buf).unwrap_or(0)
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

/// The CPU time of this whole process so far, user plus system, from
/// `getrusage(RUSAGE_SELF)`. It covers every thread, LibreOffice's
/// included, and not the children. Zero if the call fails.
pub fn cpu_time() -> std::time::Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: RUSAGE_SELF is a valid target, and the buffer is a `rusage`
    // that the call fills; it is read only when the call succeeds.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return std::time::Duration::ZERO;
    }
    // SAFETY: `getrusage` succeeded and filled the whole struct.
    let usage = unsafe { usage.assume_init() };
    timeval(usage.ru_utime).saturating_add(timeval(usage.ru_stime))
}

/// A `timeval` as a `Duration`; a negative field counts as zero.
fn timeval(tv: libc::timeval) -> std::time::Duration {
    let secs = u64::try_from(tv.tv_sec).unwrap_or(0);
    let micros = u64::try_from(tv.tv_usec).unwrap_or(0);
    std::time::Duration::from_secs(secs).saturating_add(std::time::Duration::from_micros(micros))
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

/// The private resident memory of process `pid` ([`private_rss`]), for the
/// supervisor's watchdog; `None` where it cannot be read (the process is gone,
/// or the platform has no reader).
#[cfg(target_os = "linux")]
pub fn rss_of(pid: u32) -> Option<u64> {
    private_rss(&std::fs::read(format!("/proc/{pid}/status")).ok()?)
}

/// Reads `file` from offset 0 to its end into `buf`, at most 1 MiB. For a
/// procfs file, which has no size to ask for and must be read from the start.
#[cfg(any(target_os = "linux", all(unix, test)))]
fn read_whole_at(file: &File, buf: &mut Vec<u8>) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;

    const MAX: usize = 1 << 20;
    buf.clear();
    loop {
        let start = buf.len();
        if start >= MAX {
            return Err(std::io::ErrorKind::FileTooLarge.into());
        }
        buf.resize(start + 4096, 0);
        let n = file.read_at(&mut buf[start..], start as u64)?;
        buf.truncate(start + n);
        if n == 0 {
            return Ok(());
        }
    }
}

/// The private resident memory in a `/proc/<pid>/status` text, in bytes:
/// `RssAnon` + `RssShmem`. `RssFile`, the file pages such as LibreOffice's
/// shared libraries, is left out: every worker maps the same pages, the
/// kernel can drop them, and they are about 156 MiB of a warm office
/// worker's 248 MiB `VmRSS` (2026-11-27). `RssShmem` stays in, because the
/// kernel cannot drop it. A kernel without `RssAnon` (before 4.5, or a
/// sandbox that leaves it out) gives `VmRSS`, as before. `None` when neither
/// is there. The text is bytes: the `Name:` line need not be UTF-8, and the
/// memory lines are ASCII.
pub fn private_rss(status: &[u8]) -> Option<u64> {
    let (mut anon, mut shmem, mut vm) = (None, None, None);
    for line in status.split(|&b| b == b'\n') {
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let (key, value) = (&line[..colon], &line[colon + 1..]);
        let kib = value
            .trim_ascii()
            .strip_suffix(b"kB")
            .and_then(|v| std::str::from_utf8(v.trim_ascii()).ok())
            .and_then(|v| v.parse::<u64>().ok());
        match key {
            b"RssAnon" => anon = kib,
            b"RssShmem" => shmem = kib,
            b"VmRSS" => vm = kib,
            _ => {}
        }
    }
    let kib = match anon {
        Some(anon) => anon.saturating_add(shmem.unwrap_or(0)),
        None => vm?,
    };
    Some(kib.saturating_mul(1024))
}

/// macOS, for host tests: the task's resident size.
#[cfg(target_os = "macos")]
pub fn rss_of(pid: u32) -> Option<u64> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: the buffer is a `proc_taskinfo` of `size` bytes, which is what
    // PROC_PIDTASKINFO writes; it is read only if all of it was written.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: `proc_pidinfo` filled the whole struct.
    Some(unsafe { info.assume_init() }.pti_resident_size)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn rss_of(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{cpu_time, private_rss, read_whole_at};

    /// The status of a warm office worker in the runtime image (2026-11-27),
    /// cut to the memory lines.
    const OFFICE_STATUS: &str = "Name:\tconvertd\nUmask:\t0022\nState:\tS (sleeping)\n\
        VmPeak:\t 1402916 kB\nVmHWM:\t  253616 kB\nVmRSS:\t  253616 kB\n\
        RssAnon:\t   93488 kB\nRssFile:\t  160128 kB\nRssShmem:\t       0 kB\n\
        VmData:\t  210940 kB\nThreads:\t9\n";

    #[test]
    fn private_rss_counts_anon_and_shmem_but_not_file_pages() {
        assert_eq!(private_rss(OFFICE_STATUS.as_bytes()), Some(93488 * 1024));
        let shmem = OFFICE_STATUS.replace("RssShmem:\t       0 kB", "RssShmem:\t    2048 kB");
        assert_eq!(private_rss(shmem.as_bytes()), Some((93488 + 2048) * 1024));
    }

    #[test]
    fn private_rss_falls_back_to_vm_rss_without_rss_anon() {
        let old = b"Name:\tconvertd\nVmRSS:\t  253616 kB\n";
        assert_eq!(private_rss(old), Some(253616 * 1024));
        assert_eq!(private_rss(b"Name:\tconvertd\n"), None);
        assert_eq!(
            private_rss(b"RssAnon:\t lots kB\nVmRSS:\t 5 kB\n"),
            Some(5 * 1024)
        );
        assert_eq!(private_rss(b""), None);
    }

    #[test]
    fn private_rss_reads_a_name_that_is_not_utf8_and_a_long_groups_line() {
        let mut status = b"Name:\tconv\xe2\x82\nUmask:\t0022\nGroups:\t".to_vec();
        for g in 0..3000 {
            status.extend_from_slice(format!("{} ", 100000 + g).as_bytes());
        }
        status.extend_from_slice(
            b"\nVmRSS:\t  253616 kB\nRssAnon:\t   93488 kB\nRssShmem:\t  0 kB\n",
        );
        assert!(status.len() > 16 * 1024);
        assert!(std::str::from_utf8(&status).is_err());
        assert_eq!(private_rss(&status), Some(93488 * 1024));
    }

    #[test]
    fn read_whole_at_reads_past_any_buffer_size_and_again_from_the_start() {
        let path = std::env::temp_dir().join(format!("convertd-status-{}", std::process::id()));
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mut buf = Vec::new();
        read_whole_at(&file, &mut buf).unwrap();
        assert_eq!(buf, data);
        read_whole_at(&file, &mut buf).unwrap();
        assert_eq!(buf, data);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_private_memory_and_less_than_its_vm_rss() {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let private = private_rss(status.as_bytes()).unwrap();
        let vm = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.trim().strip_suffix("kB")?.trim().parse::<u64>().ok())
            .unwrap()
            * 1024;
        assert!(
            private > 0 && private <= vm,
            "private {private}, VmRSS {vm}"
        );
        assert!(super::rss_of(std::process::id()).is_some_and(|rss| rss > 0));
    }

    /// `cargo test` runs tests as threads of one process, and
    /// `RUSAGE_SELF` counts them all. So the test checks only that the
    /// value never goes down and that this thread's own work raises it.
    #[test]
    fn cpu_time_is_monotonic_and_counts_work() {
        let start = cpu_time();
        let mut last = start;
        let spin_until = Instant::now() + Duration::from_millis(50);
        let mut x = 0u64;
        while Instant::now() < spin_until {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
            let now = cpu_time();
            assert!(now >= last, "the CPU time went down: {now:?} < {last:?}");
            last = now;
        }
        assert!(
            last > start,
            "50 ms of spinning added no CPU time: {start:?} to {last:?}"
        );
    }
}
