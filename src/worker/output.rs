//! Output files a worker writes itself, under the job's byte cap, and the
//! mapping of I/O errors onto `WorkerErrorKind` (INTERFACES.md §5, §15).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::ipc::{WorkerError, WorkerErrorKind};

/// The kind for an I/O error: ENOSPC and EDQUOT are `DiskFull` (other jobs
/// filled the disk; worth a retry), EFBIG from the job's own
/// `RLIMIT_FSIZE` is `LimitExceeded`, anything else is `Io`.
pub fn classify_io(err: &io::Error) -> WorkerErrorKind {
    match err.raw_os_error() {
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => WorkerErrorKind::DiskFull,
        Some(libc::EFBIG) => WorkerErrorKind::LimitExceeded,
        _ => WorkerErrorKind::Io,
    }
}

/// `failed to {what}: {err}`, with the kind from [`classify_io`].
pub fn io_error(what: &str, err: &io::Error) -> WorkerError {
    let kind = classify_io(err);
    let message = match kind {
        WorkerErrorKind::DiskFull => format!("failed to {what}: temp disk full"),
        WorkerErrorKind::LimitExceeded => format!("failed to {what}: output exceeds limit"),
        _ => format!("failed to {what}: {err}"),
    };
    WorkerError::new(kind, message)
}

/// The byte cap of one job, shared by every file it writes. Files written
/// through it are deleted again if the job fails.
#[derive(Debug)]
pub struct OutputBudget {
    limit: u64,
    used: u64,
    written: Vec<PathBuf>,
}

impl OutputBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            used: 0,
            written: Vec::new(),
        }
    }

    pub fn used(&self) -> u64 {
        self.used
    }

    /// Writes `bytes` to `path` if they fit in what is left. An oversized
    /// file is refused before it reaches the disk.
    pub fn write_file(&mut self, path: &Path, bytes: &[u8]) -> Result<u64, WorkerError> {
        let len = bytes.len() as u64;
        if self.used.saturating_add(len) > self.limit {
            return Err(WorkerError::new(
                WorkerErrorKind::LimitExceeded,
                "failed to write output: output exceeds limit",
            ));
        }
        self.written.push(path.to_path_buf());
        let result = File::create(path).and_then(|mut f| f.write_all(bytes));
        result.map_err(|err| io_error("write output", &err))?;
        self.used += len;
        Ok(len)
    }

    /// Deletes every file this job wrote (on failure).
    pub fn remove_written(&mut self) {
        for path in self.written.drain(..) {
            let _ = fs::remove_file(path);
        }
        self.used = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_full_and_own_cap_are_apart() {
        let kind = |code| classify_io(&io::Error::from_raw_os_error(code));
        assert_eq!(kind(libc::ENOSPC), WorkerErrorKind::DiskFull);
        assert_eq!(kind(libc::EDQUOT), WorkerErrorKind::DiskFull);
        assert_eq!(kind(libc::EFBIG), WorkerErrorKind::LimitExceeded);
        assert_eq!(kind(libc::EACCES), WorkerErrorKind::Io);
        assert_eq!(
            classify_io(&io::Error::other("no errno")),
            WorkerErrorKind::Io
        );
    }

    #[test]
    fn io_error_messages_hold_no_paths_for_the_mapped_kinds() {
        let err = io_error("write page", &io::Error::from_raw_os_error(libc::ENOSPC));
        assert_eq!(err.kind, WorkerErrorKind::DiskFull);
        assert_eq!(err.message, "failed to write page: temp disk full");
    }

    #[test]
    fn budget_refuses_before_writing_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("convertd-002-budget-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut budget = OutputBudget::new(10);
        let a = dir.join("a");
        let b = dir.join("b");
        assert_eq!(budget.write_file(&a, b"123456").unwrap(), 6);
        let err = budget.write_file(&b, b"12345").unwrap_err();
        assert_eq!(err.kind, WorkerErrorKind::LimitExceeded);
        assert!(!b.exists(), "an oversized file never reaches the disk");
        budget.remove_written();
        assert!(!a.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
