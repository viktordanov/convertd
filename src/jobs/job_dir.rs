//! `${TMP_DIR}/job-<32 hex>/`, mode 0700. The supervisor creates and
//! deletes it; workers never delete it (INTERFACES.md §6).
//!
//! Shared as `Arc<JobDir>`: the handler, every blocking task (the upload
//! sink, the response pump) and the response body each hold a clone, so
//! the directory is removed only when the last of them is done. hyper
//! drops the handler future on peer EOF mid-request, while a blocking task
//! may still be writing.

use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::disk_budget::{DiskBudget, JobBudget};
use crate::domain::conversion_service::JobPaths;
use crate::domain::document::Format;

/// The prefix of every job directory; `sweep_stale` removes only these.
pub const JOB_PREFIX: &str = "job-";

/// How long removal waits before its one retry.
const RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub struct JobDir {
    root: PathBuf,
    budget: JobBudget,
    removed: AtomicBool,
}

impl JobDir {
    /// Creates a fresh job directory under `tmp_root`. Its name is random,
    /// never the client's request ID.
    pub fn create(tmp_root: &Path, budget: &DiskBudget) -> io::Result<Arc<Self>> {
        let name = format!("{JOB_PREFIX}{}", uuid::Uuid::new_v4().simple());
        let root = tmp_root.join(name);
        DirBuilder::new().mode(0o700).create(&root)?;
        Ok(Arc::new(Self {
            root,
            budget: budget.job(),
            removed: AtomicBool::new(false),
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This job's share of `TMP_MAX_BYTES`, returned when the job drops.
    pub fn budget(&self) -> &JobBudget {
        &self.budget
    }

    /// Where the upload is written before detection.
    pub fn upload_part(&self) -> PathBuf {
        self.root.join("upload.part")
    }

    /// `source.<ext>`, with the detected format's canonical extension.
    pub fn source_path(&self, format: Format) -> PathBuf {
        self.root.join(format!("source.{}", format.extension()))
    }

    pub fn pdf_path(&self) -> PathBuf {
        self.root.join("document.pdf")
    }

    pub fn pages_dir(&self) -> PathBuf {
        self.root.join("pages")
    }

    pub fn paths(&self, format: Format) -> JobPaths {
        JobPaths {
            source: self.source_path(format),
            document_pdf: self.pdf_path(),
            pages_dir: self.pages_dir(),
        }
    }

    /// Gives back the part of this job's disk reservation that no file
    /// uses. Call after the last engine call, when nothing writes here any
    /// more: the reservations cover what each call may write (the render's
    /// is `max_output_bytes`, 768 MiB by default), but the response only
    /// needs what is on disk while it streams. Sizes come from this
    /// process's own `fstat` of every file under the job directory, never
    /// from an engine. Returns the bytes on disk. Blocks briefly.
    pub fn settle_budget(&self) -> io::Result<u64> {
        let used = disk_usage(&self.root, 2)?;
        self.budget
            .release(self.budget.reserved().saturating_sub(used));
        Ok(used)
    }

    /// Removes the directory now. Call on a thread that may block, such as
    /// the response pump's at its end.
    pub fn remove_now(&self) {
        if !self.removed.swap(true, Ordering::AcqRel) {
            remove_with_retry(&self.root);
        }
    }
}

impl Drop for JobDir {
    fn drop(&mut self) {
        if *self.removed.get_mut() {
            return;
        }
        // Error paths only: off the runtime threads when a runtime exists,
        // inline otherwise (tests, shutdown).
        let root = std::mem::take(&mut self.root);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || remove_with_retry(&root));
            }
            Err(_) => remove_with_retry(&root),
        }
    }
}

/// The sizes of the files under `dir`, `depth` levels deep at most (the
/// job directory and `pages/`). Symbolic links are not followed.
fn disk_usage(dir: &Path, depth: u8) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() && depth > 1 {
            total = total.saturating_add(disk_usage(&entry.path(), depth - 1)?);
        } else if kind.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

/// `remove_dir_all`; on `ENOTEMPTY` (a killed worker's last write landed
/// after the listing) waits 50 ms and tries once more; then logs and
/// leaves the directory for the start-up sweep.
pub fn remove_with_retry(root: &Path) {
    remove_with_retry_using(root, |path| std::fs::remove_dir_all(path));
}

/// [`remove_with_retry`] with the removal injected, for tests.
pub fn remove_with_retry_using(
    root: &Path,
    mut remove: impl FnMut(&Path) -> io::Result<()>,
) -> bool {
    match remove(root) {
        Ok(()) => return true,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return true,
        Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
        Err(err) => {
            tracing::warn!(path = %root.display(), error = %err, "failed to remove job directory");
            return false;
        }
    }
    std::thread::sleep(RETRY_DELAY);
    match remove(root) {
        Ok(()) => true,
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(err) => {
            tracing::warn!(path = %root.display(), error = %err, "failed to remove job directory; left for the start-up sweep");
            false
        }
    }
}

/// The lock file that marks a `TMP_DIR` as in use.
pub const LOCK_FILE: &str = ".lock";

/// Takes the exclusive advisory lock on `$TMP_DIR/.lock`, creating the
/// file if needed. Hold the returned file for the life of the process: the
/// lock goes with it. `Err` names why, including when another process holds
/// it, so two servers never sweep each other's job directories or worker
/// slots.
pub fn lock_tmp_dir(tmp_root: &Path) -> Result<std::fs::File, String> {
    let path = tmp_root.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "TMP_DIR {} is in use by another convertd serve (it holds {})",
            tmp_root.display(),
            path.display()
        )),
        Err(std::fs::TryLockError::Error(err)) => {
            Err(format!("failed to lock {}: {err}", path.display()))
        }
    }
}

