//! Supervisor-worker protocol and framing (lane 002). Internal; no other lane depends on it.
//!
//! Frames are a `u32` little-endian length and a UTF-8 JSON body of at most
//! [`frame::MAX_FRAME_BYTES`] (INTERFACES.md §7). Only paths and options
//! cross the pipe; document bytes and page images stay in the job directory.

pub mod facts;
pub mod frame;
pub mod protocol;

pub use frame::{FrameError, MAX_FRAME_BYTES};
pub use protocol::*;
