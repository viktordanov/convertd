//! Probes, capabilities, metrics and the drain (plan 001 step 9).

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use convertd::config::{Config, Env};
use convertd::domain::readiness::{PoolStatus, Readiness, ReadinessSnapshot, ReadinessStatus};
use convertd::engines::fake::{FakeBehavior, FakeOffice, FakePdf, FakeReadiness};
use convertd::rest::healthcheck;
use convertd::rest::server::{begin_drain, router, wait_idle};
use convertd::rest::state::{AppState, Engines};
use serde_json::Value;
use tower::ServiceExt;

const KEY: &str = "test-key";
const BOUNDARY: &str = "convertd-ops-boundary";

fn tmp(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ops-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct App {
    state: AppState,
    office: Arc<FakeOffice>,
}

fn app(name: &str, env: &[(&str, &str)], readiness: Arc<dyn Readiness>) -> App {
    let dir = tmp(name);
    let tmp_text = dir.to_str().unwrap().to_owned();
    let mut pairs = vec![
        ("API_KEY", KEY),
        ("TMP_DIR", tmp_text.as_str()),
        ("BUILD_INFO_PATH", "/nonexistent/build-info.json"),
        ("FONTS_PATH", "/nonexistent/fonts.json"),
    ];
    pairs.extend_from_slice(env);
    let config = Config::from_env(&Env::from_pairs(pairs, 2)).unwrap();
    let office = Arc::new(FakeOffice::new(FakeBehavior::Succeed));
    let state = AppState::new(
        &config,
        Engines {
            office: office.clone(),
            pdf: Arc::new(FakePdf::new(FakeBehavior::Succeed)),
            readiness,
        },
    );
    App { state, office }
}

impl App {
    fn router(&self) -> Router {
        router(self.state.clone())
    }

    async fn get(&self, path: &str) -> (StatusCode, String) {
        let mut request = Request::get(path);
        if path.starts_with("/v1") {
            request = request.header(header::AUTHORIZATION, format!("Bearer {KEY}"));
        }
        let response = self
            .router()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn convert(&self, file: &[u8], options: &str) -> StatusCode {
        let mut body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"options\"\r\n\r\n{options}\r\n\
             --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(file);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        let request = Request::post("/v1/convert")
            .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap();
        let response = self.router().oneshot(request).await.unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), 64 << 20).await;
        status
    }
}

const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n%%EOF\n";

/// A readiness handle that says what the test wants.
struct Fixed(ReadinessStatus);

impl Readiness for Fixed {
    fn readiness(&self) -> ReadinessSnapshot {
        let office = PoolStatus {
            target: 2,
            idle: 1,
            busy: 1,
        };
        ReadinessSnapshot {
            status: self.0,
            office,
            render: PoolStatus {
                target: 4,
                idle: 0,
                busy: 0,
            },
        }
    }
}

#[tokio::test]
async fn readyz_in_each_status() {
    for (status, code) in [
        (ReadinessStatus::Ready, StatusCode::OK),
        (ReadinessStatus::Starting, StatusCode::SERVICE_UNAVAILABLE),
        (ReadinessStatus::Degraded, StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let app = app("readyz", &[], Arc::new(Fixed(status)));
        let (got, body) = app.get("/readyz").await;
        assert_eq!(got, code, "{status:?}");
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], status.as_str());
        assert_eq!(
            json["pools"]["office"],
            serde_json::json!({"target": 2, "idle": 1, "busy": 1})
        );
        assert_eq!(json["pools"]["render"]["target"], 4);
    }
    // The supervisor's own drain overrides a ready engine.
    let app = app("readyz-drain", &[], Arc::new(Fixed(ReadinessStatus::Ready)));
    begin_drain(&app.state);
    let (got, body) = app.get("/readyz").await;
    assert_eq!(got, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains(r#""status":"draining""#), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draining_finishes_what_is_in_flight_and_refuses_new_work() {
    let app = app("drain", &[], Arc::new(FakeReadiness::new(true)));
    app.office
        .set_behavior(FakeBehavior::Delay(Duration::from_millis(400)));
    let docx = docx();
    let in_flight = tokio::spawn({
        let router = app.router();
        let request = convert_request(&docx);
        async move { router.oneshot(request).await.unwrap().status() }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    begin_drain(&app.state);
    assert_eq!(
        app.convert(PDF, "{}").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(
        !wait_idle(&app.state, Duration::from_millis(50)).await,
        "still busy"
    );
    assert_eq!(in_flight.await.unwrap(), StatusCode::OK);
    assert!(wait_idle(&app.state, Duration::from_secs(2)).await);
}

fn convert_request(file: &[u8]) -> Request<Body> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.docx\"\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    Request::post("/v1/convert")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap()
}

/// A DOCX whose one run names a font, so pre-flight requests it.
fn docx() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut out));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let parts: [(&str, &[u8]); 2] = [
            (
                "[Content_Types].xml",
                br#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
            ),
            (
                "word/document.xml",
                br#"<w:document xmlns:w="w"><w:r><w:rPr><w:rFonts w:ascii="Georgia"/></w:rPr></w:r></w:document>"#,
            ),
        ];
        for (name, data) in parts {
            zip.start_file(name, stored).unwrap();
            std::io::Write::write_all(&mut zip, data).unwrap();
        }
        zip.finish().unwrap();
    }
    out
}

