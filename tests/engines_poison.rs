//! Poison memory through the engine adapters, with `convertd worker test`
//! behind the real pool (level B; plan 002 step 12).
#![cfg(feature = "test-hooks")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use convertd::domain::conversion::ImageFormat;
use convertd::domain::document::Family;
use convertd::domain::engine::{
    Admission, EngineError, OfficeEngine, OfficeJob, PdfEngine, RenderJob,
};
use convertd::engines::config::Paper;
use convertd::engines::office_lok::OfficeLok;
use convertd::engines::pdf_pdfium::PdfPdfium;
use convertd::engines::poison::PoisonList;
use convertd::engines::pool::{Admission as PoolAdmission, Pool, PoolSpec};
use convertd::engines::process::{ReaperMode, Spawner};
use convertd::engines::slot::WorkerEnv;
use convertd::ipc::{Init, PROTOCOL_VERSION};
use convertd::worker::Role;

const TEST: Duration = Duration::from_secs(60);

async fn within<T>(limit: Duration, what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(limit, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {limit:?}: {what}"),
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("convertd-002-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
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

fn spec(dir: &Path) -> PoolSpec {
    PoolSpec {
        name: "test",
        role: Role::Test,
        program: PathBuf::from(env!("CARGO_BIN_EXE_convertd")),
        workers_dir: dir.join("workers"),
        profile_template: None,
        env: WorkerEnv::from_vars(Vec::new(), Paper::A4),
        init: Init {
            protocol: PROTOCOL_VERSION,
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
        start_timeout: Duration::from_secs(10),
    }
}

async fn pool(dir: &Path) -> Pool {
    let pool = Pool::start(spec(dir), Spawner::new(ReaperMode::Owned).unwrap());
    assert!(pool.wait_idle(1, Duration::from_secs(20)).await);
    pool
}

fn office_job(input: &Path, output: PathBuf, sha256: [u8; 32]) -> OfficeJob {
    OfficeJob {
        input: input.to_path_buf(),
        output,
        family: Family::Document,
        password: None,
        export: Default::default(),
        sha256,
        max_output_bytes: 1 << 20,
    }
}

/// The generation of the worker a fresh lease gets.
async fn generation(pool: &Pool) -> u32 {
    let lease = pool
        .lease(
            Instant::now() + Duration::from_secs(10),
            PoolAdmission::Queue,
        )
        .await
        .unwrap();
    lease.worker_ref().unwrap().generation
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_timeouts_poison_a_document_before_it_takes_a_worker() {
    within(
        TEST,
        "two_timeouts_poison_a_document_before_it_takes_a_worker",
        async {
            let dir = Scratch::new("poison-timeout");
            let hang = dir.join("hang.docx");
            std::fs::write(&hang, b"FAKE:HANG").unwrap();
            let fine = dir.join("fine.docx");
            std::fs::write(&fine, b"no marker").unwrap();
            let pool = pool(&dir.0).await;
            let office =
                OfficeLok::new(pool.clone()).with_poison(PoisonList::new(Duration::from_secs(60)));
            let key = [1u8; 32];
            for attempt in 1..=2 {
                let err = office
                    .convert(
                        office_job(&hang, dir.join("a.pdf"), key),
                        Admission::Queue,
                        Instant::now() + Duration::from_millis(300),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(err, EngineError::Timeout, "attempt {attempt}");
            }
            let before = generation(&pool).await;
            let started = Instant::now();
            let err = office
                .convert(
                    office_job(&hang, dir.join("b.pdf"), key),
                    Admission::Queue,
                    Instant::now() + Duration::from_secs(5),
                )
                .await
                .unwrap_err();
            assert_eq!(err, EngineError::Poisoned);
            assert!(started.elapsed() < Duration::from_millis(50));
            assert_eq!(
                generation(&pool).await,
                before,
                "the poisoned call took a worker"
            );
            // Another document is not affected.
            office
                .convert(
                    office_job(&fine, dir.join("c.pdf"), [9u8; 32]),
                    Admission::Queue,
                    Instant::now() + Duration::from_secs(5),
                )
                .await
                .expect("another document converts");
            pool.shutdown(Duration::from_secs(1)).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_after_the_retry_poisons_at_once() {
    within(TEST, "a_crash_after_the_retry_poisons_at_once", async {
        let dir = Scratch::new("poison-crash");
        let panics = dir.join("panic.docx");
        std::fs::write(&panics, b"FAKE:PANIC").unwrap();
        let pool = pool(&dir.0).await;
        let office =
            OfficeLok::new(pool.clone()).with_poison(PoisonList::new(Duration::from_secs(60)));
        let key = [2u8; 32];
        let deadline = || Instant::now() + Duration::from_secs(10);
        let err = office
            .convert(
                office_job(&panics, dir.join("a.pdf"), key),
                Admission::Queue,
                deadline(),
            )
            .await
            .unwrap_err();
        assert_eq!(err, EngineError::Crashed, "crashed twice, with the retry");
        let err = office
            .convert(
                office_job(&panics, dir.join("b.pdf"), key),
                Admission::Queue,
                deadline(),
            )
            .await
            .unwrap_err();
        assert_eq!(err, EngineError::Poisoned);
        pool.shutdown(Duration::from_secs(1)).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renders_are_poisoned_too_and_ttl_zero_turns_it_off() {
    within(
        TEST,
        "renders_are_poisoned_too_and_ttl_zero_turns_it_off",
        async {
            let dir = Scratch::new("poison-render");
            let hang = dir.join("hang.pdf");
            std::fs::write(&hang, b"%PDF FAKE:HANG").unwrap();
            let out = dir.join("pages");
            std::fs::create_dir_all(&out).unwrap();
            let job = |sha256| RenderJob {
                pdf: hang.clone(),
                password: None,
                pages: vec![1],
                format: ImageFormat::Jpeg,
                dpi: 72,
                quality: 85,
                max_edge: 0,
                out_dir: out.clone(),
                sha256,
                max_output_bytes: 1 << 20,
            };
            let pool = pool(&dir.0).await;
            let short = || Instant::now() + Duration::from_millis(300);
            let pdf =
                PdfPdfium::new(pool.clone()).with_poison(PoisonList::new(Duration::from_secs(60)));
            for _ in 0..2 {
                let err = pdf
                    .render(job([3; 32]), Admission::Queue, short())
                    .await
                    .unwrap_err();
                assert_eq!(err, EngineError::Timeout);
            }
            let err = pdf
                .render(job([3; 32]), Admission::Queue, short())
                .await
                .unwrap_err();
            assert_eq!(err, EngineError::Poisoned);

            // POISON_TTL_SECONDS=0: no list, no poison.
            let off = PdfPdfium::new(pool.clone()).with_poison(PoisonList::new(Duration::ZERO));
            for _ in 0..3 {
                let err = off
                    .render(job([4; 32]), Admission::Queue, short())
                    .await
                    .unwrap_err();
                assert_eq!(err, EngineError::Timeout);
            }
            pool.shutdown(Duration::from_secs(1)).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_render_crash_counts_by_the_same_rule_as_a_convert_crash() {
    within(
        TEST,
        "a_render_crash_counts_by_the_same_rule_as_a_convert_crash",
        async {
            let dir = Scratch::new("poison-render-crash");
            let panics = dir.join("panic.pdf");
            std::fs::write(&panics, b"%PDF FAKE:PANIC").unwrap();
            let out = dir.join("pages");
            std::fs::create_dir_all(&out).unwrap();
            let job = |sha256| RenderJob {
                pdf: panics.clone(),
                password: None,
                pages: vec![1],
                format: ImageFormat::Jpeg,
                dpi: 72,
                quality: 85,
                max_edge: 0,
                out_dir: out.clone(),
                sha256,
                max_output_bytes: 1 << 20,
            };
            let pool = pool(&dir.0).await;
            let list = PoisonList::new(Duration::from_secs(60)).unwrap();
            let pdf = PdfPdfium::new(pool.clone()).with_poison(Some(list.clone()));

            // Less than 1 s left: no retry, so the crash counts one strike.
            let no_time = [5u8; 32];
            let short = || Instant::now() + Duration::from_millis(800);
            let err = pdf
                .render(job(no_time), Admission::Queue, short())
                .await
                .unwrap_err();
            assert_eq!(err, EngineError::Crashed);
            assert!(
                !list.is_poisoned(&no_time),
                "a crash with no retry counts one strike"
            );
            let err = pdf
                .render(job(no_time), Admission::Queue, short())
                .await
                .unwrap_err();
            assert_eq!(err, EngineError::Crashed);
            assert!(list.is_poisoned(&no_time), "two single strikes poison");

            // Time to retry: the retry crashes too, which counts two at once.
            let retried = [6u8; 32];
            let long = || Instant::now() + Duration::from_secs(10);
            let err = pdf
                .render(job(retried), Admission::Queue, long())
                .await
                .unwrap_err();
            assert_eq!(err, EngineError::Crashed);
            assert!(
                list.is_poisoned(&retried),
                "a crash after its retry counts two"
            );
            let err = pdf
                .render(job(retried), Admission::Queue, long())
                .await
                .unwrap_err();
            assert_eq!(err, EngineError::Poisoned);
            pool.shutdown(Duration::from_secs(1)).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_convert_crash_whose_retry_gets_no_worker_counts_one_strike() {
    within(
        TEST,
        "a_convert_crash_whose_retry_gets_no_worker_counts_one_strike",
        async {
            let dir = Scratch::new("poison-retry-lease");
            let panics = dir.join("panic.docx");
            std::fs::write(&panics, b"FAKE:PANIC").unwrap();
            // Workers take 1.5 s to start, and the call has 1.2 s: the crash
            // leaves more than 1 s for a retry, but the retry's lease runs
            // out of time before the replacement is ready.
            let mut spec = spec(&dir.0);
            spec.init.test.ready_delay_ms = 1500;
            let pool = Pool::start(spec, Spawner::new(ReaperMode::Owned).unwrap());
            assert!(pool.wait_idle(1, Duration::from_secs(20)).await);
            let list = PoisonList::new(Duration::from_secs(60)).unwrap();
            let office = OfficeLok::new(pool.clone()).with_poison(Some(list.clone()));
            let key = [7u8; 32];
            let short = || Instant::now() + Duration::from_millis(1200);
            let err = office
                .convert(
                    office_job(&panics, dir.join("a.pdf"), key),
                    Admission::Queue,
                    short(),
                )
                .await
                .unwrap_err();
            assert_eq!(
                err,
                EngineError::Timeout,
                "the retry's lease ran out of time"
            );
            assert!(!list.is_poisoned(&key), "one strike, not two");
            let err = office
                .convert(
                    office_job(&panics, dir.join("b.pdf"), key),
                    Admission::Queue,
                    short(),
                )
                .await
                .unwrap_err();
            // The second call waits for the slow replacement, then crashes
            // with less than 1 s left: no retry, one strike either way.
            assert!(
                matches!(err, EngineError::Timeout | EngineError::Crashed),
                "{err:?}"
            );
            assert!(list.is_poisoned(&key), "two single strikes poison");
            pool.shutdown(Duration::from_secs(1)).await;
        },
    )
    .await;
}
