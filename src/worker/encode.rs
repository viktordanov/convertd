//! Page image encoders, from PDFium's BGRA bitmap straight to the file
//! bytes, and the flat-page check (005 §5).
//!
//! An [`Encoder`] keeps its buffers across the pages of a job: the encoded
//! output and, for PNG, the RGB rows. JPEG reads the BGRA bitmap as it is.
//! A PNG page larger than 1 MiB converts to RGB one row at a time into the
//! encoder, so it is never copied whole; a smaller page keeps image's
//! whole-page encoder and its exact bytes.

use std::io::Write;

use crate::ipc::ImageFormat;

/// The tolerance of the flat check, per channel.
pub const FLAT_TOLERANCE: u8 = 4;

/// Whether every pixel of a BGRA bitmap is within [`FLAT_TOLERANCE`] of
/// pixel (0,0) in each colour channel, and that pixel as RGB. The scan
/// stops at the first pixel that differs.
pub fn flat(bgra: &[u8]) -> (bool, [u8; 3]) {
    let Some(first) = bgra.get(..4) else {
        return (true, [255, 255, 255]);
    };
    let (b, g, r) = (first[0], first[1], first[2]);
    let near = |x: u8, y: u8| x.abs_diff(y) <= FLAT_TOLERANCE;
    let flat = bgra
        .as_chunks::<4>()
        .0
        .iter()
        .all(|p| near(p[0], b) && near(p[1], g) && near(p[2], r));
    (flat, [r, g, b])
}

/// The longest edge, in pixels, that each format can encode: JPEG stores
/// 16-bit sizes.
pub fn max_edge(format: ImageFormat) -> u32 {
    match format {
        ImageFormat::Jpeg => u32::from(u16::MAX),
        ImageFormat::Png => u32::MAX,
    }
}

/// Why a page did not encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// The page is larger than the format allows: the job's own limit.
    TooLarge(String),
    /// The encoder failed.
    Failed(String),
}

/// The encoders of one job, with the buffers they reuse from page to page.
#[derive(Debug, Default)]
pub struct Encoder {
    /// The encoded bytes of the last page.
    out: Vec<u8>,
    /// One RGB row for a streamed PNG, or the whole page for a small one.
    rgb: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encodes a `width` × `height` BGRA bitmap with rows of `width * 4`
    /// bytes, and returns the file bytes, valid until the next call. A page
    /// over [`max_edge`] is `TooLarge`, found before any encoder runs.
    pub fn encode(
        &mut self,
        format: ImageFormat,
        bgra: &[u8],
        width: u32,
        height: u32,
        quality: u8,
    ) -> Result<&[u8], EncodeError> {
        let limit = max_edge(format);
        if width > limit || height > limit {
            return Err(EncodeError::TooLarge(format!(
                "the page is {width} x {height} px; {format:?} allows at most {limit} px a side"
            )));
        }
        let row_bytes = width as usize * 4;
        if (row_bytes as u128) * u128::from(height) != bgra.len() as u128 {
            return Err(EncodeError::Failed(format!(
                "the bitmap holds {} bytes, not {width} x {height} x 4",
                bgra.len()
            )));
        }
        let failed = |err: String| EncodeError::Failed(err);
        self.out.clear();
        match format {
            ImageFormat::Jpeg => {
                // Both fit: checked against max_edge above.
                let (w, h) = (width as u16, height as u16);
                jpeg_encoder::Encoder::new(&mut self.out, quality.clamp(1, 100))
                    .encode(bgra, w, h, jpeg_encoder::ColorType::Bgra)
                    .map_err(|err| failed(err.to_string()))?;
            }
            ImageFormat::Png if stored_png_len(width, height) <= WHOLE_PAGE_PNG => {
                // A small page: one RGB copy of at most 1 MiB, and image's
                // encoder (fdeflate), so the bytes stay as they were.
                use image::ImageEncoder;
                use image::codecs::png::{CompressionType, FilterType, PngEncoder};
                self.rgb.clear();
                for p in bgra.as_chunks::<4>().0 {
                    self.rgb.extend_from_slice(&[p[2], p[1], p[0]]);
                }
                PngEncoder::new_with_quality(
                    &mut self.out,
                    CompressionType::Fast,
                    FilterType::Adaptive,
                )
                .write_image(&self.rgb, width, height, image::ExtendedColorType::Rgb8)
                .map_err(|err| failed(err.to_string()))?;
            }
            // zlib stores a block that does not compress, so noise or a
            // photo stays within a few bytes of a stored PNG.
            ImageFormat::Png => self.png(bgra, width, height)?,
        }
        Ok(&self.out)
    }
}

