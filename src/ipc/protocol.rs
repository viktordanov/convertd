//! The messages between the supervisor and a worker.
//!
//! These are serde types for the pipe, not domain types: `domain/` has no
//! serde, and `engines/` converts between the two.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Sent in `Init` and `Ready`; a mismatch stops the worker.
pub const PROTOCOL_VERSION: u32 = 1;

/// A password. `Debug` prints `***`; never log the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The password itself. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Supervisor to worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// The first frame after spawn.
    Init(Init),
    /// One job. One call is in flight per worker.
    Call(Call),
}

/// What a worker needs to start its engine.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Init {
    pub protocol: u32,
    pub lo_program_dir: Option<PathBuf>,
    pub pdfium_lib_dir: Option<PathBuf>,
    /// Convert the warm-up fixtures before `Ready`.
    pub warmup: bool,
    /// Start-up behaviour of the `test` role; other roles ignore it.
    #[serde(default)]
    pub test: TestInit,
}

/// Start-up faults for the `test` role.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TestInit {
    /// Answer `Fatal` instead of `Ready`.
    pub fatal: bool,
    /// Sleep this long before `Ready`, standing in for an engine bind.
    pub ready_delay_ms: u64,
}

/// One job, with the id the reply echoes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub id: u64,
    pub op: Op,
}

/// The operations. Each role answers its own and `Ping`; others get
/// `WorkerErrorKind::Unsupported`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Op {
    Ping,
    Convert(ConvertArgs),
    Inspect(InspectArgs),
    Render(RenderArgs),
    Decrypt(DecryptArgs),
    /// Fault injection; only the `test` role accepts it.
    Test(TestOp),
}

impl Op {
    /// The name used in logs and metrics.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Ping => "ping",
            Op::Convert(_) => "convert",
            Op::Inspect(_) => "inspect",
            Op::Render(_) => "render",
            Op::Decrypt(_) => "decrypt",
            Op::Test(_) => "test",
        }
    }
}

/// The office family of a `Convert` source, from detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Document,
    Spreadsheet,
    Presentation,
}

/// What to do with presentation notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlideNotes {
    #[default]
    None,
    Append,
    Only,
}

/// PDF export options, always sent explicitly so nothing comes from the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExportOptions {
    pub notes: SlideNotes,
    pub include_hidden_slides: bool,
    pub one_page_per_sheet: bool,
}

/// Page image formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Jpeg,
    Png,
}

impl ImageFormat {
    /// The file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvertArgs {
    pub input: PathBuf,
    pub output: PathBuf,
    pub family: Family,
    pub password: Option<Secret>,
    pub export: ExportOptions,
    pub max_output_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectArgs {
    pub pdf: PathBuf,
    pub password: Option<Secret>,
    /// Render the pages that `probe_pages` names at this DPI, for the
    /// blank-page probe.
    pub probe_dpi: Option<u16>,
    /// Which pages the probe renders. Unused without `probe_dpi`.
    #[serde(default)]
    pub probe_pages: ProbePages,
    pub collect_fonts: bool,
}

/// The pages of the blank-page probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbePages {
    /// The first pages of the document, at most `max` (`None`: every
    /// page). The rest count in `InspectedOut::probe_skipped`.
    All { max: Option<u32> },
    /// Only these 1-based pages, in order. Numbers past the page count are
    /// left out, and nothing counts as skipped.
    Only(Vec<u32>),
}

