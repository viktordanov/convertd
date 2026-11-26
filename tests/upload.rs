//! The streamed upload (plan 001 step 5): digests, part rules, caps, the
//! disk budget, a stalled and a vanished client. After each case no job
//! directory is left.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use convertd::config::{Config, Env};
use convertd::domain::document::Format;
use convertd::engines::fake::{FakeBehavior, FakeOffice, FakePdf, FakeReadiness};
use convertd::jobs::{DiskBudget, JobDir};
use convertd::rest::server::router;
use convertd::rest::state::{AppState, Engines};
use convertd::rest::throughput::MinRate;
use convertd::rest::upload::receive;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const KEY: &str = "test-key";
const BOUNDARY: &str = "convertd-upload-boundary-4b1d";

static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn log_lines(needle: &str) -> Vec<Value> {
    let logs = LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = buffer.clone();
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_writer(move || Capture(writer.clone()))
            .init();
        buffer
    });
    let bytes = logs.lock().unwrap().clone();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| line.contains(needle))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn tmp(name: &str) -> PathBuf {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("upload-{name}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    tmp
}

struct App {
    state: AppState,
    tmp: PathBuf,
}

impl App {
    fn new(name: &str, env: &[(&str, &str)]) -> Self {
        log_lines("");
        let tmp = tmp(name);
        let tmp_text = tmp.to_str().unwrap().to_owned();
        let mut pairs = vec![("API_KEY", KEY), ("TMP_DIR", tmp_text.as_str())];
        pairs.extend_from_slice(env);
        let config = Config::from_env(&Env::from_pairs(pairs, 2)).unwrap();
        let state = AppState::new(
            &config,
            Engines {
                office: Arc::new(FakeOffice::new(FakeBehavior::Succeed)),
                pdf: Arc::new(FakePdf::new(FakeBehavior::Succeed)),
                readiness: Arc::new(FakeReadiness::new(true)),
            },
        );
        Self { state, tmp }
    }

    fn router(&self) -> Router {
        router(self.state.clone())
    }

    async fn serve(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = self.router();
        tokio::spawn(async move { axum::serve(listener, app).await });
        addr
    }

    fn jobs(&self) -> Vec<String> {
        std::fs::read_dir(&self.tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("job-"))
            .collect()
    }

    /// Polls up to 1 s: removal runs on a blocking thread.
    async fn assert_no_jobs(&self) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !self.jobs().is_empty() {
            assert!(Instant::now() < deadline, "left: {:?}", self.jobs());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Sends `body` as a convert request; returns the status and the body.
    async fn send(&self, body: Vec<u8>) -> (StatusCode, Value) {
        let request = convert(body.len().to_string(), Body::from(body));
        let response = self.router().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

fn convert(content_length: String, body: Body) -> Request<Body> {
    Request::post("/v1/convert")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header(header::CONTENT_LENGTH, content_length)
        .body(body)
        .unwrap()
}

/// A part: `(name, file name, bytes)`.
type Part<'a> = (&'a str, Option<&'a str>, &'a [u8]);

fn multipart(parts: &[Part]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, file_name, data) in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let disposition = match file_name {
            Some(file) => {
                format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{file}\"\r\n")
            }
            None => format!("Content-Disposition: form-data; name=\"{name}\"\r\n"),
        };
        body.extend_from_slice(disposition.as_bytes());
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n%%EOF\n";

/// Deterministic noise, so nothing about the bytes is special.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digests_of_a_five_mib_file_match() {
    let tmp = tmp("digests");
    let mut data = PDF.to_vec();
    data.extend(noise(5 * 1024 * 1024));
    let body = multipart(&[("file", Some("noise.pdf"), &data)]);
    let request = convert(body.len().to_string(), Body::from(body));
    let job = JobDir::create(&tmp, &DiskBudget::new(1 << 30)).unwrap();
    let limits = Config::from_env(&Env::from_pairs([("API_KEY", "k")], 2))
        .unwrap()
        .limits;
    let mut detected = None;
    let received = receive(request, &job, &limits, MinRate::off(), &mut detected)
        .await
        .unwrap();
    assert!(detected.is_some());
    assert_eq!(received.upload.size_bytes, data.len() as u64);
    assert_eq!(
        received.upload.sha256,
        <[u8; 32]>::from(Sha256::digest(&data))
    );
    assert_eq!(received.upload.crc32, crc32fast::hash(&data));
    assert_eq!(received.upload.file_name.as_deref(), Some("noise.pdf"));
    assert_eq!(received.detection.format, Format::Pdf);
    assert_eq!(received.source, job.root().join("source.pdf"));
    assert_eq!(std::fs::read(&received.source).unwrap(), data);
    assert!(!job.upload_part().exists());
    // The reservation was trued up to the real size.
    assert_eq!(job.budget().reserved(), data.len() as u64);
}

#[tokio::test]
async fn options_before_and_after_the_file() {
    let app = App::new("order", &[]);
    for parts in [
        &[
            ("options", None, &br#"{"timeout_seconds":30}"#[..]),
            ("file", Some("a.pdf"), PDF),
        ][..],
        &[
            ("file", Some("a.pdf"), PDF),
            ("options", None, br#"{"timeout_seconds":30}"#),
        ][..],
        &[("file", Some("a.pdf"), PDF)][..],
    ] {
        let (status, body) = app.send(multipart(parts)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn part_rules_give_400() {
    let app = App::new("parts", &[]);
    let cases: [(&[Part], &str); 6] = [
        (
            &[("file", Some("a.pdf"), PDF), ("file", Some("b.pdf"), PDF)],
            "duplicate multipart part \"file\"",
        ),
        (
            &[
                ("options", None, b"{}"),
                ("options", None, b"{}"),
                ("file", Some("a.pdf"), PDF),
            ],
            "duplicate multipart part \"options\"",
        ),
        (
            &[("colour", None, b"red"), ("file", Some("a.pdf"), PDF)],
            "unknown multipart part \"colour\"",
        ),
        (
            &[("options", None, b"{}")],
            "missing multipart part \"file\"",
        ),
        (&[], "missing multipart part \"file\""),
        (
            &[("file", Some("empty.pdf"), b"")],
            "the file part is empty",
        ),
    ];
    for (parts, message) in cases {
        let (status, body) = app.send(multipart(parts)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{message}: {body}");
        assert_eq!(body["code"], "BAD_REQUEST");
        assert_eq!(body["error"], message);
        app.assert_no_jobs().await;
    }
    // Not multipart at all.
    let request = Request::post("/v1/convert")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = app.router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn options_errors() {
    let app = App::new("options", &[]);
    let cases: [(&[u8], StatusCode, &str); 6] = [
        (b"{\"output\":", StatusCode::BAD_REQUEST, "BAD_REQUEST"),
        (b"not json", StatusCode::BAD_REQUEST, "BAD_REQUEST"),
        (
            br#"{"outptu":"pdf"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
        ),
        (
            br#"{"timeout_seconds":"sixty"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
        ),
        (
            br#"{"images":{"dpi":10}}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
        ),
        (
            br#"{"pages":"5-3"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
        ),
    ];
    for (options, status, code) in cases {
        // Options first: refused before the file is stored.
        let (got, body) = app
            .send(multipart(&[
                ("options", None, options),
                ("file", Some("a.pdf"), PDF),
            ]))
            .await;
        assert_eq!(got, status, "{}: {body}", String::from_utf8_lossy(options));
        assert_eq!(body["code"], code);
        app.assert_no_jobs().await;
    }
    let (_, body) = app
        .send(multipart(&[
            ("options", None, br#"{"images":{"dpi":10}}"#),
            ("file", None, PDF),
        ]))
        .await;
    assert_eq!(body["error"], "images.dpi must be between 36 and 600");
    let big = vec![b' '; 70 * 1024];
    let (status, _) = app
        .send(multipart(&[
            ("options", None, &big),
            ("file", Some("a.pdf"), PDF),
        ]))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn a_streamed_body_over_the_cap_with_a_lying_length() {
    let app = App::new("lying", &[("MAX_UPLOAD_BYTES", "1048576")]);
    let data = noise(2 * 1024 * 1024);
    let body = multipart(&[("file", Some("big.bin"), &data)]);
    for content_length in [Some("1000"), None] {
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = body
            .chunks(64 * 1024)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        let mut request = Request::post("/v1/convert")
            .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            );
        if let Some(length) = content_length {
            request = request.header(header::CONTENT_LENGTH, length);
        }
        let request = request
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .unwrap();
        let response = app.router().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "{content_length:?}"
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 16).await.unwrap())
                .unwrap();
        assert_eq!(body["error"], "file exceeds the 1048576 byte limit");
        app.assert_no_jobs().await;
    }
}

#[tokio::test]
async fn a_spent_disk_budget_is_503() {
    let app = App::new("budget", &[("TMP_MAX_BYTES", "4096")]);
    let data = noise(8 * 1024);
    let mut upload = PDF.to_vec();
    upload.extend(&data);
    let (status, body) = app
        .send(multipart(&[("file", Some("a.pdf"), &upload)]))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "SERVICE_UNAVAILABLE");
    let lines = log_lines("disk_full");
    assert!(
        lines
            .iter()
            .any(|l| l["message"] == "http request" && l["status_code"] == 503),
        "{lines:?}"
    );
    app.assert_no_jobs().await;
    assert_eq!(app.state.disk_budget.used(), 0);
}

fn head(content_length: usize) -> Vec<u8> {
    format!(
        "POST /v1/convert HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {KEY}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {content_length}\r\n\r\n"
    )
    .into_bytes()
}

/// The status of the response on `stream`, and its body.
/// The status and body of one answer: the head, then `Content-Length`
/// bytes. It does not wait for the close, which a lingering close delays
/// while the server drains an unread upload.
fn read_response(stream: &mut TcpStream) -> (u16, String) {
    let mut all = Vec::new();
    let mut byte = [0u8; 1];
    while !all.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => all.push(byte[0]),
            _ => break,
        }
    }
    let length = String::from_utf8_lossy(&all).lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())?
    });
    match length {
        Some(length) => {
            let mut body = vec![0u8; length];
            if stream.read_exact(&mut body).is_ok() {
                all.extend_from_slice(&body);
            }
        }
        None => {
            let _ = stream.read_to_end(&mut all);
        }
    }
    let text = String::from_utf8_lossy(&all).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
    (status, body)
}

fn round_trip(addr: SocketAddr, body: &[u8]) -> u16 {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut request = head(body.len());
    request.truncate(request.len() - 2);
    request.extend_from_slice(b"Connection: close\r\n\r\n");
    stream.write_all(&request).unwrap();
    // An early answer (429, 413) closes the connection before the body is
    // written; the status still says what happened.
    let _ = stream.write_all(body);
    read_response(&mut stream).0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_length_over_the_cap_is_413_before_a_byte_is_read() {
    let app = App::new("early-413", &[("MAX_UPLOAD_BYTES", "1048576")]);
    let addr = app.serve().await;
    let (status, body) = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // The head only: if the server waited for the body, this would hang.
        stream.write_all(&head(10 * 1024 * 1024)).unwrap();
        read_response(&mut stream)
    })
    .await
    .unwrap();
    assert_eq!(status, 413, "{body}");
    assert!(body.contains("FILE_TOO_LARGE"), "{body}");
    app.assert_no_jobs().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_upload_is_400_and_releases_its_permit_and_directory() {
    let app = App::new(
        "stall",
        &[
            ("IO_IDLE_TIMEOUT_SECONDS", "1"),
            ("MAX_CONCURRENT_REQUESTS", "1"),
        ],
    );
    let addr = app.serve().await;
    let mut upload = PDF.to_vec();
    upload.extend(noise(256 * 1024));
    let body = multipart(&[("file", Some("a.pdf"), &upload)]);
    let half = body.len() / 2;
    let started = Instant::now();
    let (status, text) = tokio::task::spawn_blocking({
        let body = body.clone();
        move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream.write_all(&head(body.len())).unwrap();
            stream.write_all(&body[..half]).unwrap();
            // Then nothing: the socket stays open.
            read_response(&mut stream)
        }
    })
    .await
    .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("upload stalled"), "{text}");
    assert!(
        elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(3),
        "{elapsed:?}"
    );
    app.assert_no_jobs().await;
    // The one permit is free again.
    let status = tokio::task::spawn_blocking(move || round_trip(addr, &body))
        .await
        .unwrap();
    assert_eq!(status, 200);
    app.assert_no_jobs().await;
}

