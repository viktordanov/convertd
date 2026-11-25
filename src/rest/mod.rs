//! HTTP layer (lane 001): routes, handlers, DTOs, and the only place that
//! maps errors to status codes.

pub mod capabilities_handlers;
pub mod convert_handlers;
pub mod healthcheck;
pub mod openapi;
pub mod ops_handlers;
pub mod serialization;
pub mod server;
pub mod state;
