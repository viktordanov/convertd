//! DTOs and their transforms: the only serde types of the service. Their
//! names are the contract's, as committed in `api/openapi.json`, so a
//! rename here is an API change (AGENT-RULES rule 11: every `Option`
//! has `nullable = false`, binary fields are `String`/`Binary`).

pub mod build_info;
pub mod capabilities;
pub mod errors;
pub mod manifest;
pub mod options;
pub mod probes;
