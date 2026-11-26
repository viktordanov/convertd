//! The Grafana dashboard and the alert rules in `deploy/observability/`
//! name only metrics that convertd emits (ledger item 010, effort O1).
//!
//! The scrape test starts `convertd serve` at level B, so the pools record
//! their metrics too, sends requests until every family exists, and
//! compares each `convert_*` name in the files with the scrape. A name
//! counts when it is a sample name or a `# TYPE` family, so a histogram
//! may appear as itself or with `_bucket`, `_sum` or `_count`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const PREFIX: &str = "convert_";

/// Every `convert_*` identifier in `text`. An identifier starts at a
/// character that cannot continue a name, so `rs_convert_x` is not one.
fn metric_names(text: &str) -> BTreeSet<String> {
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut names = BTreeSet::new();
    for (at, _) in text.match_indices(PREFIX) {
        if text[..at].chars().next_back().is_some_and(is_name) {
            continue;
        }
        let rest = &text[at..];
        let end = rest.find(|c: char| !is_name(c)).unwrap_or(rest.len());
        names.insert(rest[..end].to_owned());
    }
    names
}

/// The sample names and the `# TYPE` family names of a text exposition.
fn exposed(exposition: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in exposition.lines() {
        let name = match line.strip_prefix("# TYPE ") {
            Some(rest) => rest.split_whitespace().next(),
            None if line.starts_with('#') => None,
            None => line.split(['{', ' ']).next(),
        };
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            names.insert(name.to_owned());
        }
    }
    names
}

/// The names in `named` that `exposed` does not have.
fn missing(named: &BTreeSet<String>, exposed: &BTreeSet<String>) -> Vec<String> {
    named.difference(exposed).cloned().collect()
}

/// `deploy/observability/*.{json,yaml,yml}` with their text.
fn observability_files() -> Vec<(PathBuf, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/observability");
    let mut files: Vec<(PathBuf, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e == "json" || e == "yaml" || e == "yml")
        })
        .map(|p| {
            let text = std::fs::read_to_string(&p).unwrap();
            (p, text)
        })
        .collect();
    files.sort();
    files
}

/// Every metric name that the files name, after the checks that keep the
/// scan from passing on nothing: both files exist and each names metrics.
fn named_metrics() -> BTreeSet<String> {
    let files = observability_files();
    let file_names: Vec<String> = files
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    for expected in ["grafana-dashboard.json", "prometheusrule.yaml"] {
        assert!(
            file_names.iter().any(|n| n == expected),
            "{expected} missing from deploy/observability: {file_names:?}"
        );
    }
    let mut all = BTreeSet::new();
    for (path, text) in &files {
        let names = metric_names(text);
        // The dashboard names 15 families today and the rules 3: a scan
        // that finds fewer is broken, not passing.
        let least = if path.extension().is_some_and(|e| e == "json") {
            10
        } else {
            3
        };
        assert!(
            names.len() >= least,
            "{} names only {:?}",
            path.display(),
            names
        );
        all.extend(names);
    }
    all
}

#[test]
fn the_name_scan_reports_a_misspelled_metric() {
    let exposition = "# HELP x\n# TYPE convert_requests_total counter\n\
        convert_requests_total{code=\"OK\"} 1\n\
        # TYPE convert_stage_duration_seconds histogram\n\
        convert_stage_duration_seconds_bucket{le=\"+Inf\"} 1\n\
        convert_stage_duration_seconds_sum 0.1\n\
        convert_stage_duration_seconds_count 1\n\
        convert_queue_depth 0\n";
    let have = exposed(exposition);
    let expr = r#"sum(rate(convert_requets_total[5m])) / histogram_quantile(0.5, convert_stage_duration_seconds_bucket)
        + convert_stage_duration_seconds_sum + convert_queue_depth_bucket + rs_convert_ignored"#;
    let named = metric_names(expr);
    assert_eq!(
        named,
        [
            "convert_queue_depth_bucket",
            "convert_requets_total",
            "convert_stage_duration_seconds_bucket",
            "convert_stage_duration_seconds_sum",
        ]
        .map(String::from)
        .into(),
    );
    assert_eq!(
        missing(&named, &have),
        ["convert_queue_depth_bucket", "convert_requets_total"]
    );
    assert!(have.contains("convert_stage_duration_seconds"));
}

#[test]
fn the_dashboard_parses_and_reads_the_datasource_variable() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/observability/grafana-dashboard.json");
    let dashboard: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let variables = dashboard["templating"]["list"].as_array().unwrap();
    assert!(
        variables
            .iter()
            .any(|v| v["name"] == "datasource" && v["type"] == "datasource"),
        "no datasource variable"
    );
    let mut queries = 0;
    for panel in dashboard["panels"].as_array().unwrap() {
        for target in panel["targets"].as_array().into_iter().flatten() {
            assert_eq!(
                target["datasource"]["uid"], "${datasource}",
                "panel {} has a fixed datasource",
                panel["title"]
            );
            queries += 1;
        }
    }
    assert!(queries >= 20, "only {queries} queries");
    assert!(!named_metrics().is_empty());
}

#[cfg(feature = "test-hooks")]
/// Names that the scrape test cannot make cheaply, with the reason. Each
/// must still be a literal in `src/observability/metrics.rs`.
fn not_scraped() -> Vec<(&'static str, &'static str)> {
    let mut names = vec![(
        "convert_responses_cut_total",
        "needs a 200 whose body stalls or aborts",
    )];
    if !cfg!(target_os = "linux") {
        for name in [
            "convert_supervisor_rss_bytes",
            "convert_supervisor_rss_peak_bytes",
            "convert_supervisor_open_fds",
            "convert_supervisor_threads",
        ] {
            names.push((name, "read from /proc/self, Linux only"));
        }
    }
    names
}

