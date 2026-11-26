//! `PdfEngine` over the render pool (plan 002 step 7).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::domain::conversion::ImageFormat;
use crate::domain::engine::{
    Admission, DecryptJob, Decrypted, EngineError, EngineTiming, InspectJob, Inspection, PageSize,
    PdfEngine, ProbedPage, RenderJob, Rendered, RenderedPage, WorkerRef,
};
use crate::engines::call::{RETRY_MIN_LEFT, admission, call_with_poison, unexpected, worker_ref};
use crate::engines::error::EngineFailure;
use crate::engines::poison::{Attempt, Poison, PoisonList, Strike, Work, strike_for, worse};
use crate::engines::pool::{Admission as PoolAdmission, CallError, Lease, Pool};
use crate::ipc::{
    self, DecryptArgs, InspectArgs, Op, Output, ProbePages, RenderArgs, Reply, Secret,
    WorkerErrorKind,
};

/// A render chunk has at least this many pages, so an extra chunk (a PDF
/// open and one round trip) is worth its cost.
pub const RENDER_CHUNK_MIN_PAGES: usize = 4;

/// PDFium through the render pool.
pub struct PdfPdfium {
    pool: Pool,
    poison: Option<Arc<PoisonList>>,
}

impl PdfPdfium {
    pub fn new(pool: Pool) -> Self {
        Self { pool, poison: None }
    }

    /// Refuses inspects, decrypts and renders of documents that recently
    /// timed out or crashed a worker.
    pub fn with_poison(mut self, poison: Option<Arc<PoisonList>>) -> Self {
        self.poison = poison;
        self
    }

    /// The poison handle of one inspect or decrypt; render keeps its own
    /// per-chunk rule.
    fn poison(&self, key: [u8; 32], work: Work) -> Option<Poison> {
        self.poison.as_ref().map(|list| Poison {
            list: Arc::clone(list),
            key,
            work,
        })
    }

    /// Renders the pages that went over their share on one lease, with
    /// `left` bytes. Too little time or no bytes left returns `limit`. A
    /// cap is the job's own limit, so it never strikes.
    async fn rerun(
        &self,
        args: RenderArgs,
        left: u64,
        deadline: Instant,
        limit: EngineError,
        guard: Option<Guard<'_>>,
    ) -> Result<ChunkOk, ChunkFailure> {
        if left == 0 || deadline.saturating_duration_since(Instant::now()) < RETRY_MIN_LEFT {
            return Err(ChunkFailure::engine(limit));
        }
        let lease = self
            .pool
            .lease(deadline, PoolAdmission::Continue)
            .await
            .map_err(|err| ChunkFailure::engine(EngineFailure::from(err).into()))?;
        if guard.is_some_and(|g| g.refuses(deadline)) {
            return Err(ChunkFailure::engine(EngineError::Poisoned));
        }
        tracing::debug!(
            pool = self.pool.name(),
            pages = args.pages.len(),
            left,
            "render chunks went over their share; rendering them on one worker"
        );
        render_chunk(&self.pool, lease, args, deadline, guard).await
    }
}

/// PDFium errors such as a password or a broken file leave the process
/// clean; only an internal error recycles a render worker.
pub fn recycles(kind: WorkerErrorKind) -> bool {
    kind == WorkerErrorKind::Internal
}

fn image_format(format: ImageFormat) -> ipc::ImageFormat {
    match format {
        ImageFormat::Jpeg => ipc::ImageFormat::Jpeg,
        ImageFormat::Png => ipc::ImageFormat::Png,
    }
}

/// Splits `pages` into `k` contiguous chunks of near-equal size, with
/// `k = min(workers, ceil(len / RENDER_CHUNK_MIN_PAGES))`, at least 1.
pub fn chunks(pages: &[u32], workers: usize) -> Vec<Vec<u32>> {
    let wanted = pages.len().div_ceil(RENDER_CHUNK_MIN_PAGES).max(1);
    let k = wanted.min(workers.max(1));
    let base = pages.len() / k;
    let extra = pages.len() % k;
    let mut out = Vec::with_capacity(k);
    let mut at = 0;
    for i in 0..k {
        let len = base + usize::from(i < extra);
        out.push(pages[at..at + len].to_vec());
        at += len;
    }
    out
}

