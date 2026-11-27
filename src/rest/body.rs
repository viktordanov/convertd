//! The response pump: one blocking task per response reads the PDF, or
//! writes the tar.zst, in 64 KiB chunks and sends them over `mpsc(4)` to
//! the body hyper polls.
//!
//! The pump owns the `Arc<JobDir>` and the concurrency permit, not the
//! body: hyper may stop polling a body whose client stopped reading and
//! keep the connection open (axum::serve sets no timeouts), so anything
//! the body owned would live as long as the socket. Each send has a
//! timeout of `IO_IDLE_TIMEOUT_SECONDS`; when it fires the pump stops,
//! logs `response stalled` (`RESPONSE_STALLED`) and releases both. A
//! client that keeps reading but below `RESPONSE_MIN_BYTES_PER_SECOND`
//! over one window ends the same way, logged as `response too slow`
//! (`RESPONSE_TOO_SLOW`).
//!
//! Why not `tokio::fs` + `ReaderStream`: every read would be its own
//! `spawn_blocking` hop, about 1 600 for 100 MiB against one task here.
//! The chunks in flight are the queued ones and the ones hyper holds while
//! it writes: 13 chunks for an 8 MiB PDF over loopback. Their buffers are
//! allocated once per response and reused (`Chunks`).

use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use bytes::Bytes;
use tokio::runtime::Handle;
use tokio::sync::{OwnedSemaphorePermit, mpsc};

use super::chunks::Chunks;
use super::throughput::{Meter, MinRate};
use crate::bundle::TarPlan;
use crate::jobs::JobDir;
use crate::observability::metrics;

/// Bytes per chunk.
pub const READ_CHUNK: usize = 64 * 1024;
/// Chunks queued between the pump and hyper.
const BODY_QUEUE_CHUNKS: usize = 4;

/// What the pump needs besides the file.
pub struct Pump {
    pub job: Arc<JobDir>,
    pub permit: OwnedSemaphorePermit,
    pub idle: Duration,
    /// The response's minimum throughput.
    pub rate: MinRate,
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
    /// The client read below the minimum throughput for one window.
    TooSlow,
    /// The file ended early or could not be read: the client sees a body
    /// shorter than `Content-Length`.
    Aborted,
}

/// Streams exactly `size` bytes of `file`.
pub fn file_body(file: File, size: u64, pump: Pump) -> Body {
    spawn_pump(pump, move |tx, handle, idle, meter| {
        run(file, size, &mut Chunks::default(), tx, handle, idle, meter)
    })
}

/// Streams the tar.zst of `plan`: one zstd frame, chunked, with no
/// length known in advance.
pub fn tar_body(plan: TarPlan, pump: Pump) -> Body {
    spawn_pump(pump, move |tx, handle, idle, meter| {
        write_tar(&plan, &mut Chunks::default(), tx, handle, idle, meter)
    })
}

fn write_tar(
    plan: &TarPlan,
    chunks: &mut Chunks,
    tx: &Sender,
    handle: &Handle,
    idle: Duration,
    meter: Meter,
) -> PumpEnd {
    let current = chunks.free(READ_CHUNK);
    let writer = ChannelWriter {
        chunks,
        current,
        tx,
        handle,
        idle,
        meter,
        end: None,
    };
    match plan
        .write_to(writer)
        .and_then(|mut writer| writer.flush().map(|()| writer))
    {
        Ok(_) => PumpEnd::Complete,
        Err(err) => {
            // A file shorter than planned or a read error, unless the
            // client stalled or left (the writer recorded that). The
            // writer moved into the encoder, so the reason comes back
            // through the error kind.
            match err.kind() {
                io::ErrorKind::TimedOut => PumpEnd::Stalled,
                io::ErrorKind::WouldBlock => PumpEnd::TooSlow,
                io::ErrorKind::BrokenPipe => PumpEnd::Disconnected,
                _ => {
                    let _ =
                        handle.block_on(tokio::time::timeout(idle, tx.send(Piece::Failed(err))));
                    PumpEnd::Aborted
                }
            }
        }
    }
}

/// What the pump sends to the body. The body ends cleanly only on `Done`:
/// a pump that stops without it (stalled, too slow, aborted, a timed-out
/// send, a panic) leaves the body with an error, so hyper drops the
/// connection instead of sending a chunked terminator, and a cut tar.zst
/// never looks complete.
enum Piece {
    Data(Bytes),
    Failed(io::Error),
    Done,
}

type Sender = mpsc::Sender<Piece>;

