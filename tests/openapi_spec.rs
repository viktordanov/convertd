//! The cheap invariants of the emitted spec (plan 001 step 3). The full
//! comparison with `api/openapi.yaml` is lane 004's `speccheck compare`.

use std::process::Command;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use convertd::config::{Config, Env};
use convertd::engines::fake::Unwired;
use convertd::rest::openapi;
use convertd::rest::server::router;
use convertd::rest::state::{AppState, Engines};
use serde_json::Value;
use tower::ServiceExt;

fn spec() -> Value {
    serde_json::from_str(&openapi::spec_json()).unwrap()
}

/// Runs `convertd openapi` with an empty environment.
#[allow(
    clippy::disallowed_methods,
    reason = "a test runs the binary directly; no supervisor reaper exists here"
)]
fn run_openapi() -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_convertd"))
        .arg("openapi")
        .env_clear()
        .output()
        .unwrap()
}

/// Every schema node in the document, with its JSON path.
fn walk<'a>(path: String, value: &'a Value, out: &mut Vec<(String, &'a Value)>) {
    match value {
        Value::Object(map) => {
            out.push((path.clone(), value));
            for (key, child) in map {
                walk(format!("{path}/{key}"), child, out);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                walk(format!("{path}/{i}"), child, out);
            }
        }
        _ => {}
    }
}

#[test]
fn version_and_title() {
    let spec = spec();
    assert_eq!(spec["openapi"], "3.1.0");
    assert_eq!(spec["info"]["version"], "1.0.0-draft.1");
    assert_eq!(spec["info"]["title"], "Convert API");
    assert_eq!(
        spec["components"]["securitySchemes"]["ApiKeyBearer"]["scheme"],
        "bearer"
    );
    assert_eq!(spec["security"], serde_json::json!([{"ApiKeyBearer": []}]));
}

#[test]
fn no_null_types() {
    let spec = spec();
    let mut nodes = Vec::new();
    walk(String::new(), &spec, &mut nodes);
    for (path, node) in nodes {
        if let Some(Value::Array(types)) = node.get("type") {
            assert!(!types.iter().any(|t| t == "null"), "null type at {path}");
        }
        assert_ne!(
            node.get("type"),
            Some(&Value::from("null")),
            "null type at {path}"
        );
    }
}

#[test]
fn binary_fields_are_strings() {
    let spec = spec();
    let file = &spec["components"]["schemas"]["ConvertRequest"]["properties"]["file"];
    assert_eq!(file["type"], "string");
    assert_eq!(file["format"], "binary");
    let content = &spec["paths"]["/v1/convert"]["post"]["responses"]["200"]["content"];
    for media in ["application/pdf", "application/zip"] {
        assert_eq!(content[media]["schema"]["type"], "string", "{media}");
        assert_eq!(content[media]["schema"]["format"], "binary", "{media}");
    }
    // No byte array became an array of integers.
    let mut nodes = Vec::new();
    walk(String::new(), &spec, &mut nodes);
    for (path, node) in nodes {
        if node.get("type") == Some(&Value::from("array")) {
            assert_ne!(node["items"]["type"], "integer", "integer array at {path}");
        }
    }
}

#[test]
fn schema_names_are_the_references() {
    let spec = spec();
    let schemas = spec["components"]["schemas"].as_object().unwrap();
    let mut names: Vec<&str> = schemas.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "Capabilities",
            "ConvertOptions",
            "ConvertRequest",
            "EngineInfo",
            "Error",
            "ErrorCode",
            "FormatFamily",
            "Health",
            "ImageFormat",
            "ImageOptions",
            "ImageSettings",
            "InputFormat",
            "Limits",
            "Manifest",
            "OutputKind",
            "PageInfo",
            "PdfInfo",
            "PoolStatus",
            "Readiness",
            "ServiceInfo",
            "SheetOptions",
            "SlideNotes",
            "SlideOptions",
            "SourceInfo",
            "Timings",
            "Warning",
            "WarningCode",
        ]
    );
    let responses = spec["components"]["responses"].as_object().unwrap();
    let mut names: Vec<&str> = responses.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "BadRequest",
            "ConversionTimeout",
            "FileTooLarge",
            "InternalError",
            "ServiceUnavailable",
            "TooManyRequests",
            "Unauthorized",
            "Unprocessable",
            "UnsupportedFormat",
        ]
    );
    let codes = spec["components"]["schemas"]["ErrorCode"]["enum"]
        .as_array()
        .unwrap();
    assert_eq!(codes.len(), 14);
    assert!(codes.contains(&Value::from("NOT_FOUND")));
}

/// One row per operation, taken from `api/openapi.yaml`, plus
/// `Server-Timing` on the 200 of `convertDocument` (INTERFACES §9).
struct Operation {
    path: &'static str,
    method: &'static str,
    id: &'static str,
    statuses: &'static [&'static str],
    content: &'static [(&'static str, &'static [&'static str])],
    headers: &'static [(&'static str, &'static [&'static str])],
    secured: bool,
    request_id_param: bool,
}

const ERROR_JSON: &[&str] = &["application/json"];
const REQUEST_ID: &[&str] = &["X-Request-ID"];

