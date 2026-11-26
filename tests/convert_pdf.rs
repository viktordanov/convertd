//! The first end-to-end slice (plan 001 step 7): `/v1/convert` with
//! `output=pdf` through the level-A fakes, real detection, the response
//! pump, the backstop, the permit and the response-stall rule of
//! INTERFACES.md §10.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use convertd::config::{Config, Env};
use convertd::domain::engine::EngineError;
use convertd::engines::fake::{FakeBehavior, FakeOffice, FakePdf, FakeReadiness};
use convertd::rest::server::router;
use convertd::rest::state::{AppState, Engines};
use serde_json::Value;
use tower::ServiceExt;

const KEY: &str = "test-key";

// ------------------------------------------------------------ harness ----

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

/// JSON log lines of this test binary that mention `needle`.
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

struct App {
    state: AppState,
    office: Arc<FakeOffice>,
    tmp: PathBuf,
}

impl App {
    fn new(name: &str, env: &[(&str, &str)]) -> Self {
        Self::with_pdf(name, env, FakePdf::new(FakeBehavior::Succeed))
    }

    fn with_pdf(name: &str, env: &[(&str, &str)], pdf: FakePdf) -> Self {
        log_lines("");
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("convert_pdf-{name}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let tmp_text = tmp.to_str().unwrap().to_owned();
        let mut pairs = vec![
            ("API_KEY", KEY),
            ("TMP_DIR", tmp_text.as_str()),
            ("BUILD_INFO_PATH", "/nonexistent/build-info.json"),
        ];
        pairs.extend_from_slice(env);
        let config = Config::from_env(&Env::from_pairs(pairs, 2)).unwrap();
        let office = Arc::new(FakeOffice::new(FakeBehavior::Succeed));
        let pdf = Arc::new(pdf);
        let state = AppState::new(
            &config,
            Engines {
                office: office.clone(),
                pdf: pdf.clone(),
                readiness: Arc::new(FakeReadiness::new(true)),
            },
        );
        Self { state, office, tmp }
    }

    fn router(&self) -> Router {
        router(self.state.clone())
    }

    /// Serves on a loopback port in this runtime.
    async fn serve(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = self.router();
        tokio::spawn(async move { axum::serve(listener, app).await });
        addr
    }

    /// Waits up to 2 s for every job directory to be gone.
    async fn assert_no_jobs(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let jobs: Vec<_> = std::fs::read_dir(&self.tmp)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("job-"))
                .collect();
            if jobs.is_empty() {
                return;
            }
            assert!(Instant::now() < deadline, "job directories left: {jobs:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

struct Part<'a> {
    name: &'a str,
    file_name: Option<&'a str>,
    content_type: &'a str,
    data: &'a [u8],
}

fn file(file_name: &str, data: &[u8]) -> Vec<u8> {
    multipart(&[Part {
        name: "file",
        file_name: Some(file_name),
        content_type: "application/octet-stream",
        data,
    }])
}

fn with_options(file_name: &str, data: &[u8], options: &str) -> Vec<u8> {
    multipart(&[
        Part {
            name: "options",
            file_name: None,
            content_type: "application/json",
            data: options.as_bytes(),
        },
        Part {
            name: "file",
            file_name: Some(file_name),
            content_type: "application/octet-stream",
            data,
        },
    ])
}

const BOUNDARY: &str = "convertd-test-boundary-7f3c2a9e";

fn multipart(parts: &[Part]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part.file_name {
            Some(name) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{name}\"\r\n",
                    part.name
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name).as_bytes(),
            ),
        }
        body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", part.content_type).as_bytes());
        body.extend_from_slice(part.data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn convert_request(body: Vec<u8>) -> Request<Body> {
    Request::post("/v1/convert")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

async fn send(app: &App, body: Vec<u8>) -> (Response, Vec<u8>) {
    let response = app.router().oneshot(convert_request(body)).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 256 << 20).await.unwrap().to_vec();
    (Response::from_parts(parts, Body::empty()), bytes)
}

fn error_code(body: &[u8]) -> String {
    let json: Value = serde_json::from_slice(body)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(body)));
    json["code"].as_str().unwrap().to_owned()
}

