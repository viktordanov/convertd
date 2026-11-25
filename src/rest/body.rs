//! The response pump: one blocking task per response reads the file in
//! 64 KiB chunks and sends them over `mpsc(4)` to the body hyper polls.
//!
//! The pump owns the `Arc<JobDir>` and the concurrency permit, not the
//! body: hyper may stop polling a body whose client stopped reading and
//! keep the connection open (axum::serve sets no timeouts), so anything
//! the body owned would live as long as the socket. Each send has a
//! timeout of `IO_IDLE_TIMEOUT_SECONDS`; when it fires the pump stops,
//! logs `response stalled` (`RESPONSE_STALLED`) and releases both.
//!
//! Why not `tokio::fs` + `ReaderStream`: every read would be its own
//! `spawn_blocking` hop, about 1 600 for 100 MiB against one task here.
//! Peak memory per response is about five chunks.

use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use tokio::runtime::Handle;
use tokio::sync::{OwnedSemaphorePermit, mpsc};

use crate::jobs::JobDir;

/// Bytes per chunk.
pub const READ_CHUNK: usize = 64 * 1024;
/// Chunks queued between the pump and hyper.
const BODY_QUEUE_CHUNKS: usize = 4;

/// What the pump needs besides the file.
pub struct Pump {
    pub job: Arc<JobDir>,
    pub permit: OwnedSemaphorePermit,
    pub idle: Duration,
    /// For the pump's own log lines.
    pub request_id: String,
}

/// How a pump ended, for its log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpEnd {
    Complete,
    /// The receiver is gone: the client disconnected.
    Disconnected,
    /// The client stopped reading for `idle`.
    Stalled,
    /// The file ended early or could not be read: the client sees a body
    /// shorter than `Content-Length`.
    Aborted,
}

/// Streams exactly `size` bytes of `file`.
pub fn file_body(file: File, size: u64, pump: Pump) -> Body {
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(BODY_QUEUE_CHUNKS);
    let handle = Handle::current();
    tokio::task::spawn_blocking(move || {
        let end = run(file, size, &tx, &handle, pump.idle);
        // Nothing reads the job directory any more: remove it here, on a
        // blocking thread, with no extra hop.
        pump.job.remove_now();
        drop(pump.permit);
        match end {
            PumpEnd::Complete | PumpEnd::Disconnected => {}
            PumpEnd::Stalled => {
                tracing::warn!(request_id = %pump.request_id, code = "RESPONSE_STALLED", "response stalled");
            }
            PumpEnd::Aborted => {
                tracing::warn!(request_id = %pump.request_id, code = "RESPONSE_ABORTED", "response aborted");
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Body::from_stream(stream)
}

fn run(
    mut file: File,
    size: u64,
    tx: &mpsc::Sender<io::Result<Bytes>>,
    handle: &Handle,
    idle: Duration,
) -> PumpEnd {
    let mut left = size;
    while left > 0 {
        let len = READ_CHUNK.min(usize::try_from(left).unwrap_or(READ_CHUNK));
        // A fresh allocation per chunk: queued chunks keep theirs alive.
        let mut chunk = BytesMut::zeroed(len);
        if let Err(err) = file.read_exact(&mut chunk) {
            // After the 200 a read error cannot become a status: abort the
            // body so hyper drops the connection.
            let _ = handle.block_on(tokio::time::timeout(idle, tx.send(Err(err))));
            return PumpEnd::Aborted;
        }
        match handle.block_on(tokio::time::timeout(idle, tx.send(Ok(chunk.freeze())))) {
            Ok(Ok(())) => left -= len as u64,
            Ok(Err(_)) => return PumpEnd::Disconnected,
            Err(_) => return PumpEnd::Stalled,
        }
    }
    PumpEnd::Complete
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;

    use http_body::Body as _;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::jobs::DiskBudget;

    fn job(name: &str) -> (Arc<JobDir>, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("convertd-body-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        (
            JobDir::create(&tmp, &DiskBudget::new(u64::MAX)).unwrap(),
            tmp,
        )
    }

    #[tokio::test]
    async fn streams_the_file_then_cleans_up() {
        let (job, tmp) = job("stream");
        let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let path = job.pdf_path();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        let root = job.root().to_path_buf();
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().unwrap();
        let pump = Pump {
            job,
            permit,
            idle: Duration::from_secs(5),
            request_id: "t".into(),
        };
        let body = file_body(File::open(&path).unwrap(), data.len() as u64, pump);
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        assert_eq!(&bytes[..], &data[..]);
        for _ in 0..100 {
            if !root.exists() && semaphore.available_permits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!root.exists());
        assert_eq!(semaphore.available_permits(), 1);
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[tokio::test]
    async fn a_short_file_aborts_the_body() {
        let (job, tmp) = job("short");
        let path = job.pdf_path();
        std::fs::write(&path, b"only ten b").unwrap();
        let semaphore = Arc::new(Semaphore::new(1));
        let pump = Pump {
            job,
            permit: semaphore.clone().try_acquire_owned().unwrap(),
            idle: Duration::from_secs(5),
            request_id: "t".into(),
        };
        let body = file_body(File::open(&path).unwrap(), 100, pump);
        assert!(axum::body::to_bytes(body, 1 << 20).await.is_err());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[tokio::test]
    async fn a_reader_that_stops_releases_the_permit_after_idle() {
        let (job, tmp) = job("stall");
        let path = job.pdf_path();
        std::fs::write(&path, vec![0u8; 2 * 1024 * 1024]).unwrap();
        let root = job.root().to_path_buf();
        let semaphore = Arc::new(Semaphore::new(1));
        let pump = Pump {
            job,
            permit: semaphore.clone().try_acquire_owned().unwrap(),
            idle: Duration::from_millis(200),
            request_id: "t".into(),
        };
        // Hold the body without polling it, like hyper for a stalled client.
        let body = file_body(File::open(&path).unwrap(), 2 * 1024 * 1024, pump);
        assert!(!body.is_end_stream());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(semaphore.available_permits(), 0, "still within idle");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(semaphore.available_permits(), 1);
        assert!(!root.exists());
        drop(body);
        std::fs::remove_dir_all(tmp).unwrap();
    }
}