const OPERATIONS: &[Operation] = &[
    Operation {
        path: "/v1/convert",
        method: "post",
        id: "convertDocument",
        statuses: &[
            "200", "400", "401", "413", "415", "422", "429", "500", "503", "504",
        ],
        content: &[
            ("200", &["application/pdf", "application/zip"]),
            ("400", ERROR_JSON),
            ("401", ERROR_JSON),
            ("413", ERROR_JSON),
            ("415", ERROR_JSON),
            ("422", ERROR_JSON),
            ("429", ERROR_JSON),
            ("500", ERROR_JSON),
            ("503", ERROR_JSON),
            ("504", ERROR_JSON),
        ],
        headers: &[
            (
                "200",
                &[
                    "Content-Disposition",
                    "Convert-Engine-Fingerprint",
                    "Convert-Page-Count",
                    "Convert-Source-Format",
                    "Convert-Warning-Count",
                    "Server-Timing",
                    "X-Request-ID",
                ],
            ),
            ("400", REQUEST_ID),
            ("401", REQUEST_ID),
            ("413", REQUEST_ID),
            ("415", REQUEST_ID),
            ("422", REQUEST_ID),
            ("429", &["Retry-After", "X-Request-ID"]),
            ("500", REQUEST_ID),
            ("503", &["Retry-After", "X-Request-ID"]),
            ("504", REQUEST_ID),
        ],
        secured: true,
        request_id_param: true,
    },
    Operation {
        path: "/v1/capabilities",
        method: "get",
        id: "getCapabilities",
        statuses: &["200", "401", "500"],
        content: &[
            ("200", ERROR_JSON),
            ("401", ERROR_JSON),
            ("500", ERROR_JSON),
        ],
        headers: &[
            ("200", REQUEST_ID),
            ("401", REQUEST_ID),
            ("500", REQUEST_ID),
        ],
        secured: true,
        request_id_param: true,
    },
    Operation {
        path: "/healthz",
        method: "get",
        id: "getLiveness",
        statuses: &["200"],
        content: &[("200", ERROR_JSON)],
        headers: &[],
        secured: false,
        request_id_param: false,
    },
    Operation {
        path: "/readyz",
        method: "get",
        id: "getReadiness",
        statuses: &["200", "503"],
        content: &[("200", ERROR_JSON), ("503", ERROR_JSON)],
        headers: &[],
        secured: false,
        request_id_param: false,
    },
    Operation {
        path: "/metrics",
        method: "get",
        id: "getMetrics",
        statuses: &["200"],
        content: &[("200", &["text/plain"])],
        headers: &[],
        secured: false,
        request_id_param: false,
    },
];

/// Follows a `#/components/responses/...` reference.
fn response<'a>(spec: &'a Value, node: &'a Value) -> &'a Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let name = reference.strip_prefix("#/components/responses/").unwrap();
            &spec["components"]["responses"][name]
        }
        None => node,
    }
}

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .map(|map| map.keys().map(String::as_str).collect())
        .unwrap_or_default();
    keys.sort_unstable();
    keys
}

#[test]
fn operations_match_the_reference_table() {
    let spec = spec();
    let paths = spec["paths"].as_object().unwrap();
    assert_eq!(paths.len(), OPERATIONS.len(), "{:?}", paths.keys());
    for op in OPERATIONS {
        let operation = &spec["paths"][op.path][op.method];
        assert_eq!(operation["operationId"], op.id, "{}", op.path);
        assert_eq!(keys(&operation["responses"]), op.statuses, "{}", op.id);
        for (status, media) in op.content {
            let resp = response(&spec, &operation["responses"][status]);
            assert_eq!(keys(&resp["content"]), *media, "{} {status}", op.id);
        }
        for status in op.statuses {
            let resp = response(&spec, &operation["responses"][status]);
            let want = op
                .headers
                .iter()
                .find(|(s, _)| s == status)
                .map_or(&[][..], |(_, names)| names);
            assert_eq!(keys(&resp["headers"]), want, "{} {status}", op.id);
        }
        if op.secured {
            assert!(
                operation.get("security").is_none(),
                "{} uses the global security",
                op.id
            );
        } else {
            assert_eq!(operation["security"], serde_json::json!([]), "{}", op.id);
        }
        let params = operation["parameters"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        if op.request_id_param {
            assert_eq!(params.len(), 1, "{}", op.id);
            assert_eq!(params[0]["name"], "X-Request-ID");
            assert_eq!(params[0]["in"], "header");
            assert_eq!(params[0]["schema"]["maxLength"], 128);
        } else {
            assert!(params.is_empty(), "{}", op.id);
        }
    }
    let encoding = &spec["paths"]["/v1/convert"]["post"]["requestBody"]["content"]["multipart/form-data"]
        ["encoding"];
    assert_eq!(encoding["file"]["contentType"], "application/octet-stream");
    assert_eq!(encoding["options"]["contentType"], "application/json");
}

#[test]
fn options_reject_unknown_properties() {
    let spec = spec();
    for name in [
        "ConvertOptions",
        "ImageOptions",
        "SlideOptions",
        "SheetOptions",
    ] {
        assert_eq!(
            spec["components"]["schemas"][name]["additionalProperties"], false,
            "{name}"
        );
    }
}

#[test]
fn command_is_deterministic_and_needs_no_environment() {
    let first = run_openapi();
    let second = run_openapi();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        first.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
    let text = String::from_utf8(first.stdout).unwrap();
    assert!(text.ends_with("}\n") && !text.ends_with("\n\n"));
    assert_eq!(text, format!("{}\n", openapi::spec_json()));
    let _: Value = serde_json::from_str(&text).unwrap();
}

#[tokio::test]
async fn swagger_doc_serves_the_same_bytes() {
    let config = Config::from_env(&Env::from_pairs([("API_KEY", "k")], 2)).unwrap();
    let unwired = Arc::new(Unwired);
    let state = AppState::new(
        &config,
        Engines {
            office: unwired.clone(),
            pdf: unwired.clone(),
            readiness: unwired,
        },
    );
    let response = router(state)
        .oneshot(
            Request::get("/swagger/doc.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(&body[..], run_openapi().stdout.as_slice());
}