#[tokio::test]
async fn capabilities_follow_the_schema_and_build_info() {
    let dir = tmp("caps-files");
    let build_info = dir.join("build-info.json");
    std::fs::write(
        &build_info,
        r#"{"libreoffice":"26.8.3.1","pdfium":"8179","fonts":["DejaVu Sans,DejaVu Sans Condensed","Carlito","Caladea","Carlito"]}"#,
    )
    .unwrap();
    let path = build_info.to_str().unwrap().to_owned();
    let app = app(
        "caps",
        &[("BUILD_INFO_PATH", path.as_str()), ("MAX_DPI", "200")],
        Arc::new(FakeReadiness::new(true)),
    );
    let (status, body) = app.get("/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    let caps: Value = serde_json::from_str(&body).unwrap();
    for key in [
        "service",
        "engine",
        "inputs",
        "outputs",
        "image_formats",
        "limits",
        "fonts",
    ] {
        assert!(caps.get(key).is_some(), "{key}");
    }
    assert_eq!(caps["service"]["name"], "convert");
    assert_eq!(caps["engine"]["libreoffice"], "26.8.3.1");
    assert_eq!(caps["engine"]["pdfium"], "8179");
    assert_eq!(
        caps["fonts"],
        serde_json::json!(["Caladea", "Carlito", "DejaVu Sans"])
    );
    assert_eq!(
        caps["outputs"],
        serde_json::json!(["pdf", "images", "bundle"])
    );
    assert_eq!(
        caps["image_formats"],
        serde_json::json!(["jpeg", "png", "webp"])
    );
    assert_eq!(caps["limits"]["max_dpi"], 200);
    assert_eq!(caps["limits"]["max_upload_bytes"], 125_829_120);
    let inputs: Vec<&str> = caps["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["format"].as_str().unwrap())
        .collect();
    for format in ["docx", "doc", "xls", "ppt", "pdf", "png", "webp"] {
        assert!(inputs.contains(&format), "{format}");
    }
    for input in caps["inputs"].as_array().unwrap() {
        for key in ["format", "family", "extensions", "media_types", "engine"] {
            assert!(input.get(key).is_some(), "{input}");
        }
    }
    // The fingerprint in capabilities is the one on every 200.
    let fingerprint = caps["engine"]["fingerprint"].as_str().unwrap();
    assert_eq!(app.state.fingerprint.to_str().unwrap(), fingerprint);
}

#[tokio::test]
async fn metrics_after_requests() {
    let app = app("metrics", &[], Arc::new(FakeReadiness::new(true)));
    assert_eq!(
        app.convert(PDF, r#"{"output":"images","images":{"dpi":600}}"#)
            .await,
        StatusCode::OK
    );
    assert_eq!(app.convert(&docx(), "{}").await, StatusCode::OK);
    assert_eq!(
        app.convert(b"<html>", "{}").await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let (status, text) = app.get("/metrics").await;
    assert_eq!(status, StatusCode::OK);
    for name in [
        "convert_requests_total",
        "convert_stage_duration_seconds_bucket",
        "convert_input_bytes_bucket",
        "convert_warnings_total",
        "convert_pages_rendered_total",
        "convert_verify_skipped_total",
        "convert_disk_budget_bytes",
    ] {
        assert!(text.contains(name), "{name} missing:\n{text}");
    }
    assert!(text.contains(r#"code="RENDER_CLAMPED""#), "{text}");
    assert!(text.contains(r#"code="UNSUPPORTED_FORMAT""#), "{text}");
    assert!(text.contains(r#"reason="no_font_catalog""#), "{text}");
    assert!(text.contains(r#"stage="overhead""#), "{text}");
    #[cfg(target_os = "linux")]
    for name in [
        "convert_supervisor_rss_bytes",
        "convert_supervisor_rss_peak_bytes",
        "convert_supervisor_open_fds",
        "convert_supervisor_threads",
    ] {
        assert!(text.contains(name), "{name} missing");
    }

    let off = app_with_metrics_off();
    let (status, _) = off.get("/metrics").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn app_with_metrics_off() -> App {
    app(
        "metrics-off",
        &[("METRICS_ENABLED", "false")],
        Arc::new(FakeReadiness::new(true)),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthcheck_exit_follows_readyz() {
    let app = app("healthcheck", &[], Arc::new(FakeReadiness::new(true)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = app.router();
    tokio::spawn(async move { axum::serve(listener, router).await });
    let check = move || healthcheck::check(port, healthcheck::TIMEOUT);
    assert!(tokio::task::spawn_blocking(check).await.unwrap().is_ok());
    begin_drain(&app.state);
    assert!(tokio::task::spawn_blocking(check).await.unwrap().is_err());
}

/// SIGTERM on the real binary: the request in flight finishes, a new one
/// and readyz answer 503 while the drain waits, then the process exits 0.
#[cfg(feature = "test-hooks")]
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "a test runs the binary directly; no supervisor reaper exists here"
)]
fn slow_sigterm_drains_then_exits() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Instant;

    let dir = tmp("sigterm");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_convertd"))
        .arg("serve")
        .env_clear()
        .env("HOST", "127.0.0.1")
        .env("PORT", port.to_string())
        .env("API_KEY", KEY)
        .env("TMP_DIR", &dir)
        .env("TEST_ENGINE", "fake")
        .env("SHUTDOWN_GRACE_SECONDS", "10")
        .env("BUILD_INFO_PATH", "/nonexistent")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the server never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    let request = |file: Vec<u8>| {
        let mut body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.docx\"\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(&file);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        let mut out = format!(
            "POST /v1/convert HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {KEY}\r\nConnection: close\r\n\
             Content-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(&body);
        out
    };
    let status_of = move |bytes: &[u8]| -> u16 {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(bytes).unwrap();
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        String::from_utf8_lossy(&answer)
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };
    // A DOCX whose marker makes the fake take 1.5 s.
    let slow = {
        let mut out = Vec::new();
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut out));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("[Content_Types].xml", stored).unwrap();
        zip.write_all(br#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#).unwrap();
        zip.start_file("word/document.xml", stored).unwrap();
        zip.write_all(b"<w:document><w:t>FAKE:DELAY_MS=1500</w:t></w:document>")
            .unwrap();
        zip.finish().unwrap();
        out
    };
    let in_flight = std::thread::spawn({
        let bytes = request(slow);
        move || status_of(&bytes)
    });
    std::thread::sleep(Duration::from_millis(300));
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        status_of(&request(PDF.to_vec())),
        503,
        "new work during the drain"
    );
    let readyz = {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(b"GET /readyz HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        String::from_utf8_lossy(&answer).into_owned()
    };
    assert!(readyz.starts_with("HTTP/1.1 503"), "{readyz}");
    assert!(readyz.contains(r#""status":"draining""#), "{readyz}");
    assert_eq!(
        in_flight.join().unwrap(),
        200,
        "the request in flight finishes"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the server did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status:?}");
}
