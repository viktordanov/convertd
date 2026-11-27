//! `OfficeEngine` over the office pool.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::domain::conversion::{ExportOptions, SlideNotes};
use crate::domain::document::{DocType, Family};
use crate::domain::engine::{Admission, Converted, EngineError, OfficeEngine, OfficeJob};
use crate::domain::verification::SourceFacts;
use crate::engines::call::{admission, call_with_poison, unexpected};
use crate::engines::poison::{Poison, PoisonList, Work};
use crate::engines::pool::Pool;
use crate::ipc::{self, ConvertArgs, Op, Output, Secret, WorkerErrorKind};

/// LibreOffice through the office pool.
pub struct OfficeLok {
    pool: Pool,
    poison: Option<Arc<PoisonList>>,
}

impl OfficeLok {
    pub fn new(pool: Pool) -> Self {
        Self { pool, poison: None }
    }

    /// Refuses documents that recently timed out or crashed a worker.
    pub fn with_poison(mut self, poison: Option<Arc<PoisonList>>) -> Self {
        self.poison = poison;
        self
    }
}

/// After these engine errors LibreOffice's state is suspect, so the worker
/// is recycled. The password errors leave it clean (`docs/m0-spikes.md`).
pub fn recycles(kind: WorkerErrorKind) -> bool {
    matches!(
        kind,
        WorkerErrorKind::LoadFailed
            | WorkerErrorKind::ExportFailed
            | WorkerErrorKind::EmptyDocument
            | WorkerErrorKind::LimitExceeded
            | WorkerErrorKind::Internal
    )
}

pub fn office_family(family: Family) -> Result<ipc::Family, EngineError> {
    match family {
        Family::Document => Ok(ipc::Family::Document),
        Family::Spreadsheet => Ok(ipc::Family::Spreadsheet),
        Family::Presentation => Ok(ipc::Family::Presentation),
        Family::Pdf | Family::Image => Err(EngineError::Internal(format!(
            "failed to convert: {} is not an office family",
            family.as_str()
        ))),
    }
}

pub fn export(options: &ExportOptions) -> ipc::ExportOptions {
    ipc::ExportOptions {
        notes: match options.notes {
            SlideNotes::None => ipc::SlideNotes::None,
            SlideNotes::Append => ipc::SlideNotes::Append,
            SlideNotes::Only => ipc::SlideNotes::Only,
        },
        include_hidden_slides: options.include_hidden_slides,
        one_page_per_sheet: options.one_page_per_sheet,
    }
}

pub fn doc_type(name: &str) -> DocType {
    match name {
        "text" => DocType::Text,
        "spreadsheet" => DocType::Spreadsheet,
        "presentation" => DocType::Presentation,
        "drawing" => DocType::Drawing,
        _ => DocType::Other,
    }
}

#[async_trait]
impl OfficeEngine for OfficeLok {
    async fn convert(
        &self,
        job: OfficeJob,
        adm: Admission,
        deadline: Instant,
    ) -> Result<Converted, EngineError> {
        let poison = self.poison.as_ref().map(|list| Poison {
            list: Arc::clone(list),
            key: job.sha256,
            work: Work::Convert,
        });
        let op = Op::Convert(ConvertArgs {
            input: job.input,
            output: job.output,
            family: office_family(job.family)?,
            password: job.password.map(|p| Secret::new(p.expose())),
            export: export(&job.export),
            max_output_bytes: job.max_output_bytes,
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
        let Output::Converted(out) = called.output else {
            return Err(unexpected("convert"));
        };
        tracing::debug!(
            preflight_us = called.reply.engine.preflight_us,
            convert_us = called.reply.engine.convert_us,
            "office convert"
        );
        Ok(Converted {
            pdf_bytes: out.pdf_bytes,
            doc_type: doc_type(&out.doc_type),
            facts: out.facts.as_deref().map(SourceFacts::from),
            timing: called.timing,
            worker: called.worker,
        })
    }
}
