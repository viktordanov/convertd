//! HTTP layer: routes, handlers, DTOs, and the only place that
//! maps errors to status codes.

pub mod body;
pub mod capabilities_handlers;
mod chunks;
pub mod convert_handlers;
pub mod framing;
pub mod healthcheck;
pub mod middleware;
pub mod openapi;
pub mod ops_handlers;
pub mod response;
pub mod serialization;
pub mod server;
pub mod state;
pub mod throughput;
pub mod timing;
pub mod upload;
