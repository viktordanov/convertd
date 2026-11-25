//! Metric names and recording helpers (lane 001), rendered by the
//! `/metrics` handler. One Prometheus recorder per process: the `metrics`
//! macros record into a single global recorder, installed on first use.
//!
//! Names follow `PLAN.md` and INTERFACES §11-12. Lane 002 records its own
//! pool metrics (`convert_workers`, `convert_worker_*`, `convert_queue_depth`)
//! through the same recorder.

use std::sync::OnceLock;
use std::time::Duration;

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Bucket bounds in seconds for every `_seconds` histogram.
const SECONDS_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    60.0, 120.0, 210.0,
];
/// Bucket bounds in bytes for `convert_input_bytes`.
const BYTES_BUCKETS: &[f64] = &[1e4, 1e5, 1e6, 5e6, 1e7, 2.5e7, 5e7, 1e8, 1.25e8];

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// The process-wide recorder, installed on first use.
pub fn handle() -> &'static PrometheusHandle {
    HANDLE.get_or_init(|| {
        let builder = PrometheusBuilder::new();
        let builder = builder
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Suffix("_seconds".into()),
                SECONDS_BUCKETS,
            )
            .and_then(|b| {
                b.set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Full("convert_input_bytes".into()),
                    BYTES_BUCKETS,
                )
            })
            .unwrap_or_else(|_| PrometheusBuilder::new());
        let recorder = builder.build_recorder();
        let handle = recorder.handle();
        // Fails only when another recorder was installed first; the handle
        // then renders an empty set, which a test would show.
        if metrics::set_global_recorder(recorder).is_err() {
            tracing::warn!("a metrics recorder was already installed");
        }
        handle
    })
}

/// Drains histogram samples into their buckets every `period`, so memory
/// stays bounded between scrapes. Call from inside the runtime.
pub fn spawn_upkeep(period: Duration) {
    let handle = handle();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        loop {
            tick.tick().await;
            handle.run_upkeep();
        }
    });
}

/// The disk budget at scrape time.
pub struct Snapshot {
    pub disk_used: u64,
    pub disk_max: u64,
}

/// The text exposition of every metric.
pub fn render(snapshot: Snapshot) -> String {
    let handle = handle();
    gauge!("convert_disk_budget_bytes", "state" => "used").set(snapshot.disk_used as f64);
    gauge!("convert_disk_budget_bytes", "state" => "max").set(snapshot.disk_max as f64);
    supervisor_gauges();
    handle.run_upkeep();
    handle.render()
}

/// One finished request: `convert_requests_total{output,family,code}`. A
/// 200 has code `OK`.
pub fn request(output: &'static str, family: &'static str, code: &'static str) {
    counter!("convert_requests_total", "output" => output, "family" => family, "code" => code)
        .increment(1);
}

/// `convert_stage_duration_seconds{stage,family}`.
pub fn stage(stage: &'static str, family: &'static str, duration: Duration) {
    histogram!("convert_stage_duration_seconds", "stage" => stage, "family" => family)
        .record(duration.as_secs_f64());
}

/// `convert_input_bytes{family}`.
pub fn input_bytes(family: &'static str, bytes: u64) {
    histogram!("convert_input_bytes", "family" => family).record(bytes as f64);
}

/// `convert_warnings_total{code}`.
pub fn warning(code: &'static str) {
    counter!("convert_warnings_total", "code" => code).increment(1);
}

/// `convert_verify_skipped_total{check,reason}`.
pub fn verify_skipped(check: &'static str, reason: &'static str) {
    counter!("convert_verify_skipped_total", "check" => check, "reason" => reason).increment(1);
}

/// `convert_pages_rendered_total{format}`.
pub fn pages_rendered(format: &'static str, pages: u64) {
    counter!("convert_pages_rendered_total", "format" => format).increment(pages);
}

/// A 200 whose body did not complete: `convert_responses_cut_total{code}`
/// with `RESPONSE_STALLED` or `RESPONSE_ABORTED`. Kept apart from
/// `convert_requests_total`, which counted the request at its head.
pub fn response_cut(code: &'static str) {
    counter!("convert_responses_cut_total", "code" => code).increment(1);
}

/// The four supervisor gauges of INTERFACES §11, from `/proc/self`
/// (Linux only; elsewhere they are not published).
fn supervisor_gauges() {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                let value = |prefix: &str| {
                    line.strip_prefix(prefix)
                        .and_then(|rest| rest.split_whitespace().next())
                        .and_then(|n| n.parse::<f64>().ok())
                };
                if let Some(kib) = value("VmRSS:") {
                    gauge!("convert_supervisor_rss_bytes").set(kib * 1024.0);
                } else if let Some(kib) = value("VmHWM:") {
                    gauge!("convert_supervisor_rss_peak_bytes").set(kib * 1024.0);
                } else if let Some(threads) = value("Threads:") {
                    gauge!("convert_supervisor_threads").set(threads);
                }
            }
        }
        if let Ok(fds) = std::fs::read_dir("/proc/self/fd") {
            gauge!("convert_supervisor_open_fds").set(fds.count() as f64);
        }
    }
}
