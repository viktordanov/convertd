//! The `test` role (feature `test-hooks`): no engine, fake results.
//!
//! It answers `Convert`, `Inspect`, `Render` and `Decrypt` with
//! small valid files, driven by the `FAKE:` markers of `engines::fake_markers`
//! in the input file, so the fake worker (`TEST_ENGINE=worker`) runs the
//! real pool, queue, deadline and kill. `TestOp`s act out the faults the
//! pool tests need.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::ipc::{
    ConvertArgs, ConvertedOut, DecryptArgs, DecryptedOut, Family, ImageFormat, Init, InspectArgs,
    InspectedOut, Op, Output, PageGeometry, ProbedPage, RenderArgs, RenderedOut, RenderedPage,
    TestOp, TestOutput, WorkerError, WorkerErrorKind,
};
use crate::worker::output::{OutputBudget, io_error};
use crate::worker::sys::{self, RssReader};
use crate::worker::{Engine, Handled, micros};

/// How far into the input the markers are looked for.
pub const MARKER_SCAN_BYTES: u64 = crate::engines::fake_markers::SCAN_LIMIT as u64;

/// A4 in points: the fake page size.
const PAGE: PageGeometry = PageGeometry {
    width_pt: 595.0,
    height_pt: 842.0,
};

/// The edge of the fake page images, in pixels.
const IMAGE_EDGE: u32 = 8;

/// What a marker asks the fake engine to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// No marker: succeed with one page.
    Succeed,
    /// `FAKE:DELAY_MS=<n>`: sleep, then succeed. The pool's deadline turns
    /// a long delay into `Timeout`.
    Delay { ms: u64 },
    /// `FAKE:PAGES=<n>`: succeed with `n` pages.
    Pages { n: u32 },
    /// `FAKE:HANG`: never answer.
    Hang,
    /// `FAKE:PANIC`: panic, which ends the worker with 101.
    Panic,
    /// `FAKE:<CODE>`: answer this error.
    Fail(WorkerErrorKind),
}

impl Behavior {
    fn pages(self) -> u32 {
        match self {
            Behavior::Pages { n } => n,
            _ => 1,
        }
    }
}

/// The behavior that the first `FAKE:` marker in `bytes` asks for, read
/// with the parser that the fake engine uses too.
pub fn markers(bytes: &[u8]) -> Behavior {
    use crate::engines::fake_markers::{Marker, MarkerCode, parse};

    let Some(marker) = parse(bytes) else {
        return Behavior::Succeed;
    };
    let kind = match marker {
        Marker::DelayMs(ms) => return Behavior::Delay { ms },
        Marker::Pages(n) => return Behavior::Pages { n },
        Marker::Hang => return Behavior::Hang,
        Marker::Panic => return Behavior::Panic,
        Marker::Code(code) => match code {
            MarkerCode::PasswordRequired => WorkerErrorKind::PasswordRequired,
            MarkerCode::InvalidPassword => WorkerErrorKind::InvalidPassword,
            MarkerCode::LimitExceeded => WorkerErrorKind::LimitExceeded,
            MarkerCode::ConversionFailed => WorkerErrorKind::LoadFailed,
            MarkerCode::TooManyRequests => WorkerErrorKind::FakeQueueFull,
            MarkerCode::ServiceUnavailable => WorkerErrorKind::FakeUnavailable,
            MarkerCode::InternalError => WorkerErrorKind::Internal,
        },
    };
    Behavior::Fail(kind)
}

/// The behaviour the markers in `path` ask for.
pub fn behavior_of(path: &Path) -> Result<Behavior, WorkerError> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(MARKER_SCAN_BYTES).read_to_end(&mut bytes))
        .map_err(|err| io_error("read input", &err))?;
    Ok(markers(&bytes))
}