impl Default for ProbePages {
    fn default() -> Self {
        ProbePages::All { max: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderArgs {
    pub pdf: PathBuf,
    pub password: Option<Secret>,
    /// 1-based page numbers.
    pub pages: Vec<u32>,
    pub format: ImageFormat,
    pub dpi: u16,
    pub quality: u8,
    pub max_edge: u32,
    /// Pages are written as `out_dir/NNNN.<ext>`.
    pub out_dir: PathBuf,
    pub max_output_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecryptArgs {
    pub pdf: PathBuf,
    pub password: Secret,
    pub output: PathBuf,
    pub max_output_bytes: u64,
    /// Never drop owner restrictions that the caller did not unlock. The
    /// copy is written when the user permissions grant everything or the
    /// password unlocked the owner. Otherwise nothing is written: a
    /// password that opened the file gives `Restricted`, and none gives
    /// `InvalidPassword` (an owner-only PDF). Off: remove the security of
    /// any file that opens, as before.
    #[serde(default)]
    pub check_owner: bool,
}

/// Faults the `test` role can act out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "test", content = "args", rename_all = "snake_case")]
pub enum TestOp {
    /// Sleep, then answer.
    Sleep { ms: u64 },
    /// Spin until the process has used `ms` more CPU time, then answer.
    /// It gives up after 10 × `ms` of wall time, so a broken CPU clock
    /// fails a test instead of hanging it.
    Burn { ms: u64 },
    /// Never answer.
    Hang,
    /// `abort()`: dies with SIGABRT.
    Abort,
    /// `_exit(code)` without answering.
    Exit { code: i32 },
    /// Allocate and touch `mb` MiB, and keep it.
    Allocate { mb: u32 },
    /// Write junk to fd 1, which must land on stderr, not in the frames.
    WriteStdout { bytes: u32 },
    /// Start `sleep 1000` in the worker's process group and answer its PID.
    SpawnSleeper,
    /// Answer the worker's environment.
    Env,
}

/// Worker to supervisor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// The engine is up; the worker takes calls.
    Ready(Ready),
    /// Start-up failed; the worker exits 1 after sending it.
    Fatal(Fatal),
    /// The answer to one `Call`.
    Reply(Reply),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub protocol: u32,
    pub pid: u32,
    /// LibreOffice's product version; empty for the other roles.
    pub engine_version: String,
    /// Binding PDFium or starting LibreOfficeKit, with the warm-up, inside the worker.
    pub bind_us: u64,
    pub rss_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fatal {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// The `Call` id this answers.
    pub id: u64,
    /// Conversions served by this process, this one included. A `Ping`
    /// or an `Inspect` does not count (`worker::counts_as_job`).
    pub job: u32,
    pub engine: EngineTimes,
    /// The worker's RSS after the call; 0 where it cannot be read.
    pub rss_bytes: u64,
    pub result: Result<Output, WorkerError>,
}

/// Measured inside the worker. The `_us` times other than `cpu_us` run
/// from just before the document is loaded or opened to just after the
/// last engine call. Process start, the library bind and IPC are outside
/// them. Fields that do not apply are 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EngineTimes {
    /// The whole engine span.
    pub engine_us: u64,
    pub convert_us: u64,
    pub inspect_us: u64,
    pub probe_us: u64,
    pub render_us: u64,
    pub encode_us: u64,
    pub decrypt_us: u64,
    /// The office pre-flight scan; not engine time (INTERFACES.md §15).
    pub preflight_us: u64,
    /// The worker process's CPU time over the whole call, user plus
    /// system, every thread included. Unlike the other fields it spans
    /// the whole call, the pre-flight scan included. 0 from a worker that
    /// predates the field.
    #[serde(default)]
    pub cpu_us: u64,
}

/// A successful answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "output", content = "data", rename_all = "snake_case")]
pub enum Output {
    Pong,
    Converted(ConvertedOut),
    Inspected(InspectedOut),
    Rendered(RenderedOut),
    Decrypted(DecryptedOut),
    Test(TestOutput),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvertedOut {
    pub pdf_bytes: u64,
    /// LibreOffice's document type (`text`, `spreadsheet`, `presentation`,
    /// `drawing` or `other`), so the supervisor can log a mismatch with
    /// detection.
    pub doc_type: String,
    /// The pre-flight scan of the source; `None` when it did not run.
    /// Boxed: the facts would make every `Response` as large as a `Reply`.
    #[serde(default)]
    pub facts: Option<Box<SourceFactsWire>>,
}

/// The wire form of `domain::verification::SourceFacts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFactsWire {
    pub declared_pages: Option<DeclaredPagesWire>,
    pub requested_fonts: Vec<String>,
    /// Families the source embeds (DOCX font table).
    #[serde(default)]
    pub embedded_fonts: Vec<String>,
    /// The distinct text code points that need a glyph, sorted, at most
    /// `MAX_CODE_POINTS` (`GLYPHS_MISSING`). Decoding keeps only the first
    /// `MAX_CODE_POINTS` values in the field, so the supervisor never sorts
    /// or keeps more. Peak decode memory is bounded by the frame limit, as
    /// for every other list in a reply: `Response` is internally tagged, so
    /// serde buffers the fields before this visitor runs.
    #[serde(default, deserialize_with = "bounded_code_points")]
    pub text_code_points: Vec<u32>,
    pub macros: Option<MacroFactWire>,
    /// The slides in show order and the hidden ones (presentations).
    #[serde(default)]
    pub slides: Option<SlidesWire>,
    pub skipped: Vec<(CheckWire, SkipReasonWire)>,
}

/// Reads a sequence of `u32`, keeping the first `MAX_CODE_POINTS` and
/// reading past the rest without storing it.
fn bounded_code_points<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Vec<u32>, D::Error> {
    struct Bounded;
    impl<'de> serde::de::Visitor<'de> for Bounded {
        type Value = Vec<u32>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a list of code points")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u32>, A::Error> {
            let limit = crate::domain::verification::MAX_CODE_POINTS;
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(limit));
            while let Some(cp) = seq.next_element::<u32>()? {
                if out.len() < limit {
                    out.push(cp);
                }
            }
            Ok(out)
        }
    }
    de.deserialize_seq(Bounded)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlidesWire {
    pub count: u32,
    /// 1-based positions, ascending.
    pub hidden: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredPagesWire {
    pub count: u32,
    /// The part the count came from, for example `ppt/presentation.xml`.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroFactWire {
    pub kind: MacroKindWire,
    pub part: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacroKindWire {
    Vba,
    Xlm,
    Basic,
    Script,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckWire {
    PageCount,
    Fonts,
    Macros,
    Glyphs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReasonWire {
    CapReached,
    MissingPart,
    MalformedXml,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectedOut {
    pub page_count: u32,
    /// A security handler is present.
    pub encrypted: bool,
    /// The file would not open without a password.
    pub needs_password: bool,
    /// Every page, in order.
    pub pages: Vec<PageGeometry>,
    /// Only with `probe_dpi`: the pages that `probe_pages` names.
    pub probe: Vec<ProbedPage>,
    /// Pages that `ProbePages::All { max }` left out of the probe.
    #[serde(default)]
    pub probe_skipped: u32,
    /// Only with `collect_fonts`.
    pub fonts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PageGeometry {
    pub width_pt: f32,
    pub height_pt: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbedPage {
    pub number: u32,
    pub objects: u32,
    /// Every pixel is within 4 levels per channel of pixel (0,0).
    pub flat: bool,
    /// The RGB of pixel (0,0).
    pub colour: [u8; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedOut {
    pub pages: Vec<RenderedPage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedPage {
    pub number: u32,
    /// Inside `out_dir`, for example `0001.jpg`.
    pub file_name: String,
    pub width_px: u32,
    pub height_px: u32,
    pub width_pt: f32,
    pub height_pt: f32,
    pub size_bytes: u64,
    pub objects: u32,
    pub flat: bool,
    pub colour: [u8; 3],
    pub effective_dpi: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecryptedOut {
    pub pdf_bytes: u64,
}

/// Answers to `TestOp`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "test", content = "data", rename_all = "snake_case")]
pub enum TestOutput {
    Slept,
    Allocated { rss_bytes: u64 },
    Wrote { bytes: u32 },
    Spawned { pid: u32 },
    Env { vars: Vec<(String, String)> },
}

/// An error the engine reported. The worker stays usable unless the
/// supervisor decides to recycle it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct WorkerError {
    pub kind: WorkerErrorKind,
    /// For the log only: engine messages can hold paths, so they never
    /// reach an HTTP body.
    pub message: String,
}

impl WorkerError {
    pub fn new(kind: WorkerErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerErrorKind {
    PasswordRequired,
    InvalidPassword,
    LoadFailed,
    ExportFailed,
    EmptyDocument,
    /// This job's own cap: `max_output_bytes`, EFBIG from its own
    /// `RLIMIT_FSIZE`, a pixel bomb.
    LimitExceeded,
    /// ENOSPC, EDQUOT or almost no free space: caused by other jobs, worth
    /// a retry (INTERFACES.md §5).
    DiskFull,
    Io,
    /// The role does not answer this operation.
    Unsupported,
    Internal,
    /// Decrypt with `check_owner`: the user password opened the file, but
    /// the owner was not unlocked and the file restricts permissions. No
    /// copy was written; the original stays as it is.
    Restricted,
    /// Only the `test` role sends this, for `FAKE:TOO_MANY_REQUESTS`, so
    /// level-B fixtures behave as they do at level A.
    FakeQueueFull,
    /// Only the `test` role sends this, for `FAKE:SERVICE_UNAVAILABLE`.
    FakeUnavailable,
}