/// Runs `work` on one blocking thread that owns the job directory and the
/// permit, and returns the body that receives its chunks.
fn spawn_pump(
    pump: Pump,
    work: impl FnOnce(&Sender, &Handle, Duration, Meter) -> PumpEnd + Send + 'static,
) -> Body {
    let (tx, rx) = mpsc::channel::<Piece>(BODY_QUEUE_CHUNKS);
    let handle = Handle::current();
    tokio::task::spawn_blocking(move || {
        let mut end = work(&tx, &handle, pump.idle, Meter::new(pump.rate));
        if end == PumpEnd::Complete {
            // If even this send stalls or the client left, the body ends
            // with an error: say so like any other cut.
            end = match handle.block_on(tokio::time::timeout(pump.idle, tx.send(Piece::Done))) {
                Ok(Ok(())) => PumpEnd::Complete,
                Ok(Err(_)) => PumpEnd::Disconnected,
                Err(_) => PumpEnd::Stalled,
            };
        }
        drop(tx);
        // Nothing reads the job directory any more: remove it here, on a
        // blocking thread, with no extra hop.
        pump.job.remove_now();
        drop(pump.permit);
        match end {
            PumpEnd::Complete | PumpEnd::Disconnected => {}
            PumpEnd::Stalled => {
                tracing::warn!(request_id = %pump.request_id, code = "RESPONSE_STALLED", "response stalled");
                metrics::response_cut("RESPONSE_STALLED");
            }
            PumpEnd::TooSlow => {
                tracing::warn!(request_id = %pump.request_id, code = "RESPONSE_TOO_SLOW", "response too slow");
                metrics::response_cut("RESPONSE_TOO_SLOW");
            }
            PumpEnd::Aborted => {
                tracing::warn!(request_id = %pump.request_id, code = "RESPONSE_ABORTED", "response aborted");
                metrics::response_cut("RESPONSE_ABORTED");
            }
        }
    });
    let stream = futures_util::stream::unfold(Some(rx), |rx| async move {
        let mut rx = rx?;
        match rx.recv().await {
            Some(Piece::Data(chunk)) => Some((Ok(chunk), Some(rx))),
            Some(Piece::Failed(err)) => Some((Err(err), None)),
            Some(Piece::Done) => None,
            None => Some((
                Err(io::Error::other("the response was cut before its end")),
                None,
            )),
        }
    });
    Body::from_stream(stream)
}

/// A `Write` that sends 64 KiB chunks to the body, each under the idle
/// timeout. A stalled or vanished client makes `write` fail and records
/// why in `end`.
struct ChannelWriter<'a> {
    chunks: &'a mut Chunks,
    /// The slot that collects the next chunk.
    current: usize,
    tx: &'a Sender,
    handle: &'a Handle,
    idle: Duration,
    meter: Meter,
    end: Option<PumpEnd>,
}

impl ChannelWriter<'_> {
    /// Sends the current slot's bytes as one chunk and moves to a free slot.
    fn send(&mut self) -> io::Result<()> {
        let chunk = self.chunks.slots[self.current].split().freeze();
        let bytes = chunk.len() as u64;
        // The sent chunk keeps its slot busy until hyper drops it.
        self.current = self.chunks.free(READ_CHUNK);
        match self.handle.block_on(tokio::time::timeout(
            self.idle,
            self.tx.send(Piece::Data(chunk)),
        )) {
            Ok(Ok(())) => {
                if self.meter.record(bytes).is_err() {
                    // The reason travels as the error kind: the writer
                    // itself is inside the encoder.
                    self.end = Some(PumpEnd::TooSlow);
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                Ok(())
            }
            Ok(Err(_)) => {
                self.end = Some(PumpEnd::Disconnected);
                Err(io::ErrorKind::BrokenPipe.into())
            }
            Err(_) => {
                self.end = Some(PumpEnd::Stalled);
                Err(io::ErrorKind::TimedOut.into())
            }
        }
    }
}

impl Write for ChannelWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.end.is_some() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let mut rest = data;
        while !rest.is_empty() {
            let buf = &mut self.chunks.slots[self.current];
            let n = (READ_CHUNK - buf.len()).min(rest.len());
            buf.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if buf.len() == READ_CHUNK {
                self.send()?;
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.chunks.slots[self.current].is_empty() {
            self.send()?;
        }
        Ok(())
    }
}

