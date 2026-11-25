//! `/healthz` and `/readyz` bodies.

use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::readiness::{self, ReadinessSnapshot, ReadinessStatus};

#[derive(Debug, Serialize, ToSchema)]
pub struct Health {
    #[schema(inline)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Readiness {
    #[schema(inline)]
    pub status: ReadinessStatusDto,
    #[schema(inline)]
    pub pools: Pools,
}

/// - `ready`: taking conversions.
/// - `starting`: workers are still warming up.
/// - `draining`: shutting down; in-flight jobs are finishing.
/// - `degraded`: a pool has no healthy worker.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessStatusDto {
    Ready,
    Starting,
    Draining,
    Degraded,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Pools {
    pub office: PoolStatus,
    pub render: PoolStatus,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PoolStatus {
    /// Configured number of workers.
    pub target: u32,
    /// Warm workers waiting for a job.
    pub idle: u32,
    /// Workers running a job.
    pub busy: u32,
}

impl From<readiness::PoolStatus> for PoolStatus {
    fn from(pool: readiness::PoolStatus) -> Self {
        Self {
            target: pool.target,
            idle: pool.idle,
            busy: pool.busy,
        }
    }
}

impl From<ReadinessSnapshot> for Readiness {
    fn from(snapshot: ReadinessSnapshot) -> Self {
        Self {
            status: match snapshot.status {
                ReadinessStatus::Ready => ReadinessStatusDto::Ready,
                ReadinessStatus::Starting => ReadinessStatusDto::Starting,
                ReadinessStatus::Draining => ReadinessStatusDto::Draining,
                ReadinessStatus::Degraded => ReadinessStatusDto::Degraded,
            },
            pools: Pools {
                office: snapshot.office.into(),
                render: snapshot.render.into(),
            },
        }
    }
}
