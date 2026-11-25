//! Level-A fake engines (lane 001): `FakeOffice` and `FakePdf` behind the
//! domain traits, selected with `TEST_ENGINE=fake` in `test-hooks` builds.
//! Also [`Unwired`], which stands in until lane 002's engines are wired.

use std::time::Instant;

use async_trait::async_trait;

use crate::domain::engine::{
    Admission, AssembleJob, Assembled, Converted, DecryptJob, Decrypted, EngineError, InspectJob,
    Inspection, OfficeEngine, OfficeJob, PdfEngine, RenderJob, Rendered,
};
use crate::domain::readiness::{PoolStatus, Readiness, ReadinessSnapshot, ReadinessStatus};

/// No engine: every call is `Unavailable` (503) and readiness stays
/// `starting`. `serve` uses it while lane 002's engines are not wired
/// (plan 001 step 14), so the probes still answer.
pub struct Unwired;

#[async_trait]
impl OfficeEngine for Unwired {
    async fn convert(
        &self,
        _: OfficeJob,
        _: Admission,
        _: Instant,
    ) -> Result<Converted, EngineError> {
        Err(EngineError::Unavailable)
    }
}

#[async_trait]
impl PdfEngine for Unwired {
    async fn inspect(
        &self,
        _: InspectJob,
        _: Admission,
        _: Instant,
    ) -> Result<Inspection, EngineError> {
        Err(EngineError::Unavailable)
    }
    async fn decrypt(
        &self,
        _: DecryptJob,
        _: Admission,
        _: Instant,
    ) -> Result<Decrypted, EngineError> {
        Err(EngineError::Unavailable)
    }
    async fn render(
        &self,
        _: RenderJob,
        _: Admission,
        _: Instant,
    ) -> Result<Rendered, EngineError> {
        Err(EngineError::Unavailable)
    }
    async fn assemble(
        &self,
        _: AssembleJob,
        _: Admission,
        _: Instant,
    ) -> Result<Assembled, EngineError> {
        Err(EngineError::Unavailable)
    }
}

impl Readiness for Unwired {
    fn readiness(&self) -> ReadinessSnapshot {
        ReadinessSnapshot {
            status: ReadinessStatus::Starting,
            office: PoolStatus::default(),
            render: PoolStatus::default(),
        }
    }
}
