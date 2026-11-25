//! Page image encoders, from PDFium's BGRA bitmap straight to the file
//! bytes, and the flat-page check (005 §5).

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

fn bgra_to_rgb(bgra: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(bgra.len() / 4 * 3);
    for p in bgra.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[p[2], p[1], p[0]]);
    }
    rgb
}

/// Encodes a `width` × `height` BGRA bitmap. JPEG takes BGRA directly; PNG
/// and WebP need one pass to RGB.
pub fn encode(
    format: ImageFormat,
    bgra: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    match format {
        ImageFormat::Jpeg => {
            let w =
                u16::try_from(width).map_err(|_| "the page is too wide for JPEG".to_string())?;
            let h =
                u16::try_from(height).map_err(|_| "the page is too tall for JPEG".to_string())?;
            jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100))
                .encode(bgra, w, h, jpeg_encoder::ColorType::Bgra)
                .map_err(|err| err.to_string())?;
        }
        ImageFormat::Png => {
            use image::ImageEncoder;
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};

            let rgb = bgra_to_rgb(bgra);
            PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive)
                .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
                .map_err(|err| err.to_string())?;
        }
        ImageFormat::Webp => {
            let rgb = bgra_to_rgb(bgra);
            let memory = webp::Encoder::from_rgb(&rgb, width, height)
                .encode(f32::from(quality.clamp(1, 100)));
            out.extend_from_slice(&memory);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        std::iter::repeat_n(px, (w * h) as usize)
            .flatten()
            .collect()
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
    fn every_format_encodes_a_bitmap() {
        let bgra = bitmap(16, 8, [255, 255, 255, 255]);
        for (format, magic) in [
            (ImageFormat::Jpeg, &b"\xFF\xD8"[..]),
            (ImageFormat::Png, &b"\x89PNG"[..]),
            (ImageFormat::Webp, &b"RIFF"[..]),
        ] {
            let bytes = encode(format, &bgra, 16, 8, 85).unwrap();
            assert!(bytes.starts_with(magic), "{format:?}");
        }
    }
}
