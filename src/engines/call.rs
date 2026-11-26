//! One engine call through a pool: lease, call, timing, the crash retry and
//! the error mapping, shared by the office and PDF adapters.

use std::time::{Duration, Instant};

use crate::domain::engine::{Admission as DomainAdmission, EngineError, EngineTiming, WorkerRef};
use crate::engines::error::EngineFailure;
use crate::engines::poison::{Poison, TIMEOUT_STRIKES, strikes_for};
use crate::engines::pool::{Admission, CallError, Pool};
use crate::ipc::{Op, Output, Reply, WorkerErrorKind};

/// A crash is retried once, on a fresh lease, only with this much left
/// before the deadline (INTERFACES.md §5).
pub const RETRY_MIN_LEFT: Duration = Duration::from_secs(1);

/// The result of one successful call.
pub struct Called {
    pub output: Output,
    pub reply: Reply,
    pub timing: EngineTiming,
    pub worker: WorkerRef,
}

pub fn admission(adm: DomainAdmission) -> Admission {
    match adm {
        DomainAdmission::Queue => Admission::Queue,
        DomainAdmission::Continue => Admission::Continue,
    }
}

pub fn worker_ref(r: crate::engines::pool::WorkerRef) -> WorkerRef {
    WorkerRef {
        pool: r.pool,
        slot: r.slot,
        generation: r.generation,
        job: r.job,
    }
}

const NO_WORKER: WorkerRef = WorkerRef {
    pool: "",
    slot: 0,
    generation: 0,
    job: 0,
};

/// Leases a worker of `pool` and runs `op`. A crash is retried once on a
/// new lease with `Continue`, if [`RETRY_MIN_LEFT`] is left; a timeout or
/// an engine-reported error is never retried. `recycle` names the
/// engine-reported errors after which the worker is recycled.
pub async fn call(
    pool: &Pool,
    op: Op,
    adm: Admission,
    deadline: Instant,
    recycle: fn(WorkerErrorKind) -> bool,
) -> Result<Called, EngineError> {
    call_with_poison(pool, op, adm, deadline, recycle, None).await
}

/// [`call`] with poison memory: a poisoned document is refused before the
/// lease, a timeout adds one strike, and a crash adds two when it survived
/// the retry (one when there was no time for a retry).
pub async fn call_with_poison(
    pool: &Pool,
    op: Op,
    adm: Admission,
    deadline: Instant,
    recycle: fn(WorkerErrorKind) -> bool,
    poison: Option<&Poison>,
) -> Result<Called, EngineError> {
    if let Some(p) = poison
        && p.list.is_poisoned(&p.key)
    {
        return Err(EngineError::Poisoned);
    }
    let started = Instant::now();
    let mut adm = adm;
    let mut first_queue = None;
    let mut retried = false;
    loop {
        let mut lease = match pool.lease(deadline, adm).await {
            Ok(lease) => lease,
            Err(err) => {
                // A crash whose retry got no worker is a crash without a
                // retry: one strike, as for a render chunk.
                if retried && let Some(p) = poison {
                    p.list.strike(&p.key, TIMEOUT_STRIKES);
                }
                return Err(EngineFailure::from(err).into());
            }
        };
        let queue = *first_queue.get_or_insert(lease.queued());
        let lease_time = lease.leased().saturating_sub(lease.queued());
        let spawned = lease.spawned_on_path();
        match lease.call(op.clone(), deadline).await {
            Ok(reply) => {
                let worker = lease.worker_ref().map(worker_ref).unwrap_or(NO_WORKER);
                let output = match &reply.result {
                    Ok(output) => output.clone(),
                    Err(err) => {
                        if recycle(err.kind) {
                            lease.recycle_after();
                        }
                        tracing::debug!(
                            pool = pool.name(),
                            op = op.name(),
                            kind = ?err.kind,
                            message = %err.message,
                            "engine error"
                        );
                        return Err(EngineFailure::from(err.clone()).into());
                    }
                };
                let timing = EngineTiming {
                    queue,
                    lease: lease_time,
                    spawn: spawned.map(|(spawn, _)| spawn),
                    bind: spawned.map(|(_, bind)| bind),
                    wall: started.elapsed(),
                    engine: Duration::from_micros(reply.engine.engine_us),
                    preflight: Duration::from_micros(reply.engine.preflight_us),
                };
                metrics::histogram!(
                    "convert_engine_overhead_seconds",
                    "pool" => pool.name(),
                    "op" => op.name()
                )
                .record(
                    timing
                        .wall
                        .saturating_sub(timing.engine + queue)
                        .as_secs_f64(),
                );
                return Ok(Called {
                    output,
                    reply,
                    timing,
                    worker,
                });
            }
            Err(CallError::Crashed { .. })
                if !retried
                    && deadline.saturating_duration_since(Instant::now()) >= RETRY_MIN_LEFT =>
            {
                tracing::warn!(
                    pool = pool.name(),
                    op = op.name(),
                    "engine crashed; retrying once"
                );
                retried = true;
                adm = Admission::Continue;
            }
            Err(err) => {
                if let Some(p) = poison {
                    let strikes = strikes_for(&err, retried);
                    if strikes > 0 {
                        p.list.strike(&p.key, strikes);
                    }
                }
                return Err(EngineFailure::from(err).into());
            }
        }
    }
}

/// The reply did not carry the output that the operation answers.
pub fn unexpected(op: &str) -> EngineError {
    EngineError::Internal(format!("failed to read the {op} reply: unexpected output"))
}
