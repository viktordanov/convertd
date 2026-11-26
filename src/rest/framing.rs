//! A bound on the multipart framing, ahead of multer.
//!
//! multer buffers the bytes before the first boundary until it finds the
//! boundary, and a part's headers until it finds the blank line after
//! them. Its buffer grows without a limit, and it rescans the whole buffer
//! on every read. So a request that never sends either would grow the
//! buffer toward the upload cap and cost quadratic time on a runtime
//! thread. [`FramingBody`] follows the same framing as the bytes pass and
//! fails the body once the preamble or one part's header block passes
//! [`FRAMING_LIMIT`]. Part data is not bounded here: the size limits cover
//! it.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use bytes::Bytes;
use http_body::{Frame, SizeHint};
use memchr::memmem::Finder;

/// The most bytes before the first boundary, and in one part's header
/// block (from the end of its boundary to the blank line).
pub const FRAMING_LIMIT: u64 = 16 * 1024;

/// The most bytes one step of [`Framing::feed`] looks at; a larger chunk
/// is taken in windows of this size.
const WINDOW: usize = 16 * 1024;

/// The framing passed [`FRAMING_LIMIT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the multipart {0} exceed {FRAMING_LIMIT} bytes")]
pub struct FramingTooLarge(pub &'static str);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before the first boundary.
    Preamble,
    /// Just after a boundary: `--` closes the body, anything else starts
    /// a header block.
    AfterBoundary,
    /// Inside a part's header block.
    Headers,
    /// Inside a part's data.
    Data,
    /// After the closing boundary: nothing is watched any more.
    Done,
}

/// Follows the multipart framing of a stream fed in chunks.
#[derive(Debug)]
pub struct Framing {
    state: State,
    /// `--boundary`, which opens the body (with or without a CRLF before).
    first: Finder<'static>,
    /// `\r\n--boundary`, which ends a part's data.
    delimiter: Finder<'static>,
    headers_end: Finder<'static>,
    /// The last bytes of the previous chunk, so that a pattern split over
    /// two chunks is found.
    tail: Vec<u8>,
    /// The stream offset of `tail[0]`.
    tail_at: u64,
    /// The stream offset where the bounded state began.
    state_at: u64,
}

impl Framing {
    pub fn new(boundary: &str) -> Self {
        Self {
            state: State::Preamble,
            first: Finder::new(format!("--{boundary}").as_bytes()).into_owned(),
            delimiter: Finder::new(format!("\r\n--{boundary}").as_bytes()).into_owned(),
            headers_end: Finder::new(b"\r\n\r\n").into_owned(),
            tail: Vec::new(),
            tail_at: 0,
            state_at: 0,
        }
    }

    fn keep(&self) -> usize {
        self.delimiter.needle().len().max(4) - 1
    }

    /// Whether the delimiter may be in `chunk` or across its seam with the
    /// kept tail.
    fn delimiter_near(&self, chunk: &[u8]) -> bool {
        if self.delimiter.find(chunk).is_some() {
            return true;
        }
        let mut seam = self.tail.clone();
        seam.extend_from_slice(&chunk[..self.keep().min(chunk.len())]);
        self.delimiter.find(&seam).is_some()
    }

    /// Takes the next bytes of the stream, in windows of [`WINDOW`] bytes,
    /// so that no step copies more than a window.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), FramingTooLarge> {
        for window in chunk.chunks(WINDOW) {
            self.feed_window(window)?;
        }
        Ok(())
    }

    fn feed_window(&mut self, chunk: &[u8]) -> Result<(), FramingTooLarge> {
        if self.state == State::Done || chunk.is_empty() {
            return Ok(());
        }
        // Part data is nearly all of a stream. When no delimiter is in this
        // chunk, nor across the seam with the last one, only the new tail
        // is kept: no copy of the chunk.
        if self.state == State::Data && !self.delimiter_near(chunk) {
            let keep = self.keep();
            if chunk.len() >= keep {
                self.tail_at += (self.tail.len() + chunk.len() - keep) as u64;
                self.tail.clear();
                self.tail.extend_from_slice(&chunk[chunk.len() - keep..]);
            } else {
                // A short chunk: the tail is the last bytes of both.
                self.tail.extend_from_slice(chunk);
                let drop = self.tail.len().saturating_sub(keep);
                self.tail.drain(..drop);
                self.tail_at += drop as u64;
            }
            return Ok(());
        }
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(chunk);
        let base = self.tail_at;
        let end = base + buf.len() as u64;
        let mut pos = 0usize;
        loop {
            match self.state {
                State::Preamble => match self.first.find(&buf[pos..]) {
                    Some(at) => {
                        pos += at + self.first.needle().len();
                        self.state = State::AfterBoundary;
                    }
                    None => {
                        if end - self.state_at > FRAMING_LIMIT {
                            return Err(FramingTooLarge("bytes before the first boundary"));
                        }
                        break;
                    }
                },
                State::AfterBoundary => {
                    if buf.len() - pos < 2 {
                        break;
                    }
                    if &buf[pos..pos + 2] == b"--" {
                        self.state = State::Done;
                        break;
                    }
                    self.state = State::Headers;
                    self.state_at = base + pos as u64;
                }
                State::Headers => match self.headers_end.find(&buf[pos..]) {
                    Some(at) => {
                        pos += at + 4;
                        self.state = State::Data;
                    }
                    None => {
                        if end - self.state_at > FRAMING_LIMIT {
                            return Err(FramingTooLarge("part headers"));
                        }
                        break;
                    }
                },
                State::Data => match self.delimiter.find(&buf[pos..]) {
                    Some(at) => {
                        pos += at + self.delimiter.needle().len();
                        self.state = State::AfterBoundary;
                    }
                    None => break,
                },
                State::Done => break,
            }
        }
        // Keep what a split pattern may need, but nothing already matched.
        let from = pos.max(buf.len().saturating_sub(self.keep()));
        self.tail_at = base + from as u64;
        buf.drain(..from);
        self.tail = buf;
        Ok(())
    }
}

