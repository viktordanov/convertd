//! Lingering close: after an early answer, read and discard the rest of
//! the request body for a bounded time before the connection closes.
//!
//! When the server answers while the client is still uploading (401 from
//! auth, 413 over the cap, 415, 429, 503), hyper drops the unread body and
//! closes the socket. Unread bytes in the receive buffer make the kernel
//! send a reset, and the client often sees its write fail before it reads
//! the answer: Go's `http.Transport` then reports "broken pipe" (contract
//! case 19). With this layer the dropped body moves to a task that keeps
//! reading it, so the answer goes out first and the client can finish its
//! write and read it.
//!
//! The task holds no request permit and no job directory. It stops at
//! whichever comes first: the end of the body, [`LINGER_BYTES`] read,
//! [`LINGER_TIME`], [`LINGER_IDLE`] without data, or a read error. At most
//! [`MAX_LINGERING`] bodies drain at once; beyond that a body is dropped as
//! before.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use futures_util::StreamExt;
use http_body::{Frame, SizeHint};
use tokio::sync::Semaphore;

/// The most bytes read and discarded after an early answer.
pub const LINGER_BYTES: u64 = 256 << 20;
/// The longest a body is drained after an early answer.
pub const LINGER_TIME: Duration = Duration::from_secs(10);
/// The longest wait for the next chunk while draining.
pub const LINGER_IDLE: Duration = Duration::from_secs(2);
/// The most bodies drained at the same time.
pub const MAX_LINGERING: usize = 32;

static LINGERING: Semaphore = Semaphore::const_new(MAX_LINGERING);

/// Outermost layer: every request body becomes a [`LingeringBody`].
pub async fn linger(request: Request) -> Request {
    request.map(|body| Body::new(LingeringBody::new(body)))
}

/// A request body that, if dropped before its end, drains the rest in a
/// task of its own.
pub struct LingeringBody {
    inner: Option<Body>,
    ended: bool,
}

impl LingeringBody {
    pub fn new(inner: Body) -> Self {
        Self {
            inner: Some(inner),
            ended: false,
        }
    }
}

impl http_body::Body for LingeringBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        let polled = Pin::new(inner).poll_frame(cx);
        // An error ends the body too: there is nothing left to drain.
        if matches!(polled, Poll::Ready(None | Some(Err(_)))) {
            this.ended = true;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.ended || self.inner.as_ref().is_none_or(|b| b.is_end_stream())
    }

    fn size_hint(&self) -> SizeHint {
        self.inner
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), http_body::Body::size_hint)
    }
}

impl Drop for LingeringBody {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        let Some(body) = self.inner.take() else {
            return;
        };
        if http_body::Body::is_end_stream(&body) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Ok(permit) = LINGERING.try_acquire() else {
            tracing::debug!("too many bodies draining; closing without lingering");
            return;
        };
        handle.spawn(async move {
            let drained = drain(body).await;
            drop(permit);
            tracing::debug!(
                bytes = drained.bytes,
                complete = drained.complete,
                "drained the rest of an unread request body"
            );
        });
    }
}

struct Drained {
    bytes: u64,
    complete: bool,
}

async fn drain(body: Body) -> Drained {
    let deadline = tokio::time::Instant::now() + LINGER_TIME;
    let mut stream = body.into_data_stream();
    let mut bytes = 0u64;
    loop {
        let idle = tokio::time::Instant::now() + LINGER_IDLE;
        match tokio::time::timeout_at(idle.min(deadline), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                bytes = bytes.saturating_add(chunk.len() as u64);
                if bytes >= LINGER_BYTES {
                    return Drained {
                        bytes,
                        complete: false,
                    };
                }
            }
            Ok(None) => {
                return Drained {
                    bytes,
                    complete: true,
                };
            }
            Ok(Some(Err(_))) | Err(_) => {
                return Drained {
                    bytes,
                    complete: false,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A body of `chunks` chunks of 1 KiB that counts what was read.
    fn counted(chunks: usize, read: Arc<AtomicU64>) -> Body {
        let stream = futures_util::stream::iter((0..chunks).map(move |_| {
            read.fetch_add(1024, Ordering::Relaxed);
            Ok::<_, std::io::Error>(Bytes::from(vec![0u8; 1024]))
        }));
        Body::from_stream(stream)
    }

    #[tokio::test]
    async fn a_dropped_body_is_drained_to_its_end() {
        let read = Arc::new(AtomicU64::new(0));
        let body = LingeringBody::new(counted(100, read.clone()));
        drop(body);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while read.load(Ordering::Relaxed) < 100 * 1024 {
            assert!(tokio::time::Instant::now() < deadline, "not drained");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn a_body_read_to_its_end_is_left_alone() {
        let read = Arc::new(AtomicU64::new(0));
        let body = Body::new(LingeringBody::new(counted(3, read.clone())));
        let all = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        assert_eq!(all.len(), 3 * 1024);
        assert_eq!(read.load(Ordering::Relaxed), 3 * 1024);
    }
}
