//! Supervisor side of the engines: pools, queue and process control.
//!
//! `fake.rs` and `fake_markers.rs` hold the fake engine.

pub mod call;
pub mod config;
pub mod conn;
pub mod error;
pub mod office_lok;
pub mod pdf_pdfium;
pub mod poison;
pub mod pool;
pub mod process;
pub mod select;
pub mod slot;

pub use config::EngineConfig;

// The fake engine and the shared marker parser.
pub mod fake;
pub mod fake_markers;
