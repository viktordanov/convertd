//! Memory per request does not grow with the file size. Its own test
//! binary, so the counting allocator below
//! sees only this process: the in-process server with the fake engine
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

/// Every peak ≤ 4 MiB, and the peak does not grow with the size.
///
/// The growth is checked between 16 and 100 MiB, not from 1 MiB.
/// Every size peaks at 3.06 to 3.15 MB (macOS arm64, debug,
/// 2026-11-26). The two levels are one buffer of multer or hyper more or
/// less at the peak. Before the upload was copied into pieces of its own,
/// each queued slice kept a whole multer buffer of about 496 KiB alive:
/// 3.6 to 4.1 MB on macOS, and on Linux 638 540 bytes more at 100 MiB than
/// at 16 MiB. About 4 s in a debug build.
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

/// Mean allocations of one `output=pdf` request of `size` bytes, after
/// five warm-up requests.
fn allocations_per_request(server: &Server, size: usize, requests: usize) -> usize {
    let mut chunk = Box::new([0u8; 64 * 1024]);
    let upload = upload(size);
    for _ in 0..5 {
        let mut stream = TcpStream::connect(server.addr).unwrap();
        assert_eq!(send(&mut stream, &upload, &mut chunk).0, 200);
    }
    settle();
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..requests {
        let mut stream = TcpStream::connect(server.addr).unwrap();
        assert_eq!(send(&mut stream, &upload, &mut chunk), (200, size));
    }
    settle();
    (ALLOCATIONS.load(Ordering::Relaxed) - before) / requests
}

/// Allocations per 15 KB `output=pdf` request. The ceiling is the first
/// measurement (340, debug build, macOS arm64, 2026-11-26) plus 20 %, so
/// a regression shows. Linux has not been measured.
#[test]
fn allocations_per_small_request() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = server("count");
    let per_request = allocations_per_request(&server, 15_000, 20);
    eprintln!("allocations per request: {per_request}");
    assert!(
        per_request <= ALLOCATION_CEILING,
        "{per_request} allocations per request, ceiling {ALLOCATION_CEILING}"
    );
    drop(server.runtime);
}

/// See [`allocations_per_small_request`].
const ALLOCATION_CEILING: usize = 408;

/// A 1 MiB version-3 OLE2 file whose DIFAT sectors form one long chain
/// (sectors 2, 3, ... 2046) and whose every DIFAT entry names sector 1 as
/// a FAT sector. A reader that builds the FAT from the whole DIFAT before
/// any cap reads sector 1 once per entry: about 260 000 entries, a FAT of
/// about 133 MB.
fn hostile_ole2() -> Vec<u8> {
    const SECTOR: usize = 512;
    const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
    let sectors = (MIB / SECTOR) - 1; // sectors 0..=2046 after the header
    let mut file = vec![0u8; MIB];
    let h = &mut file[..SECTOR];
    h[..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    h[0x18..0x1A].copy_from_slice(&0x3Eu16.to_le_bytes()); // minor version
    h[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes()); // major version 3
    h[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes()); // byte order
    h[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes()); // 512-byte sectors
    h[0x20..0x22].copy_from_slice(&6u16.to_le_bytes()); // 64-byte mini sectors
    h[0x2C..0x30].copy_from_slice(&(sectors as u32).to_le_bytes()); // FAT sectors
    h[0x30..0x34].copy_from_slice(&0u32.to_le_bytes()); // first directory sector
    h[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes()); // mini stream cutoff
    h[0x3C..0x40].copy_from_slice(&END_OF_CHAIN.to_le_bytes()); // no mini FAT
    h[0x44..0x48].copy_from_slice(&2u32.to_le_bytes()); // first DIFAT sector
    h[0x48..0x4C].copy_from_slice(&((sectors - 2) as u32).to_le_bytes());
    for i in 0..109 {
        h[0x4C + 4 * i..0x50 + 4 * i].copy_from_slice(&1u32.to_le_bytes());
    }
    // Sector 1, the named FAT sector: no special values, so nothing strips it.
    for word in file[2 * SECTOR..3 * SECTOR].as_chunks_mut::<4>().0 {
        word.copy_from_slice(&1u32.to_le_bytes());
    }
    // DIFAT sectors 2..=2046: 127 entries naming sector 1, then the next one.
    for sector in 2..sectors {
        let at = (sector + 1) * SECTOR;
        let block = &mut file[at..at + SECTOR];
        for word in block[..SECTOR - 4].as_chunks_mut::<4>().0 {
            word.copy_from_slice(&1u32.to_le_bytes());
        }
        let next = if sector + 1 < sectors {
            (sector + 1) as u32
        } else {
            END_OF_CHAIN
        };
        block[SECTOR - 4..].copy_from_slice(&next.to_le_bytes());
    }
    file
}

#[test]
fn hostile_ole2_detection_stays_bounded() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let bytes = hostile_ole2();
    let mut cursor = std::io::Cursor::new(&bytes);
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let result = convertd::detect::detect_file(&mut cursor, bytes.len() as u64);
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    eprintln!("hostile OLE2: {result:?}, peak {peak} bytes");
    assert!(
        matches!(
            result,
            Err(convertd::domain::detection::DetectError::Unsupported { label: "ole2" })
        ),
        "{result:?}"
    );
    assert!(peak < 4 * MIB, "peak {peak} bytes");
}

/// A part header that never ends: multer would buffer it toward the
/// upload cap. With the framing bound the server answers 400 after 16 KiB,
/// and the request's peak heap stays far below the bytes sent
///.
#[test]
fn unterminated_part_headers_stay_bounded() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = server("framing");
    let total = 32 * MIB;
    let head = format!(
        "POST /v1/convert HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {KEY}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {total}\r\n\r\n\
         --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"\r\nX-Pad: "
    )
    .into_bytes();
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let chunk = Box::new([b'a'; 64 * 1024]);
    settle();
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    writer.write_all(&head).unwrap();
    let filler = std::thread::spawn(move || {
        let mut sent = 0;
        while sent < total {
            if writer.write_all(&chunk[..]).is_err() {
                return;
            }
            sent += chunk.len();
        }
    });
    let mut status = [0u8; 12];
    stream.read_exact(&mut status).unwrap();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    drop(stream);
    let _ = filler.join();
    assert_eq!(&status[9..12], b"400");
    eprintln!("unterminated headers: peak {peak} bytes above baseline");
    assert!(peak < 4 * MIB, "peak {peak} bytes");
    drop(server.runtime);
}