fn run(
    mut file: File,
    size: u64,
    chunks: &mut Chunks,
    tx: &Sender,
    handle: &Handle,
    idle: Duration,
    mut meter: Meter,
) -> PumpEnd {
    let mut left = size;
    while left > 0 {
        let len = READ_CHUNK.min(usize::try_from(left).unwrap_or(READ_CHUNK));
        let slot = chunks.free(len);
        let buf = &mut chunks.slots[slot];
        buf.resize(len, 0);
        if let Err(err) = file.read_exact(buf) {
            // After the 200 a read error cannot become a status: abort the
            // body so hyper drops the connection.
            let _ = handle.block_on(tokio::time::timeout(idle, tx.send(Piece::Failed(err))));
            return PumpEnd::Aborted;
        }
        match handle.block_on(tokio::time::timeout(
            idle,
            tx.send(Piece::Data(buf.split().freeze())),
        )) {
            Ok(Ok(())) => {
                left -= len as u64;
                if meter.record(len as u64).is_err() {
                    return PumpEnd::TooSlow;
                }
            }
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
    use crate::bundle::TarPlan;
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

    /// Waits up to 2 s for the pump to remove the job directory and give
    /// back the permit. The pump does both on its own blocking thread.
    async fn wait_for_cleanup(root: &std::path::Path, semaphore: &Semaphore) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while root.exists() || semaphore.available_permits() != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "the pump kept the job directory or the permit"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
            rate: MinRate::off(),
            request_id: "t".into(),
        };
        let body = file_body(File::open(&path).unwrap(), data.len() as u64, pump);
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        assert_eq!(&bytes[..], &data[..]);
        wait_for_cleanup(&root, &semaphore).await;
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[tokio::test]
    async fn a_short_file_aborts_the_body() {
        let (job, tmp) = job("short");
        let path = job.pdf_path();
        std::fs::write(&path, b"only ten b").unwrap();
        let root = job.root().to_path_buf();
        let semaphore = Arc::new(Semaphore::new(1));
        let pump = Pump {
            job,
            permit: semaphore.clone().try_acquire_owned().unwrap(),
            idle: Duration::from_secs(5),
            rate: MinRate::off(),
            request_id: "t".into(),
        };
        let body = file_body(File::open(&path).unwrap(), 100, pump);
        assert!(axum::body::to_bytes(body, 1 << 20).await.is_err());
        // The body ends before the pump has removed the job directory:
        // removing `tmp` now would race it and fail with NotFound.
        wait_for_cleanup(&root, &semaphore).await;
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
            rate: MinRate::off(),
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

    /// Bytes zstd cannot shrink, so the compressed stream stays large.
    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[tokio::test]
    async fn streams_a_tar_zst_then_cleans_up() {
        let (job, tmp) = job("tar");
        let data = noise(300_000);
        std::fs::write(job.pdf_path(), &data).unwrap();
        let mut plan = TarPlan::new();
        plan.add_bytes("manifest.json", Bytes::from_static(b"{}"))
            .unwrap();
        plan.add_file("document.pdf", job.pdf_path()).unwrap();
        let root = job.root().to_path_buf();
        let semaphore = Arc::new(Semaphore::new(1));
        let pump = Pump {
            job,
            permit: semaphore.clone().try_acquire_owned().unwrap(),
            idle: Duration::from_secs(5),
            rate: MinRate::off(),
            request_id: "t".into(),
        };
        let body = tar_body(plan, pump);
        let bytes = axum::body::to_bytes(body, 8 << 20).await.unwrap();
        let tar = zstd::stream::decode_all(&bytes[..]).unwrap();
        // manifest.json: header, 2 bytes, padding; then document.pdf.
        assert_eq!(&tar[512..514], b"{}");
        assert_eq!(&tar[1024 + 512..1024 + 512 + data.len()], &data[..]);
        wait_for_cleanup(&root, &semaphore).await;
        std::fs::remove_dir_all(tmp).unwrap();
    }

    /// A body that fits the queue exactly: every chunk is sent, but the
    /// completion marker cannot be, because nobody reads. The pump gives up
    /// after `idle`, logs the cut as a stall, frees the permit, and the
    /// body ends with an error.
    #[tokio::test]
    async fn a_completion_marker_that_cannot_be_sent_is_a_cut() {
        let (job, tmp) = job("done-stall");
        std::fs::write(job.pdf_path(), vec![7u8; BODY_QUEUE_CHUNKS * READ_CHUNK]).unwrap();
        let file = File::open(job.pdf_path()).unwrap();
        let size = (BODY_QUEUE_CHUNKS * READ_CHUNK) as u64;
        let root = job.root().to_path_buf();
        let semaphore = Arc::new(Semaphore::new(1));
        let pump = Pump {
            job,
            permit: semaphore.clone().try_acquire_owned().unwrap(),
            idle: Duration::from_millis(200),
            rate: MinRate::off(),
            request_id: "t".into(),
        };
        let body = file_body(file, size, pump);
        wait_for_cleanup(&root, &semaphore).await;
        let read = axum::body::to_bytes(body, 64 << 20).await;
        assert!(read.is_err(), "the body ended cleanly without its marker");
        std::fs::remove_dir_all(tmp).unwrap();
    }

    /// Reads `body` to its end, dropping each chunk at once as hyper does
    /// after writing it, and returns the bytes read.
    async fn drain(mut body: Body) -> usize {
        let mut read = 0;
        while let Some(frame) =
            std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await
        {
            if let Ok(data) = frame.unwrap().into_data() {
                read += data.len();
            }
        }
        read
    }

    /// A pump whose buffers the test can count afterwards: slots are only
    /// ever added, so their count is the number of buffers allocated.
    fn counted(
        job: Arc<JobDir>,
        semaphore: &Arc<Semaphore>,
        work: impl FnOnce(&mut Chunks, &Sender, &Handle, Duration, Meter) -> PumpEnd + Send + 'static,
    ) -> (Body, Arc<std::sync::atomic::AtomicUsize>) {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let pump = Pump {
            job,
            permit: Arc::clone(semaphore).try_acquire_owned().unwrap(),
            idle: Duration::from_secs(5),
            rate: MinRate::off(),
            request_id: "t".into(),
        };
        let body = spawn_pump(pump, move |tx, handle, idle, meter| {
            let mut chunks = Chunks::default();
            let end = work(&mut chunks, tx, handle, idle, meter);
            seen.store(chunks.slots.len(), std::sync::atomic::Ordering::SeqCst);
            end
        });
        (body, count)
    }

    /// The pumps reuse their buffers: 64 chunks come from a handful of
    /// them. The count does not depend on the allocator handing a freed
    /// address back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pumps_reuse_a_few_buffers() {
        const CHUNKS: usize = 64;
        let data = noise(CHUNKS * READ_CHUNK);
        let semaphore = Arc::new(Semaphore::new(1));
        let mut dirs = Vec::new();
        let mut buffers = Vec::new();
        for (name, len) in [("reuse-pdf", data.len())] {
            let (job, tmp) = job(name);
            std::fs::write(job.pdf_path(), &data[..len]).unwrap();
            let file = File::open(job.pdf_path()).unwrap();
            let root = job.root().to_path_buf();
            let (body, count) = counted(job, &semaphore, move |chunks, tx, h, idle, m| {
                run(file, len as u64, chunks, tx, h, idle, m)
            });
            assert_eq!(drain(body).await, len);
            wait_for_cleanup(&root, &semaphore).await;
            buffers.push(count.load(std::sync::atomic::Ordering::SeqCst));
            dirs.push(tmp);
        }
        let (job, tmp) = job("reuse-tar");
        std::fs::write(job.pdf_path(), &data).unwrap();
        let mut plan = TarPlan::new();
        plan.add_file("document.pdf", job.pdf_path()).unwrap();
        let root = job.root().to_path_buf();
        let (body, count) = counted(job, &semaphore, move |chunks, tx, h, idle, m| {
            write_tar(&plan, chunks, tx, h, idle, m)
        });
        assert!(drain(body).await > data.len());
        wait_for_cleanup(&root, &semaphore).await;
        buffers.push(count.load(std::sync::atomic::Ordering::SeqCst));
        dirs.push(tmp);
        eprintln!("buffers for {CHUNKS} chunks: pdf, tar.zst {buffers:?}");
        // Queued chunks, the one the reader holds, and the one being
        // filled; the tar writer also holds the chunk it is sending.
        assert!(buffers[0] <= BODY_QUEUE_CHUNKS + 2, "pdf: {}", buffers[0]);
        assert!(
            buffers[1] <= BODY_QUEUE_CHUNKS + 3,
            "tar.zst: {}",
            buffers[1]
        );
        for dir in dirs {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn a_stalled_tar_zst_releases_the_permit_after_idle() {
        let (job, tmp) = job("tar-stall");
        std::fs::write(job.pdf_path(), noise(2 * 1024 * 1024)).unwrap();
        let mut plan = TarPlan::new();
        plan.add_file("document.pdf", job.pdf_path()).unwrap();
        let root = job.root().to_path_buf();
        let semaphore = Arc::new(Semaphore::new(1));
        let pump = Pump {
            job,
            permit: semaphore.clone().try_acquire_owned().unwrap(),
            idle: Duration::from_millis(200),
            rate: MinRate::off(),
            request_id: "t".into(),
        };
        let body = tar_body(plan, pump);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(semaphore.available_permits(), 0, "still within idle");
        wait_for_cleanup(&root, &semaphore).await;
        // The pump gave up: the body must end with an error, so hyper drops
        // the connection instead of sending the chunked terminator that
        // would make a cut tar.zst look complete.
        let read = axum::body::to_bytes(body, 64 << 20).await;
        assert!(read.is_err(), "a cut body ended cleanly");
        std::fs::remove_dir_all(tmp).unwrap();
    }
}
