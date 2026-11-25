//! The tar.zst bundle writer, bytes only (lane 001). The manifest comes
//! already serialized from `rest/`.

pub mod tar_zst;

pub use tar_zst::{PlanError, Source, TarPlan};