fn rendered_page(p: ipc::RenderedPage) -> RenderedPage {
    RenderedPage {
        number: p.number,
        file_name: p.file_name,
        width_px: p.width_px,
        height_px: p.height_px,
        width_pt: p.width_pt,
        height_pt: p.height_pt,
        size_bytes: p.size_bytes,
        objects: p.objects,
        flat: p.flat,
        colour: p.colour,
        effective_dpi: p.effective_dpi,
        crc32: p.crc32,
    }
}

/// The more severe of two chunk errors: timeout, then crash, then the
/// password errors, then everything else.
fn severity(err: &EngineError) -> u8 {
    match err {
        EngineError::Timeout => 0,
        EngineError::Crashed => 1,
        EngineError::PasswordRequired | EngineError::InvalidPassword => 2,
        _ => 3,
    }
}

/// A render's poison check for a lease taken after its first one (a crash
/// retry or an over-share rerun): the list, the upload's key and the work.
#[derive(Clone, Copy)]
struct Guard<'a> {
    list: &'a PoisonList,
    key: &'a [u8; 32],
    work: Work,
}

impl Guard<'_> {
    /// Whether a call dispatched now, with the time left, is refused.
    fn refuses(&self, deadline: Instant) -> bool {
        self.list
            .refuses(self.key, &Attempt::new(self.work, deadline, Instant::now()))
    }
}

/// One render strikes once: the worse strike of its chunks (a crash before
/// a timeout), with the budget of the call that earned it. Between equal
/// strikes the longest budget that still failed is the strongest evidence.
fn pick_strike(
    held: (Option<Strike>, Duration),
    chunk: (Option<Strike>, Duration),
) -> (Option<Strike>, Duration) {
    let strike = worse(held.0, chunk.0);
    if strike != held.0 {
        (strike, chunk.1)
    } else if chunk.0 == strike {
        (strike, held.1.max(chunk.1))
    } else {
        held
    }
}

/// What one render chunk gives back.
type ChunkOk = (Vec<ipc::RenderedPage>, Duration, WorkerRef);

/// A failed render chunk, with the poison strike it earned.
struct ChunkFailure {
    error: EngineError,
    strike: Option<Strike>,
    /// The time its failing worker call had: a timeout strike's budget.
    ran: Duration,
}

impl ChunkFailure {
    fn of(err: CallError, retried: bool, ran: Duration) -> Self {
        Self {
            strike: strike_for(&err, retried),
            error: EngineFailure::from(err).into(),
            ran,
        }
    }

    fn engine(error: EngineError) -> Self {
        Self {
            error,
            strike: None,
            ran: Duration::ZERO,
        }
    }
}

/// A chunk's reply: the pages, or the engine-reported error (no strike).
fn chunk_reply(lease: &mut Lease, reply: Reply, pool: &Pool) -> Result<ChunkOk, ChunkFailure> {
    let worker = lease.worker_ref().map(worker_ref).unwrap_or(WorkerRef {
        pool: pool.name(),
        slot: 0,
        generation: 0,
        job: 0,
    });
    match reply.result {
        Ok(Output::Rendered(out)) => Ok((
            out.pages,
            Duration::from_micros(reply.engine.engine_us),
            worker,
        )),
        Ok(_) => Err(ChunkFailure::engine(unexpected("render"))),
        Err(err) => {
            if recycles(err.kind) {
                lease.recycle_after();
            }
            Err(ChunkFailure::engine(EngineFailure::from(err).into()))
        }
    }
}

