//! DTOs and their transforms: the only serde types of the service. Their
//! names are the contract's, as committed in `api/openapi.json`, so a
//! rename here is an API change. Every `Option` field has
//! `#[schema(nullable = false)]`, and binary fields are `String` with
//! `format = Binary`.

pub mod build_info;
pub mod capabilities;
pub mod errors;
pub mod manifest;
pub mod options;
pub mod probes;
