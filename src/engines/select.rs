//! Which engines `convertd serve` runs, and the handle that stops them (lane 002).
//!
//! `main.rs` calls [`start`] once and [`EngineHandle::shutdown`] after the
//! server stops. Lane 002 replaces the `Unwired` branches with the real pools
//! and with level B (`TEST_ENGINE=worker`); the signature stays.

use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::domain::engine::{OfficeEngine, PdfEngine};
use crate::domain::readiness::Readiness;

use super::fake::Unwired;

/// The engines that `serve` hands to the HTTP layer.
pub struct Selected {
    pub office: Arc<dyn OfficeEngine>,
    pub pdf: Arc<dyn PdfEngine>,
    pub readiness: Arc<dyn Readiness>,
    pub handle: EngineHandle,
}

/// Stops the engines at shutdown. With no pools there is nothing to stop.
pub struct EngineHandle {
    _private: (),
}

impl EngineHandle {
    fn none() -> Self {
        Self { _private: () }
    }

    /// Stops new leases; in-flight calls finish. Called on SIGTERM.
    pub async fn drain(&self) {}

    /// Kills every worker after `grace` at the latest.
    pub async fn shutdown(self, _grace: Duration) {}
}

/// Starts the engines that `config` selects.
pub async fn start(config: &Config) -> Result<Selected, String> {
    #[cfg(feature = "test-hooks")]
    if config.test_engine == Some(crate::config::TestEngine::Fake) {
        use super::fake::{FakeOffice, FakePdf, FakeReadiness};
        tracing::warn!("using the level-A fake engines (TEST_ENGINE=fake)");
        return Ok(Selected {
            office: Arc::new(FakeOffice::from_markers()),
            pdf: Arc::new(FakePdf::from_markers()),
            readiness: Arc::new(FakeReadiness::new(true)),
            handle: EngineHandle::none(),
        });
    }
    if config.test_engine.is_some() {
        tracing::warn!("TEST_ENGINE=worker is not wired yet (lane 002); engines are unavailable");
    } else {
        tracing::warn!("the real engines are not wired yet (lane 002); engines are unavailable");
    }
    let unwired = Arc::new(Unwired);
    Ok(Selected {
        office: unwired.clone(),
        pdf: unwired.clone(),
        readiness: unwired,
        handle: EngineHandle::none(),
    })
}