fn header_str<'a>(response: &'a Response, name: &str) -> &'a str {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("no {name}"))
        .to_str()
        .unwrap()
}

// ------------------------------------------------------------ fixtures ----

/// A minimal DOCX; `marker` goes into a stored `word/document.xml`.
fn docx(text: &str) -> Vec<u8> {
    let types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    let document = format!(
        r#"<w:document><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#
    );
    stored_zip(&[
        ("[Content_Types].xml", types.as_bytes()),
        ("word/document.xml", document.as_bytes()),
    ])
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32fast::hash(data);
        let head = |sig: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&sig.to_le_bytes());
        };
        head(0x0403_4b50, &mut out);
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        head(0x0201_4b50, &mut central);
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

fn pdf(extra: &str) -> Vec<u8> {
    format!("%PDF-1.4\n%{extra}\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n").into_bytes()
}

/// A PDF of `len` bytes, its tail varied so nothing compresses it away.
fn big_pdf(len: usize) -> Vec<u8> {
    let mut data = pdf("big");
    data.extend((0..len - data.len()).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8));
    data
}

const PNG_1X1: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0\x90wS\xde";

// ---------------------------------------------------------------- tests ----

#[tokio::test]
async fn docx_to_pdf_with_every_header() {
    let app = App::new("docx", &[]);
    let (response, body) = send(&app, file("deck.pdf", &docx("Hello"))).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(header_str(&response, "content-type"), "application/pdf");
    assert_eq!(
        header_str(&response, "content-length"),
        body.len().to_string()
    );
    // Named "deck.pdf", detected as DOCX from its bytes.
    assert_eq!(header_str(&response, "convert-source-format"), "docx");
    assert_eq!(
        header_str(&response, "content-disposition"),
        "attachment; filename=\"deck.pdf\"; filename*=UTF-8''deck.pdf"
    );
    assert_eq!(header_str(&response, "convert-page-count"), "1");
    assert_eq!(header_str(&response, "convert-warning-count"), "0");
    let fingerprint = header_str(&response, "convert-engine-fingerprint");
    assert!(
        fingerprint.starts_with("sha256:") && fingerprint.len() == 19,
        "{fingerprint}"
    );
    assert_eq!(header_str(&response, "x-request-id").len(), 32);
    let timing = header_str(&response, "server-timing");
    let names: Vec<&str> = timing
        .split(", ")
        .map(|e| e.split(';').next().unwrap())
        .collect();
    assert_eq!(
        names,
        [
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
            "probe_engine",
            "verify",
            "package",
            "total"
        ],
        "{timing}"
    );
    for entry in timing.split(", ") {
        let dur = entry.split_once(";dur=").unwrap().1;
        let (ms, frac) = dur.split_once('.').unwrap();
        assert!(ms.parse::<u64>().is_ok() && frac.len() == 3, "{entry}");
    }
    assert!(body.starts_with(b"%PDF-1.4"));
    assert!(body.ends_with(b"%%EOF\n"));
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn pdf_comes_back_byte_identical() {
    let app = App::new("passthrough", &[]);
    let upload = big_pdf(300_000);
    let (response, body) = send(&app, file("report.pdf", &upload)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, "convert-source-format"), "pdf");
    assert_eq!(body, upload);
    app.assert_no_jobs().await;
}