/// A valid PDF of `pages` empty A4 pages. With more than one page it
/// carries `FAKE:PAGES=<n>`, so an inspect of a fake output sees the page
/// count the upload asked for.
pub fn fake_pdf(pages: u32) -> Vec<u8> {
    let mut pdf = Vec::with_capacity(256 + 64 * pages as usize);
    let mut offsets = Vec::with_capacity(pages as usize + 2);
    pdf.extend_from_slice(b"%PDF-1.4\n");
    if pages != 1 {
        pdf.extend_from_slice(format!("%FAKE:PAGES={pages}\n").as_bytes());
    }
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    offsets.push(pdf.len());
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
    pdf.extend_from_slice(
        format!(
            "2 0 obj\n<< /Type /Pages /Kids [{}] /Count {pages} >>\nendobj\n",
            kids.join(" ")
        )
        .as_bytes(),
    );
    for i in 0..pages {
        offsets.push(pdf.len());
        pdf.extend_from_slice(
            format!(
                "{} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] >>\nendobj\n",
                i + 3
            )
            .as_bytes(),
        );
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

/// A white 8 × 8 image in `format`.
pub fn fake_image(format: ImageFormat) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;

    let rgb = vec![255u8; (IMAGE_EDGE * IMAGE_EDGE * 3) as usize];
    let edge = IMAGE_EDGE as u16;
    let mut out = Vec::new();
    match format {
        ImageFormat::Jpeg => jpeg_encoder::Encoder::new(&mut out, 85)
            .encode(&rgb, edge, edge, jpeg_encoder::ColorType::Rgb)
            .map_err(|err| err.to_string())?,
        ImageFormat::Png => image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&rgb, IMAGE_EDGE, IMAGE_EDGE, image::ExtendedColorType::Rgb8)
            .map_err(|err| err.to_string())?,
    }
    Ok(out)
}

fn family_name(family: Family) -> &'static str {
    match family {
        Family::Document => "text",
        Family::Spreadsheet => "spreadsheet",
        Family::Presentation => "presentation",
    }
}

fn hang() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// The fake engine.
pub struct TestEngine {
    /// Memory kept by `Allocate`.
    allocations: Vec<Vec<u8>>,
    /// Children of `SpawnSleeper`; never waited for, they die with the group.
    sleepers: Vec<Child>,
    rss: RssReader,
}

