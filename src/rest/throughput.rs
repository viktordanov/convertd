//! The minimum-throughput bound for uploads and responses.
//!
//! The idle timer (`IO_IDLE_TIMEOUT_SECONDS`) ends a transfer that sends
//! nothing, but a trickle of one byte just inside each idle period resets it
//! forever and keeps the request's permit. The meter adds a floor: in every
//! window of `window` (the idle timeout), the transfer must move at least
//! `min_per_second × window` bytes. It is checked as bytes move, so a trickle
//! ends at its first chunk after one window, and a silent client still meets
//! the idle timer first.

use std::time::{Duration, Instant};

/// The floor: `min_per_second` bytes per second, averaged over each window
/// of `window`. `min_per_second == 0` turns it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinRate {
    pub min_per_second: u64,
    pub window: Duration,
}

impl MinRate {
    pub fn off() -> Self {
        Self {
            min_per_second: 0,
            window: Duration::MAX,
        }
    }
}

/// Bytes per second, measured over the window that fell below the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("too slow: {observed} bytes/s, below the minimum of {min} bytes/s")]
pub struct TooSlow {
    pub observed: u64,
    pub min: u64,
}

/// A request body that fails with [`TooSlow`] once a window carries less
/// than the floor. It meters every byte of the body: the part headers, the
/// options part and the closing boundary as well as the file.
pub struct MeteredBody {
    inner: axum::body::Body,
    meter: Meter,
}

impl MeteredBody {
    pub fn new(inner: axum::body::Body, rate: MinRate) -> Self {
        Self {
            inner,
            meter: Meter::new(rate),
        }
    }
}

impl http_body::Body for MeteredBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, axum::Error>>> {
        let polled = std::pin::Pin::new(&mut self.inner).poll_frame(cx);
        if let std::task::Poll::Ready(Some(Ok(frame))) = &polled
            && let Some(data) = frame.data_ref()
        {
            let len = data.len() as u64;
            if let Err(slow) = self.meter.record(len) {
                return std::task::Poll::Ready(Some(Err(axum::Error::new(slow))));
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// The [`TooSlow`] in `err`'s source chain, if any.
pub fn too_slow(err: &(dyn std::error::Error + 'static)) -> Option<TooSlow> {
    let mut current = Some(err);
    while let Some(err) = current {
        if let Some(slow) = err.downcast_ref::<TooSlow>() {
            return Some(*slow);
        }
        current = err.source();
    }
    None
}

/// Counts the bytes of one transfer, window by window.
#[derive(Debug)]
pub struct Meter {
    rate: MinRate,
    window_start: Instant,
    window_bytes: u64,
}

impl Meter {
    pub fn new(rate: MinRate) -> Self {
        Self {
            rate,
            window_start: Instant::now(),
            window_bytes: 0,
        }
    }

    /// Records `bytes` that just moved. `Err` when the window that has just
    /// closed carried less than the floor.
    pub fn record(&mut self, bytes: u64) -> Result<(), TooSlow> {
        self.record_at(bytes, Instant::now())
    }

    fn record_at(&mut self, bytes: u64, now: Instant) -> Result<(), TooSlow> {
        if self.rate.min_per_second == 0 {
            return Ok(());
        }
        self.window_bytes = self.window_bytes.saturating_add(bytes);
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed < self.rate.window {
            return Ok(());
        }
        let seconds = elapsed.as_secs_f64();
        let observed = (self.window_bytes as f64 / seconds) as u64;
        if observed < self.rate.min_per_second {
            return Err(TooSlow {
                observed,
                min: self.rate.min_per_second,
            });
        }
        self.window_start = now;
        self.window_bytes = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: MinRate = MinRate {
        min_per_second: 1000,
        window: Duration::from_secs(10),
    };

    #[test]
    fn a_trickle_fails_after_one_window() {
        let start = Instant::now();
        let mut meter = Meter {
            rate: RATE,
            window_start: start,
            window_bytes: 0,
        };
        // One byte every 9 s: the idle timer never fires.
        assert!(meter.record_at(1, start + Duration::from_secs(9)).is_ok());
        let err = meter
            .record_at(1, start + Duration::from_secs(18))
            .unwrap_err();
        assert_eq!(err.min, 1000);
        assert_eq!(err.observed, 0);
    }

    #[test]
    fn a_steady_transfer_passes_window_after_window() {
        let start = Instant::now();
        let mut meter = Meter {
            rate: RATE,
            window_start: start,
            window_bytes: 0,
        };
        for second in 1..=60 {
            assert!(
                meter
                    .record_at(1500, start + Duration::from_secs(second))
                    .is_ok()
            );
        }
        // A fast start does not buy a slow finish: each window stands alone.
        assert!(meter.record_at(1, start + Duration::from_secs(75)).is_err());
    }

    #[test]
    fn zero_turns_it_off() {
        let start = Instant::now();
        let mut meter = Meter {
            rate: MinRate::off(),
            window_start: start,
            window_bytes: 0,
        };
        assert!(
            meter
                .record_at(1, start + Duration::from_secs(3600))
                .is_ok()
        );
    }
}
