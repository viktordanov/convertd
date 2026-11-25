//! The supervisor's end of a worker's protocol pipes.

use std::io;
use std::os::fd::OwnedFd;

use tokio::net::unix::pipe;

use crate::ipc::frame::{read_frame_async, write_frame_async};
use crate::ipc::{FrameError, Request, Response};

/// Frames to and from one worker. Not cancel safe: a dropped `send` or
/// `recv` leaves the stream out of step, so the caller kills the worker.
pub struct Conn {
    tx: pipe::Sender,
    rx: pipe::Receiver,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
}

impl Conn {
    /// Wraps the worker's stdin (we write) and stdout (we read). Must run
    /// inside a tokio runtime.
    pub fn new(stdin: OwnedFd, stdout: OwnedFd) -> io::Result<Self> {
        Ok(Self {
            tx: pipe::Sender::from_owned_fd(stdin)?,
            rx: pipe::Receiver::from_owned_fd(stdout)?,
            rbuf: Vec::with_capacity(4096),
            wbuf: Vec::with_capacity(4096),
        })
    }

    pub async fn send(&mut self, req: &Request) -> Result<(), FrameError> {
        write_frame_async(&mut self.tx, req, &mut self.wbuf).await
    }

    /// The next frame; `None` when the worker closed its end.
    pub async fn recv(&mut self) -> Result<Option<Response>, FrameError> {
        read_frame_async(&mut self.rx, &mut self.rbuf).await
    }
}