impl TestEngine {
    pub fn start(init: &Init) -> Result<Self, String> {
        if init.test.ready_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(init.test.ready_delay_ms));
        }
        if init.test.fatal {
            return Err("failed to start the test engine: init asked for a fatal".into());
        }
        Ok(Self {
            allocations: Vec::new(),
            sleepers: Vec::new(),
            rss: RssReader::new(),
        })
    }

    fn test_op(&mut self, op: TestOp) -> Result<Output, WorkerError> {
        let out = match op {
            TestOp::Sleep { ms } => {
                std::thread::sleep(Duration::from_millis(ms));
                TestOutput::Slept
            }
            TestOp::Burn { ms } => {
                burn(Duration::from_millis(ms));
                TestOutput::Slept
            }
            TestOp::Hang => hang(),
            TestOp::Abort => std::process::abort(),
            TestOp::Exit { code } => sys::exit(code),
            TestOp::Allocate { mb } => {
                let len = (mb as usize) << 20;
                let mut block = vec![0u8; len];
                // Touch every page, so it is resident.
                for i in (0..len).step_by(4096) {
                    block[i] = 1;
                }
                self.allocations.push(std::hint::black_box(block));
                TestOutput::Allocated {
                    rss_bytes: self.rss.read(),
                }
            }
            TestOp::WriteStdout { bytes } => {
                let mut junk = vec![b'x'; bytes as usize];
                junk.push(b'\n');
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(&junk)
                    .and_then(|()| stdout.flush())
                    .map_err(|err| io_error("write stdout", &err))?;
                TestOutput::Wrote { bytes }
            }
            TestOp::SpawnSleeper => {
                let child = Command::new("sleep")
                    .arg("1000")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|err| io_error("spawn sleeper", &err))?;
                let pid = child.id();
                self.sleepers.push(child);
                TestOutput::Spawned { pid }
            }
            TestOp::Env => {
                let mut vars: Vec<(String, String)> = std::env::vars().collect();
                vars.sort();
                TestOutput::Env { vars }
            }
            TestOp::ReadEnviron { pid } => match std::fs::read(format!("/proc/{pid}/environ")) {
                Ok(bytes) => TestOutput::Read {
                    bytes: Some(bytes.len() as u64),
                    error: None,
                },
                Err(err) => TestOutput::Read {
                    bytes: None,
                    error: Some(format!("{:?}", err.kind())),
                },
            },
        };
        Ok(Output::Test(out))
    }

    /// Reads the markers in `input`, acts them out, and runs `job` with the
    /// page count they ask for.
    fn fake(
        input: &Path,
        job: impl FnOnce(u32) -> Result<Output, WorkerError>,
    ) -> Result<Output, WorkerError> {
        let behavior = behavior_of(input)?;
        match behavior {
            Behavior::Delay { ms } => std::thread::sleep(Duration::from_millis(ms)),
            Behavior::Hang => hang(),
            Behavior::Panic => panic!("FAKE:PANIC"),
            Behavior::Fail(kind) => {
                return Err(WorkerError::new(kind, format!("fake engine: {kind:?}")));
            }
            Behavior::Succeed | Behavior::Pages { .. } => {}
        }
        job(behavior.pages())
    }

    fn convert(args: ConvertArgs) -> Result<Output, WorkerError> {
        Self::fake(&args.input, |pages| {
            let mut budget = OutputBudget::new(args.max_output_bytes);
            let pdf_bytes = budget.write_file(&args.output, &fake_pdf(pages))?;
            Ok(Output::Converted(ConvertedOut {
                pdf_bytes,
                doc_type: family_name(args.family).into(),
                facts: None,
            }))
        })
    }

    fn inspect(args: InspectArgs) -> Result<Output, WorkerError> {
        Self::fake(&args.pdf, |pages| {
            let probe = match args.probe_dpi {
                Some(_) => (1..=pages)
                    .map(|number| ProbedPage {
                        number,
                        objects: 1,
                        flat: false,
                        colour: [255, 255, 255],
                    })
                    .collect(),
                None => Vec::new(),
            };
            Ok(Output::Inspected(InspectedOut {
                page_count: pages,
                encrypted: false,
                needs_password: false,
                pages: vec![PAGE; pages as usize],
                probe,
                probe_skipped: 0,
                fonts: Vec::new(),
            }))
        })
    }

    fn render(args: RenderArgs) -> Result<Output, WorkerError> {
        Self::fake(&args.pdf, |pages| {
            let image = fake_image(args.format)
                .map_err(|err| WorkerError::new(WorkerErrorKind::Internal, err))?;
            let scale = (f32::from(args.dpi) / 72.0).min(args.max_edge as f32 / PAGE.height_pt);
            let mut budget = OutputBudget::new(args.max_output_bytes);
            let mut out = Vec::with_capacity(args.pages.len());
            for &number in &args.pages {
                if number == 0 || number > pages {
                    budget.remove_written();
                    return Err(WorkerError::new(
                        WorkerErrorKind::LoadFailed,
                        format!("failed to load page {number}: the document has {pages}"),
                    ));
                }
                let file_name = format!("{number:04}.{}", args.format.extension());
                let size_bytes = match budget.write_file(&args.out_dir.join(&file_name), &image) {
                    Ok(n) => n,
                    Err(err) => {
                        budget.remove_written();
                        return Err(err);
                    }
                };
                out.push(RenderedPage {
                    number,
                    file_name,
                    width_px: IMAGE_EDGE,
                    height_px: IMAGE_EDGE,
                    width_pt: PAGE.width_pt,
                    height_pt: PAGE.height_pt,
                    size_bytes,
                    objects: 1,
                    flat: false,
                    colour: [255, 255, 255],
                    effective_dpi: scale * 72.0,
                });
            }
            Ok(Output::Rendered(RenderedOut { pages: out }))
        })
    }

    fn decrypt(args: DecryptArgs) -> Result<Output, WorkerError> {
        Self::fake(&args.pdf, |pages| {
            let mut budget = OutputBudget::new(args.max_output_bytes);
            let pdf_bytes = budget.write_file(&args.output, &fake_pdf(pages))?;
            Ok(Output::Decrypted(DecryptedOut { pdf_bytes }))
        })
    }
}

/// Spins until the process has used `cpu` more CPU time, or until 10 ×
/// `cpu` of wall time has passed, whichever comes first.
fn burn(cpu: Duration) {
    let start = sys::cpu_time();
    let give_up = Instant::now() + cpu.saturating_mul(10);
    let mut x = 0u64;
    while sys::cpu_time().saturating_sub(start) < cpu && Instant::now() < give_up {
        for _ in 0..10_000 {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
        }
    }
}

impl Engine for TestEngine {
    fn version(&self) -> String {
        String::new()
    }

