//! Supervisor side of the engines: pools, queue and process control (lane 002).
//!
//! `fake.rs` and `fake_markers.rs` belong to lane 001.

pub mod cli;
pub mod config;

pub use config::EngineConfig;

// Lane 001's level-A fakes and the shared marker parser.
pub mod fake;
pub mod fake_markers;
