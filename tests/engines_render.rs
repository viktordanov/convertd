//! The render worker with the real PDFium, through the pool and the
//! `PdfEngine` adapter (plan 002 step 7).
//!
//! These tests run where `PDFIUM_LIB_DIR` holds `libpdfium.so`, as in the
//! dev image. Elsewhere they print `skipped` and pass without checking
//! anything. Some need corpus files (`CONVERTD_CORPUS_DIR`), and the
//! encrypted PDFs and the long PDF are made with LibreOffice
//! (`LO_PROGRAM_DIR`).
#![allow(clippy::disallowed_methods)] // runs soffice to make fixtures, and reaps it

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use convertd::domain::conversion::ImageFormat;
use convertd::domain::document::Family;
use convertd::domain::engine::{
    Admission, DecryptJob, EngineError, InspectJob, OfficeEngine, OfficeJob, PdfEngine, RenderJob,
};
use convertd::domain::secret::Password;
use convertd::engines::config::Paper;
use convertd::engines::office_lok::OfficeLok;
use convertd::engines::pdf_pdfium::PdfPdfium;
use convertd::engines::pool::{Pool, PoolSpec};
use convertd::engines::process::{ReaperMode, Spawner};
use convertd::engines::slot::WorkerEnv;
use convertd::ipc::{Init, PROTOCOL_VERSION};
use convertd::worker::Role;
use convertd::worker::warmup;

const TEST: Duration = Duration::from_secs(240);
const CALL: Duration = Duration::from_secs(60);

async fn within<T>(limit: Duration, what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(limit, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {limit:?}: {what}"),
    }
}

fn dir_from(var: &str, probe: &str) -> Option<PathBuf> {
    let Some(dir) = std::env::var_os(var).map(PathBuf::from) else {
        eprintln!("skipped: {var} is not set");
        return None;
    };
    if dir.join(probe).exists() {
        Some(dir)
    } else {
        eprintln!("skipped: no {probe} in {}", dir.display());
        None
    }
}

fn lo() -> Option<PathBuf> {
    dir_from("LO_PROGRAM_DIR", "soffice.bin")
}

fn pdfium() -> Option<PathBuf> {
    dir_from("PDFIUM_LIB_DIR", "libpdfium.so")
}

