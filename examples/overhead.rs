//! 001's share of the request overhead, with the level-A fakes (plan 001,
//! Verification 1).
//!
//! ```text
//! cargo run --release --example overhead -- --requests 5000 --warmup 500 --input docx
//! cargo run --release --example overhead -- --input docx --engine-ms 30
//! cargo run --release --example overhead -- --input pdf
//! cargo run --release --example overhead -- --cold 200
//! ```
//!
//! It starts the server in-process on loopback, with `FakeOffice` and
//! `FakePdf` sleeping `--engine-ms` per call (0 by default), and drives it
//! with one raw keep-alive TCP client that writes the multipart from a
//! fixed buffer. It prints p50, p95 and p99 of:
//!
//! - `client`: wall time seen by the client, upload and download included;
//! - `overhead`: 005's formula, `total − queue − Σ *_engine`, from
//!   `Server-Timing`;
//! - `share_001`: `total − Σ engine-stage walls` (convert, inspect,
//!   render), which is 001's share, the figure for 001's 1 ms target.
//!
//! The two differ even at `--engine-ms 0`: a fake call's wall holds more
//! than its engine time, such as `FakeOffice`'s pre-flight scan and the
//! PDF it writes, as a worker call does.
//!
//! The scenarios of the plan:
//!
//! 1. warm, pre-flight on the critical path: `--engine-ms 0`;
//! 2. warm, with an engine time about the real one-page DOCX:
//!    `--engine-ms 30`. The office worker runs pre-flight before the
//!    engine, inside `convert`, so it counts in both scenarios;
//! 3. cold: `--cold N` sends N requests, each after 11 s of idle, so tokio
//!    has retired its blocking threads (`thread_keep_alive` 10 s). 200
//!    requests take about 37 minutes, so it is opt-in.
//!
//! `--input` is `docx` (a generated one-page DOCX of about 15 KB), `pdf`,
//! or a path to a file of your own.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use convertd::config::{Config, Env};
use convertd::engines::fake::{FakeBehavior, FakeOffice, FakePdf, FakeReadiness};
use convertd::rest::server::router;
use convertd::rest::state::{AppState, Engines};

const KEY: &str = "overhead-key";
const BOUNDARY: &str = "overhead-boundary";

#[derive(Parser)]
#[command(about = "001's share of the request overhead, with the level-A fakes")]
struct Args {
    /// Measured requests (warm scenarios).
    #[arg(long, default_value_t = 5000)]
    requests: usize,
    /// Requests before the measurement starts.
    #[arg(long, default_value_t = 500)]
    warmup: usize,
    /// `docx`, `pdf`, or a path to a file.
    #[arg(long, default_value = "docx")]
    input: String,
    /// Engine time of every fake call, in milliseconds.
    #[arg(long, default_value_t = 0)]
    engine_ms: u64,
    /// Run the cold scenario with this many requests instead.
    #[arg(long)]
    cold: Option<usize>,
}