impl Encoder {
    /// Streams the bitmap into `self.out` as a PNG, one RGB row at a time,
    /// with the adaptive filter and zlib at [`STREAM_ZLIB_LEVEL`].
    fn png(&mut self, bgra: &[u8], width: u32, height: u32) -> Result<(), EncodeError> {
        let failed = |err: String| EncodeError::Failed(err);
        let mut encoder = png::Encoder::new(&mut self.out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        // set_compression also sets a filter, so the filter comes after it.
        encoder.set_compression(png::Compression::Fast);
        encoder.set_deflate_compression(png::DeflateCompression::Level(STREAM_ZLIB_LEVEL));
        encoder.set_filter(png::Filter::Adaptive);
        let mut writer = encoder
            .write_header()
            .map_err(|err| failed(err.to_string()))?;
        let mut stream = writer
            .stream_writer_with_size(PNG_CHUNK)
            .map_err(|err| failed(err.to_string()))?;
        self.rgb.resize(width as usize * 3, 0);
        let row_bytes = width as usize * 4;
        if row_bytes > 0 {
            for line in bgra.chunks_exact(row_bytes) {
                bgra_row_to_rgb(line, &mut self.rgb);
                stream
                    .write_all(&self.rgb)
                    .map_err(|err| failed(err.to_string()))?;
            }
        }
        stream.finish().map_err(|err| failed(err.to_string()))
    }
}

/// A deterministic A4-like page as a BGRA bitmap, for the encoder bench
/// (`examples/encode_bench.rs`) and the fidelity test: a white sheet with
/// lines of dark glyph-like runs, a grey table grid, and a colour block
/// with a gradient and noise, as a photo would be.
#[doc(hidden)]
pub fn sample_page(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut bgra = vec![255u8; w * h * 4];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let put = |x: usize, y: usize, px: [u8; 4], bgra: &mut [u8]| {
        let at = (y * w + x) * 4;
        bgra[at..at + 4].copy_from_slice(&px);
    };
    let margin = w / 12;
    // Text: lines of 12 px glyph rows, 20 px apart, in the upper half.
    let mut y = h / 12;
    while y + 12 < h / 2 {
        let mut x = margin;
        while x + 8 < w - margin {
            let word = 3 + (next() % 9) as usize;
            for g in 0..word {
                let gx = x + g * 7;
                if gx + 6 >= w - margin {
                    break;
                }
                let shape = next();
                for dy in 0..12 {
                    for dx in 0..6 {
                        if (shape >> ((dy * 6 + dx) % 64)) & 1 == 1 {
                            let ink = 20 + (next() % 40) as u8;
                            put(gx + dx, y + dy, [ink, ink, ink, 255], &mut bgra);
                        }
                    }
                }
            }
            x += word * 7 + 7;
        }
        y += 20;
    }
    // A table: grey rules every 40 px across a band.
    for row in (h / 2..h / 2 + h / 6).step_by(40) {
        for x in margin..w - margin {
            put(x, row, [160, 160, 160, 255], &mut bgra);
        }
    }
    for col in (margin..w - margin).step_by(w / 8) {
        for y in h / 2..h / 2 + h / 6 {
            put(col, y, [160, 160, 160, 255], &mut bgra);
        }
    }
    // A photo: a gradient with noise in the lower part.
    for y in h * 3 / 4..h * 11 / 12 {
        for x in margin..w - margin {
            let n = (next() % 24) as u8;
            let r = ((x * 255 / w) as u8).saturating_add(n);
            let g = ((y * 255 / h) as u8).saturating_add(n / 2);
            put(x, y, [128u8.saturating_add(n), g, r, 255], &mut bgra);
        }
    }
    bgra
}

/// The zlib level of a streamed PNG.
///
/// Not fdeflate, which the whole-page encoder uses: png's StreamWriter
/// gives it the filter byte and the row in two calls, and its output then
/// depends on the page width (white 93 x 3777 px: +43 %, 5 x 1000: +65 %
/// against the whole-page bytes). zlib's output does not depend on the
/// calls. Level 1 was 15 to 18 % larger than the old bytes on blank pages;
/// level 2 is about a quarter of level 1 on blank and text pages, smaller
/// than the old bytes, for about 1.5 times the old encode time.
const STREAM_ZLIB_LEVEL: u8 = 2;

/// The largest stored PNG size that still takes the whole-page encoder:
/// above it, the rows stream and no page copy is made.
const WHOLE_PAGE_PNG: usize = 1024 * 1024;

/// The IDAT chunk size of the streamed PNG: one buffer of this size, and
/// 12 bytes of chunk framing for each.
const PNG_CHUNK: usize = 64 * 1024;

/// The size of an RGB PNG whose rows are stored, not compressed: the
/// signature, IHDR and IEND, the zlib stream of stored deflate blocks of
/// at most 65 535 bytes (5 bytes of header each, 2 of zlib header, 4 of
/// Adler-32), and 12 bytes of framing for each IDAT chunk.
fn stored_png_len(width: u32, height: u32) -> usize {
    let raw = (width as usize * 3 + 1) * height as usize;
    let zlib = 2 + 4 + raw + 5 * raw.div_ceil(65_535).max(1);
    8 + 25 + 12 + zlib + 12 * zlib.div_ceil(PNG_CHUNK)
}

/// One BGRA row into a same-width RGB row.
fn bgra_row_to_rgb(bgra: &[u8], rgb: &mut [u8]) {
    for (p, q) in bgra
        .as_chunks::<4>()
        .0
        .iter()
        .zip(rgb.as_chunks_mut::<3>().0.iter_mut())
    {
        *q = [p[2], p[1], p[0]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    /// Counts the heap bytes of the current thread and their peak, so a
    /// test can measure what one call allocates while other tests run.
    struct Counting;

    thread_local! {
        static LIVE: Cell<usize> = const { Cell::new(0) };
        static PEAK: Cell<usize> = const { Cell::new(0) };
    }

    fn add(bytes: usize) {
        let _ = LIVE.try_with(|live| {
            let now = live.get() + bytes;
            live.set(now);
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
        });
    }

    fn sub(bytes: usize) {
        let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(bytes)));
    }

    // SAFETY: every call goes to the system allocator; the counters are
    // const-initialised thread locals, which never allocate.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            add(layout.size());
            // SAFETY: the caller's layout goes on unchanged.
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            sub(layout.size());
            // SAFETY: `ptr` came from `alloc` with this layout.
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: as `GlobalAlloc::realloc` requires of its caller.
            let grown = unsafe { System.realloc(ptr, layout, new_size) };
            // Count only a reallocation that happened: the old block goes,
            // the new one comes, so an in-place change is not a peak.
            if !grown.is_null() {
                sub(layout.size());
                add(new_size);
            }
            grown
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    /// The peak heap bytes that `work` allocates above what was live before.
    fn peak_of(work: impl FnOnce()) -> usize {
        let base = LIVE.with(Cell::get);
        PEAK.with(|peak| peak.set(base));
        work();
        PEAK.with(Cell::get) - base
    }

    fn bitmap(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        std::iter::repeat_n(px, (w * h) as usize)
            .flatten()
            .collect()
    }

    /// A gradient with R ≠ B, so a swapped channel shows.
    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                out.extend_from_slice(&[(x % 251) as u8, (y % 241) as u8, 200, 255]);
            }
        }
        out
    }

    /// The encoder before this change: the whole page to RGB, then image's
    /// PngEncoder. Kept as the reference for the bytes and the peak.
    fn png_before(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
        use image::ImageEncoder;
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        let mut rgb = Vec::with_capacity(bgra.len() / 4 * 3);
        for p in bgra.as_chunks::<4>().0 {
            rgb.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        let mut out = Vec::new();
        PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive)
            .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    #[test]
    fn flat_allows_four_levels_and_reports_rgb() {
        let mut bgra = bitmap(4, 4, [10, 20, 30, 255]);
        assert_eq!(flat(&bgra), (true, [30, 20, 10]));
        bgra[20] = 14;
        assert!(flat(&bgra).0);
        bgra[20] = 15;
        assert!(!flat(&bgra).0);
    }

    #[test]
    fn a_page_over_the_format_limit_is_too_large_before_encoding() {
        // No bitmap is needed: the size is checked first.
        let err = Encoder::new()
            .encode(ImageFormat::Jpeg, &[], 10, 70_000, 85)
            .unwrap_err();
        assert!(matches!(err, EncodeError::TooLarge(_)), "{err:?}");
        assert_eq!(max_edge(ImageFormat::Jpeg), 65_535);
    }

    #[test]
    fn a_bitmap_of_the_wrong_size_is_refused() {
        let err = Encoder::new()
            .encode(ImageFormat::Png, &[0; 12], 2, 2, 85)
            .unwrap_err();
        assert!(matches!(err, EncodeError::Failed(_)), "{err:?}");
    }

    #[test]
    fn every_format_encodes_a_bitmap() {
        let bgra = bitmap(16, 8, [255, 255, 255, 255]);
        let mut encoder = Encoder::new();
        for (format, magic) in [
            (ImageFormat::Jpeg, &b"\xFF\xD8"[..]),
            (ImageFormat::Png, &b"\x89PNG"[..]),
        ] {
            let bytes = encoder.encode(format, &bgra, 16, 8, 85).unwrap();
            assert!(bytes.starts_with(magic), "{format:?}");
        }
    }

    /// Lines of dark "glyphs" on white, roughly like a rendered text page.
    fn text(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let ink = (y / 12) % 3 == 0 && (x / 7 + y / 12) % 5 != 0 && x % 7 < 4;
                if ink {
                    [20, 20, 20, 255]
                } else {
                    [255, 255, 255, 255]
                }
            })
            .collect()
    }

    /// Pseudo-random noise, which compresses badly: the worst case for size.
    fn noise(w: u32, h: u32) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..w * h)
            .flat_map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let [b, g, r, ..] = state.to_le_bytes();
                [b, g, r, 255]
            })
            .collect()
    }

    /// Encodes each page twice, with the new encoder and with the old one,
    /// and gives both outputs. The second new encode reuses the buffers and
    /// must give the same bytes.
    fn both(pages: Vec<(&str, u32, u32, Vec<u8>)>) -> Vec<(String, Vec<u8>, Vec<u8>)> {
        let mut encoder = Encoder::new();
        pages
            .into_iter()
            .map(|(name, w, h, bgra)| {
                let new = encoder
                    .encode(ImageFormat::Png, &bgra, w, h, 85)
                    .unwrap()
                    .to_vec();
                let again = encoder.encode(ImageFormat::Png, &bgra, w, h, 85).unwrap();
                assert_eq!(again, &new[..], "{name}: the second page differs");
                let old = png_before(&bgra, w, h);
                eprintln!(
                    "png {name} {w}x{h}: new {} B, old {} B",
                    new.len(),
                    old.len()
                );
                (name.to_owned(), new, old)
            })
            .collect()
    }

    /// A page whose stored PNG is at most 1 MiB keeps the bytes of the
    /// whole-page encoder, and its channels in RGB order.
    #[test]
    fn small_png_pages_keep_their_bytes() {
        for (name, new, old) in both(vec![
            ("gradient", 300, 200, gradient(300, 200)),
            ("narrow", 5, 1000, bitmap(5, 1000, [255, 255, 255, 255])),
            ("tiny", 8, 4, bitmap(8, 4, [255, 255, 255, 255])),
            ("noise", 256, 256, noise(256, 256)),
        ]) {
            assert_eq!(new, old, "{name}: the bytes changed");
        }
        let mut encoder = Encoder::new();
        let png = encoder
            .encode(ImageFormat::Png, &gradient(300, 200), 300, 200, 85)
            .unwrap();
        let pixel = image::load_from_memory(png)
            .unwrap()
            .to_rgb8()
            .get_pixel(10, 20)
            .0;
        assert_eq!(pixel, [200, 20, 10], "R and B swapped");
    }

    /// A larger page streams with zlib: the same pixels, channels in RGB
    /// order, but other bytes. No page grows by more than 0.2 %: blank and
    /// text pages come out smaller than before, and noise within a few
    /// bytes. The pages are just past 1 MiB stored, so the test stays in
    /// the push suite.
    #[test]
    fn large_png_pages_keep_their_pixels_and_their_size() {
        for (name, new, old) in both(vec![
            ("noise", 600, 600, noise(600, 600)),
            ("text", 640, 600, text(640, 600)),
            // The widths at which fdeflate streaming grew by 16 % and 43 %,
            // and those at which zlib level 1 grew by 18 % and 15 %.
            (
                "white 597",
                597,
                842,
                bitmap(597, 842, [255, 255, 255, 255]),
            ),
            ("white 93", 93, 3777, bitmap(93, 3777, [255, 255, 255, 255])),
            (
                "white 400",
                400,
                900,
                bitmap(400, 900, [255, 255, 255, 255]),
            ),
            (
                "white 600",
                600,
                600,
                bitmap(600, 600, [255, 255, 255, 255]),
            ),
        ]) {
            let pixels = |png: &[u8]| image::load_from_memory(png).unwrap().to_rgb8().into_raw();
            assert_eq!(pixels(&new), pixels(&old), "{name}: the pixels changed");
            assert!(
                new.len() * 1000 <= old.len() * 1002,
                "{name}: {} B against {} B",
                new.len(),
                old.len()
            );
        }
        let mut encoder = Encoder::new();
        let png = encoder
            .encode(ImageFormat::Png, &gradient(640, 600), 640, 600, 85)
            .unwrap();
        let pixel = image::load_from_memory(png)
            .unwrap()
            .to_rgb8()
            .get_pixel(10, 20)
            .0;
        assert_eq!(pixel, [200, 20, 10], "R and B swapped");
    }

    /// A page at 150 dpi on A4 streams through zlib and decodes back to
    /// exactly the input pixels, whatever deflate backend flate2 uses.
    #[test]
    fn a_streamed_a4_page_decodes_to_its_pixels() {
        let (w, h) = (1240, 1754);
        let bgra = sample_page(w, h);
        assert!(
            stored_png_len(w, h) > WHOLE_PAGE_PNG,
            "the page must stream"
        );
        let mut encoder = Encoder::new();
        let png = encoder.encode(ImageFormat::Png, &bgra, w, h, 85).unwrap();
        let decoded = image::load_from_memory(png).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (w, h));
        let want: Vec<u8> = bgra
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0]])
            .collect();
        assert!(decoded.as_raw() == &want, "the pixels changed");
    }

    /// The peak heap of encoding one page, before and after this change.
    /// Before, PNG held a whole-page RGB copy (3/4 of the BGRA bitmap)
    /// beside the output. After, it holds one row and the output.
    #[test]
    fn png_peak_holds_no_page_copy() {
        let (w, h) = (1024, 1024);
        // Flat white compresses to a few KiB, so the peak is the copies.
        let bgra = bitmap(w, h, [255, 255, 255, 255]);
        let page_rgb = (w * h * 3) as usize;
        let before = peak_of(|| {
            let _ = png_before(&bgra, w, h);
        });
        let mut encoder = Encoder::new();
        let first = peak_of(|| {
            let _ = encoder.encode(ImageFormat::Png, &bgra, w, h, 85).unwrap();
        });
        let second = peak_of(|| {
            let _ = encoder.encode(ImageFormat::Png, &bgra, w, h, 85).unwrap();
        });
        eprintln!("png peak at {w}x{h}: before {before} B, after {first} B, next page {second} B");
        assert!(
            before >= page_rgb,
            "the reference holds the RGB copy: {before}"
        );
        assert!(first < page_rgb / 4, "a page copy is back: {first} B");
        assert!(
            second <= first,
            "the next page allocates more: {second} > {first}"
        );
    }

    #[test]
    fn jpeg_reads_the_bitmap_without_a_copy() {
        let (w, h) = (1024, 1024);
        let bgra = bitmap(w, h, [255, 255, 255, 255]);
        let mut encoder = Encoder::new();
        let peak = peak_of(|| {
            let _ = encoder.encode(ImageFormat::Jpeg, &bgra, w, h, 85).unwrap();
        });
        eprintln!("jpeg peak at {w}x{h}: {peak} B");
        assert!(
            peak < bgra.len() / 2,
            "the JPEG path copied the page: {peak} B"
        );
    }
}