/// One render chunk on a lease the caller already holds, with at most one
/// retry: after a crash with at least `RETRY_MIN_LEFT` left, once more on a
/// fresh lease with `Continue`. The strike follows `poison::strike_for`,
/// the same rule as every other call.
async fn render_chunk(
    pool: &Pool,
    mut lease: Lease,
    args: RenderArgs,
    deadline: Instant,
    guard: Option<Guard<'_>>,
) -> Result<ChunkOk, ChunkFailure> {
    let op = Op::Render(args);
    // Each chunk checks at its own dispatch: a strike can land, or this
    // chunk's time can fall into a covered range, after the first lease.
    // It has not run, so it earns no strike.
    if guard.is_some_and(|g| g.refuses(deadline)) {
        return Err(ChunkFailure::engine(EngineError::Poisoned));
    }
    let ran = deadline.saturating_duration_since(Instant::now());
    match lease.call(op.clone(), deadline).await {
        Ok(reply) => return chunk_reply(&mut lease, reply, pool),
        Err(err @ CallError::Crashed { .. })
            if deadline.saturating_duration_since(Instant::now()) < RETRY_MIN_LEFT =>
        {
            return Err(ChunkFailure::of(err, false, ran));
        }
        Err(CallError::Crashed { .. }) => {}
        Err(err) => return Err(ChunkFailure::of(err, false, ran)),
    }
    drop(lease);
    tracing::warn!(pool = pool.name(), "render chunk crashed; retrying once");
    let mut retry = match pool.lease(deadline, PoolAdmission::Continue).await {
        Ok(lease) => lease,
        // The crash counts once; the retry never reached a worker.
        Err(err) => {
            return Err(ChunkFailure {
                error: EngineFailure::from(err).into(),
                strike: Some(Strike::Crash(1)),
                ran,
            });
        }
    };
    // The retry waited for a worker: check again with the time left. The
    // crash still counts once.
    if guard.is_some_and(|g| g.refuses(deadline)) {
        drop(retry);
        return Err(ChunkFailure {
            error: EngineError::Poisoned,
            strike: Some(Strike::Crash(1)),
            ran,
        });
    }
    let ran = deadline.saturating_duration_since(Instant::now());
    match retry.call(op, deadline).await {
        Ok(reply) => chunk_reply(&mut retry, reply, pool),
        Err(err) => Err(ChunkFailure::of(err, true, ran)),
    }
}

#[async_trait]
impl PdfEngine for PdfPdfium {
    async fn inspect(
        &self,
        job: InspectJob,
        adm: Admission,
        deadline: Instant,
    ) -> Result<Inspection, EngineError> {
        let poison = self.poison(
            job.sha256,
            Work::Inspect {
                probe: job.probe_dpi.is_some(),
                export: job.export,
            },
        );
        let op = Op::Inspect(InspectArgs {
            pdf: job.pdf,
            password: job.password.map(|p| Secret::new(p.expose())),
            probe_dpi: job.probe_dpi,
            // The first VERIFY_PROBE_MAX_PAGES pages (lane 001).
            probe_pages: ProbePages::All {
                max: Some(job.probe_max_pages),
            },
            collect_fonts: job.collect_fonts,
        });
        let called = call_with_poison(
            &self.pool,
            op,
            admission(adm),
            deadline,
            recycles,
            poison.as_ref(),
        )
        .await?;
        let Output::Inspected(out) = called.output else {
            return Err(unexpected("inspect"));
        };
        Ok(Inspection {
            page_count: out.page_count,
            encrypted: out.encrypted,
            needs_password: out.needs_password,
            pages: out
                .pages
                .iter()
                .map(|p| PageSize {
                    width_pt: p.width_pt,
                    height_pt: p.height_pt,
                })
                .collect(),
            probe: out
                .probe
                .iter()
                .map(|p| ProbedPage {
                    number: p.number,
                    objects: p.objects,
                    flat: p.flat,
                    colour: p.colour,
                })
                .collect(),
            probe_skipped: out.probe_skipped,
            probe_engine: Duration::from_micros(called.reply.engine.probe_us),
            fonts: out.fonts,
            timing: called.timing,
            worker: called.worker,
        })
    }

    async fn decrypt(
        &self,
        job: DecryptJob,
        adm: Admission,
        deadline: Instant,
    ) -> Result<Decrypted, EngineError> {
        let poison = self.poison(job.sha256, Work::Decrypt);
        let op = Op::Decrypt(DecryptArgs {
            pdf: job.pdf,
            password: Secret::new(job.password.expose()),
            output: job.output,
            max_output_bytes: job.max_output_bytes,
            check_owner: job.check_owner,
        });
        let called = call_with_poison(
            &self.pool,
            op,
            admission(adm),
            deadline,
            recycles,
            poison.as_ref(),
        )
        .await?;
        let Output::Decrypted(out) = called.output else {
            return Err(unexpected("decrypt"));
        };
        tracing::debug!(decrypt_us = called.reply.engine.decrypt_us, "pdf decrypt");
        Ok(Decrypted {
            pdf_bytes: out.pdf_bytes,
            timing: called.timing,
            worker: called.worker,
        })
    }

