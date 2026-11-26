//! Memory per request does not grow with the file size (plan 001,
//! Verification 3). Its own test binary, so the counting allocator below
//! sees only this process: the in-process server with the level-A fakes
//! and a raw TCP client that allocates nothing per byte.
//!
//! The tests in this file share the allocator's counters, so they take
//! one lock and run one at a time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use convertd::config::{Config, Env};
use convertd::engines::fake::{FakeBehavior, FakeOffice, FakePdf, FakeReadiness};
use convertd::rest::server::router;
use convertd::rest::state::{AppState, Engines};

/// Counts live bytes, their peak and the number of allocations.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn grew(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
}

// SAFETY: every call is forwarded to the system allocator unchanged; the
// counters only observe the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `layout` is passed on.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator with this `layout`.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `dealloc`, plus the caller's `new_size` contract.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One measurement at a time: the counters are process-wide.
static SERIAL: Mutex<()> = Mutex::new(());

const KEY: &str = "peak-key";
const BOUNDARY: &str = "peak-heap-boundary";
const MIB: usize = 1 << 20;

/// The server on its own runtime, with the fakes at engine time 0.
struct Server {
    runtime: tokio::runtime::Runtime,
    addr: SocketAddr,
}

fn server(name: &str) -> Server {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("peak_heap-{name}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let tmp_text = tmp.to_str().unwrap().to_owned();
    let pairs = vec![
        ("API_KEY", KEY),
        ("TMP_DIR", tmp_text.as_str()),
        ("BUILD_INFO_PATH", "/nonexistent/build-info.json"),
        ("MAX_UPLOAD_BYTES", "125829120"),
    ];
    let config = Config::from_env(&Env::from_pairs(pairs, 2)).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let addr = runtime.block_on(async {
        let state = AppState::new(
            &config,
            Engines {
                office: Arc::new(FakeOffice::new(FakeBehavior::Succeed)),
                pdf: Arc::new(FakePdf::new(FakeBehavior::Succeed)),
                readiness: Arc::new(FakeReadiness::new(true)),
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(state)).await });
        addr
    });
    Server { runtime, addr }
}

/// The parts of one PDF upload that are not the streamed file bytes,
/// built before the measurement starts.
struct Upload {
    head: Vec<u8>,
    prefix: Vec<u8>,
    suffix: Vec<u8>,
    size: usize,
}

fn upload(size: usize) -> Upload {
    let prefix = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\n\r\n%PDF-1.4\n"
    )
    .into_bytes();
    let suffix = format!("\r\n--{BOUNDARY}--\r\n").into_bytes();
    let length = prefix.len() + (size - 9) + suffix.len();
    let head = format!(
        "POST /v1/convert HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {KEY}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {length}\r\n\r\n"
    )
    .into_bytes();
    Upload {
        head,
        prefix,
        suffix,
        size,
    }
}

/// One request on `stream`: the file streamed from one reused 64 KiB
/// buffer, the answer read into fixed buffers. Returns the status and the
/// body length. Allocates nothing per byte.
fn send(stream: &mut TcpStream, upload: &Upload, chunk: &mut [u8; 64 * 1024]) -> (u16, usize) {
    stream.write_all(&upload.head).unwrap();
    stream.write_all(&upload.prefix).unwrap();
    let mut left = upload.size - 9;
    chunk.fill(b'x');
    while left > 0 {
        let n = left.min(chunk.len());
        stream.write_all(&chunk[..n]).unwrap();
        left -= n;
    }
    stream.write_all(&upload.suffix).unwrap();

    let mut head = [0u8; 4096];
    let mut used = 0;
    while !head[..used].ends_with(b"\r\n\r\n") {
        assert!(used < head.len(), "response head too long");
        let n = stream.read(&mut head[used..used + 1]).unwrap();
        assert_eq!(n, 1, "closed before the head");
        used += 1;
    }
    let text = std::str::from_utf8(&head[..used]).unwrap();
    let status: u16 = text.split(' ').nth(1).unwrap().parse().unwrap();
    let length: usize = text
        .split("\r\n")
        .find_map(|line| line.strip_prefix("content-length: "))
        .map_or(0, |value| value.parse().unwrap());
    let mut read = 0;
    while read < length {
        let n = stream.read(&mut chunk[..]).unwrap();
        assert!(n > 0, "closed after {read} of {length} bytes");
        read += n;
    }
    (status, read)
}

/// Waits for the server's background work (job removal) to end.
fn settle() {
    std::thread::sleep(Duration::from_millis(300));
}

/// Peak heap above the baseline for one upload of `size` bytes.
fn peak_for(server: &Server, size: usize, chunk: &mut [u8; 64 * 1024]) -> usize {
    let upload = upload(size);
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    settle();
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let (status, length) = send(&mut stream, &upload, chunk);
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    assert_eq!(status, 200);
    assert_eq!(length, size, "the PDF comes back unchanged");
    peak
}

/// Plan 001, Verification 3: every peak ≤ 4 MiB, and the peak does not
/// grow with the size.
///
/// The growth is checked between 16 and 100 MiB, not from 1 MiB as the
/// plan states. hyper reads up to about 496 KiB at a time, and up to four
/// such chunks wait in the upload queue. A 1 MiB upload is two or three
/// chunks, so it never fills the queue: it peaks at about 3.1 MB, and every
/// longer upload at 3.6 to 4.1 MB. With a queue of one chunk, every size
/// peaks at 3.06 MB (macOS arm64, debug, 2026-11-26). About 4 s in a debug
/// build.
#[test]
fn slow_peak_heap_does_not_grow_with_the_upload() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = server("sizes");
    let mut chunk = Box::new([0u8; 64 * 1024]);
    // Warm up: first-use allocations (thread pools, lazy statics).
    peak_for(&server, MIB, &mut chunk);
    let peaks: Vec<(usize, usize)> = [MIB, 16 * MIB, 100 * MIB]
        .into_iter()
        .map(|size| (size, peak_for(&server, size, &mut chunk)))
        .collect();
    eprintln!("peak heap above baseline: {peaks:?}");
    for &(size, peak) in &peaks {
        assert!(peak <= 4 * MIB, "{size} bytes: peak {peak}");
    }
    let growth = peaks[2].1.saturating_sub(peaks[1].1);
    assert!(
        growth <= 512 * 1024,
        "grew {growth} bytes from 16 to 100 MiB"
    );
    drop(server.runtime);
}

/// Allocations per 15 KB `output=pdf` request. The ceiling is the first
/// measurement (340, debug build, macOS arm64, 2026-11-26) plus 20 %, so
/// a regression shows. Linux has not been measured.
#[test]
fn allocations_per_small_request() {
    const REQUESTS: usize = 20;
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = server("count");
    let mut chunk = Box::new([0u8; 64 * 1024]);
    let upload = upload(15_000);
    for _ in 0..5 {
        let mut stream = TcpStream::connect(server.addr).unwrap();
        assert_eq!(send(&mut stream, &upload, &mut chunk).0, 200);
    }
    settle();
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..REQUESTS {
        let mut stream = TcpStream::connect(server.addr).unwrap();
        assert_eq!(send(&mut stream, &upload, &mut chunk).0, 200);
    }
    settle();
    let per_request = (ALLOCATIONS.load(Ordering::Relaxed) - before) / REQUESTS;
    eprintln!("allocations per request: {per_request}");
    assert!(
        per_request <= ALLOCATION_CEILING,
        "{per_request} allocations per request, ceiling {ALLOCATION_CEILING}"
    );
    drop(server.runtime);
}

/// See [`allocations_per_small_request`].
const ALLOCATION_CEILING: usize = 408;
