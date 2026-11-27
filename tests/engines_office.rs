//! The office worker with the real LibreOffice, through the pool and the
//! `OfficeEngine` adapter.
//!
//! These tests run where `LO_PROGRAM_DIR` holds LibreOffice, as in the dev
//! image. Elsewhere they print `skipped` and pass without checking
//! anything. Tests that count pages also need PDFium (`PDFIUM_LIB_DIR`);
//! tests that use corpus files need `CONVERTD_CORPUS_DIR` (the output
//! directory of `corpus/generate.py`).

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use convertd::domain::conversion::{ExportOptions, ImageFormat, SlideNotes};
use convertd::domain::document::Family;
use convertd::domain::engine::{
    Admission as DomainAdmission, EngineError, InspectJob, OfficeEngine, OfficeJob, PdfEngine,
    RenderJob,
};
use convertd::domain::pages::hidden_pages;
use convertd::domain::secret::Password;
use convertd::domain::verification::Slides;
use convertd::engines::config::Paper;
use convertd::engines::office_lok::OfficeLok;
use convertd::engines::pdf_pdfium::PdfPdfium;
use convertd::engines::pool::{Admission, Lease, Pool, PoolSpec};
use convertd::engines::process::{ReaperMode, Spawner};
use convertd::engines::slot::WorkerEnv;
use convertd::ipc::{self, Init, Op, Output, PROTOCOL_VERSION};
use convertd::worker::Role;
use convertd::worker::warmup;

/// The bound of one test: office workers take seconds to start.
const TEST: Duration = Duration::from_secs(240);
/// The deadline of one engine call.
const CALL: Duration = Duration::from_secs(60);

async fn within<T>(limit: Duration, what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(limit, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {limit:?}: {what}"),
    }
}

fn dir_from(var: &str, probe: &str, what: &str) -> Option<PathBuf> {
    let Some(dir) = std::env::var_os(var).map(PathBuf::from) else {
        eprintln!("skipped: no {what} ({var} is not set)");
        return None;
    };
    if dir.join(probe).exists() {
        Some(dir)
    } else {
        eprintln!("skipped: no {what} (no {probe} in {})", dir.display());
        None
    }
}

fn lo() -> Option<PathBuf> {
    dir_from("LO_PROGRAM_DIR", "soffice.bin", "engine")
}

fn pdfium() -> Option<PathBuf> {
    dir_from("PDFIUM_LIB_DIR", "libpdfium.so", "engine")
}

