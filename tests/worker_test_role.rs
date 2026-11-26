//! `convertd worker test`, driven directly over its pipes (plan 002 step 2).
#![cfg(feature = "test-hooks")]
// These tests drive one worker binary by hand, below the supervisor, and
// reap it themselves; the reaper rule is for the supervisor.
#![allow(clippy::disallowed_methods)]

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use convertd::ipc::frame::{read_frame, write_frame};
use convertd::ipc::*;

/// No worker in these tests may live longer: past it the watchdog kills
/// the worker, so every blocking read and wait ends and the test fails.
const LIMIT: Duration = Duration::from_secs(30);

struct Worker {
    child: Arc<Mutex<Child>>,
    pid: u32,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: mpsc::Receiver<Vec<u8>>,
    /// Set by the watchdog when it killed the worker.
    fired: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    next_id: u64,
}

impl Worker {
    fn spawn() -> Worker {
        Self::spawn_with(|_| {})
    }

    fn spawn_with(f: impl FnOnce(&mut Command)) -> Worker {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_convertd"));
        cmd.args(["worker", "test"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        f(&mut cmd);
        let mut child = cmd.spawn().expect("spawn worker");
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut all = Vec::new();
            let _ = stderr.read_to_end(&mut all);
            let _ = tx.send(all);
        });
        let child = Arc::new(Mutex::new(child));
        let fired = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        {
            let (child, fired, done) = (Arc::clone(&child), Arc::clone(&fired), Arc::clone(&done));
            std::thread::spawn(move || {
                let until = Instant::now() + LIMIT;
                while Instant::now() < until {
                    if done.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                fired.store(true, Ordering::Release);
                // std sends nothing once the child is reaped.
                let _ = child.lock().unwrap().kill();
            });
        }
        Worker {
            child,
            pid,
            stdin,
            stdout,
            stderr: rx,
            fired,
            done,
            next_id: 1,
        }
    }

    fn check_watchdog(&self) {
        assert!(
            !self.fired.load(Ordering::Acquire),
            "timed out after {LIMIT:?}: the watchdog killed the worker"
        );
    }

    fn send(&mut self, req: &Request) {
        write_frame(self.stdin.as_mut().unwrap(), req, &mut Vec::new()).unwrap();
    }

    fn recv(&mut self) -> Option<Response> {
        let frame = read_frame(&mut self.stdout, &mut Vec::new());
        self.check_watchdog();
        frame.unwrap()
    }

    fn init(&mut self, test: TestInit) -> Response {
        self.send(&Request::Init(Init {
            protocol: PROTOCOL_VERSION,
            test,
            ..Init::default()
        }));
        self.recv().expect("an answer to init")
    }

    fn ready(&mut self) -> Ready {
        match self.init(TestInit::default()) {
            Response::Ready(ready) => ready,
            other => panic!("expected ready, got {other:?}"),
        }
    }

    fn call(&mut self, op: Op) -> Reply {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&Request::Call(Call { id, op }));
        match self.recv() {
            Some(Response::Reply(reply)) => {
                assert_eq!(reply.id, id);
                reply
            }
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    fn ok(&mut self, op: Op) -> Output {
        self.call(op).result.expect("a successful reply")
    }

    fn err(&mut self, op: Op) -> WorkerError {
        self.call(op).result.expect_err("an error reply")
    }

    /// Closes stdin and returns the exit status and everything on stderr,
    /// within the watchdog's limit.
    fn finish(mut self) -> (ExitStatus, String) {
        drop(self.stdin.take());
        let status = loop {
            if let Some(status) = self.child.lock().unwrap().try_wait().unwrap() {
                break status;
            }
            self.check_watchdog();
            std::thread::sleep(Duration::from_millis(5));
        };
        self.check_watchdog();
        let stderr = self
            .stderr
            .recv_timeout(Duration::from_secs(10))
            .expect("timed out reading the worker's stderr");
        (status, String::from_utf8_lossy(&stderr).into_owned())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// A scratch directory, removed when the guard drops, also when the test
/// fails. Removal errors are ignored: the directory is under the temp dir,
/// and a failed clean-up must not fail or mask the test.
struct Scratch(PathBuf);

impl Scratch {
    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!("convertd-002-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn ping_then_exit_zero_on_eof() {
    let mut w = Worker::spawn();
    let ready = w.ready();
    assert_eq!(ready.protocol, PROTOCOL_VERSION);
    assert_eq!(ready.pid, w.pid);
    let reply = w.call(Op::Ping);
    assert_eq!(reply.result, Ok(Output::Pong));
    // A Ping is not a job (WORKER_MAX_JOBS counts conversions).
    assert_eq!(reply.job, 0);
    assert_eq!(w.call(Op::Test(TestOp::Sleep { ms: 0 })).job, 1);
    assert_eq!(w.call(Op::Ping).job, 1);
    let (status, _) = w.finish();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn stdout_junk_lands_on_stderr_and_frames_still_parse() {
    let mut w = Worker::spawn();
    w.ready();
    assert_eq!(
        w.ok(Op::Test(TestOp::WriteStdout { bytes: 4096 })),
        Output::Test(TestOutput::Wrote { bytes: 4096 })
    );
    assert_eq!(w.ok(Op::Ping), Output::Pong);
    let (status, stderr) = w.finish();
    assert_eq!(status.code(), Some(0));
    assert!(stderr.contains(&"x".repeat(4096)), "junk not on stderr");
}

#[test]
fn eof_before_init_exits_zero() {
    let w = Worker::spawn();
    let (status, _) = w.finish();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn fatal_at_init_exits_one() {
    let mut w = Worker::spawn();
    let answer = w.init(TestInit {
        fatal: true,
        ready_delay_ms: 0,
    });
    assert!(matches!(answer, Response::Fatal(_)), "{answer:?}");
    let (status, stderr) = w.finish();
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("failed to start the test engine"));
}

#[test]
fn protocol_mismatch_is_fatal() {
    let mut w = Worker::spawn();
    w.send(&Request::Init(Init {
        protocol: PROTOCOL_VERSION + 1,
        ..Init::default()
    }));
    assert!(matches!(w.recv(), Some(Response::Fatal(_))));
    assert_eq!(w.finish().0.code(), Some(1));
}

#[test]
fn ready_delay_counts_as_bind_time() {
    let mut w = Worker::spawn();
    let Response::Ready(ready) = w.init(TestInit {
        fatal: false,
        ready_delay_ms: 30,
    }) else {
        panic!("expected ready");
    };
    assert!(ready.bind_us >= 30_000, "bind_us {}", ready.bind_us);
    w.finish();
}

#[test]
fn exit_and_abort_end_the_process() {
    let mut w = Worker::spawn();
    w.ready();
    w.send(&Request::Call(Call {
        id: 1,
        op: Op::Test(TestOp::Exit { code: 3 }),
    }));
    assert!(w.recv().is_none());
    assert_eq!(w.finish().0.code(), Some(3));

    let mut w = Worker::spawn();
    w.ready();
    w.send(&Request::Call(Call {
        id: 1,
        op: Op::Test(TestOp::Abort),
    }));
    assert!(w.recv().is_none());
    assert_eq!(w.finish().0.signal(), Some(libc::SIGABRT));
}

#[test]
fn panic_marker_exits_101() {
    let dir = scratch("panic");
    write(&dir.join("source.pdf"), b"%PDF FAKE:PANIC");
    let mut w = Worker::spawn();
    w.ready();
    w.send(&Request::Call(Call {
        id: 1,
        op: Op::Inspect(InspectArgs {
            pdf: dir.join("source.pdf"),
            password: None,
            probe_dpi: None,
            probe_pages: Default::default(),
            collect_fonts: false,
        }),
    }));
    assert!(w.recv().is_none());
    let (status, stderr) = w.finish();
    assert_eq!(status.code(), Some(101));
    assert!(stderr.contains("FAKE:PANIC"));
}

#[test]
fn environment_is_what_the_spawner_set() {
    let mut w = Worker::spawn_with(|cmd| {
        cmd.env_clear()
            .env("HOME", "/nowhere")
            .env("LANG", "C.UTF-8");
    });
    w.ready();
    let Output::Test(TestOutput::Env { vars }) = w.ok(Op::Test(TestOp::Env)) else {
        panic!("expected env");
    };
    // macOS CoreFoundation sets __CF_USER_TEXT_ENCODING inside every process
    // that links it; it does not come from the spawner.
    let vars: Vec<(String, String)> = vars
        .into_iter()
        .filter(|(k, _)| !(cfg!(target_os = "macos") && k == "__CF_USER_TEXT_ENCODING"))
        .collect();
    assert_eq!(
        vars,
        vec![
            ("HOME".to_string(), "/nowhere".to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
        ]
    );
    w.finish();
}

fn inspect(pdf: PathBuf) -> Op {
    Op::Inspect(InspectArgs {
        pdf,
        password: None,
        probe_dpi: Some(18),
        probe_pages: Default::default(),
        collect_fonts: false,
    })
}

fn render(pdf: PathBuf, out_dir: PathBuf, pages: Vec<u32>, max_output_bytes: u64) -> Op {
    Op::Render(RenderArgs {
        pdf,
        password: None,
        pages,
        format: ImageFormat::Jpeg,
        dpi: 150,
        quality: 85,
        max_edge: 10_000,
        out_dir,
        max_output_bytes,
    })
}

#[test]
fn fake_operations_write_valid_outputs() {
    let dir = scratch("fake-ops");
    let source = dir.join("source.docx");
    write(&source, b"PK fake docx with FAKE:PAGES=3 inside");
    let pdf = dir.join("document.pdf");
    let pages = dir.join("pages");
    std::fs::create_dir_all(&pages).unwrap();
    let mut w = Worker::spawn();
    w.ready();

    let reply = w.call(Op::Convert(ConvertArgs {
        input: source.clone(),
        output: pdf.clone(),
        family: Family::Document,
        password: None,
        export: ExportOptions::default(),
        max_output_bytes: 1 << 20,
    }));
    let Ok(Output::Converted(converted)) = reply.result else {
        panic!("expected converted, got {:?}", reply.result);
    };
    assert_eq!(converted.doc_type, "text");
    assert_eq!(converted.pdf_bytes, std::fs::metadata(&pdf).unwrap().len());
    assert!(std::fs::read(&pdf).unwrap().starts_with(b"%PDF-"));
    assert_eq!(reply.engine.engine_us, reply.engine.convert_us);

    // The page count survives into the converted PDF.
    let Output::Inspected(inspected) = w.ok(inspect(pdf.clone())) else {
        panic!("expected inspected");
    };
    assert_eq!(inspected.page_count, 3);
    assert_eq!(inspected.pages.len(), 3);
    assert_eq!(inspected.probe.len(), 3);

    let Output::Rendered(rendered) = w.ok(render(pdf.clone(), pages.clone(), vec![1, 3], 1 << 20))
    else {
        panic!("expected rendered");
    };
    let names: Vec<_> = rendered
        .pages
        .iter()
        .map(|p| p.file_name.as_str())
        .collect();
    assert_eq!(names, ["0001.jpg", "0003.jpg"]);
    for page in &rendered.pages {
        let bytes = std::fs::read(pages.join(&page.file_name)).unwrap();
        assert_eq!(bytes.len() as u64, page.size_bytes);
        assert!(bytes.starts_with(b"\xFF\xD8"));
    }

    let decrypted_pdf = dir.join("decrypted.pdf");
    let Output::Decrypted(decrypted) = w.ok(Op::Decrypt(DecryptArgs {
        pdf: pdf.clone(),
        password: Secret::new("pw"),
        output: decrypted_pdf.clone(),
        max_output_bytes: 1 << 20,
        check_owner: false,
    })) else {
        panic!("expected decrypted");
    };
    assert_eq!(
        decrypted.pdf_bytes,
        std::fs::metadata(&decrypted_pdf).unwrap().len()
    );

    w.finish();
}

#[test]
fn markers_and_caps_give_errors() {
    let dir = scratch("fake-errors");
    let locked = dir.join("locked.pdf");
    write(&locked, b"%PDF-1.7 %FAKE:PASSWORD_REQUIRED");
    let plain = dir.join("plain.pdf");
    write(&plain, b"%PDF-1.7");
    let pages = dir.join("pages");
    std::fs::create_dir_all(&pages).unwrap();
    let mut w = Worker::spawn();
    w.ready();

    assert_eq!(
        w.err(inspect(locked)).kind,
        WorkerErrorKind::PasswordRequired
    );
    assert_eq!(
        w.err(render(plain.clone(), pages.clone(), vec![2], 1 << 20))
            .kind,
        WorkerErrorKind::LoadFailed,
        "page 2 of a one-page document"
    );
    // Two pages of a fake JPEG do not fit in 1 byte; nothing is left behind.
    let err = w.err(render(plain, pages.clone(), vec![1], 1));
    assert_eq!(err.kind, WorkerErrorKind::LimitExceeded);
    assert_eq!(std::fs::read_dir(&pages).unwrap().count(), 0);

    w.finish();
}