fn corpus() -> Option<PathBuf> {
    dir_from("CONVERTD_CORPUS_DIR", "pdf/text3.pdf")
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

fn spec(dir: &Path, name: &'static str, role: Role, init: Init, size: u16) -> PoolSpec {
    PoolSpec {
        name,
        role,
        program: PathBuf::from(env!("CARGO_BIN_EXE_convertd")),
        workers_dir: dir.join("workers"),
        profile_template: None,
        env: WorkerEnv::from_vars(std::env::vars_os(), Paper::A4),
        init,
        size,
        max_jobs: 1_000,
        max_rss: u64::MAX,
        kill_rss: u64::MAX,
        max_age: Duration::from_secs(3600),
        age_check: Duration::from_secs(30),
        queue_capacity: 8,
        queue_wait: Duration::from_secs(30),
        start_timeout: Duration::from_secs(90),
    }
}

fn render_spec(dir: &Path, pdfium: &Path, size: u16) -> PoolSpec {
    spec(
        dir,
        "render",
        Role::Render,
        Init {
            protocol: PROTOCOL_VERSION,
            pdfium_lib_dir: Some(pdfium.to_path_buf()),
            warmup: true,
            ..Init::default()
        },
        size,
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

fn deadline() -> Instant {
    Instant::now() + CALL
}

fn inspect_job(pdf: &Path, password: Option<&str>, probe_dpi: Option<u16>) -> InspectJob {
    InspectJob {
        pdf: pdf.to_path_buf(),
        password: password.map(Password::new),
        probe_dpi,
        collect_fonts: false,
    }
}

fn render_job(
    pdf: &Path,
    pages: Vec<u32>,
    format: ImageFormat,
    dpi: u16,
    out_dir: &Path,
) -> RenderJob {
    std::fs::create_dir_all(out_dir).unwrap();
    RenderJob {
        pdf: pdf.to_path_buf(),
        password: None,
        pages,
        format,
        dpi,
        quality: 85,
        max_edge: 10_000,
        out_dir: out_dir.to_path_buf(),
        sha256: [0; 32],
        max_output_bytes: 256 << 20,
    }
}

/// A PDF made by LibreOffice from a generated DOCX, with PDF export options
/// given as JSON (for the encryption settings), through `soffice`.
fn soffice_pdf(lo: &Path, dir: &Scratch, name: &str, pages: usize, filter: &str) -> PathBuf {
    let source = dir.join(&format!("{name}.docx"));
    let mut text = String::new();
    for n in 1..=pages {
        text.push_str(&format!("Page {n}. "));
    }
    std::fs::write(&source, warmup::docx(&text)).unwrap();
    let out = dir.join(&format!("{name}-out"));
    std::fs::create_dir_all(&out).unwrap();
    let profile = format!("file://{}", dir.join(&format!("{name}-profile")).display());
    let mut child = std::process::Command::new(lo.join("soffice"))
        .arg(format!("-env:UserInstallation={profile}"))
        .args(["--headless", "--norestore", "--convert-to"])
        .arg(format!("pdf:writer_pdf_Export:{filter}"))
        .arg("--outdir")
        .arg(&out)
        .arg(&source)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "soffice failed: {status:?}");
            break;
        }
        if Instant::now() > until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("soffice did not finish in 120 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let pdf = out.join(format!("{name}.pdf"));
    assert!(pdf.exists(), "soffice wrote no PDF");
    pdf
}

/// PIDs of the `convertd worker render` children of this process (Linux).
fn render_worker_pids() -> Vec<i32> {
    let me = std::process::id();
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse::<u32>().ok());
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        if ppid == Some(me) && String::from_utf8_lossy(&cmdline).contains("worker\0render") {
            pids.push(pid);
        }
    }
    pids
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corpus_pdf_inspects_and_renders_in_every_format() {
    let (Some(pdfium), Some(corpus)) = (pdfium(), corpus()) else {
        return;
    };
    within(
        TEST,
        "a_corpus_pdf_inspects_and_renders_in_every_format",
        async {
            let dir = Scratch::new("render-formats");
            let started = Instant::now();
            let pool = ready(render_spec(dir.path(), &pdfium, 1)).await;
            eprintln!("render pool ready after {:?}", started.elapsed());
            let pdf = PdfPdfium::new(pool.clone());
            let source = corpus.join("pdf/text3.pdf");
            let inspection = pdf
                .inspect(
                    inspect_job(&source, None, Some(18)),
                    Admission::Queue,
                    deadline(),
                )
                .await
                .unwrap();
            assert_eq!(inspection.page_count, 3);
            assert!(!inspection.encrypted && !inspection.needs_password);
            assert_eq!(inspection.probe.len(), 3);
            let page = inspection.pages[0];
            eprintln!(
                "text3.pdf: page 1 {} x {} pt, probe {:?}, inspect engine {:?}",
                page.width_pt, page.height_pt, inspection.probe_engine, inspection.timing.engine
            );
            for format in [ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::Webp] {
                let out = dir.join(format.extension());
                let rendered = pdf
                    .render(
                        render_job(&source, vec![1, 2, 3], format, 150, &out),
                        Admission::Continue,
                        deadline(),
                    )
                    .await
                    .unwrap();
                assert_eq!(rendered.pages.len(), 3);
                for p in &rendered.pages {
                    let bytes = std::fs::read(out.join(&p.file_name)).unwrap();
                    assert_eq!(bytes.len() as u64, p.size_bytes);
                    assert_eq!(Some(crc32fast::hash(&bytes)), p.crc32);
                    assert_eq!(
                        p.width_px,
                        (p.width_pt * 150.0 / 72.0).round() as u32,
                        "{p:?}"
                    );
                }
                eprintln!(
                    "{format:?} at 150 DPI, 3 pages: engine {:?}, wall {:?}",
                    rendered.timing.engine, rendered.timing.wall
                );
            }
            pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_white_page_is_flat_with_objects() {
    let (Some(pdfium), Some(corpus)) = (pdfium(), corpus()) else {
        return;
    };
    within(TEST, "a_white_page_is_flat_with_objects", async {
        let dir = Scratch::new("render-flat");
        let pool = ready(render_spec(dir.path(), &pdfium, 1)).await;
        let pdf = PdfPdfium::new(pool.clone());
        let inspection = pdf
            .inspect(
                inspect_job(&corpus.join("pdf/white-rect.pdf"), None, Some(18)),
                Admission::Queue,
                deadline(),
            )
            .await
            .unwrap();
        let probe = inspection.probe[0];
        assert!(probe.flat && probe.objects > 0, "{probe:?}");
        let text = pdf
            .inspect(
                inspect_job(&corpus.join("pdf/text3.pdf"), None, Some(18)),
                Admission::Queue,
                deadline(),
            )
            .await
            .unwrap();
        assert!(!text.probe[0].flat, "a text page is not flat");
        pool.shutdown(Duration::from_secs(5)).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_encrypted_pdfs_inspect_and_decrypt() {
    let (Some(lo), Some(pdfium)) = (lo(), pdfium()) else {
        return;
    };
    within(TEST, "slow_encrypted_pdfs_inspect_and_decrypt", async {
        let dir = Scratch::new("render-encrypted");
        let locked = soffice_pdf(
            &lo,
            &dir,
            "locked",
            2,
            r#"{"EncryptFile":{"type":"boolean","value":"true"},"DocumentOpenPassword":{"type":"string","value":"secret"}}"#,
        );
        let owner = soffice_pdf(
            &lo,
            &dir,
            "owner",
            1,
            r#"{"RestrictPermissions":{"type":"boolean","value":"true"},"PermissionPassword":{"type":"string","value":"owner"},"Printing":{"type":"long","value":"0"}}"#,
        );
        let pool = ready(render_spec(dir.path(), &pdfium, 1)).await;
        let pdf = PdfPdfium::new(pool.clone());
        let inspect = |path: &Path, pw: Option<&str>| {
            pdf.inspect(inspect_job(path, pw, None), Admission::Queue, deadline())
        };
        assert_eq!(inspect(&locked, None).await.unwrap_err(), EngineError::PasswordRequired);
        assert_eq!(
            inspect(&locked, Some("wrong")).await.unwrap_err(),
            EngineError::InvalidPassword
        );
        let right = inspect(&locked, Some("secret")).await.unwrap();
        assert!(right.encrypted && right.needs_password, "{right:?}");
        let owner_only = inspect(&owner, None).await.unwrap();
        assert!(
            owner_only.encrypted && !owner_only.needs_password,
            "{owner_only:?}"
        );

        let decrypt = |output: PathBuf, pw: &str, cap: u64| {
            pdf.decrypt(
                DecryptJob {
                    pdf: locked.clone(),
                    password: Password::new(pw),
                    output,
                    max_output_bytes: cap,
                },
                Admission::Queue,
                deadline(),
            )
        };
        assert_eq!(
            decrypt(dir.join("wrong.pdf"), "wrong", 1 << 30).await.unwrap_err(),
            EngineError::InvalidPassword
        );
        let capped = decrypt(dir.join("capped.pdf"), "secret", 1024).await.unwrap_err();
        assert!(matches!(capped, EngineError::LimitExceeded(_)), "{capped:?}");
        assert!(!dir.join("capped.pdf").exists());
        let plain = dir.join("plain.pdf");
        let decrypted = decrypt(plain.clone(), "secret", 1 << 30).await.unwrap();
        eprintln!("decrypted copy: {} bytes, engine {:?}", decrypted.pdf_bytes, decrypted.timing.engine);
        // The copy opens without a password and has no security handler.
        let copy = inspect(&plain, None).await.unwrap();
        assert!(!copy.encrypted && !copy.needs_password, "{copy:?}");
        assert_eq!(copy.page_count, right.page_count);
        // The first page renders the same from both files at 72 DPI.
        let mut from_locked = render_job(&locked, vec![1], ImageFormat::Png, 72, &dir.join("a"));
        from_locked.password = Some(Password::new("secret"));
        let a = pdf.render(from_locked, Admission::Queue, deadline()).await.unwrap();
        let b = pdf
            .render(
                render_job(&plain, vec![1], ImageFormat::Png, 72, &dir.join("b")),
                Admission::Queue,
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(a.pages[0].crc32, b.pages[0].crc32, "the decrypted page differs");
        pool.shutdown(Duration::from_secs(5)).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_render_cap_leaves_no_partial_files_and_the_worker_serves_on() {
    let (Some(pdfium), Some(corpus)) = (pdfium(), corpus()) else {
        return;
    };
    within(
        TEST,
        "the_render_cap_leaves_no_partial_files_and_the_worker_serves_on",
        async {
            let dir = Scratch::new("render-cap");
            let pool = ready(render_spec(dir.path(), &pdfium, 1)).await;
            let pdf = PdfPdfium::new(pool.clone());
            let source = corpus.join("pdf/text3.pdf");
            let out = dir.join("pages");
            let one = pdf
                .render(
                    render_job(&source, vec![1], ImageFormat::Png, 150, &out),
                    Admission::Queue,
                    deadline(),
                )
                .await
                .unwrap();
            std::fs::remove_dir_all(&out).unwrap();
            let mut capped = render_job(&source, vec![1, 2, 3], ImageFormat::Png, 150, &out);
            capped.max_output_bytes = one.pages[0].size_bytes * 2;
            let err = pdf
                .render(capped, Admission::Queue, deadline())
                .await
                .unwrap_err();
            assert!(matches!(err, EngineError::LimitExceeded(_)), "{err:?}");
            assert_eq!(
                std::fs::read_dir(&out).unwrap().count(),
                0,
                "partial files stay"
            );
            let again = pdf
                .render(
                    render_job(&source, vec![2], ImageFormat::Jpeg, 72, &out),
                    Admission::Queue,
                    deadline(),
                )
                .await
                .unwrap();
            assert_eq!(
                again.workers[0].generation, one.workers[0].generation,
                "a limit error does not recycle a render worker"
            );
            pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}

/// Makes a `pages`-page PDF with the office pool.
async fn long_pdf(lo: &Path, dir: &Scratch, pages: usize) -> PathBuf {
    let office_pool = ready(spec(
        &dir.join("office"),
        "office",
        Role::Office,
        Init {
            protocol: PROTOCOL_VERSION,
            lo_program_dir: Some(lo.to_path_buf()),
            ..Init::default()
        },
        1,
    ))
    .await;
    let body: Vec<String> = (1..=pages).map(|n| format!("Page {n}.")).collect();
    let source = dir.join("long.docx");
    std::fs::write(&source, docx_with_breaks(&body)).unwrap();
    let output = dir.join("long.pdf");
    OfficeLok::new(office_pool.clone())
        .convert(
            OfficeJob {
                input: source,
                output: output.clone(),
                family: Family::Document,
                password: None,
                export: Default::default(),
                sha256: [0; 32],
                max_output_bytes: 256 << 20,
            },
            Admission::Queue,
            deadline(),
        )
        .await
        .expect("the long PDF");
    office_pool.shutdown(Duration::from_secs(5)).await;
    output
}

fn docx_with_breaks(paragraphs: &[String]) -> Vec<u8> {
    // One paragraph per page; warmup::docx holds one paragraph, so the
    // page breaks go into its text run as XML.
    let joined = paragraphs.join(r#"</w:t></w:r><w:r><w:br w:type="page"/></w:r><w:r><w:t>"#);
    warmup::docx(&joined)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_a_long_render_splits_over_two_workers_and_survives_a_kill() {
    let (Some(lo), Some(pdfium)) = (lo(), pdfium()) else {
        return;
    };
    within(
        TEST,
        "slow_a_long_render_splits_over_two_workers_and_survives_a_kill",
        async {
            let dir = Scratch::new("render-chunks");
            let source = long_pdf(&lo, &dir, 35).await;
            let pool = ready(render_spec(dir.path(), &pdfium, 2)).await;
            let pdf = PdfPdfium::new(pool.clone());
            let pages: Vec<u32> = (1..=35).collect();
            let inspection = pdf
                .inspect(
                    inspect_job(&source, None, None),
                    Admission::Queue,
                    deadline(),
                )
                .await
                .unwrap();
            assert_eq!(inspection.page_count, 35);
            let rendered = pdf
                .render(
                    render_job(
                        &source,
                        pages.clone(),
                        ImageFormat::Jpeg,
                        150,
                        &dir.join("a"),
                    ),
                    Admission::Queue,
                    deadline(),
                )
                .await
                .unwrap();
            assert_eq!(rendered.pages.len(), 35);
            assert_eq!(rendered.workers.len(), 2, "two chunks");
            assert!(rendered.pages.windows(2).all(|w| w[0].number < w[1].number));
            eprintln!(
                "35 pages, JPEG 150 DPI, 2 chunks: longest chunk {:?}, wall {:?}",
                rendered.timing.engine, rendered.timing.wall
            );

            // Kill one chunk's worker while the chunks run; the chunk is retried.
            let pids = render_worker_pids();
            assert_eq!(pids.len(), 2, "two render workers: {pids:?}");
            let victim = pids[0];
            let killer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                rustix::process::kill_process_group(
                    rustix::process::Pid::from_raw(victim).unwrap(),
                    rustix::process::Signal::KILL,
                )
            });
            let again = pdf
                .render(
                    render_job(&source, pages, ImageFormat::Png, 300, &dir.join("b")),
                    Admission::Queue,
                    deadline(),
                )
                .await
                .expect("the killed chunk is retried");
            assert!(killer.await.unwrap().is_ok());
            assert_eq!(again.pages.len(), 35);
            eprintln!(
                "after the kill: workers {:?}, wall {:?}",
                again.workers, again.timing.wall
            );
            pool.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}
