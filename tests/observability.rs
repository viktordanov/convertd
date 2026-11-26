//! The Grafana dashboard and the alert rules in `deploy/observability/`
//! name only metrics that convertd emits (ledger item 010, effort O1).
//!
//! The scrape test starts `convertd serve` at level B, so the pools record
//! their metrics too, and sends requests until every family exists. Two
//! sets of names must then be sample names of the scrape:
//!
//! - every `convert_*` identifier anywhere in the files, comments and
//!   panel descriptions included;
//! - every metric selector in a query (the `expr` of a panel or a rule, and
//!   the metric of a `label_values` variable), whatever its prefix, so a
//!   typo such as `covert_workers` fails too.
//!
//! A histogram counts only with its `_bucket`, `_sum` or `_count` suffix,
//! because Prometheus has no series under the bare family name.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const PREFIX: &str = "convert_";
/// Stands for a `{__name__=...}` matcher, which the check does not support.
const NAME_MATCHER: &str = "{__name__} matcher (write the metric name before the braces)";

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

/// The index after the closing `close` that matches the opening byte at
/// `at`, skipping quoted strings; the end of `b` when it never closes.
fn skip_group(b: &[u8], at: usize, close: u8) -> usize {
    let open = b[at];
    let mut depth = 0usize;
    let mut i = at;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' | b'`' => {
                i = skip_string(b, i);
                continue;
            }
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// The index after the string that starts at `at`.
fn skip_string(b: &[u8], at: usize) -> usize {
    let quote = b[at];
    let mut i = at + 1;
    while i < b.len() {
        match b[i] {
            b'\\' if quote != b'`' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// The metric names that a PromQL expression selects: every identifier
/// that is not a keyword, an aggregation, a function call, a label in
/// `{...}` or `by (...)`, or part of a string, a number or a `[...]` range.
/// A `__name__` matcher gives [`NAME_MATCHER`].
fn query_metrics(expr: &str) -> BTreeSet<String> {
    const KEYWORDS: &[&str] = &[
        "and",
        "or",
        "unless",
        "bool",
        "offset",
        "inf",
        "nan",
        "sum",
        "min",
        "max",
        "avg",
        "count",
        "stddev",
        "stdvar",
        "topk",
        "bottomk",
        "quantile",
        "count_values",
        "group",
        "limitk",
        "limit_ratio",
    ];
    const LABEL_LISTS: &[&str] = &[
        "by",
        "without",
        "on",
        "ignoring",
        "group_left",
        "group_right",
    ];
    let starts = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c == b':';
    let continues = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b':';
    let b = expr.as_bytes();
    let mut names = BTreeSet::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' | b'`' => i = skip_string(b, i),
            b'{' => {
                let end = skip_group(b, i, b'}');
                // A name matcher selects a metric that the scan does not
                // read; record a name that no scrape has, so it fails.
                if expr[i..end].contains("__name__") {
                    names.insert(NAME_MATCHER.to_owned());
                }
                i = end;
            }
            b'[' => i = skip_group(b, i, b']'),
            c if c.is_ascii_digit() || c == b'.' || c == b'$' => {
                i += 1;
                while i < b.len() && (continues(b[i]) || b[i] == b'.') {
                    i += 1;
                }
            }
            c if starts(c) => {
                let start = i;
                while i < b.len() && continues(b[i]) {
                    i += 1;
                }
                let word = &expr[start..i];
                let mut next = i;
                while next < b.len() && b[next].is_ascii_whitespace() {
                    next += 1;
                }
                let call = next < b.len() && b[next] == b'(';
                if LABEL_LISTS.contains(&word) {
                    if call {
                        i = skip_group(b, next, b')');
                    }
                } else if !call && !KEYWORDS.contains(&word) {
                    names.insert(word.to_owned());
                }
            }
            _ => i += 1,
        }
    }
    names
}

/// The sample names of a text exposition: for a histogram, the names with
/// `_bucket`, `_sum` and `_count`.
fn exposed(exposition: &str) -> BTreeSet<String> {
    exposition
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split(['{', ' ']).next())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
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

/// The queries of a dashboard: every `expr`, and the metric of each
/// `label_values(metric{...}, label)` variable, from both its `definition`
/// (what Grafana shows) and its `query` (what Grafana runs).
fn dashboard_queries(value: &serde_json::Value, out: &mut Vec<String>) {
    let label_values = |text: &str| {
        text.trim()
            .strip_prefix("label_values(")
            .and_then(|rest| rest.rsplit_once(','))
            .map(|(metric, _)| metric.to_owned())
            .unwrap_or_else(|| panic!("unknown variable query {text}"))
    };
    match value {
        serde_json::Value::Object(map)
            if map.get("type").and_then(|t| t.as_str()) == Some("query") =>
        {
            let definition = map["definition"].as_str().expect("a variable definition");
            let query = map["query"]
                .as_str()
                .or_else(|| map["query"]["query"].as_str())
                .expect("a variable query");
            out.push(label_values(definition));
            out.push(label_values(query));
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                match (key.as_str(), item.as_str()) {
                    ("expr", Some(expr)) => out.push(expr.to_owned()),
                    _ => dashboard_queries(item, out),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                dashboard_queries(item, out);
            }
        }
        _ => {}
    }
}

/// The `expr:` values of a rules file: a `|` or `>` block, or a plain
/// value with its continuation lines (any line indented deeper than the
/// `expr:` key belongs to the value). A quoted value fails the test: its
/// escapes would need a YAML parser, and a block scalar needs none.
fn rule_queries(yaml: &str) -> Vec<String> {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut out = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        // The key's own indent: `- expr:` opens a list item at the dash.
        let trimmed = line.trim_start();
        let (indent, key) = match trimmed.strip_prefix("- ") {
            Some(rest) => (line.len() - rest.trim_start().len(), rest.trim_start()),
            None => (line.len() - trimmed.len(), trimmed),
        };
        let Some(value) = key.strip_prefix("expr:") else {
            continue;
        };
        let value = value.trim();
        let mut parts = Vec::new();
        assert!(
            !value.starts_with(['"', '\'']),
            "write expr as a | block, not a quoted string: {line}"
        );
        if !(value.starts_with('|') || value.starts_with('>')) {
            parts.push(value);
        }
        parts.extend(
            lines[n + 1..]
                .iter()
                .take_while(|l| l.trim().is_empty() || l.len() - l.trim_start().len() > indent)
                .copied(),
        );
        out.push(parts.join("\n"));
    }
    out
}

/// Every metric name that the files name, after the checks that keep the
/// scan from passing on nothing: both files exist and each has queries.
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
        let json = path.extension().is_some_and(|e| e == "json");
        let queries = if json {
            let mut out = Vec::new();
            dashboard_queries(&serde_json::from_str(text).unwrap(), &mut out);
            out
        } else {
            rule_queries(text)
        };
        // The dashboard has 42 queries today and the rules 4: a scan that
        // finds fewer is broken, not passing.
        let least = if json { 30 } else { 4 };
        assert!(
            queries.len() >= least,
            "{} has only {} queries",
            path.display(),
            queries.len()
        );
        for query in &queries {
            let names = query_metrics(query);
            assert!(!names.is_empty(), "no metric in {query}");
            all.extend(names);
        }
        all.extend(metric_names(text));
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
        convert_workers{state=\"busy\"} 0\n";
    let have = exposed(exposition);
    assert!(!have.contains("convert_stage_duration_seconds"));

    let text = r#"sum(rate(convert_requets_total[5m])) + convert_stage_duration_seconds
        + convert_stage_duration_seconds_sum + rs_convert_ignored"#;
    let named = metric_names(text);
    assert_eq!(
        missing(&named, &have),
        ["convert_requets_total", "convert_stage_duration_seconds"]
    );

    let query = r#"histogram_quantile(0.99, sum by (le, stage) (rate(convert_stage_duration_seconds_bucket{stage=~"a|b", x="y,}"}[$__rate_interval])))
        / ignoring(state) (covert_workers{state="max"} > 0) * 1e4
        + (sum(rate(convert_requests_total{code!="OK"}[5m])) or vector(0))
        + max_over_time(sum by (pool) (convert_workers{state="busy"})[1m:]) and on (pod) up or {__name__="covert_workers"}"#;
    let selected = query_metrics(query);
    assert_eq!(
        selected,
        [
            "convert_requests_total",
            "convert_stage_duration_seconds_bucket",
            "convert_workers",
            "covert_workers",
            "up",
            NAME_MATCHER,
        ]
        .map(String::from)
        .into()
    );
    assert_eq!(
        missing(&selected, &have),
        ["covert_workers", "up", NAME_MATCHER]
    );
}

#[test]
fn the_query_extractors_find_every_query() {
    let yaml = "rules:\n  - alert: A\n    expr: |\n      sum(a_total)\n        > 0\n    for: 5m\n  - alert: B\n    expr: b_total > 1\n  - alert: C\n    expr: c_total > 0\n      and covert_d > 0\n    for: 1m\n  - expr: rate(covert_e[5m])\n    alert: E\n";
    assert_eq!(
        rule_queries(yaml),
        [
            "      sum(a_total)\n        > 0",
            "b_total > 1",
            "c_total > 0\n      and covert_d > 0",
            "rate(covert_e[5m])"
        ]
    );
    let dashboard = serde_json::json!({
        "panels": [{"targets": [{"expr": "rate(x_total[1m])"}]}],
        "templating": {"list": [{
            "type": "query",
            "definition": "label_values(y_bytes{n=~\"$n\"}, pod)",
            "query": {"query": "label_values(covert_y_bytes, pod)", "refId": "v"}
        }]}
    });
    let mut out = Vec::new();
    dashboard_queries(&dashboard, &mut out);
    assert_eq!(
        out,
        ["rate(x_total[1m])", "y_bytes{n=~\"$n\"}", "covert_y_bytes"]
    );
}

#[test]
#[should_panic(expected = "not a quoted string")]
fn a_quoted_rule_expression_is_refused() {
    rule_queries(
        "  - alert: A\n    expr: \"convert_workers{state=\\\"busy\\\"} + covert_workers\"\n",
    );
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
    assert!(queries >= 30, "only {queries} queries");
    assert!(!named_metrics().is_empty());
}

#[cfg(feature = "test-hooks")]
/// Names that the scrape test cannot make cheaply: the name, the reason,
/// and the file that must name it instead.
fn not_scraped() -> Vec<(&'static str, &'static str, &'static str)> {
    // A stalled response takes a 64 MiB upload and a 1 s idle timeout;
    // that test asserts the counter on its own scrape.
    let mut names = vec![(
        "convert_responses_cut_total",
        "needs a 200 whose body stalls",
        "tests/convert_pdf.rs",
    )];
    if !cfg!(target_os = "linux") {
        for name in [
            "convert_supervisor_rss_bytes",
            "convert_supervisor_rss_peak_bytes",
            "convert_supervisor_open_fds",
            "convert_supervisor_threads",
        ] {
            names.push((
                name,
                "read from /proc/self, Linux only",
                "src/observability/metrics.rs",
            ));
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
    // Two office conversions at once on one worker, so the second waits
    // (the queue gauge exists only after a wait), a render with a clamped
    // DPI (a warning and rendered pages), a render worker that panics (a
    // restart), and a format the service refuses.
    // A loaded runner can miss the overlap, so try again until the gauge
    // shows, within a deadline.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !served.get("/metrics").contains("convert_queue_depth") {
        assert!(Instant::now() < deadline, "no office request ever waited");
        std::thread::scope(|scope| {
            let first =
                scope.spawn(|| served.convert(&level_b::docx_with("FAKE:DELAY_MS=600"), "{}"));
            std::thread::sleep(Duration::from_millis(150));
            assert_eq!(served.convert(&level_b::docx_with("hello"), "{}"), 200);
            assert_eq!(first.join().unwrap(), 200);
        });
    }
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

    let mut absent = Vec::new();
    for name in missing(&named, &have) {
        match not_scraped().into_iter().find(|(n, _, _)| *n == name) {
            Some((_, reason, file)) => {
                let text =
                    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(file))
                        .unwrap();
                assert!(
                    text.contains(name.as_str()),
                    "{name} is not scraped ({reason}) and {file} does not name it"
                );
            }
            None => absent.push(name),
        }
    }
    assert!(
        absent.is_empty(),
        "deploy/observability names metrics that convertd does not emit: {absent:?}\nscrape:\n{scrape}"
    );
}
