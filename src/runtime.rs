//! Tokio runtime builders. Only `serve` builds a runtime;
//! `worker`, `openapi` and `healthcheck` run without one.

use std::io;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};

/// Runtime worker threads for `serve` unless `TOKIO_WORKER_THREADS` is set.
/// The cores belong to the engine workers, which are separate processes.
pub const SERVE_WORKER_THREADS: usize = 2;

/// The blocking pool cap. `MAX_CONCURRENT_REQUESTS` (16) keeps it from queueing.
pub const MAX_BLOCKING_THREADS: usize = 32;

/// Idle blocking threads exit after this, so idle RSS returns to baseline.
pub const THREAD_KEEP_ALIVE: Duration = Duration::from_secs(10);

/// The multi-thread runtime for `serve`. `worker_threads` is
/// `TOKIO_WORKER_THREADS`, parsed by `config`.
pub fn serve(worker_threads: Option<usize>) -> io::Result<Runtime> {
    Builder::new_multi_thread()
        .worker_threads(worker_threads.unwrap_or(SERVE_WORKER_THREADS))
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .thread_keep_alive(THREAD_KEEP_ALIVE)
        .thread_name("convertd")
        .enable_all()
        .build()
}
