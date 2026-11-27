//! The text and the image count of a PDF, for the `text_present`,
//! `text_absent` and `pdf_images` expectations (ledger item 011, S2).

/// The most decompressed content that one page may give, as in the service's
/// own limits: a corpus PDF is small, and a larger one is a failure.
const MAX_PAGE_CONTENT: usize = 64 << 20;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PdfFacts {
    /// The text of every page, in page order, with all white space removed,
    /// so that the line breaks and word gaps of the extraction do not decide
    /// a match.
    pub text: String,
    /// The image XObjects in the file, soft masks included.
    pub images: usize,
}

/// Reads the text of every page and counts the image XObjects of `pdf`.
pub fn read(pdf: &[u8]) -> Result<PdfFacts, String> {
    let doc =
        lopdf::Document::load_mem(pdf).map_err(|err| format!("failed to parse the PDF: {err}"))?;
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    let text = doc
        .extract_text_with_limit(&pages, MAX_PAGE_CONTENT)
        .map_err(|err| format!("failed to extract the PDF text: {err}"))?;
    let images = doc
        .objects
        .values()
        .filter(|object| {
            object.as_stream().is_ok_and(|stream| {
                stream
                    .dict
                    .get(b"Subtype")
                    .and_then(lopdf::Object::as_name)
                    .is_ok_and(|name| name == b"Image")
            })
        })
        .count();
    Ok(PdfFacts {
        text: squeeze(&text),
        images,
    })
}

/// `text` without white space, the form that [`PdfFacts::text`] holds.
pub fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One page with two lines of text in a standard font, and one image
    /// XObject. The xref offsets are not checked by lopdf's loader, which
    /// rebuilds a broken table.
    fn pdf() -> Vec<u8> {
        let content = b"BT /F1 12 Tf 72 720 Td (Before the link.) Tj 0 -14 Td (CACHED FIELD) Tj ( RESULT) Tj ET";
        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n");
        out.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        out.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
        out.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] \
              /Resources << /Font << /F1 4 0 R >> /XObject << /Im1 6 0 R >> >> /Contents 5 0 R >>\nendobj\n",
        );
        out.extend_from_slice(
            b"4 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>\nendobj\n",
        );
        out.extend_from_slice(
            format!("5 0 obj\n<< /Length {} >>\nstream\n", content.len()).as_bytes(),
        );
        out.extend_from_slice(content);
        out.extend_from_slice(b"\nendstream\nendobj\n");
        out.extend_from_slice(
            b"6 0 obj\n<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
              /BitsPerComponent 8 /Length 1 >>\nstream\n\x80\nendstream\nendobj\n",
        );
        out.extend_from_slice(b"trailer\n<< /Size 7 /Root 1 0 R >>\n%%EOF\n");
        out
    }

    #[test]
    fn reads_the_text_without_white_space_and_counts_images() {
        let facts = read(&pdf()).expect("parse");
        assert!(
            facts.text.contains(&squeeze("CACHED FIELD RESULT")),
            "{}",
            facts.text
        );
        assert!(facts.text.contains("Beforethelink."), "{}", facts.text);
        assert!(!facts.text.contains(&squeeze("Debian GNU/Linux")));
        assert_eq!(facts.images, 1);
    }

    #[test]
    fn a_broken_pdf_is_an_error() {
        assert!(read(b"not a pdf").is_err());
    }
}