/// At start-up: removes the `job-*` children of `tmp_root` that a crash
/// left behind, and nothing else. Returns how many it removed.
pub fn sweep_stale(tmp_root: &Path) -> io::Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(tmp_root)? {
        let entry = entry?;
        let is_job = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(JOB_PREFIX));
        if is_job
            && entry.file_type()?.is_dir()
            && remove_with_retry_using(&entry.path(), |p| std::fs::remove_dir_all(p))
        {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("convertd-jobdir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn layout_and_mode() {
        let tmp = scratch("layout");
        let budget = DiskBudget::new(1 << 20);
        let job = JobDir::create(&tmp, &budget).unwrap();
        let name = job.root().file_name().unwrap().to_str().unwrap().to_owned();
        assert!(name.starts_with("job-") && name.len() == 36, "{name}");
        let mode = std::fs::metadata(job.root()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert_eq!(job.upload_part(), job.root().join("upload.part"));
        assert_eq!(job.source_path(Format::Jpeg), job.root().join("source.jpg"));
        assert_eq!(
            job.paths(Format::Docx).document_pdf,
            job.root().join("document.pdf")
        );
        assert_eq!(job.pages_dir(), job.root().join("pages"));
        let root = job.root().to_path_buf();
        assert!(job.budget().try_reserve(100));
        assert_eq!(budget.used(), 100);
        drop(job);
        assert!(!root.exists(), "removed inline without a runtime");
        assert_eq!(budget.used(), 0);
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn the_tmp_dir_lock_is_exclusive() {
        let tmp = scratch("lock");
        let held = lock_tmp_dir(&tmp).unwrap();
        let err = lock_tmp_dir(&tmp).unwrap_err();
        assert!(err.contains("in use by another convertd serve"), "{err}");
        drop(held);
        assert!(
            lock_tmp_dir(&tmp).is_ok(),
            "free again after the holder ends"
        );
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn settle_keeps_what_is_on_disk() {
        let tmp = scratch("settle");
        let budget = DiskBudget::new(1 << 20);
        let job = JobDir::create(&tmp, &budget).unwrap();
        assert!(job.budget().try_reserve(500_000));
        std::fs::write(job.pdf_path(), vec![0u8; 1000]).unwrap();
        std::fs::create_dir(job.pages_dir()).unwrap();
        std::fs::write(job.pages_dir().join("0001.png"), vec![0u8; 300]).unwrap();
        assert_eq!(job.settle_budget().unwrap(), 1300);
        assert_eq!(job.budget().reserved(), 1300);
        assert_eq!(budget.used(), 1300);
        // Never more than was reserved.
        std::fs::write(job.pdf_path(), vec![0u8; 900_000]).unwrap();
        assert_eq!(job.settle_budget().unwrap(), 900_300);
        assert_eq!(budget.used(), 1300);
        drop(job);
        assert_eq!(budget.used(), 0);
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn remove_now_then_drop() {
        let tmp = scratch("remove-now");
        let job = JobDir::create(&tmp, &DiskBudget::new(1)).unwrap();
        std::fs::write(job.upload_part(), b"x").unwrap();
        job.remove_now();
        assert!(!job.root().exists());
        drop(job);
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn retries_once_on_enotempty() {
        let mut calls = 0;
        let ok = remove_with_retry_using(Path::new("/x"), |_| {
            calls += 1;
            if calls == 1 {
                Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty))
            } else {
                Ok(())
            }
        });
        assert!(ok);
        assert_eq!(calls, 2);

        let mut calls = 0;
        let ok = remove_with_retry_using(Path::new("/x"), |_| {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty))
        });
        assert!(!ok);
        assert_eq!(calls, 2, "one retry, no more");

        let mut calls = 0;
        let ok = remove_with_retry_using(Path::new("/x"), |_| {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert!(!ok);
        assert_eq!(calls, 1, "no retry for other errors");
    }

    #[test]
    fn sweep_removes_only_job_directories() {
        let tmp = scratch("sweep");
        for dir in ["job-aaa", "job-bbb/pages", "workers/office-0", "keep"] {
            std::fs::create_dir_all(tmp.join(dir)).unwrap();
        }
        std::fs::write(tmp.join("job-file"), b"not a directory").unwrap();
        std::fs::write(tmp.join("job-bbb/pages/0001.jpg"), b"x").unwrap();
        assert_eq!(sweep_stale(&tmp).unwrap(), 2);
        let mut left: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["job-file", "keep", "workers"]);
        std::fs::remove_dir_all(tmp).unwrap();
    }
}
