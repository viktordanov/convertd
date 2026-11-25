//! `convertd engine …`: the engines without HTTP, for debugging and benchmarks (lane 002).

use std::ffi::OsString;
use std::process::ExitCode;

/// The hidden `engine` subcommands (INTERFACES.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCommand {
    Convert,
    Inspect,
    Render,
    Assemble,
    Decrypt,
    Bench,
    Soak,
}

impl EngineCommand {
    /// The name used on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            EngineCommand::Convert => "convert",
            EngineCommand::Inspect => "inspect",
            EngineCommand::Render => "render",
            EngineCommand::Assemble => "assemble",
            EngineCommand::Decrypt => "decrypt",
            EngineCommand::Bench => "bench",
            EngineCommand::Soak => "soak",
        }
    }
}

/// Runs one engine command. `args` are the arguments after the command
/// name, unparsed, so this lane defines its own flags without touching
/// `main.rs`.
///
/// `bench`, `convert`, `inspect`, `render` and `decrypt` exist; `soak`
/// (plan 002 step 13) and `assemble` (image inputs are out of v1) answer
/// "not implemented yet".
pub fn run(command: EngineCommand, args: Vec<OsString>) -> ExitCode {
    match command {
        EngineCommand::Bench => bench::run(args),
        EngineCommand::Convert
        | EngineCommand::Inspect
        | EngineCommand::Render
        | EngineCommand::Decrypt => ops::run(command, args),
        other => {
            eprintln!("convertd engine {}: not implemented yet", other.as_str());
            ExitCode::from(2)
        }
    }
}

/// `convertd engine convert|inspect|render|decrypt`: one engine call through
/// a pool of one worker and the engine adapter, with no HTTP and no domain
/// service. Each call prints one JSON line with its timing.
pub mod ops {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use clap::Parser;

    use super::EngineCommand;
    use crate::domain::conversion::{ExportOptions, ImageFormat, SlideNotes};
    use crate::domain::engine::{
        Admission, DecryptJob, EngineTiming, InspectJob, OfficeEngine, OfficeJob, PdfEngine,
        RenderJob, WorkerRef,
    };
    use crate::domain::secret::Password;
    use crate::engines::config::Paper;
    use crate::engines::office_lok::OfficeLok;
    use crate::engines::pdf_pdfium::PdfPdfium;
    use crate::engines::pool::{Pool, PoolSpec};
    use crate::engines::process::{ReaperMode, Spawner};
    use crate::engines::slot::WorkerEnv;
    use crate::ipc::{Init, PROTOCOL_VERSION};
    use crate::worker::Role;

    /// The deadline of one call.
    const CALL: Duration = Duration::from_secs(210);

    #[derive(Debug, Parser)]
    #[command(name = "convertd engine convert")]
    struct ConvertArgs {
        input: PathBuf,
        output: PathBuf,
        /// The environment variable that holds the password (never argv).
        #[arg(long)]
        password_env: Option<String>,
        /// none, append or only.
        #[arg(long, default_value = "none")]
        notes: String,
        #[arg(long)]
        hidden_slides: bool,
        #[arg(long)]
        one_page_per_sheet: bool,
        /// Calls, each on a fresh lease of the same worker.
        #[arg(long, default_value_t = 1)]
        n: usize,
    }

    #[derive(Debug, Parser)]
    #[command(name = "convertd engine inspect")]
    struct InspectArgs {
        pdf: PathBuf,
        #[arg(long)]
        probe_dpi: Option<u16>,
        #[arg(long)]
        password_env: Option<String>,
        #[arg(long, default_value_t = 1)]
        n: usize,
    }

    #[derive(Debug, Parser)]
    #[command(name = "convertd engine render")]
    struct RenderArgs {
        pdf: PathBuf,
        out_dir: PathBuf,
        /// `1-5`, `1,3,5` or both; all pages when left out.
        #[arg(long)]
        pages: Option<String>,
        /// jpeg, png or webp.
        #[arg(long, default_value = "jpeg")]
        format: String,
        #[arg(long, default_value_t = 150)]
        dpi: u16,
        #[arg(long, default_value_t = 85)]
        quality: u8,
        #[arg(long, default_value_t = 0)]
        max_edge: u32,
        #[arg(long)]
        password_env: Option<String>,
        /// Render workers; the pages are split over them.
        #[arg(long, default_value_t = 1)]
        workers: u16,
        #[arg(long, default_value_t = 1)]
        n: usize,
    }

