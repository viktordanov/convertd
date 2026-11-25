//! Spike B: PDFium rendering and encoding throughput, one process, one thread.
//!
//! spike-pdfium <libpdfium dir> <dpi> <pdf>...
//!
//! For each PDF: time to bind and open, then for every page the render time and the
//! encode time per output format, kept apart so the slow stage is visible.

use std::{io::Cursor, path::Path, time::Instant};

use image::{DynamicImage, ImageEncoder, codecs::png};
use pdfium_render::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let lib_dir = &args[1];
    let dpi: f32 = args[2].parse().unwrap();

    let t = Instant::now();
    let pdfium = Pdfium::new(
        Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(lib_dir))
            .expect("failed to bind pdfium"),
    );
    println!("bind_ms={}", t.elapsed().as_millis());

    for path in &args[3..] {
        run(&pdfium, Path::new(path), dpi);
    }
}

#[derive(Default)]
struct Totals {
    render: u128,
    jpeg_image: (u128, usize),
    jpeg_encoder: (u128, usize),
    png_fast: (u128, usize),
    webp_lossy: (u128, usize),
    webp_lossless: (u128, usize),
}

fn run(pdfium: &Pdfium, path: &Path, dpi: f32) {
    let t = Instant::now();
    let doc = pdfium.load_pdf_from_file(path, None).expect("failed to open pdf");
    let pages = doc.pages();
    let open_ms = t.elapsed().as_millis();
    let first = pages.get(0).unwrap();
    println!(
        "pdf={} pages={} open_ms={open_ms} page0_pt={:.0}x{:.0}",
        path.file_name().unwrap().to_string_lossy(),
        pages.len(),
        first.width().value,
        first.height().value
    );

    let config = PdfRenderConfig::new().scale_page_by_factor(dpi / 72.0);
    let mut tot = Totals::default();
    let mut px = (0, 0);
    for page in pages.iter() {
        let t = Instant::now();
        let bitmap = page.render_with_config(&config).expect("failed to render");
        let img = bitmap.as_image().expect("failed to convert bitmap");
        tot.render += t.elapsed().as_micros();
        px = (img.width(), img.height());
        let rgb = DynamicImage::ImageRgb8(img.to_rgb8());

        tot.jpeg_image = add(tot.jpeg_image, time(|| {
            let mut out = Vec::new();
            rgb.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85))
                .unwrap();
            out
        }));
        tot.jpeg_encoder = add(tot.jpeg_encoder, time(|| {
            let mut out = Vec::new();
            let rgb8 = rgb.as_rgb8().unwrap();
            jpeg_encoder::Encoder::new(&mut out, 85)
                .encode(rgb8.as_raw(), rgb8.width() as u16, rgb8.height() as u16, jpeg_encoder::ColorType::Rgb)
                .unwrap();
            out
        }));
        tot.png_fast = add(tot.png_fast, time(|| {
            let mut out = Cursor::new(Vec::new());
            let rgb8 = rgb.as_rgb8().unwrap();
            png::PngEncoder::new_with_quality(&mut out, png::CompressionType::Fast, png::FilterType::Adaptive)
                .write_image(rgb8.as_raw(), rgb8.width(), rgb8.height(), image::ExtendedColorType::Rgb8)
                .unwrap();
            out.into_inner()
        }));
        tot.webp_lossy = add(tot.webp_lossy, time(|| {
            webp::Encoder::from_image(&rgb).unwrap().encode(85.0).to_vec()
        }));
        tot.webp_lossless = add(tot.webp_lossless, time(|| {
            let mut out = Vec::new();
            rgb.write_with_encoder(image::codecs::webp::WebPEncoder::new_lossless(&mut out)).unwrap();
            out
        }));
    }

    let n = pages.len() as u128;
    println!("  dpi={dpi} page_px={}x{} render_ms_per_page={:.1}", px.0, px.1, tot.render as f64 / n as f64 / 1000.0);
    for (name, (ms, bytes)) in [
        ("jpeg q85 (image)", tot.jpeg_image),
        ("jpeg q85 (jpeg-encoder)", tot.jpeg_encoder),
        ("png fast (image)", tot.png_fast),
        ("webp q85 (libwebp)", tot.webp_lossy),
        ("webp lossless (image)", tot.webp_lossless),
    ] {
        println!(
            "  {name:<24} encode_ms_per_page={:>6.1} kb_per_page={:>5}",
            ms as f64 / n as f64 / 1000.0,
            bytes as u128 / n / 1024
        );
    }
}

fn time(f: impl FnOnce() -> Vec<u8>) -> (u128, usize) {
    let t = Instant::now();
    let out = f();
    (t.elapsed().as_micros(), out.len())
}

fn add(a: (u128, usize), b: (u128, usize)) -> (u128, usize) {
    (a.0 + b.0, a.1 + b.1)
}