/// A request body that fails with [`FramingTooLarge`] as soon as the
/// framing passes its bound.
pub struct FramingBody {
    inner: Body,
    framing: Framing,
}

impl FramingBody {
    pub fn new(inner: Body, boundary: &str) -> Self {
        Self {
            inner,
            framing: Framing::new(boundary),
        }
    }
}

impl http_body::Body for FramingBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled
            && let Some(data) = frame.data_ref()
            && let Err(err) = self.framing.feed(data)
        {
            return Poll::Ready(Some(Err(axum::Error::new(err))));
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The [`FramingTooLarge`] in `err`'s source chain, if any.
pub fn framing_too_large(err: &(dyn std::error::Error + 'static)) -> Option<FramingTooLarge> {
    let mut current = Some(err);
    while let Some(err) = current {
        if let Some(found) = err.downcast_ref::<FramingTooLarge>() {
            return Some(*found);
        }
        current = err.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: &str = "bnd";

    /// Feeds `stream` in chunks of `size`.
    fn feed(stream: &[u8], size: usize) -> Result<State, FramingTooLarge> {
        let mut framing = Framing::new(B);
        for chunk in stream.chunks(size.max(1)) {
            framing.feed(chunk)?;
        }
        Ok(framing.state)
    }

    fn body(data_len: usize) -> Vec<u8> {
        let mut out = b"--bnd\r\nContent-Disposition: form-data; name=\"options\"\r\n\r\n{}\r\n--bnd\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n".to_vec();
        out.extend(std::iter::repeat_n(b'x', data_len));
        out.extend_from_slice(b"\r\n--bnd--\r\n");
        out
    }

    #[test]
    fn a_well_formed_body_reaches_the_end_in_any_chunking() {
        let stream = body(100_000);
        for size in [1, 2, 3, 7, 64, 4096, stream.len()] {
            assert_eq!(feed(&stream, size), Ok(State::Done), "chunks of {size}");
        }
    }

    #[test]
    fn part_data_is_not_bounded() {
        assert_eq!(feed(&body(1 << 20), 65_536), Ok(State::Done));
    }

    #[test]
    fn unterminated_headers_and_preamble_are_refused() {
        let mut headers = b"--bnd\r\nX-Pad: ".to_vec();
        headers.extend(std::iter::repeat_n(b'a', 20_000));
        assert_eq!(feed(&headers, 4096), Err(FramingTooLarge("part headers")));
        // Just under the bound passes.
        let mut short = b"--bnd\r\nX-Pad: ".to_vec();
        short.extend(std::iter::repeat_n(b'a', 16_000));
        assert_eq!(feed(&short, 4096), Ok(State::Headers));

        let preamble = vec![b'p'; 20_000];
        assert_eq!(
            feed(&preamble, 4096),
            Err(FramingTooLarge("bytes before the first boundary"))
        );
    }

    #[test]
    fn a_later_part_with_endless_headers_is_refused() {
        let mut stream = body(10);
        stream.truncate(stream.len() - b"--\r\n".len());
        stream.extend_from_slice(b"\r\nX-Pad: ");
        stream.extend(std::iter::repeat_n(b'a', 20_000));
        assert_eq!(feed(&stream, 1000), Err(FramingTooLarge("part headers")));
    }
}