#[cfg(feature = "test-hooks")]
mod level_b {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    const KEY: &str = "observability-key";
    const BOUNDARY: &str = "convertd-observability-boundary";
    const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n%%EOF\n";

    /// `convertd serve` at level B, killed on drop.
    pub struct Served {
        child: std::process::Child,
        port: u16,
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "a test runs the binary directly; no supervisor reaper exists here"
    )]
    impl Served {
        pub fn start() -> Self {
            let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("observability");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let child = std::process::Command::new(env!("CARGO_BIN_EXE_convertd"))
                .arg("serve")
                .env_clear()
                .env("HOST", "127.0.0.1")
                .env("PORT", port.to_string())
                .env("API_KEY", KEY)
                .env("TMP_DIR", &dir)
                .env("SHUTDOWN_GRACE_SECONDS", "5")
                .env("BUILD_INFO_PATH", "/nonexistent/build-info.json")
                .env("FONTS_PATH", "/nonexistent/fonts.json")
                .env("TEST_ENGINE", "worker")
                .env("OFFICE_WORKERS", "1")
                .env("RENDER_WORKERS", "1")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let served = Self { child, port };
            let deadline = Instant::now() + Duration::from_secs(30);
            while !served.get("/readyz").starts_with("HTTP/1.1 200") {
                assert!(Instant::now() < deadline, "the server never became ready");
                std::thread::sleep(Duration::from_millis(50));
            }
            served
        }

        /// The whole answer, or "" when nothing answers.
        fn exchange(&self, request: &[u8]) -> String {
            let Ok(mut stream) = TcpStream::connect(("127.0.0.1", self.port)) else {
                return String::new();
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            if stream.write_all(request).is_err() {
                return String::new();
            }
            let mut answer = Vec::new();
            let _ = stream.read_to_end(&mut answer);
            String::from_utf8_lossy(&answer).into_owned()
        }

        pub fn get(&self, path: &str) -> String {
            self.exchange(
                format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes(),
            )
        }

        /// The status of one `POST /v1/convert`.
        pub fn convert(&self, file: &[u8], options: &str) -> u16 {
            let mut body = format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"options\"\r\n\
                 Content-Type: application/json\r\n\r\n{options}\r\n\
                 --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\n\r\n"
            )
            .into_bytes();
            body.extend_from_slice(file);
            body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
            let mut request = format!(
                "POST /v1/convert HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {KEY}\r\n\
                 Connection: close\r\nContent-Type: multipart/form-data; boundary={BOUNDARY}\r\n\
                 Content-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            request.extend_from_slice(&body);
            let answer = self.exchange(&request);
            answer
                .split(' ')
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "a test runs the binary directly; no supervisor reaper exists here"
    )]
    impl Drop for Served {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    /// A DOCX whose text holds `marker` for the test worker.
    pub fn docx_with(marker: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("[Content_Types].xml", stored).unwrap();
        zip.write_all(br#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#).unwrap();
        zip.start_file("word/document.xml", stored).unwrap();
        zip.write_all(
            format!(
                r#"<w:document xmlns:w="w"><w:r><w:rPr><w:rFonts w:ascii="Georgia"/></w:rPr><w:t>{marker}</w:t></w:r></w:document>"#
            )
            .as_bytes(),
        )
        .unwrap();
        zip.finish().unwrap();
        out
    }

    pub fn pdf_with(marker: &str) -> Vec<u8> {
        let mut out = PDF.to_vec();
        out.extend_from_slice(format!("% {marker}\n").as_bytes());
        out
    }
}

/// The real check: every name in the files is on `/metrics` once each
/// family has a sample, or is a known exception that the source defines.
#[cfg(feature = "test-hooks")]
#[test]
fn every_metric_the_dashboard_and_rules_name_is_emitted() {
    use std::time::{Duration, Instant};

    let named = named_metrics();
    let served = level_b::Served::start();
    // An office conversion, a render with a clamped DPI (a warning and
    // rendered pages), a render worker that panics (a restart), and a
    // format the service refuses.
    assert_eq!(served.convert(&level_b::docx_with("hello"), "{}"), 200);
    assert_eq!(
        served.convert(
            &level_b::pdf_with("plain"),
            r#"{"output":"images","images":{"dpi":600}}"#
        ),
        200
    );
    assert_eq!(
        served.convert(&level_b::pdf_with("FAKE:PANIC"), r#"{"output":"images"}"#),
        422
    );
    assert_eq!(served.convert(b"<html></html>", "{}"), 415);

    // The pool counts a restart when it reaps the worker, after the reply.
    let deadline = Instant::now() + Duration::from_secs(10);
    let scrape = loop {
        let answer = served.get("/metrics");
        if answer.contains("convert_worker_restarts_total") || Instant::now() >= deadline {
            break answer;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(scrape.starts_with("HTTP/1.1 200"), "{scrape}");
    let have = exposed(&scrape);

    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/observability/metrics.rs"),
    )
    .unwrap();
    let mut absent = Vec::new();
    for name in missing(&named, &have) {
        match not_scraped().into_iter().find(|(n, _)| *n == name) {
            Some((_, reason)) => assert!(
                source.contains(&format!("\"{name}\"")),
                "{name} is not scraped ({reason}) and src/observability/metrics.rs does not define it"
            ),
            None => absent.push(name),
        }
    }
    assert!(
        absent.is_empty(),
        "deploy/observability names metrics that convertd does not emit: {absent:?}\nscrape:\n{scrape}"
    );
}