/// Image inputs are out of v1 (owner, 2026-11-26): 415 before any engine.
#[tokio::test]
async fn an_image_is_unsupported() {
    let app = App::new("image", &[]);
    let (response, body) = send(&app, file("dot.png", PNG_1X1)).await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(error_code(&body), "UNSUPPORTED_FORMAT");
    assert!(String::from_utf8_lossy(&body).contains("png"));
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn each_engine_error_maps_to_its_status() {
    let app = App::new("errors", &[("QUEUE_WAIT_SECONDS", "7")]);
    let cases = [
        (EngineError::QueueFull, 429, "TOO_MANY_REQUESTS", Some("1")),
        (
            EngineError::QueueTimeout,
            429,
            "TOO_MANY_REQUESTS",
            Some("7"),
        ),
        (
            EngineError::Unavailable,
            503,
            "SERVICE_UNAVAILABLE",
            Some("5"),
        ),
        (EngineError::DiskFull, 503, "SERVICE_UNAVAILABLE", Some("5")),
        (EngineError::Timeout, 504, "CONVERSION_TIMEOUT", None),
        (
            EngineError::PasswordRequired,
            422,
            "PASSWORD_REQUIRED",
            None,
        ),
        (EngineError::InvalidPassword, 422, "INVALID_PASSWORD", None),
        (
            EngineError::LimitExceeded("pixels".into()),
            422,
            "LIMIT_EXCEEDED",
            None,
        ),
        (
            EngineError::Failed("bad".into()),
            422,
            "CONVERSION_FAILED",
            None,
        ),
        (EngineError::Crashed, 422, "CONVERSION_FAILED", None),
        (EngineError::Poisoned, 422, "CONVERSION_FAILED", None),
        (
            EngineError::Internal("boom".into()),
            500,
            "INTERNAL_ERROR",
            None,
        ),
    ];
    for (err, status, code, retry_after) in cases {
        app.office.set_behavior(FakeBehavior::Fail(err.clone()));
        let started = Instant::now();
        let (response, body) = send(&app, file("a.docx", &docx("x"))).await;
        assert_eq!(response.status().as_u16(), status, "{err:?}");
        assert_eq!(error_code(&body), code, "{err:?}");
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .map(|v| v.to_str().unwrap()),
            retry_after,
            "{err:?}"
        );
        assert!(response.headers().contains_key("server-timing"), "{err:?}");
        assert!(response.headers().contains_key("x-request-id"), "{err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{err:?} returned at once"
        );
    }
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn a_delay_past_the_deadline_is_a_504_at_the_deadline() {
    let app = App::new("delay", &[]);
    app.office
        .set_behavior(FakeBehavior::Delay(Duration::from_secs(30)));
    let started = Instant::now();
    let (response, body) = send(
        &app,
        with_options("a.docx", &docx("x"), r#"{"timeout_seconds":5}"#),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(error_code(&body), "CONVERSION_TIMEOUT");
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["error"],
        "the conversion did not finish within 5 seconds"
    );
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_millis(5250),
        "{elapsed:?}"
    );
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn a_hung_engine_is_cut_by_the_backstop() {
    let app = App::new("hang", &[]);
    app.office.set_behavior(FakeBehavior::Hang);
    let started = Instant::now();
    let (response, body) = send(
        &app,
        with_options("a.docx", &docx("x"), r#"{"timeout_seconds":5}"#),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(error_code(&body), "CONVERSION_TIMEOUT");
    // timeout_seconds + 250 ms backstop + 100 ms.
    assert!(elapsed >= Duration::from_millis(5250), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(5350), "{elapsed:?}");
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn encrypted_pdf_needs_the_password_and_is_decrypted() {
    let app = App::with_pdf(
        "encrypted",
        &[],
        FakePdf::new(FakeBehavior::Succeed).with_password("secret"),
    );
    let upload = pdf("/Encrypt 5 0 R");

    let (response, body) = send(&app, file("locked.pdf", &upload)).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "PASSWORD_REQUIRED");

    let (response, body) = send(
        &app,
        with_options("locked.pdf", &upload, r#"{"password":"wrong"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "INVALID_PASSWORD");

    let (response, body) = send(
        &app,
        with_options("locked.pdf", &upload, r#"{"password":"secret"}"#),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(header_str(&response, "convert-source-format"), "pdf");
    // The decrypted copy, not the upload.
    assert_ne!(body, upload);
    assert!(memchr::memmem::find(&body, b"/Encrypt").is_none());
    let lines = log_lines(header_str(&response, "x-request-id"));
    let finished = lines
        .iter()
        .find(|l| l["message"] == "conversion finished")
        .unwrap();
    assert!(finished["decrypt_us"].as_u64().is_some(), "{finished}");
    app.assert_no_jobs().await;
}

/// An owner-password PDF opens without a password, so with none it passes
/// through. A password given for it is checked as the owner password: the
/// right one decrypts, a wrong one is 422 INVALID_PASSWORD (review, lane
/// 001 item 9). The fake marks such a PDF with `/OwnerOnly`.
#[tokio::test]
async fn an_owner_password_is_verified_when_given() {
    let app = App::with_pdf(
        "owner-only",
        &[],
        FakePdf::new(FakeBehavior::Succeed).with_password("owner"),
    );
    let upload = pdf("/Encrypt 5 0 R /OwnerOnly");

    let (response, body) = send(&app, file("perms.pdf", &upload)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, upload, "no password: passed through unchanged");

    // An empty password is no password: the original, restrictions and
    // all, never a decrypted copy.
    let (response, body) = send(
        &app,
        with_options("perms.pdf", &upload, r#"{"password":""}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, upload, "empty password: passed through unchanged");

    let (response, body) = send(
        &app,
        with_options("perms.pdf", &upload, r#"{"password":"wrong"}"#),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(error_code(&body), "INVALID_PASSWORD");

    let (response, body) = send(
        &app,
        with_options("perms.pdf", &upload, r#"{"password":"owner"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        memchr::memmem::find(&body, b"/Encrypt").is_none(),
        "decrypted"
    );
    app.assert_no_jobs().await;
}

/// A user-password PDF with owner restrictions, opened with the user
/// password only: decrypt keeps the restrictions, so the original
/// encrypted PDF is the answer, unchanged. The owner password gives the
/// decrypted copy (review, lane 001 item 9). The fake marks such a PDF
/// with `/Restricted`.
#[tokio::test]
async fn a_restricted_pdf_with_the_user_password_passes_through() {
    let app = App::with_pdf(
        "restricted",
        &[],
        FakePdf::new(FakeBehavior::Succeed)
            .with_password("user")
            .with_owner_password("owner"),
    );
    let upload = pdf("/Encrypt 5 0 R /Restricted");

    let (response, body) = send(
        &app,
        with_options("locked.pdf", &upload, r#"{"password":"user"}"#),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(body, upload, "the original, restrictions kept");

    let (response, body) = send(
        &app,
        with_options(
            "locked.pdf",
            &upload,
            r#"{"password":"user","output":"images"}"#,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "images still render");
    drop(body);

    let (response, body) = send(
        &app,
        with_options("locked.pdf", &upload, r#"{"password":"owner"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        memchr::memmem::find(&body, b"/Encrypt").is_none(),
        "decrypted"
    );
    app.assert_no_jobs().await;
}

/// An encrypted OOXML package: an OLE2 file with the two streams.
fn encrypted_ooxml() -> Vec<u8> {
    let mut compound = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    for name in ["/EncryptionInfo", "/EncryptedPackage"] {
        let mut stream = compound.create_stream(name).unwrap();
        stream.write_all(b"not really encrypted").unwrap();
    }
    compound.flush().unwrap();
    compound.into_inner().into_inner()
}

#[tokio::test]
async fn encrypted_ooxml_needs_a_password_before_any_engine_runs() {
    let app = App::new("encrypted-ooxml", &[]);
    // An engine call would fail with 500: the 422 proves there was none.
    app.office
        .set_behavior(FakeBehavior::Fail(EngineError::Internal("called".into())));
    let (response, body) = send(&app, file("locked.docx", &encrypted_ooxml())).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "PASSWORD_REQUIRED");

    app.office.set_behavior(FakeBehavior::Succeed);
    let options = r#"{"password":"secret"}"#;
    let (response, _) = send(
        &app,
        with_options("locked.xlsx", &encrypted_ooxml(), options),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // The fake reports a text document for the provisional Document family.
    assert_eq!(header_str(&response, "convert-source-format"), "docx");
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn refused_inputs() {
    let app = App::new("refused", &[]);
    let (response, body) = send(&app, file("page.html", b"<!DOCTYPE html><html></html>")).await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(error_code(&body), "UNSUPPORTED_FORMAT");
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "unsupported format: html");
    assert!(response.headers().contains_key("server-timing"));
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn server_timing_follows_detection() {
    let app = App::new("timing-errors", &[]);
    let bad = r#"{"images":{"dpi":10}}"#;
    // Options after the file: refused after detection, so it is timed.
    let parts = multipart(&[
        Part {
            name: "file",
            file_name: Some("a.pdf"),
            content_type: "application/octet-stream",
            data: &pdf("x"),
        },
        Part {
            name: "options",
            file_name: None,
            content_type: "application/json",
            data: bad.as_bytes(),
        },
    ]);
    let (response, body) = send(&app, parts).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "VALIDATION_ERROR");
    let timing = header_str(&response, "server-timing");
    assert!(
        timing.contains("recv;dur=") && timing.contains("detect;dur="),
        "{timing}"
    );
    // Options first: refused before detection, so no Server-Timing.
    let (response, _) = send(&app, with_options("a.pdf", &pdf("x"), bad)).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!response.headers().contains_key("server-timing"));
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn markers_are_ignored_by_in_process_fakes() {
    // FakeBehavior is set directly; a marker in the upload changes nothing.
    let app = App::new("no-markers", &[]);
    let (response, _) = send(&app, file("a.docx", &docx("FAKE:CONVERSION_FAILED"))).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn markers_drive_the_marker_fakes() {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join("convert_pdf-markers");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let tmp_text = tmp.to_str().unwrap().to_owned();
    let env = Env::from_pairs([("API_KEY", KEY), ("TMP_DIR", tmp_text.as_str())], 2);
    let config = Config::from_env(&env).unwrap();
    let state = AppState::new(
        &config,
        Engines {
            office: Arc::new(FakeOffice::from_markers()),
            pdf: Arc::new(FakePdf::from_markers()),
            readiness: Arc::new(FakeReadiness::new(true)),
        },
    );
    let app = router(state);
    let cases = [
        ("FAKE:PASSWORD_REQUIRED", 422, Some("PASSWORD_REQUIRED")),
        ("FAKE:INVALID_PASSWORD", 422, Some("INVALID_PASSWORD")),
        ("FAKE:LIMIT_EXCEEDED", 422, Some("LIMIT_EXCEEDED")),
        ("FAKE:CONVERSION_FAILED", 422, Some("CONVERSION_FAILED")),
        ("FAKE:TOO_MANY_REQUESTS", 429, Some("TOO_MANY_REQUESTS")),
        ("FAKE:SERVICE_UNAVAILABLE", 503, Some("SERVICE_UNAVAILABLE")),
        ("FAKE:INTERNAL_ERROR", 500, Some("INTERNAL_ERROR")),
        ("FAKE:PANIC", 500, Some("INTERNAL_ERROR")),
        ("FAKE:DELAY_MS=200", 200, None),
        ("FAKE:PAGES=3", 200, None),
    ];
    for (marker, status, code) in cases {
        let started = Instant::now();
        let response = app
            .clone()
            .oneshot(convert_request(file("m.docx", &docx(marker))))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status, "{marker}");
        if marker == "FAKE:PAGES=3" {
            assert_eq!(response.headers()["convert-page-count"], "3");
        }
        if marker.starts_with("FAKE:DELAY") {
            assert!(started.elapsed() >= Duration::from_millis(200));
        }
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        if let Some(code) = code {
            assert_eq!(error_code(&body), code, "{marker}");
        }
    }
    // A PDF marker reaches the PDF fake.
    let response = app
        .clone()
        .oneshot(convert_request(file("m.pdf", &pdf("FAKE:PAGES=4"))))
        .await
        .unwrap();
    assert_eq!(response.headers()["convert-page-count"], "4");
    let response = app
        .clone()
        .oneshot(convert_request(with_options(
            "m.docx",
            &docx("FAKE:DELAY_MS=60000"),
            r#"{"timeout_seconds":5}"#,
        )))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_permit_gives_429_before_the_body_is_read() {
    let app = App::new("permit", &[("MAX_CONCURRENT_REQUESTS", "1")]);
    app.office
        .set_behavior(FakeBehavior::Delay(Duration::from_millis(800)));
    let slow = tokio::spawn({
        let router = app.router();
        async move {
            router
                .oneshot(convert_request(file("a.docx", &docx("x"))))
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The second request's body is never polled: it has no end.
    let never = Body::from_stream(futures_util::stream::pending::<
        Result<bytes::Bytes, std::io::Error>,
    >());
    let request = Request::post("/v1/convert")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(never)
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(1), app.router().oneshot(request))
        .await
        .expect("429 without reading the body")
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "1");
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(error_code(&body), "TOO_MANY_REQUESTS");
    assert_eq!(slow.await.unwrap().status(), StatusCode::OK);
}

// ------------------------------------------------------ over real TCP ----

/// Writes the request head for a convert call.
fn head(content_length: usize, authorization: &str) -> Vec<u8> {
    format!(
        "POST /v1/convert HTTP/1.1\r\nHost: test\r\nAuthorization: {authorization}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {content_length}\r\n\r\n"
    )
    .into_bytes()
}

/// Reads until the end of the response head; returns the status code.
fn read_status(stream: &mut TcpStream) -> u16 {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).unwrap();
        assert!(
            n > 0,
            "closed before the head: {}",
            String::from_utf8_lossy(&buf)
        );
        buf.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&buf);
    line.split(' ').nth(1).unwrap().parse().unwrap()
}

/// One whole request with `Connection: close`; returns the status.
fn round_trip(addr: SocketAddr, body: &[u8]) -> u16 {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut request = head(body.len(), &format!("Bearer {KEY}"));
    request.truncate(request.len() - 2);
    request.extend_from_slice(b"Connection: close\r\n\r\n");
    stream.write_all(&request).unwrap();
    // A 429 is sent before the body is read; the server may then close.
    let _ = stream.write_all(body);
    let status = read_status(&mut stream);
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    status
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_stops_reading_releases_the_permit_after_the_idle_timeout() {
    let app = App::new(
        "response-stall",
        &[
            ("MAX_CONCURRENT_REQUESTS", "1"),
            ("IO_IDLE_TIMEOUT_SECONDS", "1"),
        ],
    );
    let addr = app.serve().await;
    let upload = big_pdf(64 * 1024 * 1024);
    let body = file("big.pdf", &upload);
    let small = file("small.pdf", &pdf("small"));

    let stalled = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(&head(body.len(), &format!("Bearer {KEY}")))
            .unwrap();
        stream.write_all(&body).unwrap();
        let status = read_status(&mut stream);
        // Stop reading; keep the socket open.
        (status, stream)
    });
    let (status, stream) = stalled.await.unwrap();
    assert_eq!(status, 200);
    let stalled_at = Instant::now();

    // While the pump still waits, the one permit is taken.
    let busy = tokio::task::spawn_blocking({
        let small = small.clone();
        move || round_trip(addr, &small)
    });
    assert_eq!(busy.await.unwrap(), 429);

    // After IO_IDLE_TIMEOUT_SECONDS the pump gives up and a new request succeeds.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let small = small.clone();
        let status = tokio::task::spawn_blocking(move || round_trip(addr, &small))
            .await
            .unwrap();
        if status == 200 {
            break;
        }
        assert_eq!(status, 429);
        assert!(Instant::now() < deadline, "the permit was never released");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        stalled_at.elapsed() >= Duration::from_millis(900),
        "{:?}",
        stalled_at.elapsed()
    );
    let stalls = log_lines("RESPONSE_STALLED");
    assert!(
        stalls.iter().any(|l| l["message"] == "response stalled"),
        "{stalls:?}"
    );
    app.assert_no_jobs().await;
    drop(stream);
}

/// `timeout_seconds` covers everything up to the headers. A render that
/// returns only at the deadline lands after the service's last check but
/// inside the backstop's grace, so the handler's check before packaging is
/// what answers 504, not a stream.
#[tokio::test]
async fn slow_a_bundle_past_its_deadline_is_504_not_a_stream() {
    let app = App::with_pdf(
        "deadline-package",
        &[],
        FakePdf::new(FakeBehavior::Succeed).with_render_until_deadline(),
    );
    let (response, body) = send(
        &app,
        with_options(
            "a.pdf",
            &pdf("x"),
            r#"{"output":"bundle","timeout_seconds":5}"#,
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::GATEWAY_TIMEOUT,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(error_code(&body), "CONVERSION_TIMEOUT");
    app.assert_no_jobs().await;
}

/// A bundle whose client stops reading is cut by the pump. Over a real
/// socket the cut must show: no chunked terminator after the last bytes
/// the client gets, and `complete=false` in the access log (review, lane
/// 001 item 5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_a_cut_bundle_has_no_chunked_terminator() {
    let app = App::new(
        "cut-bundle",
        &[
            ("MAX_CONCURRENT_REQUESTS", "1"),
            ("IO_IDLE_TIMEOUT_SECONDS", "1"),
        ],
    );
    let addr = app.serve().await;
    // Bytes that zstd cannot shrink, so the bundle outgrows the socket
    // buffers (big_pdf's filler compresses to almost nothing).
    let mut upload = pdf("big");
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    upload.extend((0..24 << 20).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }));
    let body = with_options("big.pdf", &upload, r#"{"output":"bundle"}"#);
    let (head_text, rest) = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .write_all(&head(body.len(), &format!("Bearer {KEY}")))
            .unwrap();
        stream.write_all(&body).unwrap();
        let mut head_bytes = Vec::new();
        let mut byte = [0u8; 1];
        while !head_bytes.ends_with(b"\r\n\r\n") {
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            head_bytes.push(byte[0]);
        }
        // Stop reading well past the idle timeout: the socket buffers fill
        // and the pump's next send waits more than 1 s, so it cuts the body.
        std::thread::sleep(Duration::from_millis(5000));
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
        (String::from_utf8_lossy(&head_bytes).into_owned(), rest)
    })
    .await
    .unwrap();
    assert!(head_text.starts_with("HTTP/1.1 200"), "{head_text}");
    assert!(
        head_text.contains("transfer-encoding: chunked"),
        "{head_text}"
    );
    assert!(
        !rest.ends_with(b"0\r\n\r\n"),
        "a cut bundle ended with the chunked terminator"
    );
    let request_id = head_text
        .lines()
        .find_map(|line| line.strip_prefix("x-request-id: "))
        .unwrap()
        .trim()
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(3);
    let access = loop {
        let lines = log_lines(&request_id);
        if let Some(line) = lines.into_iter().find(|l| l["message"] == "http request") {
            break line;
        }
        assert!(Instant::now() < deadline, "no access log line");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(access["complete"], false, "{access}");
    app.assert_no_jobs().await;
}

/// A client that keeps reading, but slowly, never trips the pump's idle
/// timeout. The minimum-throughput floor ends the response after a window
/// below it and frees the permit (review, lane 001 item 3). The floor is set
/// high so that the reader can stay fast enough for the idle timer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_a_reader_below_the_floor_releases_the_permit() {
    let app = App::new(
        "response-slow",
        &[
            ("MAX_CONCURRENT_REQUESTS", "1"),
            ("IO_IDLE_TIMEOUT_SECONDS", "2"),
            ("RESPONSE_MIN_BYTES_PER_SECOND", "67108864"),
        ],
    );
    let addr = app.serve().await;
    let upload = big_pdf(96 * 1024 * 1024);
    let body = file("big.pdf", &upload);
    let small = file("small.pdf", &pdf("small"));

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = std::thread::spawn({
        let stop = stop.clone();
        move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .write_all(&head(body.len(), &format!("Bearer {KEY}")))
                .unwrap();
            stream.write_all(&body).unwrap();
            let status = read_status(&mut stream);
            // 256 KiB every 50 ms, about 5 MB/s: the pump never waits near
            // the 2 s idle timeout, but stays below the 64 MiB/s floor.
            let mut buf = vec![0u8; 256 * 1024];
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
            status
        }
    });
    // The upload, conversion and headers take a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(8);
    loop {
        let small = small.clone();
        let status = tokio::task::spawn_blocking(move || round_trip(addr, &small))
            .await
            .unwrap();
        if status == 200 {
            break;
        }
        assert_eq!(status, 429);
        assert!(Instant::now() < deadline, "the slow reader kept the permit");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(reader.join().unwrap(), 200);
    let cuts = log_lines("RESPONSE_TOO_SLOW");
    assert!(
        cuts.iter().any(|l| l["message"] == "response too slow"),
        "{cuts:?}"
    );
    app.assert_no_jobs().await;
}

/// Streams `total` bytes of body after `head`, in 64 KiB writes, until the
/// server closes; returns how many bytes went out.
/// Writes `total` bytes, counting them in `sent`; returns the write error
/// that stopped it, if any.
fn stream_body(
    mut stream: TcpStream,
    total: usize,
    sent: &std::sync::atomic::AtomicUsize,
) -> Option<std::io::ErrorKind> {
    use std::sync::atomic::Ordering;
    let chunk = vec![b'x'; 64 * 1024];
    while sent.load(Ordering::Relaxed) < total {
        let left = total - sent.load(Ordering::Relaxed);
        match stream.write(&chunk[..chunk.len().min(left)]) {
            Ok(n) => {
                sent.fetch_add(n, Ordering::Relaxed);
            }
            Err(err) => return Some(err.kind()),
        }
    }
    None
}

/// The answer comes while the upload is still arriving, and the server
/// then keeps reading (lingering close), so the client's write never
/// fails with a broken pipe or a reset (contract case 19).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_401_and_413_arrive_while_the_client_still_uploads() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let app = App::new("early", &[("MAX_UPLOAD_BYTES", "1048576")]);
    let addr = app.serve().await;
    let total = 64 * 1024 * 1024;
    for (authorization, want) in [("Bearer wrong", 401), (&*format!("Bearer {KEY}"), 413)] {
        let authorization = authorization.to_owned();
        let (status, answered_at, write_error, sent) = tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream.write_all(&head(total, &authorization)).unwrap();
            let writer = stream.try_clone().unwrap();
            let sent = Arc::new(AtomicUsize::new(0));
            let upload = std::thread::spawn({
                let sent = sent.clone();
                move || stream_body(writer, total, &sent)
            });
            let status = read_status(&mut stream);
            let answered_at = sent.load(Ordering::Relaxed);
            let write_error = upload.join().unwrap();
            (
                status,
                answered_at,
                write_error,
                sent.load(Ordering::Relaxed),
            )
        })
        .await
        .unwrap();
        assert_eq!(status, want);
        assert!(
            answered_at < total,
            "the answer came before the upload ended ({answered_at} of {total} bytes)"
        );
        assert_eq!(write_error, None, "{want}: the upload was cut");
        assert_eq!(sent, total);
    }
    app.assert_no_jobs().await;
}

/// `preflight` is the office worker's scan: sent for office inputs (see
/// above), never for a PDF, which has none.
#[tokio::test]
async fn a_pdf_has_no_preflight_stage() {
    let app = App::new("timing-pdf", &[]);
    let (response, _) = send(&app, with_options("a.pdf", &pdf("x"), "{}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let timing = header_str(&response, "server-timing");
    assert!(timing.contains("inspect;dur="), "{timing}");
    assert!(!timing.contains("preflight"), "{timing}");
    app.assert_no_jobs().await;
}
