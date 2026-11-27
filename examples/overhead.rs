//! Request overhead from `Server-Timing`: the check with the
//! level-A fakes in-process, and the benchmark of record against a
//! running server or image (`src/README.md`, Measurement).
//!
//! ```text
//! # In-process, level-A fakes
//! cargo run --release --example overhead -- --requests 5000 --warmup 500 --input docx
//! cargo run --release --example overhead -- --input docx --engine-ms 30
//! cargo run --release --example overhead -- --cold 200
//!
//! # A running server or image
//! CONVERT_API_KEY=... cargo run --release --example overhead -- \
//!   --url http://127.0.0.1:8080 --input corpus/out/docx/one-page.docx \
//!   --requests 2000 --warmup 200 --samples bench/docx-pdf.jsonl
//! ```
//!
//! One raw keep-alive TCP client sends the same multipart request again
//! and again, one at a time, and reads each answer into a fixed buffer
//! (`Content-Length` or chunked). Every answer must be 200. For each
//! measured request it keeps every `Server-Timing` stage and computes:
//!
//! - `overhead` = `total − queue − Σ *_engine` (convert, inspect, probe,
//!   render): the 5 ms gate;
//! - `overhead_strict` = `overhead` + `inspect_engine` + `probe_engine`;
//! - `share_supervisor` = `total − Σ engine-stage walls` (convert, inspect,
//!   render);
//! - `share_workers` = `Σ (stage wall − stage engine time) − queue` over the
//!   same stages (probe runs inside inspect);
//! - `client`: the wall time the client saw, upload and download included.
//!
//! It prints one Markdown table: count, p50, p95, p99 and mean of every
//! stage that was sent and of the five figures. `--samples` also writes
//! one JSON line per measured request, with every stage in microseconds;
//! `--report a.jsonl b.jsonl ...` sends nothing and prints the table
//! pooled over such files.
//!
//! In-process, `FakeOffice` and `FakePdf` sleep `--engine-ms` per call (0
//! by default). The cold scenario (`--cold N`) waits 11 s before each
//! request, so tokio has retired its blocking threads; 200 requests take
//! about 37 minutes, so it is opt-in.
//!
//! `--input` is `docx` (a generated DOCX of about 15 KB), `pdf`, or a path.
//! `--options` is the JSON of the `options` part, for example
//! `{"output":"bundle","images":{"format":"jpeg","dpi":72}}`.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use convertd::config::{Config, Env};
use convertd::engines::fake::{FakeBehavior, FakeOffice, FakePdf, FakeReadiness};
use convertd::rest::server::router;
use convertd::rest::state::{AppState, Engines};

const IN_PROCESS_KEY: &str = "overhead-key";
const BOUNDARY: &str = "overhead-boundary";

/// The 18 `Server-Timing` names, in the order the server sends them.
const STAGES: [&str; 18] = [
    "recv",
    "detect",
    "preflight",
    "queue",
    "lease",
    "spawn",
    "bind",
    "convert",
    "convert_engine",
    "inspect",
    "inspect_engine",
    "probe",
    "probe_engine",
    "render",
    "render_engine",
    "verify",
    "package",
    "total",
];

#[derive(Parser)]
#[command(about = "Request overhead from Server-Timing, in-process or against a server")]
struct Args {
    /// Measured requests.
    #[arg(long, default_value_t = 5000)]
    requests: usize,
    /// Requests before the measurement starts.
    #[arg(long, default_value_t = 500)]
    warmup: usize,
    /// `docx`, `pdf`, or a path to a file.
    #[arg(long, default_value = "docx")]
    input: String,
    /// The JSON of the `options` part; none by default.
    #[arg(long)]
    options: Option<String>,
    /// A running server, `http://host:port`. Without it the server runs
    /// in-process with the level-A fakes.
    #[arg(long)]
    url: Option<String>,
    /// The API key for `--url`; defaults to `$CONVERT_API_KEY`.
    #[arg(long)]
    api_key: Option<String>,
    /// In-process only: engine time of every fake call, in milliseconds.
    #[arg(long, default_value_t = 0)]
    engine_ms: u64,
    /// Run the cold scenario with this many requests instead.
    #[arg(long)]
    cold: Option<usize>,
    /// Write one JSON line per measured request to this file.
    #[arg(long)]
    samples: Option<PathBuf>,
    /// A note for the client row, such as what lies between client and
    /// server.
    #[arg(long)]
    client_note: Option<String>,
    /// Send nothing: print the table pooled over these `--samples` files.
    #[arg(long, num_args = 1..)]
    report: Vec<PathBuf>,
}