    /// Splits the pages over the leased worker and any idle ones (never
    /// waiting for more), runs the chunks together, and returns the pages
    /// in order. `engine` is the longest chunk. Each chunk gets an equal
    /// share of `max_output_bytes`. Chunks that go over their share run
    /// again on one lease with what the others left, so the job fails on
    /// its cap exactly when one worker would.
    async fn render(
        &self,
        job: RenderJob,
        adm: Admission,
        deadline: Instant,
    ) -> Result<Rendered, EngineError> {
        let started = Instant::now();
        let attempt = Attempt::new(
            Work::Render {
                pages: job.pages.len() as u32,
            },
            deadline,
            started,
        );
        if let Some(list) = &self.poison
            && list.refuses(&job.sha256, &attempt)
        {
            return Err(EngineError::Poisoned);
        }
        let guard = self.poison.as_deref().map(|list| Guard {
            list,
            key: &job.sha256,
            work: attempt.work,
        });
        let first = self
            .pool
            .lease(deadline, admission(adm))
            .await
            .map_err(|err| EngineError::from(EngineFailure::from(err)))?;
        let queue = first.queued();
        let lease_time = first.leased().saturating_sub(queue);
        // Check again with the time a worker gets now: under queue pressure
        // a request that is left with no more time than a timed-out one is
        // refused instead of killing another worker.
        if let Some(list) = &self.poison
            && list.refuses(
                &job.sha256,
                &Attempt::new(attempt.work, deadline, Instant::now()),
            )
        {
            drop(first);
            return Err(EngineError::Poisoned);
        }
        let spawned = first.spawned_on_path();
        let wanted = job.pages.len().div_ceil(RENDER_CHUNK_MIN_PAGES).max(1);
        let mut leases = vec![first];
        while leases.len() < wanted {
            match self.pool.try_lease() {
                Some(lease) => leases.push(lease),
                None => break,
            }
        }
        let parts = chunks(&job.pages, leases.len());
        leases.truncate(parts.len());
        let split = parts.len() > 1;
        let share = job.max_output_bytes / parts.len() as u64;
        let args = |pages: Vec<u32>, max_output_bytes: u64| RenderArgs {
            pdf: job.pdf.clone(),
            password: job.password.as_ref().map(|p| Secret::new(p.expose())),
            pages,
            format: image_format(job.format),
            dpi: job.dpi,
            quality: job.quality,
            max_edge: job.max_edge,
            out_dir: job.out_dir.clone(),
            max_output_bytes,
        };
        let runs = leases
            .into_iter()
            .zip(parts.iter().cloned())
            .map(|(lease, pages)| {
                render_chunk(&self.pool, lease, args(pages, share), deadline, guard)
            });
        let results = futures_util::future::join_all(runs).await;
        let mut pages = Vec::with_capacity(job.pages.len());
        let mut workers = Vec::new();
        let mut longest = Duration::ZERO;
        let mut failure: Option<EngineError> = None;
        let mut strike: Option<Strike> = None;
        // The budget of the call whose strike counts: a timeout covers only
        // requests that give a worker no more time than that call had.
        let mut strike_ran = Duration::ZERO;
        // Pages of the chunks that went over their share: an equal share
        // keeps the disk within the reservation, but the job's cap is the
        // whole budget, so these get what the other chunks left.
        let mut over: Vec<u32> = Vec::new();
        let mut over_error: Option<EngineError> = None;
        for (chunk_pages, result) in parts.into_iter().zip(results) {
            match result {
                Ok((chunk, engine, worker)) => {
                    pages.extend(chunk.into_iter().map(rendered_page));
                    longest = longest.max(engine);
                    workers.push(worker);
                }
                Err(chunk) if split && matches!(chunk.error, EngineError::LimitExceeded(_)) => {
                    over.extend(chunk_pages);
                    over_error = Some(chunk.error);
                }
                Err(chunk) => {
                    (strike, strike_ran) =
                        pick_strike((strike, strike_ran), (chunk.strike, chunk.ran));
                    let more_severe = failure
                        .as_ref()
                        .is_none_or(|old| severity(&chunk.error) < severity(old));
                    if more_severe {
                        failure = Some(chunk.error);
                    }
                }
            }
        }
        if failure.is_none()
            && let Some(limit) = over_error
        {
            over.sort_unstable();
            let finished: u64 = pages.iter().map(|p: &RenderedPage| p.size_bytes).sum();
            let left = job.max_output_bytes.saturating_sub(finished);
            match self
                .rerun(args(over, left), left, deadline, limit, guard)
                .await
            {
                Ok((chunk, engine, worker)) => {
                    pages.extend(chunk.into_iter().map(rendered_page));
                    // The rerun comes after the parallel chunks.
                    longest += engine;
                    workers.push(worker);
                }
                Err(chunk) => {
                    strike = chunk.strike;
                    strike_ran = chunk.ran;
                    failure = Some(chunk.error);
                }
            }
        }
        if let Some(err) = failure {
            // One document: the strikes of its worst chunk, by the same rule
            // as call_with_poison.
            if let Some(list) = &self.poison
                && let Some(strike) = strike
            {
                let ran = Attempt {
                    work: attempt.work,
                    budget: strike_ran,
                };
                list.strike(&job.sha256, strike, &ran);
            }
            return Err(err);
        }
        pages.sort_by_key(|p| p.number);
        let timing = EngineTiming {
            queue,
            lease: lease_time,
            spawn: spawned.map(|(spawn, _)| spawn),
            bind: spawned.map(|(_, bind)| bind),
            wall: started.elapsed(),
            engine: longest,
            preflight: Duration::ZERO,
        };
        metrics::histogram!(
            "convert_engine_overhead_seconds",
            "pool" => self.pool.name(),
            "op" => "render"
        )
        .record(
            timing
                .wall
                .saturating_sub(timing.engine + queue)
                .as_secs_f64(),
        );
        Ok(Rendered {
            pages,
            timing,
            workers,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_render_keeps_the_worse_strike_and_the_longest_failed_budget() {
        let s = Duration::from_secs;
        let none = (None, Duration::ZERO);
        let timeout = |b| (Some(Strike::Timeout), s(b));
        assert_eq!(pick_strike(none, timeout(10)), timeout(10));
        assert_eq!(pick_strike(timeout(10), timeout(2)), timeout(10));
        assert_eq!(pick_strike(timeout(2), timeout(10)), timeout(10));
        let crash = (Some(Strike::Crash(2)), s(3));
        assert_eq!(pick_strike(timeout(10), crash), crash, "a crash is worse");
        assert_eq!(pick_strike(crash, timeout(10)), crash);
        assert_eq!(pick_strike(timeout(10), none), timeout(10));
    }

    use super::*;

    #[test]
    fn chunks_are_contiguous_near_equal_and_bounded() {
        let pages: Vec<u32> = (1..=35).collect();
        let parts = chunks(&pages, 2);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], (1..=18).collect::<Vec<_>>());
        assert_eq!(parts[1], (19..=35).collect::<Vec<_>>());
        // Fewer pages than a chunk's minimum: one chunk.
        assert_eq!(chunks(&[1, 2, 3], 8), vec![vec![1, 2, 3]]);
        // At most ceil(len / 4) chunks, even with more workers.
        assert_eq!(chunks(&(1..=9).collect::<Vec<_>>(), 8).len(), 3);
        assert_eq!(chunks(&[], 3), vec![Vec::<u32>::new()]);
    }

    #[test]
    fn the_most_severe_chunk_error_wins() {
        assert!(severity(&EngineError::Timeout) < severity(&EngineError::Crashed));
        assert!(severity(&EngineError::Crashed) < severity(&EngineError::PasswordRequired));
        assert!(
            severity(&EngineError::InvalidPassword) < severity(&EngineError::Failed("x".into()))
        );
    }
}
