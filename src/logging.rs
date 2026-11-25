//! Process-wide logging (lane 001): `LOG_MODE=prod` writes JSON lines,
//! `debug` writes pretty text, both to stdout.

use tracing::level_filters::LevelFilter;

use crate::config::LogMode;

/// The global subscriber could not be installed.
#[derive(Debug, thiserror::Error)]
#[error("failed to install the log subscriber: {0}")]
pub struct LoggingError(String);

/// Installs the global subscriber. Call once, from `serve` only: workers
/// never log through tracing.
pub fn init(mode: LogMode) -> Result<(), LoggingError> {
    let builder = tracing_subscriber::fmt().with_writer(std::io::stdout);
    let result = match mode {
        LogMode::Prod => builder
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .with_max_level(LevelFilter::INFO)
            .try_init(),
        LogMode::Debug => builder
            .pretty()
            .with_max_level(LevelFilter::DEBUG)
            .try_init(),
    };
    result.map_err(|err| LoggingError(err.to_string()))
}
