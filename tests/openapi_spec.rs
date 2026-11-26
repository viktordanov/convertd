//! The cheap invariants of the emitted spec (plan 001 step 3). The spec
//! itself is the contract: `api/openapi.json` is committed, and
//! `generate.sh --check` fails when the emitted spec drifts from it.

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
    for media in ["application/pdf", "application/zstd"] {
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
