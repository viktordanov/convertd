//! convertd: the document conversion service. `src/README.md` describes
//! the modules and the process model.

pub mod bundle;
pub mod config;
pub mod detect;
pub mod domain;
pub mod engines;
pub mod ipc;
pub mod jobs;
pub mod logging;
pub mod observability;
pub mod preflight;
pub mod rest;
pub mod runtime;
pub mod worker;
