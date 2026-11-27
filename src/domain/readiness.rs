//! Whether the engines can take work, for `/readyz`. Kept off the engine
//! traits: the engines report it per pool (`Engines::readiness()`).

/// The spec's `Readiness.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessStatus {
    Ready,
    Starting,
    Draining,
    Degraded,
}

impl ReadinessStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadinessStatus::Ready => "ready",
            ReadinessStatus::Starting => "starting",
            ReadinessStatus::Draining => "draining",
            ReadinessStatus::Degraded => "degraded",
        }
    }
}

/// One pool's worker counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStatus {
    pub target: u32,
    pub idle: u32,
    pub busy: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadinessSnapshot {
    pub status: ReadinessStatus,
    pub office: PoolStatus,
    pub render: PoolStatus,
}

/// A cheap, non-blocking snapshot of the engines.
pub trait Readiness: Send + Sync {
    fn readiness(&self) -> ReadinessSnapshot;
}
