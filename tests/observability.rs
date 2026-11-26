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
/// Stands for a `{...}` selector without a metric name before it, such as
/// `{pool="office"}`, `{__name__="x"}` or the quoted `{"x"}`, which the
/// check cannot verify.
const NAME_MATCHER: &str = "a {...} selector without a metric name before it";
/// Stands for a `$variable` outside a label matcher or a range.
const VARIABLE: &str = "a $variable or [[variable]] outside {...} and [...]";

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
            // A comment runs to the end of its line, quotes included.
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
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

/// Whether a `{...}` group has a string where a label name goes: right
/// after `{` or `,`. Prometheus 3 reads `{"x"}` as the metric `x`.
fn quoted_name(group: &[u8]) -> bool {
    let mut previous = 0u8;
    let mut i = 0;
    while i < group.len() {
        match group[i] {
            b'"' | b'\'' | b'`' => {
                if previous == b'{' || previous == b',' {
                    return true;
                }
                i = skip_string(group, i);
                previous = b'"';
                continue;
            }
            c if c.is_ascii_whitespace() => {}
            c => previous = c,
        }
        i += 1;
    }
    false
}

/// The metric names that a PromQL expression selects: every identifier
/// that is not a keyword, an aggregation, a function call, a label in
/// `{...}` or `by (...)`, or part of a string, a number or a `[...]` range.
/// A `# comment` is skipped. A `__name__` matcher gives [`NAME_MATCHER`].
fn query_metrics(expr: &str) -> BTreeSet<String> {
    const KEYWORDS: &[&str] = &["and", "or", "unless", "bool", "inf", "nan"];
    // An aggregation only when `(`, `by` or `without` follows; otherwise
    // the word is a metric name, as PromQL reads it.
    const AGGREGATIONS: &[&str] = &[
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
    // Whether the last token was a metric name, for the `{` that follows.
    let mut after_metric = false;
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let was_metric = std::mem::replace(&mut after_metric, false);
        match b[i] {
            b'"' | b'\'' | b'`' => i = skip_string(b, i),
            // A comment runs to the end of its line.
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => {
                let end = skip_group(b, i, b'}');
                // Braces without a metric name before them select every
                // metric, and a name inside them (`__name__`, or a quoted
                // name where a label name goes) escapes the scan. Record a
                // name that no scrape has, so the check fails.
                if !was_metric || expr[i..end].contains("__name__") || quoted_name(&b[i..end]) {
                    names.insert(NAME_MATCHER.to_owned());
                }
                i = end;
            }
            // `[[name]]` is the older Grafana variable syntax, not a range.
            b'[' if b.get(i + 1) == Some(&b'[') => {
                names.insert(VARIABLE.to_owned());
                i = skip_group(b, i, b']');
            }
            b'[' => i = skip_group(b, i, b']'),
            // A dashboard variable outside `{...}` and `[...]` can stand for
            // a metric name that the check cannot see.
            b'$' => {
                names.insert(VARIABLE.to_owned());
                i += 1;
                while i < b.len() && continues(b[i]) {
                    i += 1;
                }
            }
            c if c.is_ascii_digit() || c == b'.' => {
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
                // PromQL keywords ignore case; metric names do not.
                let lower = word.to_ascii_lowercase();
                let lower = lower.as_str();
                // Bytes, not str slices: a label value can be UTF-8.
                let grouped = ["by", "without"].iter().any(|g| {
                    b.get(next..next + g.len())
                        .is_some_and(|w| w.eq_ignore_ascii_case(g.as_bytes()))
                        && !b.get(next + g.len()).is_some_and(|c| continues(*c))
                });
                // `offset` is a modifier only before its duration.
                let modifier = lower == "offset"
                    && b.get(next)
                        .is_some_and(|c| c.is_ascii_digit() || *c == b'-' || *c == b'$');
                if LABEL_LISTS.contains(&lower) && call {
                    i = skip_group(b, next, b')');
                } else {
                    // A function call, a keyword, a bare `group_left` or an
                    // aggregation with its `(` or `by` is not a metric.
                    let syntax = call
                        || KEYWORDS.contains(&lower)
                        || modifier
                        || lower == "group_left"
                        || lower == "group_right"
                        || (AGGREGATIONS.contains(&lower) && grouped);
                    if !syntax {
                        names.insert(word.to_owned());
                        after_metric = true;
                    }
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
    // The word `expr` anywhere, comments included: every rule key in any
    // YAML form (`"expr":`, a flow mapping) counts, so a form that the line
    // scan below does not read fails the test instead of escaping it.
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let keys = yaml
        .match_indices("expr")
        .filter(|(at, _)| {
            !yaml[..*at].chars().next_back().is_some_and(is_word)
                && !yaml[at + 4..].chars().next().is_some_and(is_word)
        })
        .count();
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
    assert_eq!(
        out.len(),
        keys,
        "write each expr as its own `expr:` line in block style, and use the word expr nowhere else"
    );
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
        // The two examples have 42 and 4 queries today: a scan that finds
        // fewer is broken, not passing. Any other file needs one query.
        let least = match path.file_name().and_then(|n| n.to_str()) {
            Some("grafana-dashboard.json") => 30,
            Some("prometheusrule.yaml") => 4,
            _ => 1,
        };
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
    assert!(quoted_name(br#"{"covert_x"}"#));
    assert!(quoted_name(br#"{pod="a", "covert_x"}"#));
    assert!(!quoted_name(br#"{pod="a,{", code!~"x|y"}"#));
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
        + max_over_time(sum by (pool) (convert_workers{state="busy"})[1m:]) and on (pod) up # office capacity
        + convert_workers {pool="render"}"#;
    let selected = query_metrics(query);
    assert_eq!(
        selected,
        [
            "convert_requests_total",
            "convert_stage_duration_seconds_bucket",
            "convert_workers",
            "covert_workers",
            "up",
        ]
        .map(String::from)
        .into()
    );
    assert_eq!(missing(&selected, &have), ["covert_workers", "up"]);
    // A comment inside braces hides neither the closing brace nor what follows.
    let names =
        query_metrics("convert_workers{pool=\"office\" # don't count render\n} + covert_workers");
    assert_eq!(
        names,
        ["convert_workers", "covert_workers"]
            .map(String::from)
            .into()
    );
    // A selector that names no metric before its braces cannot be checked.
    for bare in [
        r#"convert_workers or {__name__="covert_workers"}"#,
        r#"convert_workers + {"covert_x", pod="a"}"#,
        r#"sum({pool="office"}) / sum(convert_workers)"#,
    ] {
        let names = query_metrics(bare);
        assert!(names.contains(NAME_MATCHER), "{bare}: {names:?}");
        assert!(missing(&names, &have).contains(&NAME_MATCHER.to_owned()));
    }
    let names =
        query_metrics(r#"convert_workers{pod=~"$pod"}[$__rate_interval] or $worker_metric"#);
    assert_eq!(
        names,
        ["convert_workers", VARIABLE].map(String::from).into()
    );
    let names = query_metrics("convert_workers or [[worker_metric]]");
    assert_eq!(
        names,
        ["convert_workers", VARIABLE].map(String::from).into()
    );
    // An aggregation name without `(`, `by` or `without` is a metric.
    let names = query_metrics(
        "sum by (pool) (convert_workers) or sum without (pod) (x) or count or without \
         or SUM BY (pool) (y) / ignoring (pod) z * on(pod) group_left w offset 5m \
         or offset or v{pod=~\"東京.*\"} offset -1h",
    );
    assert_eq!(
        names,
        [
            "convert_workers",
            "count",
            "offset",
            "v",
            "w",
            "without",
            "x",
            "y",
            "z"
        ]
        .map(String::from)
        .into()
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
#[should_panic(expected = "in block style")]
fn a_flow_style_rule_is_refused() {
    rule_queries("  - alert: A\n    expr: up\n  - {alert: B, expr: covert_workers == 0}\n");
}

#[test]
#[should_panic(expected = "in block style")]
fn a_quoted_rule_key_is_refused() {
    rule_queries("  - alert: A\n    expr: up\n  - alert: B\n    \"expr\": covert_workers > 0\n");
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
        dir: std::path::PathBuf,
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "a test runs the binary directly; no supervisor reaper exists here"
    )]
    impl Served {
        pub fn start() -> Self {
            let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("observability-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_convertd"))
                .arg("serve")
                .env_clear()
                .env("HOST", "127.0.0.1")
                .env("PORT", "0")
                .env("API_KEY", KEY)
                .env("TMP_DIR", &dir)
                .env("SHUTDOWN_GRACE_SECONDS", "5")
                .env("BUILD_INFO_PATH", "/nonexistent/build-info.json")
                .env("FONTS_PATH", "/nonexistent/fonts.json")
                .env("TEST_ENGINE", "worker")
                .env("OFFICE_WORKERS", "1")
                .env("RENDER_WORKERS", "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            // The server binds port 0 and logs its address on stdout; the
            // reader drains the log to the end so the child never blocks.
            let stdout = child.stdout.take().unwrap();
            let mut served = Self {
                child,
                port: 0,
                dir: dir.clone(),
            };
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    if event["message"] == "server listening"
                        && let Some(addr) = event["addr"].as_str()
                        && let Ok(addr) = addr.parse::<std::net::SocketAddr>()
                    {
                        let _ = tx.send(addr.port());
                    }
                }
            });
            served.port = rx
                .recv_timeout(Duration::from_secs(30))
                .expect("the server never logged its address");
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
            let _ = std::fs::remove_dir_all(&self.dir);
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
                // The whole identifier: `{name}_v2` in the file does not count.
                assert!(
                    metric_names(&text).contains(&name),
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