/// A trickle that sends a little just inside every idle period never trips
/// the idle timer. The minimum-throughput floor ends it after one window,
/// with 400, and frees the permit (review, lane 001 item 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trickled_upload_is_400_and_releases_its_permit() {
    let app = App::new(
        "trickle",
        &[
            ("IO_IDLE_TIMEOUT_SECONDS", "1"),
            ("UPLOAD_MIN_BYTES_PER_SECOND", "65536"),
            ("MAX_CONCURRENT_REQUESTS", "1"),
        ],
    );
    let addr = app.serve().await;
    let mut upload = PDF.to_vec();
    upload.extend(noise(256 * 1024));
    let body = multipart(&[("file", Some("a.pdf"), &upload)]);
    let started = Instant::now();
    let (status, text) = tokio::task::spawn_blocking({
        let body = body.clone();
        move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream.write_all(&head(body.len())).unwrap();
            let reader = stream.try_clone().unwrap();
            // 64 bytes every 300 ms: well inside the 1 s idle timer.
            let trickle = std::thread::spawn(move || {
                for chunk in body.chunks(64) {
                    if stream.write_all(chunk).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            });
            let mut reader = reader;
            let answer = read_response(&mut reader);
            drop(reader);
            drop(trickle);
            answer
        }
    })
    .await
    .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("upload too slow"), "{text}");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    app.assert_no_jobs().await;
    let status = tokio::task::spawn_blocking(move || round_trip(addr, &body))
        .await
        .unwrap();
    assert_eq!(status, 200, "the permit is free again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_closes_mid_upload_releases_everything_at_once() {
    let app = App::new(
        "vanish",
        &[
            ("MAX_CONCURRENT_REQUESTS", "1"),
            ("IO_IDLE_TIMEOUT_SECONDS", "30"),
        ],
    );
    let addr = app.serve().await;
    let mut upload = PDF.to_vec();
    upload.extend(noise(4 * 1024 * 1024));
    let body = multipart(&[("file", Some("a.pdf"), &upload)]);
    let half = body.len() / 2;
    let stream = tokio::task::spawn_blocking({
        let body = body.clone();
        move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream.write_all(&head(body.len())).unwrap();
            stream.write_all(&body[..half]).unwrap();
            stream
        }
    })
    .await
    .unwrap();
    // Close only once the server is mid-upload: it holds the permit and the
    // job directory, and waits for the rest of the body. Closing earlier
    // races the request against the follow-up for the one permit.
    let started = Instant::now();
    while app.jobs().is_empty() || app.state.inflight.available_permits() != 0 {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the upload never started: jobs {:?}",
            app.jobs()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stream.shutdown(Shutdown::Both);
    drop(stream);
    // Well inside the 30 s idle timeout.
    let started = Instant::now();
    loop {
        let job_gone = app.jobs().is_empty();
        let permit_free = app.state.inflight.available_permits() == 1;
        if job_gone && permit_free {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "jobs {:?}, permit free {permit_free}",
            app.jobs()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let status = tokio::task::spawn_blocking(move || round_trip(addr, &body))
        .await
        .unwrap();
    assert_eq!(status, 200);
    app.assert_no_jobs().await;
}
