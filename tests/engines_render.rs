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
    dir_from("CONVERTD_CORPUS_DIR", "pdf/text3.pdf", "corpus")
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
            // JPEG at 150 DPI; PNG and WebP at 72 DPI, so a debug build stays fast.
            for (format, dpi) in [
                (ImageFormat::Jpeg, 150u16),
                (ImageFormat::Png, 72),
                (ImageFormat::Webp, 72),
            ] {
                let out = dir.join(format.extension());
                let rendered = pdf
                    .render(
                        render_job(&source, vec![1, 2, 3], format, dpi, &out),
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
                    let expected = (p.width_pt * f32::from(dpi) / 72.0).round() as u32;
                    assert_eq!(p.width_px, expected, "{p:?}");
                }
                eprintln!(
                    "{format:?} at {dpi} DPI, 3 pages: engine {:?}, wall {:?}",
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

/// A PDF written by hand: one page of `size` × `size` pt for each content
/// stream, with a correct cross-reference table.
fn handmade_pdf(size: u32, contents: &[String]) -> Vec<u8> {
    let n = contents.len();
    // Objects: 1 catalog, 2 pages, then a page and its content per entry.
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
    ];
    for (i, content) in contents.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {size} {size}] /Contents {} 0 R /Resources << >> >>",
            4 + 2 * i
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len() + 1
        ));
    }
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// A page with R ≠ B keeps its channels in every format: PDFium's byte
/// order reversal is off, so the encoders get BGRA as they expect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_coloured_page_keeps_its_channels() {
    let Some(pdfium) = pdfium() else {
        return;
    };
    within(TEST, "a_coloured_page_keeps_its_channels", async {
        let dir = Scratch::new("render-colour");
        // #3333B2: 0.2, 0.2, 0.698 in DeviceRGB.
        let source = dir.join("blue.pdf");
        std::fs::write(
            &source,
            handmade_pdf(72, &["0.2 0.2 0.698 rg 0 0 72 72 re f".to_string()]),
        )
        .unwrap();
        let pool = ready(render_spec(dir.path(), &pdfium, 1)).await;
        let pdf = PdfPdfium::new(pool.clone());
        let expected = [0x33u8, 0x33, 0xB2];
        for (format, tolerance) in [
            (ImageFormat::Png, 0u8),
            (ImageFormat::Jpeg, 8),
            (ImageFormat::Webp, 8),
        ] {
            let out = dir.join(format.extension());
            let rendered = pdf
                .render(
                    render_job(&source, vec![1], format, 72, &out),
                    Admission::Continue,
                    deadline(),
                )
                .await
                .unwrap();
            let page = &rendered.pages[0];
            assert!(page.flat, "{format:?}: {page:?}");
            assert_eq!(page.colour, expected, "{format:?}: the flat colour");
            let bytes = std::fs::read(out.join(&page.file_name)).unwrap();
            let image = image::load_from_memory(&bytes).unwrap().to_rgb8();
            let pixel = image.get_pixel(image.width() / 2, image.height() / 2).0;
            for (channel, (got, want)) in pixel.iter().zip(expected).enumerate() {
                assert!(
                    got.abs_diff(want) <= tolerance,
                    "{format:?}: channel {channel} is {got}, want {want} ± {tolerance} (pixel {pixel:?})"
                );
            }
            assert!(pixel[2] > pixel[0] + 100, "{format:?}: blue is not blue: {pixel:?}");
        }
        pool.shutdown(Duration::from_secs(5)).await;
    })
    .await;
}

/// A page of 2 × 2 pt squares in pseudo-random colours: a PNG of it does
/// not compress well.
fn noise_page(size: u32, seed: u64) -> String {
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) % 1000) as f32 / 1000.0
    };
    let mut out = String::new();
    for y in (0..size).step_by(2) {
        for x in (0..size).step_by(2) {
            out.push_str(&format!(
                "{:.3} {:.3} {:.3} rg {x} {y} 2 2 re f\n",
                next(),
                next(),
                next()
            ));
        }
    }
    out
}

/// The output cap is the job's, not the chunk's: when the heavy pages all
/// fall in one chunk, that chunk runs again with what the others left, so
/// two workers succeed and fail exactly where one worker does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_output_cap_does_not_depend_on_the_number_of_render_workers() {
    let Some(pdfium) = pdfium() else {
        return;
    };
    within(
        TEST,
        "the_output_cap_does_not_depend_on_the_number_of_render_workers",
        async {
            let dir = Scratch::new("render-cap-split");
            // Pages 1-10 are noise, pages 11-20 are blank: with two workers,
            // chunk 1 holds nearly all the bytes.
            let contents: Vec<String> = (0..20u64)
                .map(|i| {
                    if i < 10 {
                        noise_page(120, i)
                    } else {
                        "1 1 1 rg 0 0 1 1 re f".to_string()
                    }
                })
                .collect();
            let source = dir.join("half-heavy.pdf");
            std::fs::write(&source, handmade_pdf(120, &contents)).unwrap();
            let all: Vec<u32> = (1..=20).collect();

            for pool_dir in ["one", "two"] {
                std::fs::create_dir_all(dir.join(pool_dir)).unwrap();
            }
            let one = ready(render_spec(&dir.join("one"), &pdfium, 1)).await;
            let rendered = PdfPdfium::new(one.clone())
                .render(
                    render_job(&source, all.clone(), ImageFormat::Png, 72, &dir.join("a")),
                    Admission::Continue,
                    deadline(),
                )
                .await
                .unwrap();
            let total: u64 = rendered.pages.iter().map(|p| p.size_bytes).sum();
            let heavy: u64 = rendered.pages[..10].iter().map(|p| p.size_bytes).sum();
            eprintln!("total {total} bytes, pages 1-10 {heavy} bytes");
            assert!(
                heavy > total / 2 + 1024,
                "the first half must be the heavy one"
            );
            let cap = total + 1024;
            let mut job = render_job(&source, all.clone(), ImageFormat::Png, 72, &dir.join("b"));
            job.max_output_bytes = cap;
            let alone = PdfPdfium::new(one.clone())
                .render(job, Admission::Continue, deadline())
                .await;
            assert!(alone.is_ok(), "one worker: {alone:?}");
            one.shutdown(Duration::from_secs(5)).await;

            let two = ready(render_spec(&dir.join("two"), &pdfium, 2)).await;
            let pdf = PdfPdfium::new(two.clone());
            let mut job = render_job(&source, all.clone(), ImageFormat::Png, 72, &dir.join("c"));
            job.max_output_bytes = cap;
            let split = pdf
                .render(job, Admission::Continue, deadline())
                .await
                .unwrap_or_else(|err| panic!("two workers, cap {cap}: {err:?}"));
            assert_eq!(split.pages.len(), 20);
            let numbers: Vec<u32> = split.pages.iter().map(|p| p.number).collect();
            assert_eq!(numbers, all);
            assert_eq!(split.pages.iter().map(|p| p.size_bytes).sum::<u64>(), total);
            assert!(
                split.workers.len() >= 2,
                "the render did not split: {:?}",
                split.workers
            );

            // One byte under the total fails on two workers as on one.
            let mut job = render_job(&source, all.clone(), ImageFormat::Png, 72, &dir.join("d"));
            job.max_output_bytes = total - 1;
            let under = pdf.render(job, Admission::Continue, deadline()).await;
            assert!(
                matches!(under, Err(EngineError::LimitExceeded(_))),
                "{under:?}"
            );
            two.shutdown(Duration::from_secs(5)).await;
        },
    )
    .await;
}
