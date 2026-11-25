//! Signatures in the first bytes of the upload.

use crate::domain::document::Format;

/// How many leading bytes detection reads.
pub const HEAD_LEN: usize = 4096;

/// What the first bytes say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniff {
    Format(Format),
    /// `PK\x03\x04`: OOXML, ODF or another ZIP.
    Zip,
    /// `D0 CF 11 E0 A1 B1 1A E1`: legacy Office or encrypted OOXML.
    Ole2,
    /// Not accepted; the label names what was seen, for the 415 message.
    Unsupported(&'static str),
}

const OLE2: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Classifies `head`, the first bytes of the file (up to [`HEAD_LEN`]).
pub fn sniff(head: &[u8]) -> Sniff {
    if memchr::memmem::find(&head[..head.len().min(1024)], b"%PDF-").is_some() {
        return Sniff::Format(Format::Pdf);
    }
    if head.starts_with(b"{\\rtf") {
        return Sniff::Format(Format::Rtf);
    }
    if head.starts_with(PNG) {
        return Sniff::Format(Format::Png);
    }
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Sniff::Format(Format::Jpeg);
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Sniff::Format(Format::Gif);
    }
    if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        return Sniff::Format(Format::Tiff);
    }
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return Sniff::Format(Format::Webp);
    }
    if is_bmp(head) {
        return Sniff::Format(Format::Bmp);
    }
    if head.starts_with(b"PK\x03\x04") {
        return Sniff::Zip;
    }
    if head.starts_with(OLE2) {
        return Sniff::Ole2;
    }
    Sniff::Unsupported(label(head))
}

/// `BM` and a DIB header size of 12, 40, 52, 56, 108 or 124.
fn is_bmp(head: &[u8]) -> bool {
    head.starts_with(b"BM")
        && head
            .get(14..18)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .is_some_and(|size| matches!(size, 12 | 40 | 52 | 56 | 108 | 124))
}

/// A label for something we do not accept: `html`, `xml`, `zip`, `text`
/// or `unknown`.
fn label(head: &[u8]) -> &'static str {
    if head.starts_with(b"PK\x05\x06") {
        return "zip";
    }
    let text = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let start = text
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(text.len());
    let text = &text[start..];
    let starts_with = |prefix: &[u8]| {
        text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    if starts_with(b"<!doctype html") || starts_with(b"<html") {
        return "html";
    }
    if starts_with(b"<?xml") {
        return "xml";
    }
    if !head.is_empty() && std::str::from_utf8(head).is_ok() && !head.contains(&0) {
        return "text";
    }
    "unknown"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0; 12]);
        bmp.extend_from_slice(&40u32.to_le_bytes());
        let cases: [(&[u8], Sniff); 11] = [
            (b"%PDF-1.7\n", Sniff::Format(Format::Pdf)),
            (b"\x00\x00junk before %PDF-1.4", Sniff::Format(Format::Pdf)),
            (b"{\\rtf1\\ansi", Sniff::Format(Format::Rtf)),
            (b"\x89PNG\r\n\x1a\n....", Sniff::Format(Format::Png)),
            (b"\xFF\xD8\xFF\xE0", Sniff::Format(Format::Jpeg)),
            (b"GIF89a", Sniff::Format(Format::Gif)),
            (b"MM\x00*", Sniff::Format(Format::Tiff)),
            (b"RIFF\x10\x00\x00\x00WEBPVP8 ", Sniff::Format(Format::Webp)),
            (&bmp, Sniff::Format(Format::Bmp)),
            (b"PK\x03\x04", Sniff::Zip),
            (b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1", Sniff::Ole2),
        ];
        for (head, want) in cases {
            assert_eq!(sniff(head), want, "{:?}", String::from_utf8_lossy(head));
        }
        let mut late = vec![b' '; 1020];
        late.extend_from_slice(b"%PDF-1.4");
        assert_ne!(sniff(&late), Sniff::Format(Format::Pdf));
    }

    #[test]
    fn labels() {
        assert_eq!(
            sniff(b"\xEF\xBB\xBF  \n<!DOCTYPE html><html>"),
            Sniff::Unsupported("html")
        );
        assert_eq!(sniff(b"<html><body>"), Sniff::Unsupported("html"));
        assert_eq!(
            sniff(b"<?xml version=\"1.0\"?><a/>"),
            Sniff::Unsupported("xml")
        );
        assert_eq!(sniff(b"hello, world\n"), Sniff::Unsupported("text"));
        assert_eq!(sniff(b"\x00\x01\x02\x03"), Sniff::Unsupported("unknown"));
        assert_eq!(sniff(b""), Sniff::Unsupported("unknown"));
        assert_eq!(sniff(b"BM\x00\x00"), Sniff::Unsupported("unknown"));
        assert_eq!(sniff(b"PK\x05\x06\0\0"), Sniff::Unsupported("zip"));
    }
}
