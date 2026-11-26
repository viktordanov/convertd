//! Metric names and recording helpers (lane 001), rendered by the
//! `/metrics` handler. One Prometheus recorder per process: the `metrics`
//! macros record into a single global recorder, installed on first use.
//!
//! Names follow `PLAN.md` and INTERFACES §11-12. Lane 002 records its own
//! pool metrics (`convert_workers`, `convert_worker_*`, `convert_queue_depth`)
//! through the same recorder; the pool records a worker call's CPU time
//! through [`worker_cpu`].

use std::sync::OnceLock;
use std::time::Duration;

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Bucket bounds in seconds for every `_seconds` histogram.
const SECONDS_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    60.0, 120.0, 210.0,
];
/// Bucket bounds in seconds for `convert_worker_cpu_seconds`: 1 ms to 60 s.
const CPU_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
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
            // A full match wins over the `_seconds` suffix.
            .and_then(|b| {
                b.set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Full("convert_worker_cpu_seconds".into()),
                    CPU_BUCKETS,
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
    verify_skipped_by(check, reason, 1);
}

/// `convert_verify_skipped_total{check,reason}`, by `count`: the pages a
/// per-page check skipped.
pub fn verify_skipped_by(check: &'static str, reason: &'static str, count: u64) {
    counter!("convert_verify_skipped_total", "check" => check, "reason" => reason).increment(count);
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

/// One worker call's CPU time, user plus system, as the worker reported it:
/// `convert_worker_cpu_seconds{pool,op}`. The pool records every reply,
/// an engine error included; a call that times out, crashes or is killed
/// sends no reply and records nothing.
pub fn worker_cpu(pool: &'static str, op: &'static str, cpu: Duration) {
    histogram!("convert_worker_cpu_seconds", "pool" => pool, "op" => op)
        .record(cpu.as_secs_f64());
}

/// The four supervisor gauges of INTERFACES §11, from `/proc/self`
/// (Linux only; elsewhere they are not published).
fn supervisor_gauges() {
    // cfg! rather than #[cfg]: the code compiles and is linted everywhere.
    if cfg!(target_os = "linux") {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            let parsed = parse_status(&status);
            if let Some(bytes) = parsed.rss_bytes {
                gauge!("convert_supervisor_rss_bytes").set(bytes as f64);
            }
            if let Some(bytes) = parsed.rss_peak_bytes {
                gauge!("convert_supervisor_rss_peak_bytes").set(bytes as f64);
            }
            if let Some(threads) = parsed.threads {
                gauge!("convert_supervisor_threads").set(threads as f64);
            }
        }
        if let Ok(fds) = std::fs::read_dir("/proc/self/fd") {
            gauge!("convert_supervisor_open_fds").set(fds.count() as f64);
        }
    }
}

/// What `/proc/self/status` says about memory and threads.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProcStatus {
    pub rss_bytes: Option<u64>,
    pub rss_peak_bytes: Option<u64>,
    pub threads: Option<u64>,
}

/// Parses the `VmRSS`, `VmHWM` (both in kB) and `Threads` lines.
pub fn parse_status(status: &str) -> ProcStatus {
    let mut out = ProcStatus::default();
    for line in status.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok());
        match key {
            "VmRSS" => out.rss_bytes = value.map(|kib| kib * 1024),
            "VmHWM" => out.rss_peak_bytes = value.map(|kib| kib * 1024),
            "Threads" => out.threads = value,
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_status_lines() {
        let status =
            "Name:\tconvertd\nVmHWM:\t   20480 kB\nVmRSS:\t   10240 kB\nThreads:\t7\nSigQ:\t0/1\n";
        assert_eq!(
            parse_status(status),
            ProcStatus {
                rss_bytes: Some(10_485_760),
                rss_peak_bytes: Some(20_971_520),
                threads: Some(7),
            }
        );
        assert_eq!(parse_status("garbage\nVmRSS: x kB"), ProcStatus::default());
    }
}
