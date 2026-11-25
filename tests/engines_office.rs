//! The office worker with the real LibreOffice, through the pool (plan 002
//! step 8). These tests need LibreOffice at `LO_PROGRAM_DIR`, as in the dev
//! image; where it is missing they print why and pass without running.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use convertd::engines::config::Paper;
use convertd::engines::pool::{Admission, Lease, Pool, PoolSpec};
use convertd::engines::process::{ReaperMode, Spawner};
use convertd::engines::slot::WorkerEnv;
use convertd::ipc::*;
use convertd::worker::Role;

/// The bound of one test: an office worker start takes seconds.
const TEST: Duration = Duration::from_secs(180);
/// The deadline of one engine call.
const CALL: Duration = Duration::from_secs(60);

/// `fut`, or a test failure naming `what` once `limit` has passed.
async fn within<T>(limit: Duration, what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(limit, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {limit:?}: {what}"),
    }
}

/// The LibreOffice program directory, or `None` (with a note) when this
/// machine has none.
fn lo_program_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("LO_PROGRAM_DIR")?);
    if dir.join("soffice.bin").exists() {
        Some(dir)
    } else {
        eprintln!("skipped: no LibreOffice at {}", dir.display());
        None
    }
}

/// A scratch directory, removed on drop.
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spec(dir: &Path, lo: PathBuf) -> PoolSpec {
    PoolSpec {
        name: "office",
        role: Role::Office,
        program: PathBuf::from(env!("CARGO_BIN_EXE_convertd")),
        workers_dir: dir.join("workers"),
        profile_template: None,
        env: WorkerEnv::from_process(Paper::A4),
        init: Init {
            protocol: PROTOCOL_VERSION,
            lo_program_dir: Some(lo),
            ..Init::default()
        },
        size: 1,
        max_jobs: 1_000,
        max_rss: u64::MAX,
        kill_rss: u64::MAX,
        max_age: Duration::from_secs(3600),
        age_check: Duration::from_secs(30),
        queue_capacity: 4,
        queue_wait: Duration::from_secs(10),
        start_timeout: Duration::from_secs(60),
    }
}

/// A minimal valid DOCX with one paragraph per entry of `paragraphs`.
pub fn docx(paragraphs: &[&str]) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>"))
        .collect();
    zip_of(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#
                .to_owned(),
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#
                .to_owned(),
        ),
        (
            "word/document.xml",
            format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
            ),
        ),
    ])
}

fn zip_of(entries: &[(&str, String)]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(data.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn convert(input: PathBuf, output: PathBuf, password: Option<&str>) -> Op {
    Op::Convert(ConvertArgs {
        input,
        output,
        family: Family::Document,
        password: password.map(Secret::new),
        export: ExportOptions::default(),
        max_output_bytes: 64 << 20,
    })
}

async fn lease(pool: &Pool) -> Lease {
    within(
        Duration::from_secs(90),
        "an office worker",
        pool.lease(Instant::now() + Duration::from_secs(90), Admission::Queue),
    )
    .await
    .expect("a lease")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_docx_converts_through_the_office_pool() {
    let Some(lo) = lo_program_dir() else { return };
    within(TEST, "a_docx_converts_through_the_office_pool", async {
        let dir = Scratch::new("office-docx");
        let source = dir.path().join("source.docx");
        std::fs::write(&source, docx(&["Hello from convertd.", "Second paragraph."])).unwrap();
        let pool = Pool::start(spec(dir.path(), lo), Spawner::new(ReaperMode::Owned).unwrap());
        let started = Instant::now();
        assert!(
            pool.wait_idle(1, Duration::from_secs(90)).await,
            "the office worker did not get ready: {:?}",
            pool.status()
        );
        let ready_after = started.elapsed();

        let mut times = Vec::new();
        for round in 1..=3 {
            let output = dir.path().join(format!("document-{round}.pdf"));
            let mut lease = lease(&pool).await;
            let wall = Instant::now();
            let reply = lease
                .call(convert(source.clone(), output.clone(), None), Instant::now() + CALL)
                .await
                .expect("a reply");
            let wall = wall.elapsed();
            let Ok(Output::Converted(converted)) = reply.result else {
                panic!("expected converted, got {:?}", reply.result);
            };
            let pdf = std::fs::read(&output).unwrap();
            assert!(pdf.starts_with(b"%PDF-"), "not a PDF");
            assert_eq!(converted.pdf_bytes, pdf.len() as u64);
            assert_eq!(converted.doc_type, "text");
            assert!(converted.facts.is_some(), "pre-flight ran");
            eprintln!(
                "round {round}: wall {wall:?}, engine {} us, preflight {} us, rss {} MiB, pdf {} bytes, worker {:?}",
                reply.engine.engine_us,
                reply.engine.preflight_us,
                reply.rss_bytes >> 20,
                pdf.len(),
                lease.worker_ref()
            );
            times.push(reply.engine.engine_us);
        }
        eprintln!("pool ready after {ready_after:?}; engine us per round {times:?}");
        pool.shutdown(Duration::from_secs(5)).await;
    })
    .await;
}