    #[derive(Debug, Parser)]
    #[command(name = "convertd engine decrypt")]
    struct DecryptArgs {
        pdf: PathBuf,
        output: PathBuf,
        #[arg(long)]
        password_env: String,
    }

    fn parse<T: Parser>(name: &str, args: Vec<OsString>) -> Result<T, ExitCode> {
        T::try_parse_from(std::iter::once(OsString::from(name)).chain(args)).map_err(|err| {
            let _ = err.print();
            ExitCode::from(2)
        })
    }

    pub fn run(command: EngineCommand, args: Vec<OsString>) -> ExitCode {
        let runtime = match crate::runtime::serve(None) {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("convertd engine: failed to build the runtime: {err}");
                return ExitCode::FAILURE;
            }
        };
        let result = match command {
            EngineCommand::Convert => match parse::<ConvertArgs>("convert", args) {
                Ok(a) => runtime.block_on(convert(a)),
                Err(code) => return code,
            },
            EngineCommand::Inspect => match parse::<InspectArgs>("inspect", args) {
                Ok(a) => runtime.block_on(inspect(a)),
                Err(code) => return code,
            },
            EngineCommand::Render => match parse::<RenderArgs>("render", args) {
                Ok(a) => runtime.block_on(render(a)),
                Err(code) => return code,
            },
            EngineCommand::Decrypt => match parse::<DecryptArgs>("decrypt", args) {
                Ok(a) => runtime.block_on(decrypt(a)),
                Err(code) => return code,
            },
            _ => Err("not an engine operation".into()),
        };
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("convertd engine {}: {err}", command.as_str());
                ExitCode::FAILURE
            }
        }
    }

    fn env_dir(name: &str, default: &str) -> PathBuf {
        std::env::var_os(name).map_or_else(|| PathBuf::from(default), PathBuf::from)
    }

    fn password(var: Option<&str>) -> Result<Option<Password>, String> {
        var.map(|name| {
            std::env::var(name)
                .map(Password::new)
                .map_err(|_| format!("failed to read the password: {name} is not set"))
        })
        .transpose()
    }

    /// A started pool of `size` workers of `role`, ready.
    async fn pool(role: Role, size: u16, base: &Path) -> Result<Pool, String> {
        let program =
            std::env::current_exe().map_err(|err| format!("failed to find the binary: {err}"))?;
        let paper = match std::env::var("DEFAULT_PAPER").as_deref() {
            Ok("Letter") => Paper::Letter,
            _ => Paper::A4,
        };
        let spec = PoolSpec {
            name: role.as_str(),
            role,
            program,
            workers_dir: base.join("workers"),
            profile_template: std::env::var_os("LO_PROFILE_TEMPLATE").map(PathBuf::from),
            env: WorkerEnv::from_process(paper)
                .with_path_prefix(&env_dir("LO_PROGRAM_DIR", "/opt/libreoffice/program")),
            init: Init {
                protocol: PROTOCOL_VERSION,
                lo_program_dir: Some(env_dir("LO_PROGRAM_DIR", "/opt/libreoffice/program")),
                pdfium_lib_dir: Some(env_dir("PDFIUM_LIB_DIR", "/opt/pdfium/lib")),
                warmup: true,
                ..Init::default()
            },
            size,
            max_jobs: u32::MAX,
            max_rss: u64::MAX,
            kill_rss: u64::MAX,
            max_age: Duration::from_secs(86_400),
            age_check: Duration::from_secs(30),
            queue_capacity: 16,
            queue_wait: Duration::from_secs(60),
            start_timeout: Duration::from_secs(60),
        };
        let spawner = Spawner::new(ReaperMode::Owned)
            .map_err(|err| format!("failed to start the spawner: {err}"))?;
        let pool = Pool::start(spec, spawner);
        if !pool
            .wait_idle(usize::from(size), Duration::from_secs(90))
            .await
        {
            pool.shutdown(Duration::from_secs(1)).await;
            return Err(format!("the {} pool did not get ready", role.as_str()));
        }
        Ok(pool)
    }

    fn base() -> PathBuf {
        std::env::temp_dir().join(format!("convertd-engine-{}", std::process::id()))
    }

    fn line(op: &str, timing: &EngineTiming, workers: &[WorkerRef], extra: &str) {
        let us = |d: Duration| u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        let overhead = timing.wall.saturating_sub(timing.engine + timing.queue);
        let worker = workers
            .iter()
            .map(|w| format!("\"{}-{}\"", w.pool, w.slot))
            .collect::<Vec<_>>()
            .join(",");
        let jobs = workers
            .iter()
            .map(|w| w.job.to_string())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{{\"op\":\"{op}\",\"wall_us\":{},\"engine_us\":{},\"overhead_us\":{},\"queue_us\":{},\"workers\":[{worker}],\"jobs\":[{jobs}]{extra}}}",
            us(timing.wall),
            us(timing.engine),
            us(overhead),
            us(timing.queue),
        );
    }

    async fn convert(args: ConvertArgs) -> Result<(), String> {
        let input = std::fs::canonicalize(&args.input)
            .map_err(|err| format!("failed to open {}: {err}", args.input.display()))?;
        let size = std::fs::metadata(&input)
            .map_err(|err| err.to_string())?
            .len();
        let family = {
            let mut file = std::fs::File::open(&input).map_err(|err| err.to_string())?;
            match crate::detect::detect_file(&mut file, size) {
                Ok(detection) => detection.format.family(),
                // Detection does not read every container yet (encrypted
                // OOXML is OLE2): fall back to the extension here.
                Err(err) => family_by_extension(&input)
                    .ok_or_else(|| format!("failed to detect the format: {err:?}"))?,
            }
        };
        let notes = match args.notes.as_str() {
            "none" => SlideNotes::None,
            "append" => SlideNotes::Append,
            "only" => SlideNotes::Only,
            other => return Err(format!("unknown --notes {other:?}")),
        };
        let output = std::path::absolute(&args.output).map_err(|err| err.to_string())?;
        let password = password(args.password_env.as_deref())?;
        let base = base();
        let pool = pool(Role::Office, 1, &base).await?;
        let office = OfficeLok::new(pool.clone());
        let mut result = Ok(());
        for _ in 0..args.n.max(1) {
            let job = OfficeJob {
                input: input.clone(),
                output: output.clone(),
                family,
                password: password.clone(),
                export: ExportOptions {
                    notes,
                    include_hidden_slides: args.hidden_slides,
                    one_page_per_sheet: args.one_page_per_sheet,
                },
                sha256: [0; 32],
                max_output_bytes: 1 << 30,
            };
            match office
                .convert(job, Admission::Queue, Instant::now() + CALL)
                .await
            {
                Ok(c) => line(
                    "convert",
                    &c.timing,
                    &[c.worker],
                    &format!(
                        ",\"pdf_bytes\":{},\"doc_type\":\"{:?}\"",
                        c.pdf_bytes, c.doc_type
                    ),
                ),
                Err(err) => {
                    result = Err(format!("{err:?}"));
                    break;
                }
            }
        }
        pool.shutdown(Duration::from_secs(2)).await;
        let _ = std::fs::remove_dir_all(&base);
        result
    }

    fn family_by_extension(path: &Path) -> Option<crate::domain::document::Family> {
        use crate::domain::document::Family;
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "docx" | "docm" | "dotx" | "doc" | "odt" | "rtf" => Some(Family::Document),
            "xlsx" | "xlsm" | "xls" | "ods" => Some(Family::Spreadsheet),
            "pptx" | "pptm" | "ppsx" | "potx" | "ppt" | "odp" => Some(Family::Presentation),
            _ => None,
        }
    }

    fn inspect_job(pdf: &Path, password: Option<Password>, probe_dpi: Option<u16>) -> InspectJob {
        InspectJob {
            pdf: pdf.to_path_buf(),
            password,
            probe_dpi,
            collect_fonts: false,
        }
    }

    async fn inspect(args: InspectArgs) -> Result<(), String> {
        let pdf = std::fs::canonicalize(&args.pdf).map_err(|err| err.to_string())?;
        let password = password(args.password_env.as_deref())?;
        let base = base();
        let pool = pool(Role::Render, 1, &base).await?;
        let engine = PdfPdfium::new(pool.clone());
        let mut result = Ok(());
        for _ in 0..args.n.max(1) {
            match engine
                .inspect(
                    inspect_job(&pdf, password.clone(), args.probe_dpi),
                    Admission::Queue,
                    Instant::now() + CALL,
                )
                .await
            {
                Ok(i) => {
                    let first = i
                        .pages
                        .first()
                        .map_or((0.0, 0.0), |p| (p.width_pt, p.height_pt));
                    let blank = i.probe.iter().filter(|p| p.objects > 0 && p.flat).count();
                    line(
                        "inspect",
                        &i.timing,
                        &[i.worker],
                        &format!(
                            ",\"page_count\":{},\"encrypted\":{},\"needs_password\":{},\"page1_pt\":[{},{}],\"probe_us\":{},\"blank_pages\":{blank}",
                            i.page_count,
                            i.encrypted,
                            i.needs_password,
                            first.0,
                            first.1,
                            i.probe_engine.as_micros()
                        ),
                    );
                }
                Err(err) => {
                    result = Err(format!("{err:?}"));
                    break;
                }
            }
        }
        pool.shutdown(Duration::from_secs(2)).await;
        let _ = std::fs::remove_dir_all(&base);
        result
    }

    /// Parses `1-5,7` into sorted, de-duplicated page numbers.
    pub fn parse_pages(spec: &str, count: u32) -> Result<Vec<u32>, String> {
        let mut pages = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (from, to) = match part.split_once('-') {
                Some((a, "")) => (a, count.to_string()),
                Some((a, b)) => (a, b.to_string()),
                None => (part, part.to_string()),
            };
            let from: u32 = from
                .trim()
                .parse()
                .map_err(|_| format!("bad page {part:?}"))?;
            let to: u32 = to
                .trim()
                .parse()
                .map_err(|_| format!("bad page {part:?}"))?;
            if from == 0 || from > to || to > count {
                return Err(format!("page range {part:?} is outside 1-{count}"));
            }
            pages.extend(from..=to);
        }
        pages.sort_unstable();
        pages.dedup();
        Ok(pages)
    }

    async fn render(args: RenderArgs) -> Result<(), String> {
        let pdf = std::fs::canonicalize(&args.pdf).map_err(|err| err.to_string())?;
        let format = match args.format.as_str() {
            "jpeg" | "jpg" => ImageFormat::Jpeg,
            "png" => ImageFormat::Png,
            "webp" => ImageFormat::Webp,
            other => return Err(format!("unknown --format {other:?}")),
        };
        std::fs::create_dir_all(&args.out_dir).map_err(|err| err.to_string())?;
        let out_dir = std::fs::canonicalize(&args.out_dir).map_err(|err| err.to_string())?;
        let password = password(args.password_env.as_deref())?;
        let base = base();
        let pool = pool(Role::Render, args.workers.max(1), &base).await?;
        let engine = PdfPdfium::new(pool.clone());
        let mut result = Ok(());
        let count = engine
            .inspect(
                inspect_job(&pdf, password.clone(), None),
                Admission::Queue,
                Instant::now() + CALL,
            )
            .await
            .map_err(|err| format!("{err:?}"))?
            .page_count;
        let pages = match &args.pages {
            Some(spec) => parse_pages(spec, count)?,
            None => (1..=count).collect(),
        };
        for _ in 0..args.n.max(1) {
            let job = RenderJob {
                pdf: pdf.clone(),
                password: password.clone(),
                pages: pages.clone(),
                format,
                dpi: args.dpi,
                quality: args.quality,
                max_edge: args.max_edge,
                out_dir: out_dir.clone(),
                sha256: [0; 32],
                max_output_bytes: 1 << 30,
            };
            match engine
                .render(job, Admission::Queue, Instant::now() + CALL)
                .await
            {
                Ok(r) => {
                    let bytes: u64 = r.pages.iter().map(|p| p.size_bytes).sum();
                    line(
                        "render",
                        &r.timing,
                        &r.workers,
                        &format!(",\"pages\":{},\"bytes\":{bytes}", r.pages.len()),
                    );
                }
                Err(err) => {
                    result = Err(format!("{err:?}"));
                    break;
                }
            }
        }
        pool.shutdown(Duration::from_secs(2)).await;
        let _ = std::fs::remove_dir_all(&base);
        result
    }

    async fn decrypt(args: DecryptArgs) -> Result<(), String> {
        let pdf = std::fs::canonicalize(&args.pdf).map_err(|err| err.to_string())?;
        let output = std::path::absolute(&args.output).map_err(|err| err.to_string())?;
        let password = password(Some(&args.password_env))?
            .ok_or_else(|| "failed to read the password".to_string())?;
        let base = base();
        let pool = pool(Role::Render, 1, &base).await?;
        let engine = PdfPdfium::new(pool.clone());
        let result = engine
            .decrypt(
                DecryptJob {
                    pdf,
                    password,
                    output,
                    max_output_bytes: 1 << 30,
                },
                Admission::Queue,
                Instant::now() + CALL,
            )
            .await
            .map(|d| {
                line(
                    "decrypt",
                    &d.timing,
                    &[d.worker],
                    &format!(",\"pdf_bytes\":{}", d.pdf_bytes),
                )
            })
            .map_err(|err| format!("{err:?}"));
        pool.shutdown(Duration::from_secs(2)).await;
        let _ = std::fs::remove_dir_all(&base);
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn page_ranges() {
            assert_eq!(parse_pages("1-3,5", 9).unwrap(), vec![1, 2, 3, 5]);
            assert_eq!(parse_pages("7-", 9).unwrap(), vec![7, 8, 9]);
            assert_eq!(parse_pages("2,2,1", 3).unwrap(), vec![1, 2]);
            assert!(parse_pages("0", 3).is_err());
            assert!(parse_pages("2-9", 3).is_err());
            assert!(parse_pages("x", 3).is_err());
        }
    }
}

