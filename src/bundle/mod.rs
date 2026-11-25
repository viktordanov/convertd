//! The stored-ZIP bundle writer, bytes only (lane 001). The manifest comes
//! already serialized from `rest/`.

pub mod zip_stored;

pub use zip_stored::{PlanError, Source, ZipPlan};