    fn handle(&mut self, op: Op) -> Handled {
        let started = Instant::now();
        let (result, slot): (_, fn(&mut crate::ipc::EngineTimes, u64)) = match op {
            Op::Ping => (Ok(Output::Pong), |_, _| {}),
            Op::Test(op) => (self.test_op(op), |_, _| {}),
            Op::Convert(args) => (Self::convert(args), |t, us| t.convert_us = us),
            Op::Inspect(args) => (Self::inspect(args), |t, us| t.inspect_us = us),
            Op::Render(args) => (Self::render(args), |t, us| t.render_us = us),
            Op::Decrypt(args) => (Self::decrypt(args), |t, us| t.decrypt_us = us),
        };
        let us = micros(started.elapsed());
        let mut handled = match result {
            Ok(out) => Handled::ok(out),
            Err(err) => Handled::err(err),
        };
        handled.engine.engine_us = us;
        slot(&mut handled.engine, us);
        handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_drive_behavior() {
        let cases: &[(&[u8], Behavior)] = &[
            (b"%PDF-1.4 no marker", Behavior::Succeed),
            (b"%FAKE:DELAY_MS=250\n", Behavior::Delay { ms: 250 }),
            (b"x FAKE:PAGES=12 y", Behavior::Pages { n: 12 }),
            (b"FAKE:HANG", Behavior::Hang),
            (b"FAKE:PANIC", Behavior::Panic),
            (
                b"FAKE:PASSWORD_REQUIRED",
                Behavior::Fail(WorkerErrorKind::PasswordRequired),
            ),
            (
                b"FAKE:INVALID_PASSWORD",
                Behavior::Fail(WorkerErrorKind::InvalidPassword),
            ),
            (
                b"FAKE:LIMIT_EXCEEDED",
                Behavior::Fail(WorkerErrorKind::LimitExceeded),
            ),
            (
                b"FAKE:CONVERSION_FAILED",
                Behavior::Fail(WorkerErrorKind::LoadFailed),
            ),
            (
                b"FAKE:TOO_MANY_REQUESTS",
                Behavior::Fail(WorkerErrorKind::FakeQueueFull),
            ),
            (
                b"FAKE:SERVICE_UNAVAILABLE",
                Behavior::Fail(WorkerErrorKind::FakeUnavailable),
            ),
            (
                b"FAKE:INTERNAL_ERROR",
                Behavior::Fail(WorkerErrorKind::Internal),
            ),
            // The first marker wins; unknown ones are skipped.
            (
                b"FAKE:NOPE FAKE:PAGES=3 FAKE:HANG",
                Behavior::Pages { n: 3 },
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                markers(bytes),
                *expected,
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn markers_past_the_first_mib_are_ignored() {
        let mut bytes = vec![b' '; MARKER_SCAN_BYTES as usize];
        bytes.extend_from_slice(b"FAKE:HANG");
        assert_eq!(markers(&bytes), Behavior::Succeed);
    }

    #[test]
    fn fake_pdf_carries_the_page_count_and_a_valid_xref() {
        let pdf = fake_pdf(3);
        assert!(pdf.starts_with(b"%PDF-1.4\n"));
        assert_eq!(markers(&pdf), Behavior::Pages { n: 3 });
        assert_eq!(markers(&fake_pdf(1)), Behavior::Succeed);
        // Every xref entry points at its object.
        let text = String::from_utf8(pdf.clone()).unwrap();
        let xref = text.find("xref\n").unwrap();
        let entries: Vec<usize> = text[xref..]
            .lines()
            .skip(3)
            .take(5)
            .map(|l| l[..10].parse().unwrap())
            .collect();
        for (i, offset) in entries.iter().enumerate() {
            assert!(text[*offset..].starts_with(&format!("{} 0 obj", i + 1)));
        }
        let startxref: usize = text.lines().rev().nth(1).unwrap().parse().unwrap();
        assert_eq!(startxref, xref);
    }

    #[test]
    fn fake_images_decode() {
        for (format, magic) in [
            (ImageFormat::Jpeg, &b"\xFF\xD8"[..]),
            (ImageFormat::Png, &b"\x89PNG"[..]),
        ] {
            let bytes = fake_image(format).unwrap();
            assert!(bytes.starts_with(magic), "{format:?}");
        }
    }
}
