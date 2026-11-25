//! Supervisor side of the engines: pools, queue and process control (lane 002).
//!
//! `fake.rs` and `fake_markers.rs` belong to lane 001.

pub mod cli;
pub mod config;
pub mod conn;
pub mod process;
pub mod slot;

pub use config::EngineConfig;