fn corpus() -> Option<PathBuf> {
    dir_from("CONVERTD_CORPUS_DIR", "docx/small.docx", "corpus")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("convertd-002-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn base_spec(dir: &Path, name: &'static str, role: Role, init: Init) -> PoolSpec {
    PoolSpec {
        name,
        role,
        program: PathBuf::from(env!("CARGO_BIN_EXE_convertd")),
        workers_dir: dir.join("workers"),
        profile_template: None,
        env: WorkerEnv::from_vars(std::env::vars_os(), Paper::A4),
        init,
        size: 1,
        max_jobs: 1_000,
        max_rss: u64::MAX,
        kill_rss: u64::MAX,
        max_age: Duration::from_secs(3600),
        age_check: Duration::from_secs(30),
        queue_capacity: 4,
        queue_wait: Duration::from_secs(30),
        start_timeout: Duration::from_secs(90),
        oom_events: None,
    }
}

fn office_spec(dir: &Path, lo: &Path, warmup: bool) -> PoolSpec {
    let mut spec = base_spec(
        dir,
        "office",
        Role::Office,
        Init {
            protocol: PROTOCOL_VERSION,
            lo_program_dir: Some(lo.to_path_buf()),
            warmup,
            ..Init::default()
        },
    );
    spec.env = spec.env.with_path_prefix(lo);
    spec
}

fn render_spec(dir: &Path, pdfium: &Path) -> PoolSpec {
    base_spec(
        dir,
        "render",
        Role::Render,
        Init {
            protocol: PROTOCOL_VERSION,
            pdfium_lib_dir: Some(pdfium.to_path_buf()),
            warmup: true,
            ..Init::default()
        },
    )
}

async fn ready(spec: PoolSpec) -> Pool {
    let size = spec.size as usize;
    let pool = Pool::start(spec, Spawner::new(ReaperMode::Owned).unwrap());
    assert!(
        pool.wait_idle(size, Duration::from_secs(120)).await,
        "the pool did not get ready: {:?}",
        pool.status()
    );
    pool
}

async fn lease(pool: &Pool) -> Lease {
    within(
        Duration::from_secs(120),
        "a worker",
        pool.lease(Instant::now() + Duration::from_secs(120), Admission::Queue),
    )
    .await
    .expect("a lease")
}

fn deadline() -> Instant {
    Instant::now() + CALL
}

/// A DOCX with `pages` pages, one paragraph each, divided by page breaks.
fn docx_pages(pages: usize) -> Vec<u8> {
    let body: String = (1..=pages)
        .map(|n| {
            let brk = if n < pages {
                r#"<w:r><w:br w:type="page"/></w:r>"#
            } else {
                ""
            };
            format!("<w:p><w:r><w:t>Page {n}.</w:t></w:r>{brk}</w:p>")
        })
        .collect();
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in [
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.to_owned(),
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_owned(),
        ),
        (
            "word/document.xml",
            format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#),
        ),
    ] {
        zip.start_file(name, stored).unwrap();
        zip.write_all(data.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn job(input: PathBuf, output: PathBuf, family: Family) -> OfficeJob {
    OfficeJob {
        input,
        output,
        family,
        password: None,
        export: ExportOptions::default(),
        sha256: [0; 32],
        max_output_bytes: 256 << 20,
    }
}

fn convert_op(input: PathBuf, output: PathBuf, family: ipc::Family) -> Op {
    Op::Convert(ipc::ConvertArgs {
        input,
        output,
        family,
        password: None,
        export: ipc::ExportOptions::default(),
        max_output_bytes: 64 << 20,
    })
}

/// The page count of `pdf`, from the render pool.
async fn pages_of(pdf: &PdfPdfium, path: &Path) -> u32 {
    pdf.inspect(
        InspectJob {
            pdf: path.to_path_buf(),
            password: None,
            probe_dpi: None,
            collect_fonts: false,
            probe_max_pages: u32::MAX,
            sha256: [0; 32],
            export: None,
        },
        DomainAdmission::Queue,
        deadline(),
    )
    .await
    .expect("an inspection")
    .page_count
}

fn copy_in(corpus: &Path, rel: &str, dir: &Scratch, name: &str) -> PathBuf {
    let to = dir.join(name);
    std::fs::copy(corpus.join(rel), &to).unwrap();
    to
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_docx_converts_through_the_office_pool() {
    let Some(lo) = lo() else { return };
    within(TEST, "a_docx_converts_through_the_office_pool", async {
        let dir = Scratch::new("office-docx");
        let source = dir.join("source.docx");
        std::fs::write(&source, warmup::docx("Hello from convertd.")).unwrap();
        let started = Instant::now();
        let pool = ready(office_spec(dir.path(), &lo, false)).await;
        let ready_after = started.elapsed();
        let mut times = Vec::new();
        for round in 1..=3 {
            let output = dir.join(&format!("document-{round}.pdf"));
            let mut lease = lease(&pool).await;
            let reply = lease
                .call(
                    convert_op(source.clone(), output.clone(), ipc::Family::Document),
                    deadline(),
                )
                .await
                .expect("a reply");
            let Ok(Output::Converted(converted)) = reply.result else {
                panic!("expected converted, got {:?}", reply.result);
            };
            let pdf = std::fs::read(&output).unwrap();
            assert!(pdf.starts_with(b"%PDF-"), "not a PDF");
            assert_eq!(converted.pdf_bytes, pdf.len() as u64);
            assert_eq!(converted.doc_type, "text");
            assert!(converted.facts.is_some(), "pre-flight ran");
            times.push(reply.engine.engine_us);
        }
        eprintln!("no warm-up: ready after {ready_after:?}; engine us per job {times:?}");
        pool.shutdown(Duration::from_secs(5)).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_warm_up_makes_the_first_job_of_each_family_cheap() {
    let Some(lo) = lo() else { return };
    within(
        TEST,
        "slow_warm_up_makes_the_first_job_of_each_family_cheap",
        async {
            let dir = Scratch::new("office-warmup");
            let started = Instant::now();
            let pool = ready(office_spec(dir.path(), &lo, true)).await;
            eprintln!("with warm-up: ready after {:?}", started.elapsed());
            let docs: [(&str, Vec<u8>, ipc::Family); 3] = [
                (
                    "docx",
                    warmup::docx("A real document."),
                    ipc::Family::Document,
                ),
                (
                    "xlsx",
                    warmup::xlsx("A real sheet"),
                    ipc::Family::Spreadsheet,
                ),
                (
                    "pptx",
                    warmup::pptx("A real slide."),
                    ipc::Family::Presentation,
                ),
            ];
            for (ext, bytes, family) in docs {
                let source = dir.join(&format!("source.{ext}"));
                std::fs::write(&source, bytes).unwrap();
                let mut times = Vec::new();
                for round in 1..=3 {
                    let output = dir.join(&format!("{ext}-{round}.pdf"));
                    let mut lease = lease(&pool).await;
                    let reply = lease
                        .call(convert_op(source.clone(), output, family), deadline())
                        .await
                        .expect("a reply");
                    assert!(
                        matches!(reply.result, Ok(Output::Converted(_))),
                        "{ext}: {:?}",
                        reply.result
                    );
                    times.push(reply.engine.engine_us);
                }
                eprintln!("{ext}: engine us per job {times:?}");
                // The first job may cost a little more, never a cold start.
                assert!(
                    times[0] <= times[2] * 3 + 100_000,
                    "{ext}: first job {} us against third {} us",
                    times[0],
                    times[2]
                );
            }
            pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_docx_needs_the_right_password_in_one_worker() {
    let (Some(lo), Some(corpus)) = (lo(), corpus()) else {
        return;
    };
    within(
        TEST,
        "an_encrypted_docx_needs_the_right_password_in_one_worker",
        async {
            let dir = Scratch::new("office-password");
            let source = copy_in(&corpus, "docx/encrypted.docx", &dir, "source.docx");
            let pool = ready(office_spec(dir.path(), &lo, false)).await;
            let office = OfficeLok::new(pool.clone());
            let attempt = |password: Option<&str>| {
                let mut job = job(source.clone(), dir.join("document.pdf"), Family::Document);
                job.password = password.map(Password::new);
                office.convert(job, DomainAdmission::Queue, deadline())
            };
            assert_eq!(
                attempt(None).await.unwrap_err(),
                EngineError::PasswordRequired
            );
            assert_eq!(
                attempt(Some("wrong")).await.unwrap_err(),
                EngineError::InvalidPassword
            );
            let converted = attempt(Some("secret")).await.expect("the right password");
            assert!(converted.pdf_bytes > 0);
            // All three ran in the first worker: the password errors do not recycle.
            assert_eq!(converted.worker.generation, 1, "{:?}", converted.worker);
            assert_eq!(converted.worker.job, 3);
            pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_corpus_files_convert_with_their_page_counts() {
    let (Some(lo), Some(pdfium), Some(corpus)) = (lo(), pdfium(), corpus()) else {
        return;
    };
    within(
        TEST,
        "slow_corpus_files_convert_with_their_page_counts",
        async {
            let dir = Scratch::new("office-corpus");
            let office_pool = ready(office_spec(dir.path(), &lo, true)).await;
            let render_pool = ready(render_spec(dir.path(), &pdfium)).await;
            let office = OfficeLok::new(office_pool.clone());
            let pdf = PdfPdfium::new(render_pool.clone());
            let cases: [(&str, Family, ExportOptions, Option<u32>); 8] = [
                (
                    "docx/small.docx",
                    Family::Document,
                    ExportOptions::default(),
                    None,
                ),
                (
                    "docx/long.docx",
                    Family::Document,
                    ExportOptions::default(),
                    None,
                ),
                (
                    "pptx/deck20.pptx",
                    Family::Presentation,
                    ExportOptions::default(),
                    Some(20),
                ),
                (
                    "pptx/deck20.pptx",
                    Family::Presentation,
                    ExportOptions {
                        notes: SlideNotes::Append,
                        ..ExportOptions::default()
                    },
                    Some(40),
                ),
                (
                    "pptx/deck20.pptx",
                    Family::Presentation,
                    ExportOptions {
                        notes: SlideNotes::Only,
                        ..ExportOptions::default()
                    },
                    Some(20),
                ),
                (
                    "pptx/hidden.pptx",
                    Family::Presentation,
                    ExportOptions::default(),
                    Some(3),
                ),
                (
                    "pptx/hidden.pptx",
                    Family::Presentation,
                    ExportOptions {
                        include_hidden_slides: true,
                        ..ExportOptions::default()
                    },
                    Some(5),
                ),
                (
                    "xlsx/sheet3.xlsx",
                    Family::Spreadsheet,
                    ExportOptions {
                        one_page_per_sheet: true,
                        ..ExportOptions::default()
                    },
                    Some(3),
                ),
            ];
            let mut results = Vec::new();
            for (i, (rel, family, export, expected)) in cases.into_iter().enumerate() {
                let ext = rel.rsplit('.').next().unwrap();
                let source = copy_in(&corpus, rel, &dir, &format!("source-{i}.{ext}"));
                let output = dir.join(&format!("document-{i}.pdf"));
                let mut job = job(source, output.clone(), family);
                job.export = export;
                let converted = office
                    .convert(job, DomainAdmission::Queue, deadline())
                    .await
                    .unwrap_or_else(|err| panic!("{rel} {export:?}: {err:?}"));
                let inspection = pdf
                    .inspect(
                        InspectJob {
                            pdf: output.clone(),
                            password: None,
                            probe_dpi: None,
                            collect_fonts: false,
                            probe_max_pages: u32::MAX,
                            sha256: [0; 32],
                            export: None,
                        },
                        DomainAdmission::Queue,
                        deadline(),
                    )
                    .await
                    .expect("an inspection");
                let count = inspection.page_count;
                let first = inspection.pages[0];
                results.push((
                    count,
                    (
                        first.width_pt.round() as u32,
                        first.height_pt.round() as u32,
                    ),
                ));
                eprintln!(
                    "{rel} {export:?}: {count} pages, {} bytes, engine {:?}, wall {:?}",
                    converted.pdf_bytes, converted.timing.engine, converted.timing.wall
                );
                if let Some(expected) = expected {
                    assert_eq!(count, expected, "{rel} {export:?}");
                }
            }
            // The export keys must change the output, not only match a
            // count: notes "only" gives notes pages (portrait), not the
            // slides of "none", and one page per sheet differs from the
            // default sheet layout.
            let (none, only) = (results[2], results[4]);
            eprintln!("deck20 first page: none {:?}, only {:?}", none.1, only.1);
            assert_ne!(none.1, only.1, "ExportOnlyNotesPages changed nothing");
            let source = copy_in(&corpus, "xlsx/sheet3.xlsx", &dir, "sheet-default.xlsx");
            let output = dir.join("sheet-default.pdf");
            office
                .convert(
                    job(source, output.clone(), Family::Spreadsheet),
                    DomainAdmission::Queue,
                    deadline(),
                )
                .await
                .expect("the default sheet export");
            let default_sheet = pages_of(&pdf, &output).await;
            eprintln!(
                "sheet3.xlsx: default {default_sheet} pages, one page per sheet {}",
                results[7].0
            );
            assert_ne!(
                default_sheet, results[7].0,
                "SinglePageSheets changed nothing"
            );
            office_pool.shutdown(Duration::from_secs(5)).await;
            render_pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}

/// The pages of `pdf` whose render is more than 1 % pure red: the pages
/// of the corpus's red hidden slide, and its notes pages, which show the
/// slide as a thumbnail.
async fn red_pages(pdf: &PdfPdfium, path: &Path, count: u32, out: &Path) -> Vec<u32> {
    std::fs::create_dir_all(out).unwrap();
    let rendered = pdf
        .render(
            RenderJob {
                pdf: path.to_path_buf(),
                password: None,
                pages: (1..=count).collect(),
                format: ImageFormat::Png,
                dpi: 36,
                quality: 85,
                max_edge: 4096,
                out_dir: out.to_path_buf(),
                sha256: [0; 32],
                max_output_bytes: 64 << 20,
            },
            DomainAdmission::Queue,
            deadline(),
        )
        .await
        .expect("a render");
    rendered
        .pages
        .iter()
        .filter(|page| {
            let image = image::open(out.join(&page.file_name)).unwrap().to_rgb8();
            let red = image
                .pixels()
                .filter(|p| p[0] > 200 && p[1] < 60 && p[2] < 60)
                .count();
            red * 100 > image.pixels().len()
        })
        .map(|page| page.number)
        .collect()
}

/// Hidden slides with each notes mode, on the real LibreOffice: the page
/// counts that `domain::pages::hidden_pages` predicts from the slide list,
/// and the pages it marks against the pages that show the hidden slide.
/// `pptx-hidden3` and `odp-hidden` have 3 slides, the 2nd hidden and red.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_hidden_slides_give_the_pages_the_slide_list_predicts() {
    let (Some(lo), Some(pdfium), Some(corpus)) = (lo(), pdfium(), corpus()) else {
        return;
    };
    within(
        TEST,
        "slow_hidden_slides_give_the_pages_the_slide_list_predicts",
        async {
            let dir = Scratch::new("office-hidden");
            let office_pool = ready(office_spec(dir.path(), &lo, true)).await;
            let render_pool = ready(render_spec(dir.path(), &pdfium)).await;
            let office = OfficeLok::new(office_pool.clone());
            let pdf = PdfPdfium::new(render_pool.clone());
            let three = Slides {
                count: 3,
                hidden: vec![2],
            };
            let mut failures = Vec::new();
            let mut n = 0;
            for rel in ["pptx/hidden3.pptx", "odf/hidden.odp"] {
                for notes in [SlideNotes::None, SlideNotes::Append, SlideNotes::Only] {
                    for include_hidden_slides in [false, true] {
                        let export = ExportOptions {
                            notes,
                            include_hidden_slides,
                            one_page_per_sheet: false,
                        };
                        let ext = rel.rsplit('.').next().unwrap();
                        n += 1;
                        let source = copy_in(
                            &corpus,
                            rel,
                            &dir,
                            &format!("hidden-{notes:?}-{include_hidden_slides}-{n}.{ext}"),
                        );
                        let output =
                            dir.join(&format!("hidden-{notes:?}-{include_hidden_slides}-{n}.pdf"));
                        let mut job = job(source, output.clone(), Family::Presentation);
                        job.export = export;
                        let converted = office
                            .convert(job, DomainAdmission::Queue, deadline())
                            .await
                            .unwrap_or_else(|err| panic!("{rel} {export:?}: {err:?}"));
                        assert_eq!(
                            converted.facts.and_then(|f| f.slides).as_ref(),
                            Some(&three),
                            "{rel}"
                        );
                        let count = pages_of(&pdf, &output).await;
                        // Slides or notes pages: one per exported slide.
                        let slides = if include_hidden_slides { 3 } else { 2 };
                        let expected = match notes {
                            SlideNotes::Append => 2 * slides,
                            SlideNotes::None | SlideNotes::Only => slides,
                        };
                        let marks = hidden_pages(Some(&three), &export, count);
                        let pages = dir.join(&format!("pages-{n}"));
                        let red = red_pages(&pdf, &output, count, &pages).await;
                        eprintln!("{rel} {export:?}: {count} pages, marks {marks:?}, red {red:?}");
                        if count != expected {
                            failures.push(format!(
                                "{rel} {export:?}: {count} pages, expected {expected}"
                            ));
                        }
                        // The marks are the pages that show the hidden slide:
                        // none without the option, where LibreOffice drops it.
                        if red != marks {
                            failures.push(format!(
                                "{rel} {export:?}: red pages {red:?}, marks {marks:?}"
                            ));
                        }
                    }
                }
            }
            office_pool.shutdown(Duration::from_secs(5)).await;
            render_pool.shutdown(Duration::from_secs(5)).await;
            assert!(failures.is_empty(), "{failures:#?}");
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_worker_times_out_and_a_new_one_serves_the_next_call() {
    let Some(lo) = lo() else { return };
    within(
        TEST,
        "a_stopped_worker_times_out_and_a_new_one_serves_the_next_call",
        async {
            let dir = Scratch::new("office-sigstop");
            let source = dir.join("source.docx");
            std::fs::write(&source, warmup::docx("Stopped.")).unwrap();
            let pool = ready(office_spec(dir.path(), &lo, false)).await;
            let mut stopped = lease(&pool).await;
            let pid = stopped.pid().unwrap();
            let first = stopped.worker_ref().unwrap().generation;
            rustix::process::kill_process(
                rustix::process::Pid::from_raw(pid as i32).unwrap(),
                rustix::process::Signal::STOP,
            )
            .unwrap();
            let limit = Instant::now() + Duration::from_millis(1500);
            let err = stopped
                .call(
                    convert_op(source.clone(), dir.join("a.pdf"), ipc::Family::Document),
                    limit,
                )
                .await
                .unwrap_err();
            let late = Instant::now().saturating_duration_since(limit);
            assert_eq!(err, convertd::engines::pool::CallError::Timeout);
            eprintln!("timeout returned {late:?} after the deadline");
            assert!(late < Duration::from_millis(250), "late by {late:?}");
            drop(stopped);
            let replaced = Instant::now();
            let mut next = lease(&pool).await;
            eprintln!("replacement ready after {:?}", replaced.elapsed());
            assert_ne!(next.worker_ref().unwrap().generation, first);
            let reply = next
                .call(
                    convert_op(source, dir.join("b.pdf"), ipc::Family::Document),
                    deadline(),
                )
                .await
                .unwrap();
            assert!(matches!(reply.result, Ok(Output::Converted(_))));
            drop(next);
            pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_killed_mid_call_is_retried_once() {
    let Some(lo) = lo() else { return };
    within(TEST, "a_worker_killed_mid_call_is_retried_once", async {
        let dir = Scratch::new("office-sigkill");
        let source = dir.join("source.docx");
        std::fs::write(&source, docx_pages(400)).unwrap();
        let pool = ready(office_spec(dir.path(), &lo, false)).await;
        let pid = {
            let lease = lease(&pool).await;
            lease.pid().unwrap()
        };
        let office = OfficeLok::new(pool.clone());
        let killer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            rustix::process::kill_process_group(
                rustix::process::Pid::from_raw(pid as i32).unwrap(),
                rustix::process::Signal::KILL,
            )
        });
        let converted = office
            .convert(
                job(source, dir.join("document.pdf"), Family::Document),
                DomainAdmission::Queue,
                deadline(),
            )
            .await
            .expect("the retry succeeds");
        assert!(killer.await.unwrap().is_ok(), "the kill reached the worker");
        assert_ne!(
            converted.worker.generation, 1,
            "the retry ran on a new worker"
        );
        eprintln!(
            "retried on {:?}, wall {:?}",
            converted.worker, converted.timing.wall
        );
        pool.shutdown(Duration::from_secs(5)).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_output_cap_gives_limit_exceeded_and_the_next_job_works() {
    let Some(lo) = lo() else { return };
    within(
        TEST,
        "the_output_cap_gives_limit_exceeded_and_the_next_job_works",
        async {
            let dir = Scratch::new("office-cap");
            let source = dir.join("source.docx");
            std::fs::write(&source, docx_pages(50)).unwrap();
            // With warm-up, as in production.
            let pool = ready(office_spec(dir.path(), &lo, true)).await;
            let office = OfficeLok::new(pool.clone());
            let mut capped = job(source.clone(), dir.join("capped.pdf"), Family::Document);
            capped.max_output_bytes = 4 * 1024;
            let err = office
                .convert(capped, DomainAdmission::Queue, deadline())
                .await
                .unwrap_err();
            assert!(matches!(err, EngineError::LimitExceeded(_)), "{err:?}");
            assert!(!dir.join("capped.pdf").exists(), "the partial file stays");
            let converted = office
                .convert(
                    job(source, dir.join("full.pdf"), Family::Document),
                    DomainAdmission::Queue,
                    deadline(),
                )
                .await
                .expect("the next job");
            eprintln!(
                "after the cap: {} bytes on {:?}",
                converted.pdf_bytes, converted.worker
            );
            pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_disk_gives_disk_full() {
    let Some(lo) = lo() else { return };
    let Some(small) = std::env::var_os("CONVERTD_SMALL_DISK").map(PathBuf::from) else {
        eprintln!("skipped: CONVERTD_SMALL_DISK is not set");
        return;
    };
    within(TEST, "a_full_disk_gives_disk_full", async {
        let dir = Scratch::new("office-disk");
        let source = dir.join("source.docx");
        std::fs::write(&source, docx_pages(20)).unwrap();
        // Fill the small disk, so even a small PDF does not fit.
        let filler = small.join("filler");
        let _ = std::fs::write(&filler, vec![0u8; 64 << 20]);
        let pool = ready(office_spec(dir.path(), &lo, false)).await;
        let office = OfficeLok::new(pool.clone());
        let err = office
            .convert(
                job(source, small.join("document.pdf"), Family::Document),
                DomainAdmission::Queue,
                deadline(),
            )
            .await
            .unwrap_err();
        let _ = std::fs::remove_file(&filler);
        let _ = std::fs::remove_file(small.join("document.pdf"));
        assert_eq!(err, EngineError::DiskFull);
        pool.shutdown(Duration::from_secs(5)).await;
    })
    .await;
}

/// The first page of each source, `(width, height)` in points rounded,
/// converted by an office pool for each paper: A4 first, then Letter.
async fn first_pages(
    lo: &Path,
    pdfium: &Path,
    scratch: &str,
    sources: &[(&str, Vec<u8>, Family)],
) -> Vec<Vec<(u32, u32)>> {
    let dir = Scratch::new(scratch);
    let render_pool = ready(render_spec(dir.path(), pdfium)).await;
    let pdf = PdfPdfium::new(render_pool.clone());
    let mut sizes = Vec::new();
    for (paper, name) in [(Paper::A4, "a4"), (Paper::Letter, "letter")] {
        let mut spec = office_spec(&dir.join(name), lo, false);
        spec.name = if name == "a4" {
            "office-a4"
        } else {
            "office-letter"
        };
        spec.env = WorkerEnv::from_vars(std::env::vars_os(), paper).with_path_prefix(lo);
        let pool = ready(spec).await;
        let mut per_paper = Vec::new();
        for (index, (file, bytes, family)) in sources.iter().enumerate() {
            // A job directory of its own, as the service gives every job.
            let job_dir = dir.join(&format!("{name}-{index}"));
            std::fs::create_dir_all(&job_dir).unwrap();
            let source = job_dir.join(file);
            std::fs::write(&source, bytes).unwrap();
            let output = job_dir.join("document.pdf");
            OfficeLok::new(pool.clone())
                .convert(
                    job(source, output.clone(), *family),
                    DomainAdmission::Queue,
                    deadline(),
                )
                .await
                .expect("converted");
            let inspection = pdf
                .inspect(
                    InspectJob {
                        pdf: output,
                        password: None,
                        probe_dpi: None,
                        collect_fonts: false,
                        probe_max_pages: u32::MAX,
                        sha256: [0; 32],
                        export: None,
                    },
                    DomainAdmission::Queue,
                    deadline(),
                )
                .await
                .unwrap();
            let page = inspection.pages[0];
            eprintln!(
                "DEFAULT_PAPER={name} {file}: {} x {} pt",
                page.width_pt, page.height_pt
            );
            per_paper.push((page.width_pt.round() as u32, page.height_pt.round() as u32));
            // The worker's copy with page sizes is gone.
            assert!(
                !job_dir.join(".convertd-paper").exists(),
                "the copy stayed behind"
            );
            assert_eq!(
                &std::fs::read(job_dir.join(file)).unwrap(),
                bytes,
                "the source is not back"
            );
        }
        sizes.push(per_paper);
        pool.shutdown(Duration::from_secs(5)).await;
    }
    render_pool.shutdown(Duration::from_secs(5)).await;
    sizes
}

const A4_PT: (u32, u32) = (595, 842);
const LETTER_PT: (u32, u32) = (612, 792);

/// `DEFAULT_PAPER` reaches LibreOffice as `LC_PAPER`, and decides the page of
/// a document that leaves the page to LibreOffice: here an XLSX without
/// `<pageSetup>`, which keeps the default page style of Calc. LibreOffice
/// takes that page from `PaperInfo::getSystemDefaultPaper()`, which reads
/// `LC_PAPER` through `newlocale(LC_PAPER_MASK, "")`, but only while
/// `ooSetupSystemLocale` is empty. LibreOfficeKit's start-up commits it as
/// en-US (`desktop/source/lib/init.cxx`, `setLanguageAndLocale`), which gives
/// Letter. The image keeps it empty with `docker/libreoffice/convertd-paper.xcd`
/// and ships the en_GB.UTF-8 and en_US.UTF-8 locale data that `LC_PAPER` needs.
///
/// An RTF is no probe: RTF defines Letter as its own default page size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_paper_sets_the_page_size_of_a_sheet_without_one() {
    let (Some(lo), Some(pdfium)) = (lo(), pdfium()) else {
        return;
    };
    within(
        TEST,
        "default_paper_sets_the_page_size_of_a_sheet_without_one",
        async {
            let sizes = first_pages(
                &lo,
                &pdfium,
                "office-paper",
                &[(
                    "source.xlsx",
                    warmup::xlsx("No page setup."),
                    Family::Spreadsheet,
                )],
            )
            .await;
            assert_eq!(
                sizes,
                vec![vec![A4_PT], vec![LETTER_PT]],
                "LC_PAPER did not decide the paper"
            );
        },
    )
    .await;
}

/// A DOCX whose sections name no page size follows `DEFAULT_PAPER` too. The
/// DOCX importer of LibreOffice 26.8.3 sets Letter for a section without
/// `w:pgSz` and never asks the locale
/// (`sw/source/writerfilter/dmapper/PropertyMap.cxx`, `SectionPropertyMap`),
/// so the office worker loads a copy that names the size
/// (`worker::docx_paper`). A DOCX that names Letter keeps it on A4.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_paper_sets_the_page_size_of_a_docx_without_one() {
    let (Some(lo), Some(pdfium)) = (lo(), pdfium()) else {
        return;
    };
    within(
        TEST,
        "default_paper_sets_the_page_size_of_a_docx_without_one",
        async {
            const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
            let letter = warmup::stored_zip(&[
                (
                    "[Content_Types].xml",
                    String::from_utf8(read_part(&warmup::docx("x"), "[Content_Types].xml")).unwrap(),
                ),
                (
                    "_rels/.rels",
                    String::from_utf8(read_part(&warmup::docx("x"), "_rels/.rels")).unwrap(),
                ),
                (
                    "word/document.xml",
                    format!(
                        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W}"><w:body><w:p><w:r><w:t>Letter.</w:t></w:r></w:p><w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr></w:body></w:document>"#
                    ),
                ),
            ]);
            let sizes = first_pages(
                &lo,
                &pdfium,
                "office-docx-paper",
                &[
                    ("source.docx", warmup::docx("No page size."), Family::Document),
                    ("source.docx", letter, Family::Document),
                ],
            )
            .await;
            assert_eq!(
                sizes,
                vec![vec![A4_PT, LETTER_PT], vec![LETTER_PT, LETTER_PT]],
                "DEFAULT_PAPER did not decide the page of the DOCX without w:pgSz"
            );
        },
    )
    .await;
}

/// One part of a stored ZIP, for building a fixture from another.
fn read_part(zip: &[u8], name: &str) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(zip);
    let entries = convertd::detect::zip_scan::list(&mut cursor, zip.len() as u64).unwrap();
    let (_, entry) = entries.into_iter().find(|(n, _)| n == name).unwrap();
    convertd::detect::zip_scan::read_entry(&mut cursor, &entry, 1 << 20)
        .unwrap()
        .unwrap()
}

/// The encrypted deck and sheet, as `corpus/expected.json` names them.
const ENCRYPTED_FIXTURES: [&str; 2] = ["pptx/encrypted.pptx", "xlsx/encrypted.xlsx"];

/// An encrypted OOXML upload is detected only as encrypted, so the
/// supervisor names it `source.docx` and asks for a document. LibreOffice
/// finds the real type from the content, and the worker picks the export
/// keys from that type. Uses `pptx/encrypted.pptx` and `xlsx/encrypted.xlsx`
/// from `corpus/generate.py`: `deck20.pptx` and `sheet3.xlsx` with OOXML
/// agile encryption and the password `secret`. With `CONVERTD_CORPUS_DIR`
/// set, a missing file fails the test, so a renamed fixture cannot turn it
/// into a skip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_decks_and_sheets_named_docx_keep_their_export_keys() {
    let (Some(lo), Some(pdfium), Some(corpus)) = (lo(), pdfium(), corpus()) else {
        return;
    };
    for file in ENCRYPTED_FIXTURES {
        assert!(
            corpus.join(file).exists(),
            "{file} is missing from {}: corpus/generate.py writes it",
            corpus.display()
        );
    }
    within(
        TEST,
        "encrypted_decks_and_sheets_named_docx_keep_their_export_keys",
        async {
            let dir = Scratch::new("office-encrypted-ooxml");
            let office_pool = ready(office_spec(dir.path(), &lo, true)).await;
            let render_pool = ready(render_spec(dir.path(), &pdfium)).await;
            let office = OfficeLok::new(office_pool.clone());
            let pdf = PdfPdfium::new(render_pool.clone());
            let cases = [
                (
                    ENCRYPTED_FIXTURES[0],
                    ExportOptions {
                        notes: SlideNotes::Append,
                        ..ExportOptions::default()
                    },
                    40,
                ),
                (
                    ENCRYPTED_FIXTURES[1],
                    ExportOptions {
                        one_page_per_sheet: true,
                        ..ExportOptions::default()
                    },
                    3,
                ),
            ];
            for (i, (file, export, expected)) in cases.into_iter().enumerate() {
                let case = dir.join(&format!("case-{i}"));
                std::fs::create_dir_all(&case).unwrap();
                let source = case.join("source.docx");
                std::fs::copy(corpus.join(file), &source).unwrap();
                let output = case.join("document.pdf");
                let mut job = job(source, output.clone(), Family::Document);
                job.password = Some(Password::new("secret"));
                job.export = export;
                let converted = office
                    .convert(job, DomainAdmission::Queue, deadline())
                    .await
                    .unwrap_or_else(|err| panic!("{file}: {err:?}"));
                let pages = pages_of(&pdf, &output).await;
                eprintln!(
                    "encrypted {file} as source.docx: {:?}, {pages} pages",
                    converted.doc_type
                );
                assert_eq!(pages, expected, "{file}: the export keys of its real type");
            }
            office_pool.shutdown(Duration::from_secs(5)).await;
            render_pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}

/// The RSS watchdog against a real office worker. An
/// office worker holds about 250 MB at Ready, so a 64 MiB kill limit trips
/// on the first sample of the call. The watchdog samples every 200 ms, so
/// the job must run longer than that: 4000 pages take about a second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_office_worker_over_the_kill_limit_gives_limit_exceeded() {
    let Some(lo) = lo() else { return };
    within(
        TEST,
        "an_office_worker_over_the_kill_limit_gives_limit_exceeded",
        async {
            let dir = Scratch::new("office-kill-rss");
            let source = dir.join("source.docx");
            std::fs::write(&source, docx_pages(4000)).unwrap();
            let mut spec = office_spec(dir.path(), &lo, false);
            spec.kill_rss = 64 << 20;
            let pool = ready(spec).await;
            let office = OfficeLok::new(pool.clone());
            let started = Instant::now();
            let err = office
                .convert(
                    job(source, dir.join("document.pdf"), Family::Document),
                    DomainAdmission::Queue,
                    deadline(),
                )
                .await
                .unwrap_err();
            let took = started.elapsed();
            eprintln!("watchdog: {err:?} after {took:?}");
            assert!(matches!(err, EngineError::LimitExceeded(_)), "{err:?}");
            assert!(took < Duration::from_secs(2), "took {took:?}");
            assert!(!dir.join("document.pdf").exists(), "a partial PDF stayed");
            pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}
