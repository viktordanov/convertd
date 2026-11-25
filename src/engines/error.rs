//! The engine failures of INTERFACES.md §5, and the mapping from pool and
//! worker errors onto them.
//!
//! The variants mirror the domain's `EngineError`; step 9 converts one
//! into the other. Messages are fixed texts: engine messages can hold
//! paths, so they go to the log only.

use crate::engines::pool::{CallError, LeaseError};
use crate::ipc::{WorkerError, WorkerErrorKind};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineFailure {
    #[error("the queue is full")]
    QueueFull,
    #[error("the queue wait ran out")]
    QueueTimeout,
    #[error("no worker is available")]
    Unavailable,
    /// ENOSPC, EDQUOT or almost no free space: other jobs filled the disk.
    #[error("the temp disk is full")]
    DiskFull,
    #[error("the conversion timed out")]
    Timeout,
    #[error("the document needs a password")]
    PasswordRequired,
    #[error("the password is wrong")]
    InvalidPassword,
    /// This job's own cap.
    #[error("{0}")]
    LimitExceeded(String),
    #[error("{0}")]
    Failed(String),
    #[error("the engine crashed")]
    Crashed,
    #[error("the document failed before")]
    Poisoned,
    #[error("{0}")]
    Internal(String),
}

impl From<LeaseError> for EngineFailure {
    fn from(err: LeaseError) -> Self {
        match err {
            LeaseError::QueueFull => EngineFailure::QueueFull,
            LeaseError::QueueTimeout => EngineFailure::QueueTimeout,
            LeaseError::DeadlineExpired => EngineFailure::Timeout,
            LeaseError::Unavailable => EngineFailure::Unavailable,
        }
    }
}

impl From<CallError> for EngineFailure {
    fn from(err: CallError) -> Self {
        match err {
            CallError::Timeout => EngineFailure::Timeout,
            // A worker that breaks the protocol was killed like a crashed one.
            CallError::Crashed { .. } | CallError::Protocol(_) => EngineFailure::Crashed,
        }
    }
}

impl From<WorkerError> for EngineFailure {
    fn from(err: WorkerError) -> Self {
        match err.kind {
            WorkerErrorKind::PasswordRequired => EngineFailure::PasswordRequired,
            WorkerErrorKind::InvalidPassword => EngineFailure::InvalidPassword,
            WorkerErrorKind::LoadFailed => EngineFailure::Failed("failed to load document".into()),
            WorkerErrorKind::ExportFailed => {
                EngineFailure::Failed("failed to export document".into())
            }
            WorkerErrorKind::EmptyDocument => {
                EngineFailure::Failed("the document has no pages".into())
            }
            WorkerErrorKind::LimitExceeded => {
                EngineFailure::LimitExceeded("output exceeds limit".into())
            }
            WorkerErrorKind::DiskFull => EngineFailure::DiskFull,
            WorkerErrorKind::Io => EngineFailure::Internal("failed to access job files".into()),
            WorkerErrorKind::Unsupported => {
                EngineFailure::Internal("the worker does not support the operation".into())
            }
            WorkerErrorKind::Internal => EngineFailure::Internal("the engine failed".into()),
            WorkerErrorKind::FakeQueueFull => EngineFailure::QueueFull,
            WorkerErrorKind::FakeUnavailable => EngineFailure::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_errors() {
        use EngineFailure as F;
        assert_eq!(F::from(LeaseError::QueueFull), F::QueueFull);
        assert_eq!(F::from(LeaseError::QueueTimeout), F::QueueTimeout);
        assert_eq!(F::from(LeaseError::DeadlineExpired), F::Timeout);
        assert_eq!(F::from(LeaseError::Unavailable), F::Unavailable);
    }

    #[test]
    fn call_errors() {
        use EngineFailure as F;
        assert_eq!(F::from(CallError::Timeout), F::Timeout);
        assert_eq!(F::from(CallError::Crashed { status: None }), F::Crashed);
        assert_eq!(F::from(CallError::Protocol("x".into())), F::Crashed);
    }

    #[test]
    fn worker_errors_keep_disk_full_apart_from_the_jobs_own_cap() {
        use EngineFailure as F;
        let map = |kind| F::from(WorkerError::new(kind, "/secret/path/source.docx"));
        assert_eq!(map(WorkerErrorKind::DiskFull), F::DiskFull);
        assert!(matches!(
            map(WorkerErrorKind::LimitExceeded),
            F::LimitExceeded(_)
        ));
        assert_eq!(map(WorkerErrorKind::PasswordRequired), F::PasswordRequired);
        assert_eq!(map(WorkerErrorKind::InvalidPassword), F::InvalidPassword);
        for kind in [
            WorkerErrorKind::LoadFailed,
            WorkerErrorKind::ExportFailed,
            WorkerErrorKind::EmptyDocument,
        ] {
            assert!(matches!(map(kind), F::Failed(_)), "{kind:?}");
        }
        for kind in [
            WorkerErrorKind::Io,
            WorkerErrorKind::Unsupported,
            WorkerErrorKind::Internal,
        ] {
            assert!(matches!(map(kind), F::Internal(_)), "{kind:?}");
        }
        assert_eq!(map(WorkerErrorKind::FakeQueueFull), F::QueueFull);
        assert_eq!(map(WorkerErrorKind::FakeUnavailable), F::Unavailable);
        // No worker message reaches the failure text.
        for kind in [
            WorkerErrorKind::LoadFailed,
            WorkerErrorKind::LimitExceeded,
            WorkerErrorKind::Io,
        ] {
            assert!(!map(kind).to_string().contains("secret"));
        }
    }
}