/// `convertd engine bench`: the supervisor's own cost per call and per
/// worker start, with no engine time (plan 002 step 6).
pub mod bench {
    use std::ffi::OsString;
    use std::process::ExitCode;
    use std::time::Duration;

    use clap::Parser;

    /// The flags of `convertd engine bench`.
    #[derive(Debug, Parser)]
    #[command(name = "convertd engine bench")]
    pub struct BenchArgs {
        /// The worker role: `test` (needs feature `test-hooks`), `office`
        /// (LibreOffice at `LO_PROGRAM_DIR`) or `render` (PDFium at
        /// `PDFIUM_LIB_DIR`).
        #[arg(long, default_value = "test")]
        pub role: String,
        /// Measured lease + ping + return round trips.
        #[arg(long, default_value_t = 10_000)]
        pub n: usize,
        /// Concurrent callers; the pool has this many workers.
        #[arg(long, default_value_t = 1)]
        pub concurrency: usize,
        /// Measured worker starts (spawn to `Ready`).
        #[arg(long, default_value_t = 50)]
        pub spawns: usize,
        /// Untimed round trips before measuring.
        #[arg(long, default_value_t = 500)]
        pub warmup: usize,
        /// Office: convert this file to PDF three times; render: render its
        /// first page as JPEG at 150 DPI three times. Prints each job's
        /// engine and wall time, so the first job shows against the third.
        #[arg(long)]
        pub file: Option<std::path::PathBuf>,
    }

