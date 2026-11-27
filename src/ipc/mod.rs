//! Supervisor-worker protocol and framing. Internal to the service: the
//! supervisor (`engines::conn`) and the worker are its only two ends.
//!
//! Frames are a `u32` little-endian length and a UTF-8 JSON body of at most
//! [`frame::MAX_FRAME_BYTES`]. Only paths and options
//! cross the pipe; document bytes and page images stay in the job directory.

pub mod facts;
pub mod frame;
pub mod protocol;

pub use frame::{FrameError, MAX_FRAME_BYTES};
pub use protocol::*;
