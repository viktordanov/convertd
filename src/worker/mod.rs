//! Engine worker processes (lane 002): the only place with `unsafe` and engine calls.
//!
//! `main.rs` dispatches `convertd worker <role>` here before any runtime,
//! logging or configuration exists. A worker is one thread: it reads
//! `Init`, starts its engine, answers `Ready` (or `Fatal`), then serves one
//! call at a time until its input pipe closes.

pub mod encode;
pub mod office;
pub mod output;
pub mod render;
pub mod sys;
#[cfg(feature = "test-hooks")]
pub mod test_worker;
pub mod warmup;

use std::fs::File;
use std::time::{Duration, Instant};

use crate::ipc::FrameError;
use crate::ipc::frame::{read_frame, write_frame};
use crate::ipc::{
    EngineTimes, Fatal, Init, Op, Output, PROTOCOL_VERSION, Ready, Reply, Request, Response,
    WorkerError,
};

/// The role a worker process runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// LibreOfficeKit: office documents to PDF.
    Office,
    /// PDFium: inspect, render and decrypt PDFs.
    Render,
    /// Fake answers driven by `FAKE:` markers (INTERFACES.md §10).
    #[cfg(feature = "test-hooks")]
    Test,
}

impl Role {
    /// The name used on the command line and in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Office => "office",
            Role::Render => "render",
            #[cfg(feature = "test-hooks")]
            Role::Test => "test",
        }
    }
}

/// The answer of an engine to one operation.
pub struct Handled {
    pub result: Result<Output, WorkerError>,
    pub engine: EngineTimes,
}

impl Handled {
    pub fn ok(output: Output) -> Self {
        Self {
            result: Ok(output),
            engine: EngineTimes::default(),
        }
    }

    pub fn err(err: WorkerError) -> Self {
        Self {
            result: Err(err),
            engine: EngineTimes::default(),
        }
    }
}

/// One engine inside a worker process. `Ping` never reaches it.
pub trait Engine {
    /// The engine's version for `Ready`; empty when it has none.
    fn version(&self) -> String;
    fn handle(&mut self, op: Op) -> Handled;
}

/// Runs a worker until it leaves. A worker never returns: it ends with `_exit`.
pub fn run(role: Role) -> ! {
    let sys::ProtocolIo {
        mut input,
        mut output,
    } = match sys::setup() {
        Ok(io) => io,
        Err(err) => die(2, &format!("failed to set up the worker process: {err}")),
    };
    sys::install_panic_hook();

    let mut buf = Vec::with_capacity(4096);
    let mut out = Vec::with_capacity(4096);
    let init = match read_frame::<_, Request>(&mut input, &mut buf) {
        Ok(Some(Request::Init(init))) => init,
        Ok(None) => sys::exit(0),
        Ok(Some(Request::Call(_))) => die(2, "failed to start the worker: no init frame"),
        Err(err) => die(2, &format!("failed to read init: {err}")),
    };
    if init.protocol != PROTOCOL_VERSION {
        let message = format!(
            "failed to start the worker: protocol {} is not {PROTOCOL_VERSION}",
            init.protocol
        );
        fatal(&mut output, &mut out, message);
    }

    let bind_started = Instant::now();
    let engine = start_engine(role, &init);
    let bind_us = micros(bind_started.elapsed());
    let engine = match engine {
        Ok(engine) => engine,
        Err(message) => fatal(&mut output, &mut out, message),
    };
    let mut rss = sys::RssReader::new();
    let ready = Response::Ready(Ready {
        protocol: PROTOCOL_VERSION,
        pid: std::process::id(),
        engine_version: engine.version(),
        bind_us,
        rss_bytes: rss.read(),
    });
    send(&mut output, &ready, &mut out);
    serve(engine, input, output, rss)
}

fn start_engine(role: Role, init: &Init) -> Result<Box<dyn Engine>, String> {
    match role {
        Role::Office => office::OfficeWorker::start(init).map(|e| Box::new(e) as Box<dyn Engine>),
        Role::Render => render::RenderWorker::start(init).map(|e| Box::new(e) as Box<dyn Engine>),
        #[cfg(feature = "test-hooks")]
        Role::Test => test_worker::TestEngine::start(init).map(|e| Box::new(e) as Box<dyn Engine>),
    }
}

fn serve(
    mut engine: Box<dyn Engine>,
    mut input: File,
    mut output: File,
    mut rss: sys::RssReader,
) -> ! {
    let mut buf = Vec::with_capacity(4096);
    let mut out = Vec::with_capacity(4096);
    let mut jobs: u32 = 0;
    loop {
        let call = match read_frame::<_, Request>(&mut input, &mut buf) {
            Ok(Some(Request::Call(call))) => call,
            // The supervisor closed the pipe: leave at once.
            Ok(None) => sys::exit(0),
            Ok(Some(Request::Init(_))) => die(2, "failed to serve: a second init frame"),
            Err(err) => die(2, &format!("failed to read a call: {err}")),
        };
        jobs = jobs.saturating_add(1);
        let handled = match call.op {
            Op::Ping => Handled::ok(Output::Pong),
            op => engine.handle(op),
        };
        let rss_bytes = rss.read();
        let reply = Response::Reply(Reply {
            id: call.id,
            job: jobs,
            engine: handled.engine,
            rss_bytes,
            result: handled.result,
        });
        match write_frame(&mut output, &reply, &mut out) {
            Ok(()) => {}
            // Refused before any byte was written: answer with an error that
            // fits, instead of leaving without a reply.
            Err(FrameError::TooLarge { .. }) => {
                let small = Response::Reply(Reply {
                    id: call.id,
                    job: jobs,
                    engine: handled.engine,
                    rss_bytes,
                    result: Err(oversize_reply()),
                });
                send(&mut output, &small, &mut out);
            }
            Err(err) => die(0, &format!("failed to send a frame: {err}")),
        }
    }
}

/// The error for a reply that does not fit in one frame.
pub fn oversize_reply() -> WorkerError {
    WorkerError::new(
        crate::ipc::WorkerErrorKind::LimitExceeded,
        "failed to answer: the reply exceeds the 16 MiB frame limit",
    )
}

fn send(output: &mut File, msg: &Response, buf: &mut Vec<u8>) {
    if let Err(err) = write_frame(output, msg, buf) {
        // A broken pipe means the supervisor is gone.
        die(0, &format!("failed to send a frame: {err}"));
    }
}

fn fatal(output: &mut File, buf: &mut Vec<u8>, message: String) -> ! {
    eprintln!("convertd worker: {message}");
    let _ = write_frame(output, &Response::Fatal(Fatal { message }), buf);
    sys::exit(1)
}

fn die(code: i32, message: &str) -> ! {
    eprintln!("convertd worker: {message}");
    sys::exit(code)
}

/// Whole microseconds, saturating.
pub fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}
