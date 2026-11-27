//! The tar.zst bundle writer, bytes only. The manifest comes
//! already serialized from `rest/`.

pub mod tar_zst;

pub use tar_zst::{PlanError, Source, TarPlan};