fn main() {
    let args = Args::parse();
    let (name, file) = input(&args.input);
    let tmp = std::env::temp_dir().join(format!("convertd-overhead-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("create the temp directory");
    let runtime = convertd::runtime::serve(None).expect("build the runtime");
    let addr = runtime.block_on(start(&tmp, Duration::from_millis(args.engine_ms)));
    let request = request(&name, &file);

    let mut client = Client::connect(addr);
    let samples = match args.cold {
        Some(count) => {
            client.run(&request);
            let mut samples = Vec::with_capacity(count);
            for i in 0..count {
                std::thread::sleep(Duration::from_secs(11));
                // A new connection each time: the old one may have idled out.
                client = Client::connect(addr);
                samples.push(client.run(&request));
                eprint!("\rcold request {} of {count}", i + 1);
            }
            eprintln!();
            samples
        }
        None => {
            for _ in 0..args.warmup {
                client.run(&request);
            }
            (0..args.requests).map(|_| client.run(&request)).collect()
        }
    };
    let scenario = match (args.cold, args.engine_ms) {
        (Some(_), _) => "cold",
        (None, 0) => "warm, engine 0 ms",
        (None, _) => "warm, engine > 0 ms",
    };
    println!(
        "{scenario}: input {name} ({} bytes), engine {} ms, {} requests",
        file.len(),
        args.engine_ms,
        samples.len()
    );
    report("client", samples.iter().map(|s| s.client));
    report("overhead", samples.iter().map(|s| s.overhead));
    report("share_001", samples.iter().map(|s| s.share_001));
    drop(runtime);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The input's file name and bytes.
fn input(input: &str) -> (String, Vec<u8>) {
    match input {
        "docx" => ("small.docx".into(), small_docx()),
        "pdf" => ("small.pdf".into(), small_pdf()),
        path => {
            let path = PathBuf::from(path);
            let name = path
                .file_name()
                .map_or_else(|| "input".into(), |n| n.to_string_lossy().into_owned());
            (name, std::fs::read(&path).expect("read --input"))
        }
    }
}

/// A one-page DOCX of about 15 KB: a stored package, so its size is the
/// text's.
fn small_docx() -> Vec<u8> {
    let mut out = Vec::new();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("[Content_Types].xml", stored).expect("zip");
    zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#).expect("zip");
    zip.start_file("word/document.xml", stored).expect("zip");
    let mut document = String::from(
        r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
    );
    while document.len() < 14_500 {
        document.push_str(r#"<w:p><w:r><w:rPr><w:rFonts w:ascii="Liberation Serif"/></w:rPr><w:t>The quick brown fox jumps over the lazy dog.</w:t></w:r></w:p>"#);
    }
    document.push_str("</w:body></w:document>");
    zip.write_all(document.as_bytes()).expect("zip");
    zip.finish().expect("zip");
    out
}

fn small_pdf() -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec();
    pdf.resize(15_000, b' ');
    pdf.extend_from_slice(b"\n%%EOF\n");
    pdf
}

async fn start(tmp: &Path, engine: Duration) -> SocketAddr {
    let tmp_text = tmp.to_string_lossy().into_owned();
    let pairs = vec![
        ("API_KEY", KEY),
        ("TMP_DIR", tmp_text.as_str()),
        ("BUILD_INFO_PATH", "/nonexistent/build-info.json"),
    ];
    let config = Config::from_env(&Env::from_pairs(pairs, 2)).expect("configuration");
    let state = AppState::new(
        &config,
        Engines {
            office: Arc::new(FakeOffice::new(FakeBehavior::Succeed).with_engine_time(engine)),
            pdf: Arc::new(FakePdf::new(FakeBehavior::Succeed).with_engine_time(engine)),
            readiness: Arc::new(FakeReadiness::new(true)),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("address");
    tokio::spawn(async move { axum::serve(listener, router(state)).await });
    addr
}

/// The whole request, built once.
fn request(name: &str, file: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let mut out = format!(
        "POST /v1/convert HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {KEY}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(&body);
    out
}

struct Sample {
    client: Duration,
    overhead: Duration,
    share_001: Duration,
}

/// One keep-alive connection that reads each answer into a fixed buffer.
struct Client {
    stream: TcpStream,
    buf: Box<[u8; 64 * 1024]>,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect");
        stream.set_nodelay(true).expect("nodelay");
        Self {
            stream,
            buf: Box::new([0u8; 64 * 1024]),
        }
    }

    fn run(&mut self, request: &[u8]) -> Sample {
        let started = Instant::now();
        self.stream.write_all(request).expect("send");
        let mut head = Vec::with_capacity(1024);
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = self.stream.read(&mut byte).expect("read the head");
            assert_eq!(n, 1, "closed before the head");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let header = |name: &str| {
            head.split("\r\n")
                .find_map(|line| line.strip_prefix(name))
                .map(str::to_owned)
        };
        let length: usize = header("content-length: ")
            .and_then(|v| v.parse().ok())
            .expect("a PDF answer has Content-Length");
        let mut read = 0;
        while read < length {
            let n = self.stream.read(&mut self.buf[..]).expect("read the body");
            assert!(n > 0, "closed in the body");
            read += n;
        }
        let client = started.elapsed();
        let timing = header("server-timing: ").expect("Server-Timing");
        let stage = |name: &str| -> Duration {
            timing
                .split(", ")
                .find_map(|entry| {
                    let (entry_name, dur) = entry.split_once(";dur=")?;
                    (entry_name == name).then(|| dur.parse::<f64>().ok())?
                })
                .map_or(Duration::ZERO, |ms| Duration::from_secs_f64(ms / 1000.0))
        };
        let total = stage("total");
        let engines = [
            "convert_engine",
            "inspect_engine",
            "probe_engine",
            "render_engine",
        ]
        .into_iter()
        .map(stage)
        .sum::<Duration>();
        let walls = ["convert", "inspect", "render"]
            .into_iter()
            .map(stage)
            .sum::<Duration>();
        Sample {
            client,
            overhead: total.saturating_sub(stage("queue")).saturating_sub(engines),
            share_001: total.saturating_sub(walls),
        }
    }
}

fn report(name: &str, values: impl Iterator<Item = Duration>) {
    let mut values: Vec<Duration> = values.collect();
    values.sort_unstable();
    let at = |p: f64| {
        let index = ((values.len() as f64 - 1.0) * p).round() as usize;
        values[index].as_secs_f64() * 1000.0
    };
    println!(
        "  {name:<10} p50 {:7.3} ms   p95 {:7.3} ms   p99 {:7.3} ms",
        at(0.50),
        at(0.95),
        at(0.99)
    );
}
