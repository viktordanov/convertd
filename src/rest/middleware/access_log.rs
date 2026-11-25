//! One `http request` event per request, emitted when the response body
//! ends or is dropped: the only way to time a streamed response to its last
//! byte. Probes and `/metrics` are not logged.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use http_body::{Frame, SizeHint};

use super::request_id::RequestId;
use crate::rest::response::ErrorDetail;

/// Paths that are never logged.
const QUIET: &[&str] = &["/healthz", "/readyz", "/metrics"];

pub async fn access_log(request: Request, next: Next) -> Response {
    if QUIET.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    let started = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.as_str().to_owned())
        .unwrap_or_default();
    let response = next.run(request).await;
    let (parts, body) = response.into_parts();
    let complete = http_body::Body::is_end_stream(&body);
    // hyper stops polling after Content-Length bytes, so a full body may
    // never report its end.
    let expected = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .or_else(|| http_body::Body::size_hint(&body).exact());
    let guard = LogGuard {
        started,
        method,
        path,
        request_id,
        status: parts.status,
        detail: parts.extensions.get::<ErrorDetail>().map(|d| d.0.clone()),
        bytes: 0,
        expected,
        complete,
    };
    Response::from_parts(parts, Body::new(LoggedBody { inner: body, guard }))
}

/// Logs on drop: after the last frame, or when hyper drops the body of a
/// client that went away.
struct LogGuard {
    started: Instant,
    method: Method,
    path: String,
    request_id: String,
    status: StatusCode,
    detail: Option<String>,
    bytes: u64,
    expected: Option<u64>,
    complete: bool,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let duration_us = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let complete = self.complete || self.expected == Some(self.bytes);
        tracing::info!(
            method = %self.method,
            path = %self.path,
            status_code = self.status.as_u16(),
            duration_us,
            bytes = self.bytes,
            complete,
            request_id = %self.request_id,
            detail = self.detail.as_deref(),
            "http request"
        );
    }
}

struct LoggedBody {
    inner: Body,
    guard: LogGuard,
}

impl http_body::Body for LoggedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.guard.bytes += data.len() as u64;
                }
            }
            Poll::Ready(None) => self.guard.complete = true,
            _ => {}
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
