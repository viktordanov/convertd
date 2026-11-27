//! The deflate work of the service on the host: the streamed PNG encoder
//! of `worker/encode.rs` on 20 A4 pages at 150 dpi (1240 x 1754), and the
//! pre-flight inflate of DOCX files. Each is timed 5 times; the median is
//! printed. For comparing flate2 backends.
//!
//! ```text
//! cargo run --release --example encode_bench -- corpus/out/docx/*.docx
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use convertd::domain::document::Family;
use convertd::ipc::ImageFormat;
use convertd::preflight::{Printed, scan};
use convertd::worker::encode::{Encoder, sample_page};

const RUNS: usize = 5;
const FRAMES: usize = 20;
const SCANS: usize = 50;

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn main() {
    let (w, h) = (1240, 1754);
    let page = sample_page(w, h);
    let mut encoder = Encoder::new();
    let mut size = 0;
    let mut times = Vec::new();
    for _ in 0..RUNS {
        let started = Instant::now();
        for _ in 0..FRAMES {
            size = encoder
                .encode(ImageFormat::Png, &page, w, h, 85)
                .expect("encode")
                .len();
        }
        times.push(started.elapsed());
    }
    let encode = median(times.clone());
    println!(
        "png encode: {FRAMES} frames {w}x{h}: median {:.1} ms per run, {:.2} ms per frame, {size} B per page; runs {:?}",
        encode.as_secs_f64() * 1e3,
        encode.as_secs_f64() * 1e3 / FRAMES as f64,
        times
            .iter()
            .map(|t| format!("{:.1}", t.as_secs_f64() * 1e3))
            .collect::<Vec<_>>()
    );

    let files: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if files.is_empty() {
        return;
    }
    let mut times = Vec::new();
    for _ in 0..RUNS {
        let started = Instant::now();
        for _ in 0..SCANS {
            for file in &files {
                std::hint::black_box(scan(file, Family::Document, Printed::default()));
            }
        }
        times.push(started.elapsed());
    }
    let inflate = median(times.clone());
    println!(
        "preflight scan: {} DOCX x {SCANS}: median {:.1} ms per run; runs {:?}",
        files.len(),
        inflate.as_secs_f64() * 1e3,
        times
            .iter()
            .map(|t| format!("{:.1}", t.as_secs_f64() * 1e3))
            .collect::<Vec<_>>()
    );
}
