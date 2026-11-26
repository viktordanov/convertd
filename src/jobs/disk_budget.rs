//! `TMP_MAX_BYTES`: a reservation budget for everything written under
//! `TMP_DIR`, so an `emptyDir` overflow never evicts the pod.
//!
//! Every byte a job may write is reserved first. Per job:
//!
//! - upload: `Content-Length` (or `MAX_UPLOAD_BYTES`), trued up to the
//!   real size afterwards;
//! - office PDF: `min(cap, max(64 MiB, 8 × source))`, plus source + 1 MiB
//!   for a DOCX, DOCM or DOTX with `DEFAULT_PAPER=A4` (the worker's copy
//!   with page sizes);
//! - decrypt: source size + 1 MiB;
//! - pages: the cap, `min(TMP_MAX_BYTES / 4, 4 GiB − 1 MiB)`.
//!
//! A failed reservation is 503 (other jobs hold the disk); reaching
//! `max_output_bytes` is the engine's 422 `LIMIT_EXCEEDED`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::domain::conversion_service::OutputBudget;

/// The service-wide budget. Cheap to clone.
#[derive(Debug, Clone)]
pub struct DiskBudget {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    used: AtomicU64,
    max: u64,
}

impl DiskBudget {
    pub fn new(max: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                used: AtomicU64::new(0),
                max,
            }),
        }
    }

    pub fn used(&self) -> u64 {
        self.inner.used.load(Ordering::Relaxed)
    }

    pub fn max(&self) -> u64 {
        self.inner.max
    }

    /// A job's share, empty at first; everything it reserves is returned
    /// when it drops.
    pub fn job(&self) -> JobBudget {
        JobBudget {
            budget: self.clone(),
            reserved: AtomicU64::new(0),
        }
    }

    fn try_take(&self, bytes: u64) -> bool {
        let max = self.inner.max;
        self.inner
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|&total| total <= max)
            })
            .is_ok()
    }

    fn give_back(&self, bytes: u64) {
        self.inner.used.fetch_sub(bytes, Ordering::AcqRel);
    }
}

/// One job's reservations.
#[derive(Debug)]
pub struct JobBudget {
    budget: DiskBudget,
    reserved: AtomicU64,
}

impl JobBudget {
    /// Reserves `bytes` more; `false` when the budget is spent.
    pub fn try_reserve(&self, bytes: u64) -> bool {
        if !self.budget.try_take(bytes) {
            return false;
        }
        self.reserved.fetch_add(bytes, Ordering::AcqRel);
        true
    }

    /// Returns up to `bytes` of this job's reservation early, for example
    /// when an upload turns out smaller than its `Content-Length`.
    pub fn release(&self, bytes: u64) {
        let released = self
            .reserved
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                Some(held - bytes.min(held))
            })
            .map_or(0, |held| bytes.min(held));
        self.budget.give_back(released);
    }

    pub fn reserved(&self) -> u64 {
        self.reserved.load(Ordering::Acquire)
    }
}

impl Drop for JobBudget {
    fn drop(&mut self) {
        self.budget.give_back(*self.reserved.get_mut());
    }
}

impl OutputBudget for JobBudget {
    fn reserve(&self, bytes: u64) -> bool {
        self.try_reserve(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_bounded_and_returned() {
        let budget = DiskBudget::new(100);
        let a = budget.job();
        let b = budget.job();
        assert!(a.try_reserve(60));
        assert!(!b.try_reserve(41));
        assert!(b.try_reserve(40));
        assert_eq!(budget.used(), 100);
        assert!(!a.try_reserve(1));
        a.release(50);
        assert_eq!((a.reserved(), budget.used()), (10, 50));
        a.release(1000);
        assert_eq!((a.reserved(), budget.used()), (0, 40));
        drop(b);
        assert_eq!(budget.used(), 0);
        assert!(!budget.job().try_reserve(101));
        assert!(!budget.job().try_reserve(u64::MAX));
    }

    #[test]
    fn dropping_a_job_returns_everything() {
        let budget = DiskBudget::new(1000);
        {
            let job = budget.job();
            assert!(job.try_reserve(300));
            assert!(OutputBudget::reserve(&job, 200));
            assert_eq!(budget.used(), 500);
        }
        assert_eq!(budget.used(), 0);
    }
}
