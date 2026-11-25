//! `PdfEngine` over the render pool (plan 002 step 7).

use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::domain::conversion::ImageFormat;
use crate::domain::engine::{
    Admission, AssembleJob, Assembled, DecryptJob, Decrypted, EngineError, EngineTiming,
    InspectJob, Inspection, PageSize, PdfEngine, ProbedPage, RenderJob, Rendered, RenderedPage,
    WorkerRef,
};
use crate::engines::call::{Called, admission, call, unexpected, worker_ref};
use crate::engines::error::EngineFailure;
use crate::engines::pool::{Admission as PoolAdmission, CallError, Lease, Pool};
use crate::ipc::{self, DecryptArgs, InspectArgs, Op, Output, RenderArgs, Secret, WorkerErrorKind};

/// A render chunk has at least this many pages, so an extra chunk (a PDF
/// open and one round trip) is worth its cost.
pub const RENDER_CHUNK_MIN_PAGES: usize = 4;

/// PDFium through the render pool.
pub struct PdfPdfium {
    pool: Pool,
}

impl PdfPdfium {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
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

/// One render chunk on a lease the caller already holds. A crash is
/// retried once on a fresh lease with `Continue`.
async fn render_chunk(
    pool: &Pool,
    mut lease: Lease,
    args: RenderArgs,
    deadline: Instant,
) -> Result<(Vec<ipc::RenderedPage>, Duration, WorkerRef), EngineError> {
    let op = Op::Render(args);
    let first = lease.call(op.clone(), deadline).await;
    let reply = match first {
        Ok(reply) => {
            let worker = lease.worker_ref().map(worker_ref);
            (reply, worker)
        }
        Err(CallError::Crashed { .. })
            if deadline.saturating_duration_since(Instant::now())
                >= crate::engines::call::RETRY_MIN_LEFT =>
        {
            drop(lease);
            let Called {
                output,
                reply,
                worker,
                ..
            } = call(pool, op, PoolAdmission::Continue, deadline, recycles).await?;
            let Output::Rendered(out) = output else {
                return Err(unexpected("render"));
            };
            return Ok((
                out.pages,
                Duration::from_micros(reply.engine.engine_us),
                worker,
            ));
        }
        Err(err) => return Err(EngineFailure::from(err).into()),
    };
    let (reply, worker) = reply;
    let worker = worker.unwrap_or(WorkerRef {
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
        Ok(_) => Err(unexpected("render")),
        Err(err) => {
            if recycles(err.kind) {
                lease.recycle_after();
            }
            Err(EngineFailure::from(err).into())
        }
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
        for result in results {
            match result {
                Ok((chunk, engine, worker)) => {
                    pages.extend(chunk.into_iter().map(rendered_page));
                    longest = longest.max(engine);
                    workers.push(worker);
                }
                Err(err) => {
                    let worse = failure
                        .as_ref()
                        .is_none_or(|old| severity(&err) < severity(old));
                    if worse {
                        failure = Some(err);
                    }
                }
            }
        }
        if let Some(err) = failure {
            return Err(err);
        }
        pages.sort_by_key(|p| p.number);
        Ok(Rendered {
            pages,
            timing: EngineTiming {
                queue,
                lease: lease_time,
                spawn: spawned.map(|(spawn, _)| spawn),
                bind: spawned.map(|(_, bind)| bind),
                wall: started.elapsed(),
                engine: longest,
            },
            workers,
        })
    }

    async fn assemble(
        &self,
        _job: AssembleJob,
        _adm: Admission,
        _deadline: Instant,
    ) -> Result<Assembled, EngineError> {
        Err(EngineError::Internal(
            "image assembly is not built yet".into(),
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
