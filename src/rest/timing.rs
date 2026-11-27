//! The `Server-Timing` header and the overhead figures (`src/README.md`,
//! Measurement).

use std::fmt::Write;
use std::time::Duration;

use axum::http::HeaderValue;

use crate::domain::conversion::StageTimings;

/// Everything the header reports for one request.
#[derive(Debug, Clone, Default)]
pub struct RequestTimings {
    /// From the handler's start to the end of the upload; outside `total`.
    pub recv: Option<Duration>,
    pub detect: Option<Duration>,
    pub stages: StageTimings,
    /// Building the response head.
    pub package: Option<Duration>,
    /// From the end of the upload to the start of the response.
    pub total: Option<Duration>,
}

impl RequestTimings {
    /// `overhead` = total − queue − Σ worker engine time (the 5 ms gate).
    pub fn overhead(&self) -> Duration {
        self.total
            .unwrap_or_default()
            .saturating_sub(self.stages.queue)
            .saturating_sub(self.stages.engine_total())
    }

    /// `overhead_strict` = `overhead` + inspect and probe engine time.
    pub fn overhead_strict(&self) -> Duration {
        self.overhead() + self.stages.checks_engine()
    }

    /// `name;dur=<ms>` entries with 3 decimals, in the order below. Stages
    /// that did not run are left out; `spawn` and `bind` are always sent.
    /// `preflight` is the office worker's scan (`EngineTiming.preflight`),
    /// sent only for office inputs. `probe` has no wall figure of its own:
    /// the probe runs inside inspect.
    pub fn header(&self) -> HeaderValue {
        let s = &self.stages;
        let entries: [(&str, Option<Duration>); 18] = [
            ("recv", self.recv),
            ("detect", self.detect),
            ("preflight", s.preflight),
            ("queue", Some(s.queue)),
            ("lease", Some(s.lease)),
            ("spawn", Some(s.spawn)),
            ("bind", Some(s.bind)),
            ("convert", s.convert),
            ("convert_engine", s.convert_engine),
            ("inspect", s.inspect),
            ("inspect_engine", s.inspect_engine),
            ("probe", None),
            ("probe_engine", s.probe_engine),
            ("render", s.render),
            ("render_engine", s.render_engine),
            ("verify", s.verify),
            ("package", self.package),
            ("total", self.total),
        ];
        let mut out = String::with_capacity(320);
        for (name, value) in entries {
            let Some(value) = value else { continue };
            if !out.is_empty() {
                out.push_str(", ");
            }
            let micros = value.as_micros();
            let _ = write!(out, "{name};dur={}.{:03}", micros / 1000, micros % 1000);
        }
        // Names and digits only: always a valid header value.
        HeaderValue::from_str(&out).unwrap_or_else(|_| HeaderValue::from_static(""))
    }
}

/// Whole microseconds, for the logs.
pub fn micros(value: Duration) -> u64 {
    u64::try_from(value.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_format_and_order() {
        let timings = RequestTimings {
            recv: Some(Duration::from_micros(1500)),
            detect: Some(Duration::from_micros(12)),
            stages: StageTimings {
                queue: Duration::from_micros(2),
                convert: Some(Duration::from_millis(31)),
                convert_engine: Some(Duration::from_millis(30)),
                verify: Some(Duration::ZERO),
                ..Default::default()
            },
            package: Some(Duration::from_micros(40)),
            total: Some(Duration::from_micros(31_250)),
        };
        assert_eq!(
            timings.header(),
            "recv;dur=1.500, detect;dur=0.012, queue;dur=0.002, lease;dur=0.000, \
             spawn;dur=0.000, bind;dur=0.000, convert;dur=31.000, convert_engine;dur=30.000, \
             verify;dur=0.000, package;dur=0.040, total;dur=31.250"
        );
        assert_eq!(timings.overhead(), Duration::from_micros(1248));
    }

    #[test]
    fn strict_overhead_adds_the_checks() {
        let timings = RequestTimings {
            stages: StageTimings {
                inspect_engine: Some(Duration::from_millis(2)),
                probe_engine: Some(Duration::from_millis(1)),
                ..Default::default()
            },
            total: Some(Duration::from_millis(10)),
            ..Default::default()
        };
        assert_eq!(timings.overhead(), Duration::from_millis(7));
        assert_eq!(timings.overhead_strict(), Duration::from_millis(10));
    }
}
