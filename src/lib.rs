//! convertd: the document conversion service.
//!
//! The module owners are fixed in `docs/ledger/INTERFACES.md` §3.

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
