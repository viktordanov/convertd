//! `PdfEngine` over the render pool (plan 002 step 7).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::domain::conversion::ImageFormat;
use crate::domain::engine::{
    Admission, AssembleJob, Assembled, DecryptJob, Decrypted, EngineError, EngineTiming,
    InspectJob, Inspection, PageSize, PdfEngine, ProbedPage, RenderJob, Rendered, RenderedPage,
    WorkerRef,
};
use crate::engines::call::{RETRY_MIN_LEFT, admission, call, unexpected, worker_ref};
use crate::engines::error::EngineFailure;
use crate::engines::poison::{PoisonList, TIMEOUT_STRIKES, strikes_for};
use crate::engines::pool::{Admission as PoolAdmission, CallError, Lease, Pool};
use crate::ipc::{
    self, DecryptArgs, InspectArgs, Op, Output, RenderArgs, Reply, Secret, WorkerErrorKind,
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

    /// Refuses renders of documents that recently timed out or crashed a
    /// worker.
    pub fn with_poison(mut self, poison: Option<Arc<PoisonList>>) -> Self {
        self.poison = poison;
        self
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
        ImageFormat::Webp => ipc::ImageFormat::Webp,
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

/// What one render chunk gives back.
type ChunkOk = (Vec<ipc::RenderedPage>, Duration, WorkerRef);

/// A failed render chunk, with the poison strikes it earned.
struct ChunkFailure {
    error: EngineError,
    strikes: u32,
}

impl ChunkFailure {
    fn of(err: CallError, retried: bool) -> Self {
        Self {
            strikes: strikes_for(&err, retried),
            error: EngineFailure::from(err).into(),
        }
    }

    fn engine(error: EngineError) -> Self {
        Self { error, strikes: 0 }
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
/// fresh lease with `Continue`. The strikes follow `poison::strikes_for`,
/// the same rule as every other call.
async fn render_chunk(
    pool: &Pool,
    mut lease: Lease,
    args: RenderArgs,
    deadline: Instant,
) -> Result<ChunkOk, ChunkFailure> {
    let op = Op::Render(args);
    match lease.call(op.clone(), deadline).await {
        Ok(reply) => return chunk_reply(&mut lease, reply, pool),
        Err(err @ CallError::Crashed { .. })
            if deadline.saturating_duration_since(Instant::now()) < RETRY_MIN_LEFT =>
        {
            return Err(ChunkFailure::of(err, false));
        }
        Err(CallError::Crashed { .. }) => {}
        Err(err) => return Err(ChunkFailure::of(err, false)),
    }
    drop(lease);
    tracing::warn!(pool = pool.name(), "render chunk crashed; retrying once");
    let mut retry = match pool.lease(deadline, PoolAdmission::Continue).await {
        Ok(lease) => lease,
        // The crash counts once; the retry never reached a worker.
        Err(err) => {
            return Err(ChunkFailure {
                error: EngineFailure::from(err).into(),
                strikes: TIMEOUT_STRIKES,
            });
        }
    };
    match retry.call(op, deadline).await {
        Ok(reply) => chunk_reply(&mut retry, reply, pool),
        Err(err) => Err(ChunkFailure::of(err, true)),
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
        let op = Op::Inspect(InspectArgs {
            pdf: job.pdf,
            password: job.password.map(|p| Secret::new(p.expose())),
            probe_dpi: job.probe_dpi,
            collect_fonts: job.collect_fonts,
        });
        let called = call(&self.pool, op, admission(adm), deadline, recycles).await?;
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
        let op = Op::Decrypt(DecryptArgs {
            pdf: job.pdf,
            password: Secret::new(job.password.expose()),
            output: job.output,
            max_output_bytes: job.max_output_bytes,
        });
        let called = call(&self.pool, op, admission(adm), deadline, recycles).await?;
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
    /// in order. `engine` is the longest chunk; each chunk gets an equal
    /// share of `max_output_bytes`.
    async fn render(
        &self,
        job: RenderJob,
        adm: Admission,
        deadline: Instant,
    ) -> Result<Rendered, EngineError> {
        if let Some(list) = &self.poison
            && list.is_poisoned(&job.sha256)
        {
            return Err(EngineError::Poisoned);
        }
        let started = Instant::now();
        let first = self
            .pool
            .lease(deadline, admission(adm))
            .await
            .map_err(|err| EngineError::from(EngineFailure::from(err)))?;
        let queue = first.queued();
        let lease_time = first.leased().saturating_sub(queue);
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
        let share = job.max_output_bytes / parts.len() as u64;
        let runs = leases.into_iter().zip(parts).map(|(lease, pages)| {
            let args = RenderArgs {
                pdf: job.pdf.clone(),
                password: job.password.as_ref().map(|p| Secret::new(p.expose())),
                pages,
                format: image_format(job.format),
                dpi: job.dpi,
                quality: job.quality,
                max_edge: job.max_edge,
                out_dir: job.out_dir.clone(),
                max_output_bytes: share,
            };
            render_chunk(&self.pool, lease, args, deadline)
        });
        let results = futures_util::future::join_all(runs).await;
        let mut pages = Vec::with_capacity(job.pages.len());
        let mut workers = Vec::new();
        let mut longest = Duration::ZERO;
        let mut failure: Option<EngineError> = None;
        let mut strikes = 0;
        for result in results {
            match result {
                Ok((chunk, engine, worker)) => {
                    pages.extend(chunk.into_iter().map(rendered_page));
                    longest = longest.max(engine);
                    workers.push(worker);
                }
                Err(chunk) => {
                    strikes = strikes.max(chunk.strikes);
                    let worse = failure
                        .as_ref()
                        .is_none_or(|old| severity(&chunk.error) < severity(old));
                    if worse {
                        failure = Some(chunk.error);
                    }
                }
            }
        }
        if let Some(err) = failure {
            // One document: the strikes of its worst chunk, by the same rule
            // as call_with_poison.
            if let Some(list) = &self.poison
                && strikes > 0
            {
                list.strike(&job.sha256, strikes);
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

    async fn assemble(
        &self,
        _job: AssembleJob,
        _adm: Admission,
        _deadline: Instant,
    ) -> Result<Assembled, EngineError> {
        // 422, not 500: a 500 would tell callers to retry (the lead's ruling).
        Err(EngineError::Failed(
            "image inputs are not supported yet".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
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