    /// Percentiles of a sample, in microseconds.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Summary {
        pub n: usize,
        pub p50: f64,
        pub p95: f64,
        pub p99: f64,
        pub max: f64,
        pub mean: f64,
    }

    /// Nearest-rank percentiles over `samples` (any order).
    pub fn summarize(samples: &mut [Duration]) -> Summary {
        samples.sort_unstable();
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        let rank = |p: f64| {
            let n = samples.len();
            let i = ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
            us(samples[i])
        };
        if samples.is_empty() {
            return Summary {
                n: 0,
                p50: 0.0,
                p95: 0.0,
                p99: 0.0,
                max: 0.0,
                mean: 0.0,
            };
        }
        let total: Duration = samples.iter().sum();
        Summary {
            n: samples.len(),
            p50: rank(0.50),
            p95: rank(0.95),
            p99: rank(0.99),
            max: us(samples[samples.len() - 1]),
            mean: us(total) / samples.len() as f64,
        }
    }

    impl Summary {
        pub fn json(&self) -> String {
            format!(
                "{{\"n\":{},\"p50\":{:.1},\"p95\":{:.1},\"p99\":{:.1},\"max\":{:.1},\"mean\":{:.1}}}",
                self.n, self.p50, self.p95, self.p99, self.max, self.mean
            )
        }

        pub fn row(&self, name: &str) -> String {
            format!(
                "{name:<22} {:>7} {:>9.1} {:>9.1} {:>9.1} {:>9.1}",
                self.n, self.p50, self.p95, self.p99, self.max
            )
        }
    }

    pub fn run(args: Vec<OsString>) -> ExitCode {
        let argv = std::iter::once(OsString::from("convertd engine bench")).chain(args);
        let args = match BenchArgs::try_parse_from(argv) {
            Ok(args) => args,
            Err(err) => {
                let _ = err.print();
                return ExitCode::from(2);
            }
        };
        let role = match args.role.as_str() {
            #[cfg(feature = "test-hooks")]
            "test" => crate::worker::Role::Test,
            #[cfg(not(feature = "test-hooks"))]
            "test" => {
                eprintln!(
                    "convertd engine bench: --role test needs a build with --features test-hooks"
                );
                return ExitCode::from(2);
            }
            "office" => crate::worker::Role::Office,
            "render" => crate::worker::Role::Render,
            other => {
                eprintln!("convertd engine bench: unknown role {other:?}");
                return ExitCode::from(2);
            }
        };
        bench_role(&args, role)
    }

    fn bench_role(args: &BenchArgs, role: crate::worker::Role) -> ExitCode {
        let runtime = match crate::runtime::serve(None) {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("convertd engine bench: failed to build the runtime: {err}");
                return ExitCode::FAILURE;
            }
        };
        match runtime.block_on(measure(args, role)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("convertd engine bench: {err}");
                ExitCode::FAILURE
            }
        }
    }

    async fn measure(args: &BenchArgs, role: crate::worker::Role) -> Result<(), String> {
        use std::time::Instant;

        use crate::engines::config::Paper;
        use crate::engines::pool::{Admission, Pool, PoolSpec, start_once};
        use crate::engines::process::{ReaperMode, Spawner};
        use crate::engines::slot::WorkerEnv;
        use crate::ipc::{Init, Op, PROTOCOL_VERSION};
        use crate::worker::Role;

        let concurrency = args.concurrency.max(1);
        let base = std::env::temp_dir().join(format!("convertd-bench-{}", std::process::id()));
        let program =
            std::env::current_exe().map_err(|err| format!("failed to find the binary: {err}"))?;
        let dir_var = |name: &str, default: &str| {
            std::env::var_os(name).map_or_else(|| std::path::PathBuf::from(default), Into::into)
        };
        let init = match role {
            Role::Office => Init {
                protocol: PROTOCOL_VERSION,
                lo_program_dir: Some(dir_var("LO_PROGRAM_DIR", "/opt/libreoffice/program")),
                warmup: true,
                ..Init::default()
            },
            Role::Render => Init {
                protocol: PROTOCOL_VERSION,
                pdfium_lib_dir: Some(dir_var("PDFIUM_LIB_DIR", "/opt/pdfium/lib")),
                warmup: true,
                ..Init::default()
            },
            #[cfg(feature = "test-hooks")]
            Role::Test => Init {
                protocol: PROTOCOL_VERSION,
                ..Init::default()
            },
        };
        let spec = PoolSpec {
            name: role.as_str(),
            role,
            program,
            workers_dir: base.join("workers"),
            profile_template: None,
            env: WorkerEnv::from_process(Paper::A4)
                .with_path_prefix(&dir_var("LO_PROGRAM_DIR", "/opt/libreoffice/program")),
            init,
            size: u16::try_from(concurrency).map_err(|_| "too many workers".to_string())?,
            max_jobs: u32::MAX,
            max_rss: u64::MAX,
            kill_rss: u64::MAX,
            max_age: Duration::from_secs(86_400),
            age_check: Duration::from_secs(30),
            queue_capacity: concurrency * 2,
            queue_wait: Duration::from_secs(10),
            start_timeout: Duration::from_secs(60),
        };
        let spawner = Spawner::new(ReaperMode::Owned)
            .map_err(|err| format!("failed to start the spawner: {err}"))?;

        // Spawn to Ready, one at a time, on the pool's own start path.
        let mut prepare = Vec::with_capacity(args.spawns);
        let mut spawn = Vec::with_capacity(args.spawns);
        let mut bind = Vec::with_capacity(args.spawns);
        let mut rss = Vec::with_capacity(args.spawns);
        for i in 0..args.spawns {
            let slot = u16::try_from(1000 + i % 8).unwrap_or(1000);
            let times = start_once(&spec, &spawner, slot).await?;
            prepare.push(times.prepare);
            spawn.push(times.spawn);
            bind.push(times.bind);
            rss.push(times.rss_bytes);
        }

        let pool = Pool::start(spec, spawner);
        if !pool.wait_idle(concurrency, Duration::from_secs(60)).await {
            pool.shutdown(Duration::from_secs(1)).await;
            return Err(format!("the pool did not get ready: {:?}", pool.status()));
        }
        let round = |pool: Pool, count: usize| async move {
            let mut total = Vec::with_capacity(count);
            let mut lease_only = Vec::with_capacity(count);
            let mut call_only = Vec::with_capacity(count);
            for _ in 0..count {
                let t0 = Instant::now();
                let deadline = t0 + Duration::from_secs(10);
                let mut lease = pool
                    .lease(deadline, Admission::Queue)
                    .await
                    .map_err(|err| err.to_string())?;
                let t1 = Instant::now();
                lease
                    .call(Op::Ping, deadline)
                    .await
                    .map_err(|err| err.to_string())?
                    .result
                    .map_err(|err| err.to_string())?;
                let t2 = Instant::now();
                drop(lease);
                total.push(t0.elapsed());
                lease_only.push(t1 - t0);
                call_only.push(t2 - t1);
            }
            Ok::<_, String>((total, lease_only, call_only))
        };
        let per_task = |n: usize| n.div_ceil(concurrency);
        let run_all = |n: usize| {
            let tasks: Vec<_> = (0..concurrency)
                .map(|_| tokio::spawn(round(pool.clone(), per_task(n))))
                .collect();
            async move {
                let mut all = (Vec::new(), Vec::new(), Vec::new());
                for task in tasks {
                    let (t, l, c) = task.await.map_err(|err| err.to_string())??;
                    all.0.extend(t);
                    all.1.extend(l);
                    all.2.extend(c);
                }
                Ok::<_, String>(all)
            }
        };
        run_all(args.warmup).await?;
        let wall = Instant::now();
        let (mut total, mut lease_only, mut call_only) = run_all(args.n).await?;
        let wall = wall.elapsed();
        let pool_status = pool.status();
        let jobs = match &args.file {
            Some(file) => real_jobs(&pool, role, file, &base).await?,
            None => Vec::new(),
        };
        pool.shutdown(Duration::from_secs(1)).await;
        let _ = std::fs::remove_dir_all(&base);

        let total = summarize(&mut total);
        let lease_only = summarize(&mut lease_only);
        let call_only = summarize(&mut call_only);
        let prepare = summarize(&mut prepare);
        let spawn = summarize(&mut spawn);
        let bind = summarize(&mut bind);
        rss.sort_unstable();
        let rss_p50 = rss.get(rss.len() / 2).copied().unwrap_or(0);

        eprintln!(
            "convertd engine bench --role {}: n={} concurrency={} spawns={} ({:.0} calls/s, {:?})",
            role.as_str(),
            total.n,
            concurrency,
            spawn.n,
            total.n as f64 / wall.as_secs_f64(),
            pool_status
        );
        eprintln!(
            "{:<22} {:>7} {:>9} {:>9} {:>9} {:>9}",
            "microseconds", "n", "p50", "p95", "p99", "max"
        );
        eprintln!("{}", total.row("lease+ping+return"));
        eprintln!("{}", lease_only.row("  lease"));
        eprintln!("{}", call_only.row("  ping call"));
        eprintln!("{}", spawn.row("spawn to ready"));
        eprintln!("{}", bind.row("  bind (in worker)"));
        eprintln!("{}", prepare.row("slot prepare"));
        eprintln!("worker rss at ready, p50: {rss_p50} bytes");
        for (i, (engine, wall)) in jobs.iter().enumerate() {
            eprintln!(
                "job {}: engine {:.1} ms, wall {:.1} ms",
                i + 1,
                engine.as_secs_f64() * 1e3,
                wall.as_secs_f64() * 1e3
            );
        }
        println!(
            "{{\"op\":\"bench\",\"role\":\"{}\",\"concurrency\":{concurrency},\"lease_ping_us\":{},\"lease_us\":{},\"call_us\":{},\"spawn_us\":{},\"bind_us\":{},\"prepare_us\":{},\"rss_bytes_p50\":{rss_p50}}}",
            role.as_str(),
            total.json(),
            lease_only.json(),
            call_only.json(),
            spawn.json(),
            bind.json(),
            prepare.json()
        );
        Ok(())
    }

    /// Three real jobs on `file`, each on a fresh lease: (engine, wall).
    async fn real_jobs(
        pool: &crate::engines::pool::Pool,
        role: crate::worker::Role,
        file: &std::path::Path,
        base: &std::path::Path,
    ) -> Result<Vec<(Duration, Duration)>, String> {
        use std::time::Instant;

        use crate::engines::pool::Admission;
        use crate::ipc::{ConvertArgs, ExportOptions, Family, ImageFormat, Op, RenderArgs};
        use crate::worker::Role;

        let input = std::fs::canonicalize(file)
            .map_err(|err| format!("failed to open {}: {err}", file.display()))?;
        let family = match input.extension().and_then(|e| e.to_str()) {
            Some("xlsx" | "xls" | "ods" | "csv") => Family::Spreadsheet,
            Some("pptx" | "ppt" | "odp") => Family::Presentation,
            _ => Family::Document,
        };
        let mut out = Vec::new();
        for i in 0..3 {
            let op = match role {
                Role::Office => Op::Convert(ConvertArgs {
                    input: input.clone(),
                    output: base.join(format!("job-{i}.pdf")),
                    family,
                    password: None,
                    export: ExportOptions::default(),
                    max_output_bytes: 1 << 30,
                }),
                _ => {
                    let out_dir = base.join(format!("pages-{i}"));
                    std::fs::create_dir_all(&out_dir).map_err(|err| err.to_string())?;
                    Op::Render(RenderArgs {
                        pdf: input.clone(),
                        password: None,
                        pages: vec![1],
                        format: ImageFormat::Jpeg,
                        dpi: 150,
                        quality: 85,
                        max_edge: 0,
                        out_dir,
                        max_output_bytes: 1 << 30,
                    })
                }
            };
            let started = Instant::now();
            let deadline = started + Duration::from_secs(120);
            let mut lease = pool
                .lease(deadline, Admission::Queue)
                .await
                .map_err(|err| err.to_string())?;
            let reply = lease
                .call(op, deadline)
                .await
                .map_err(|err| err.to_string())?;
            if let Err(err) = &reply.result {
                return Err(format!("job {} failed: {err}", i + 1));
            }
            out.push((
                Duration::from_micros(reply.engine.engine_us),
                started.elapsed(),
            ));
        }
        Ok(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn nearest_rank_percentiles() {
            let mut samples: Vec<Duration> = (1..=100).map(Duration::from_micros).collect();
            let s = summarize(&mut samples);
            assert_eq!((s.p50, s.p95, s.p99, s.max), (50.0, 95.0, 99.0, 100.0));
            assert!((s.mean - 50.5).abs() < 1e-9);
            assert_eq!(summarize(&mut []).n, 0);
        }

        #[test]
        fn flags_parse() {
            let args = BenchArgs::try_parse_from([
                "bench",
                "--role",
                "test",
                "--n",
                "10",
                "--concurrency",
                "2",
            ])
            .unwrap();
            assert_eq!((args.n, args.concurrency, args.spawns), (10, 2, 50));
        }
    }
}
