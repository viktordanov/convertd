//! Errors over HTTP: auth, request IDs, fallbacks and the
//! panic layer. Each error has a JSON body, a code and `X-Request-ID`. The
//! mapping of every domain error is the table test in `rest/response.rs`.

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use convertd::config::{Config, Env};
use convertd::engines::fake::Unwired;
use convertd::rest::server::{router, router_with};
use convertd::rest::state::{AppState, Engines};
use serde_json::Value;
use tower::ServiceExt;

/// Log lines of this test binary, in JSON.
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

fn logs() -> Arc<Mutex<Vec<u8>>> {
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = buffer.clone();
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_writer(move || Capture(writer.clone()))
            .init();
        buffer
    })
    .clone()
}

/// The JSON log lines that mention `needle`.
fn log_lines(needle: &str) -> Vec<Value> {
    let bytes = logs().lock().unwrap().clone();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| line.contains(needle))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn state() -> AppState {
    let env = Env::from_pairs(
        [
            ("API_KEY", "current-key"),
            ("API_KEY_PREVIOUS", "previous-key"),
        ],
        2,
    );
    let config = Config::from_env(&env).unwrap();
    let unwired = Arc::new(Unwired);
    AppState::new(
        &config,
        Engines {
            office: unwired.clone(),
            pdf: unwired.clone(),
            readiness: unwired,
        },
    )
}

async fn send(app: Router, request: Request<Body>) -> (Response, Value) {
    let response = app.oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 1 << 20).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (Response::from_parts(parts, Body::empty()), json)
}

fn capabilities(authorization: Option<&str>) -> Request<Body> {
    let mut request = Request::get("/v1/capabilities");
    if let Some(value) = authorization {
        request = request.header(header::AUTHORIZATION, value);
    }
    request.body(Body::empty()).unwrap()
}

fn assert_error(response: &Response, body: &Value, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status, "{body}");
    assert_eq!(body["code"], code, "{body}");
    assert!(
        body["error"].as_str().is_some_and(|m| !m.is_empty()),
        "{body}"
    );
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let id = response.headers()["x-request-id"].to_str().unwrap();
    assert!(!id.is_empty());
}

#[tokio::test]
async fn unauthorized_without_or_with_a_wrong_key() {
    for authorization in [
        None,
        Some("Bearer wrong"),
        Some("current-key"),
        Some("Basic current-key"),
    ] {
        let (response, body) = send(router(state()), capabilities(authorization)).await;
        assert_error(&response, &body, StatusCode::UNAUTHORIZED, "UNAUTHORIZED");
        assert_eq!(body["error"], "missing or invalid API key");
    }
    // /v1/convert is behind the same key, before its body is read.
    let request = Request::post("/v1/convert").body(Body::from("x")).unwrap();
    let (response, body) = send(router(state()), request).await;
    assert_error(&response, &body, StatusCode::UNAUTHORIZED, "UNAUTHORIZED");
}

#[tokio::test]
async fn current_and_previous_keys_are_accepted() {
    for key in ["current-key", "previous-key"] {
        let auth = format!("Bearer {key}");
        let (response, body) = send(router(state()), capabilities(Some(&auth))).await;
        assert_eq!(response.status(), StatusCode::OK, "{key}");
        assert_eq!(body["service"]["name"], "convert");
        assert!(body["inputs"].as_array().is_some_and(|i| !i.is_empty()));
    }
}

#[tokio::test]
async fn probes_need_no_key() {
    let request = Request::get("/healthz").body(Body::empty()).unwrap();
    let (response, body) = send(router(state()), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body["status"], "ok");
    let request = Request::get("/readyz").body(Body::empty()).unwrap();
    let (response, body) = send(router(state()), request).await;
    // The unwired engines are never ready.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "starting");
    assert_eq!(body["pools"]["office"]["target"], 0);
}

#[tokio::test]
async fn request_ids_are_echoed_or_replaced() {
    let echoed = "7f3c2a9e-4b1d-4f6a-9a51-0c2e8d7b6a14";
    let too_long = "x".repeat(129);
    for (sent, kept) in [
        (echoed, true),
        (too_long.as_str(), false),
        ("with space", false),
    ] {
        let request = Request::get("/healthz")
            .header("x-request-id", sent)
            .body(Body::empty())
            .unwrap();
        let (response, _) = send(router(state()), request).await;
        let got = response.headers()["x-request-id"].to_str().unwrap();
        if kept {
            assert_eq!(got, sent);
        } else {
            assert_ne!(got, sent);
            assert_eq!(got.len(), 32, "{got}");
        }
    }
    let (response, _) = send(router(state()), capabilities(None)).await;
    assert_eq!(response.headers()["x-request-id"].len(), 32);
}

#[tokio::test]
async fn unknown_path_and_wrong_method() {
    let request = Request::get("/nope").body(Body::empty()).unwrap();
    let (response, body) = send(router(state()), request).await;
    assert_error(&response, &body, StatusCode::NOT_FOUND, "NOT_FOUND");

    let request = Request::delete("/healthz").body(Body::empty()).unwrap();
    let (response, body) = send(router(state()), request).await;
    assert_error(
        &response,
        &body,
        StatusCode::METHOD_NOT_ALLOWED,
        "BAD_REQUEST",
    );

    let request = Request::get("/v1/convert")
        .header(header::AUTHORIZATION, "Bearer current-key")
        .body(Body::empty())
        .unwrap();
    let (response, body) = send(router(state()), request).await;
    assert_error(
        &response,
        &body,
        StatusCode::METHOD_NOT_ALLOWED,
        "BAD_REQUEST",
    );
}

#[tokio::test]
async fn a_panic_is_a_500_with_a_request_id_and_a_log_line() {
    logs();
    async fn boom() -> &'static str {
        panic!("test panic")
    }
    let app = router_with(state(), Router::new().route("/panic", get(boom)));
    let id = "panic-test-request-id";
    let request = Request::get("/panic")
        .header("x-request-id", id)
        .body(Body::empty())
        .unwrap();
    let (response, body) = send(app, request).await;
    assert_error(
        &response,
        &body,
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL_ERROR",
    );
    assert_eq!(body["error"], "internal server error");
    assert_eq!(response.headers()["x-request-id"], id);

    let lines = log_lines(id);
    let line = lines
        .iter()
        .find(|l| l["message"] == "http request")
        .unwrap_or_else(|| panic!("no access-log line: {lines:?}"));
    assert_eq!(line["status_code"], 500);
    assert_eq!(line["path"], "/panic");
    assert_eq!(line["method"], "GET");
    assert!(line["duration_us"].as_u64().is_some());
    assert!(line["detail"].as_str().unwrap().contains("test panic"));
}

#[tokio::test]
async fn probes_are_not_logged() {
    logs();
    let id = "quiet-probe-request-id";
    let request = Request::get("/healthz")
        .header("x-request-id", id)
        .body(Body::empty())
        .unwrap();
    let _ = send(router(state()), request).await;
    assert!(log_lines(id).is_empty());
}
