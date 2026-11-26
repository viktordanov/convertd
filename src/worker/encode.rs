//! Page image encoders, from PDFium's BGRA bitmap straight to the file
//! bytes, and the flat-page check (005 §5).
//!
//! An [`Encoder`] keeps its buffers across the pages of a job: the encoded
//! output and, for PNG, one RGB row. JPEG reads the BGRA bitmap as it is;
//! PNG converts it to RGB one row at a time into the encoder, so no page
//! is ever copied whole.

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
    /// One RGB row for PNG.
    row: Vec<u8>,
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
        if bgra.len() != row_bytes * height as usize {
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
            ImageFormat::Png => {
                self.png(bgra, width, height, png::Compression::Fast)?;
                // As png's own whole-image path does for fdeflate: data that
                // does not compress, such as noise or photos, is stored
                // instead, so the file never grows past a stored PNG.
                if self.out.len() > stored_png_len(width, height) {
                    self.out.clear();
                    self.png(bgra, width, height, png::Compression::NoCompression)?;
                }
            }
        }
        Ok(&self.out)
    }
}

impl Encoder {
    /// Streams the bitmap into `self.out` as a PNG, one RGB row at a time.
    /// Fast gets the adaptive filter, as image's PngEncoder sets it with
    /// CompressionType::Fast; a stored image needs no filter.
    fn png(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        compression: png::Compression,
    ) -> Result<(), EncodeError> {
        let failed = |err: String| EncodeError::Failed(err);
        let mut encoder = png::Encoder::new(&mut self.out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        // set_compression also sets a filter, so the filter comes after it.
        encoder.set_compression(compression);
        encoder.set_filter(match compression {
            png::Compression::NoCompression => png::Filter::NoFilter,
            _ => png::Filter::Adaptive,
        });
        let mut writer = encoder
            .write_header()
            .map_err(|err| failed(err.to_string()))?;
        let mut stream = writer
            .stream_writer_with_size(PNG_CHUNK)
            .map_err(|err| failed(err.to_string()))?;
        self.row.resize(width as usize * 3, 0);
        let row_bytes = width as usize * 4;
        if row_bytes > 0 {
            for line in bgra.chunks_exact(row_bytes) {
                bgra_row_to_rgb(line, &mut self.row);
                stream
                    .write_all(&self.row)
                    .map_err(|err| failed(err.to_string()))?;
            }
        }
        stream.finish().map_err(|err| failed(err.to_string()))
    }
}

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
            add(new_size);
            sub(layout.size());
            // SAFETY: as `GlobalAlloc::realloc` requires of its caller.
            unsafe { System.realloc(ptr, layout, new_size) }
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

    /// The streamed PNG holds the same pixels as the whole-page encoder,
    /// channels in RGB order. The bytes are not the same: png's
    /// StreamWriter feeds fdeflate the filter byte and the row apart, where
    /// `write_image_data` feeds them together, so the deflate stream
    /// differs. With the stored fallback for data that does not compress
    /// (noise), the size stays within 5 % of the whole-page output.
    #[test]
    fn png_keeps_the_pixels_and_the_size_of_the_whole_page_encoder() {
        let mut encoder = Encoder::new();
        for (name, w, h, bgra) in [
            ("gradient", 300, 200, gradient(300, 200)),
            ("noise", 256, 256, noise(256, 256)),
            ("white", 640, 480, bitmap(640, 480, [255, 255, 255, 255])),
            // So small that the framing outweighs the rows.
            ("tiny", 8, 4, bitmap(8, 4, [255, 255, 255, 255])),
        ] {
            let streamed = encoder
                .encode(ImageFormat::Png, &bgra, w, h, 85)
                .unwrap()
                .to_vec();
            let before = png_before(&bgra, w, h);
            let pixels = |png: &[u8]| image::load_from_memory(png).unwrap().to_rgb8().into_raw();
            assert_eq!(
                pixels(&streamed),
                pixels(&before),
                "{name}: the pixels changed"
            );
            eprintln!(
                "png {name} {w}x{h}: streamed {} B, whole page {} B",
                streamed.len(),
                before.len()
            );
            assert!(
                streamed.len() * 100 <= before.len() * 105,
                "{name}: {} B against {} B",
                streamed.len(),
                before.len()
            );
            // A second page reuses the buffers and gives the same bytes.
            let again = encoder.encode(ImageFormat::Png, &bgra, w, h, 85).unwrap();
            assert_eq!(again, &streamed[..], "{name}: the second page differs");
        }
        let image = image::load_from_memory(
            encoder
                .encode(ImageFormat::Png, &gradient(300, 200), 300, 200, 85)
                .unwrap(),
        )
        .unwrap()
        .to_rgb8();
        assert_eq!(image.get_pixel(10, 20).0, [200, 20, 10], "R and B swapped");
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
