//! HTTP layer (lane 001): routes, handlers, DTOs, and the only place that
//! maps errors to status codes.

pub mod healthcheck;
pub mod ops_handlers;
pub mod serialization;
pub mod server;