fn main() {
    let args = Args::parse();
    if !args.report.is_empty() {
        let samples: Vec<Sample> = args.report.iter().flat_map(|p| read_samples(p)).collect();
        println!(
            "pooled over {} files: {} requests",
            args.report.len(),
            samples.len()
        );
        println!();
        print!("{}", table(&samples, args.client_note.as_deref()));
        return;
    }
    let (name, file) = input(&args.input);
    let mut runtime = None;
    let mut tmp = None;
    let (addr, key) = match &args.url {
        Some(url) => {
            let key = args
                .api_key
                .clone()
                .or_else(|| std::env::var("CONVERT_API_KEY").ok())
                .expect("--api-key or CONVERT_API_KEY");
            (resolve(url), key)
        }
        None => {
            let dir =
                std::env::temp_dir().join(format!("convertd-overhead-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create the temp directory");
            let rt = convertd::runtime::serve(None).expect("build the runtime");
            let addr = rt.block_on(start(&dir, Duration::from_millis(args.engine_ms)));
            runtime = Some(rt);
            tmp = Some(dir);
            (addr, IN_PROCESS_KEY.to_owned())
        }
    };
    let request = request(&name, &file, args.options.as_deref(), &key);

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

    let target = match &args.url {
        Some(url) => format!("server {url}"),
        None => format!("in-process fakes, engine {} ms", args.engine_ms),
    };
    let scenario = if args.cold.is_some() { "cold" } else { "warm" };
    println!(
        "{scenario}, {target}: input {name} ({} bytes), options {}, {} measured after {} warm-up",
        file.len(),
        args.options.as_deref().unwrap_or("none"),
        samples.len(),
        if args.cold.is_some() { 1 } else { args.warmup },
    );
    println!();
    print!("{}", table(&samples, args.client_note.as_deref()));
    let count = |f: &dyn Fn(&Sample) -> bool| samples.iter().filter(|s| f(s)).count();
    println!();
    println!(
        "requests with a worker started on their path (spawn or bind > 0): {}; with a queue wait > 0.1 ms: {}",
        count(&|s| s.stage("spawn") > 0.0 || s.stage("bind") > 0.0),
        count(&|s| s.stage("queue") > 0.1),
    );
    if let Some(path) = &args.samples {
        write_samples(path, &samples);
    }
    drop(runtime);
    if let Some(dir) = tmp {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn resolve(url: &str) -> SocketAddr {
    let host_port = url
        .strip_prefix("http://")
        .expect("--url must start with http://")
        .trim_end_matches('/');
    host_port
        .to_socket_addrs()
        .expect("resolve --url")
        .next()
        .expect("--url has an address")
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
        ("API_KEY", IN_PROCESS_KEY),
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

/// The whole request, built once: the `options` part (if any), then the
/// file.
fn request(name: &str, file: &[u8], options: Option<&str>, key: &str) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(options) = options {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"options\"\r\n\
                 Content-Type: application/json\r\n\r\n{options}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let mut out = format!(
        "POST /v1/convert HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {key}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(&body);
    out
}

/// One measured request: every stage the server sent, in milliseconds.
struct Sample {
    client: f64,
    body_bytes: u64,
    stages: [Option<f64>; 18],
}

impl Sample {
    fn stage(&self, name: &str) -> f64 {
        STAGES
            .iter()
            .position(|s| *s == name)
            .and_then(|i| self.stages[i])
            .unwrap_or(0.0)
    }

    fn overhead(&self) -> f64 {
        let engines: f64 = [
            "convert_engine",
            "inspect_engine",
            "probe_engine",
            "render_engine",
        ]
        .iter()
        .map(|s| self.stage(s))
        .sum();
        self.stage("total") - self.stage("queue") - engines
    }

    fn overhead_strict(&self) -> f64 {
        self.overhead() + self.stage("inspect_engine") + self.stage("probe_engine")
    }

    fn share_supervisor(&self) -> f64 {
        let walls: f64 = ["convert", "inspect", "render"]
            .iter()
            .map(|s| self.stage(s))
            .sum();
        self.stage("total") - walls
    }

    fn share_workers(&self) -> f64 {
        let convert = self.stage("convert") - self.stage("convert_engine");
        let inspect =
            self.stage("inspect") - self.stage("inspect_engine") - self.stage("probe_engine");
        let render = self.stage("render") - self.stage("render_engine");
        convert + inspect + render - self.stage("queue")
    }
}

/// One keep-alive connection that reads each answer into a fixed buffer.
struct Client {
    stream: BufReader<TcpStream>,
    buf: Box<[u8; 64 * 1024]>,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect");
        stream.set_nodelay(true).expect("nodelay");
        stream
            .set_read_timeout(Some(Duration::from_secs(300)))
            .expect("read timeout");
        Self {
            stream: BufReader::with_capacity(64 * 1024, stream),
            buf: Box::new([0u8; 64 * 1024]),
        }
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        let n = self.stream.read_line(&mut line).expect("read a line");
        assert!(n > 0, "closed in the answer");
        line
    }

    /// Reads exactly `length` bytes into the fixed buffer, discarding them.
    fn discard(&mut self, mut length: u64) {
        while length > 0 {
            let want = length.min(self.buf.len() as u64) as usize;
            let n = self
                .stream
                .read(&mut self.buf[..want])
                .expect("read the body");
            assert!(n > 0, "closed in the body");
            length -= n as u64;
        }
    }

    fn run(&mut self, request: &[u8]) -> Sample {
        let started = Instant::now();
        self.stream.get_mut().write_all(request).expect("send");
        let status = self.line();
        let mut head = Vec::new();
        loop {
            let line = self.line();
            if line == "\r\n" {
                break;
            }
            head.push(line.trim_end().to_owned());
        }
        let header = |name: &str| {
            head.iter().find_map(|line| {
                let (n, v) = line.split_once(": ")?;
                n.eq_ignore_ascii_case(name).then(|| v.to_owned())
            })
        };
        let body_bytes = if let Some(length) = header("content-length") {
            let length: u64 = length.parse().expect("Content-Length");
            self.discard(length);
            length
        } else {
            assert_eq!(
                header("transfer-encoding").as_deref(),
                Some("chunked"),
                "{status}"
            );
            let mut total = 0;
            loop {
                let size_line = self.line();
                let size =
                    u64::from_str_radix(size_line.trim_end().split(';').next().unwrap_or("0"), 16)
                        .expect("chunk size");
                if size == 0 {
                    // Trailers, then the blank line.
                    while self.line() != "\r\n" {}
                    break;
                }
                self.discard(size);
                assert_eq!(self.line(), "\r\n", "chunk end");
                total += size;
            }
            total
        };
        let client = started.elapsed().as_secs_f64() * 1000.0;
        assert!(status.starts_with("HTTP/1.1 200"), "{status} {head:?}");
        let timing = header("server-timing").expect("Server-Timing");
        let mut stages = [None; 18];
        for entry in timing.split(", ") {
            let Some((name, dur)) = entry.split_once(";dur=") else {
                continue;
            };
            if let (Some(i), Ok(ms)) = (STAGES.iter().position(|s| *s == name), dur.parse()) {
                stages[i] = Some(ms);
            }
        }
        Sample {
            client,
            body_bytes,
            stages,
        }
    }
}

/// count, p50, p95, p99 and mean of `values`, in milliseconds.
fn stats(mut values: Vec<f64>) -> Option<(usize, f64, f64, f64, f64)> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let at = |p: f64| values[((values.len() as f64 - 1.0) * p).round() as usize];
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    Some((values.len(), at(0.50), at(0.95), at(0.99), mean))
}

fn table(samples: &[Sample], client_note: Option<&str>) -> String {
    let mut out = String::from(
        "| Figure | n | p50 ms | p95 ms | p99 ms | mean ms |\n| --- | ---: | ---: | ---: | ---: | ---: |\n",
    );
    let mut row = |name: &str, values: Vec<f64>| {
        if let Some((n, p50, p95, p99, mean)) = stats(values) {
            let _ = writeln!(
                out,
                "| {name} | {n} | {p50:.3} | {p95:.3} | {p99:.3} | {mean:.3} |"
            );
        }
    };
    for (i, name) in STAGES.iter().enumerate() {
        row(
            &format!("`{name}`"),
            samples.iter().filter_map(|s| s.stages[i]).collect(),
        );
    }
    row(
        "**overhead** (gate)",
        samples.iter().map(Sample::overhead).collect(),
    );
    row(
        "overhead_strict",
        samples.iter().map(Sample::overhead_strict).collect(),
    );
    row(
        "share_supervisor",
        samples.iter().map(Sample::share_supervisor).collect(),
    );
    row(
        "share_workers",
        samples.iter().map(Sample::share_workers).collect(),
    );
    let client = match client_note {
        Some(note) => format!("client ({note})"),
        None => "client".to_owned(),
    };
    row(&client, samples.iter().map(|s| s.client).collect());
    out
}

/// Reads a `--samples` file back.
fn read_samples(path: &Path) -> Vec<Sample> {
    let text = std::fs::read_to_string(path).expect("read --report file");
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect("a JSON line");
            let ms = |key: &str| {
                value
                    .get(key)
                    .and_then(serde_json::Value::as_f64)
                    .map(|us| us / 1000.0)
            };
            let mut stages = [None; 18];
            for (i, name) in STAGES.iter().enumerate() {
                stages[i] = ms(&format!("{name}_us"));
            }
            Sample {
                client: ms("client_us").unwrap_or(0.0),
                body_bytes: value
                    .get("body_bytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                stages,
            }
        })
        .collect()
}

/// One JSON line per sample: every stage sent, in microseconds.
fn write_samples(path: &Path, samples: &[Sample]) {
    let mut out = String::new();
    for sample in samples {
        out.push('{');
        let _ = write!(
            out,
            "\"client_us\":{},\"body_bytes\":{}",
            (sample.client * 1000.0).round(),
            sample.body_bytes
        );
        for (i, name) in STAGES.iter().enumerate() {
            if let Some(ms) = sample.stages[i] {
                let _ = write!(out, ",\"{name}_us\":{}", (ms * 1000.0).round());
            }
        }
        let _ = write!(
            out,
            ",\"overhead_us\":{},\"overhead_strict_us\":{}",
            (sample.overhead() * 1000.0).round(),
            (sample.overhead_strict() * 1000.0).round()
        );
        out.push_str("}\n");
    }
    std::fs::write(path, out).expect("write --samples");
}
