//! The `/BaseFont` names of a PDF, for the font check (V2).

use std::collections::BTreeSet;

/// Every `/BaseFont` name in `pdf`, object streams included, as written
/// (with any subset prefix such as `BAAAAA+`).
pub fn base_fonts(pdf: &[u8]) -> Result<BTreeSet<String>, String> {
    let doc =
        lopdf::Document::load_mem(pdf).map_err(|err| format!("failed to parse the PDF: {err}"))?;
    let mut names = BTreeSet::new();
    for object in doc.objects.values() {
        if let Ok(dict) = object.as_dict()
            && let Ok(name) = dict.get(b"BaseFont").and_then(lopdf::Object::as_name)
        {
            names.insert(String::from_utf8_lossy(name).into_owned());
        }
    }
    Ok(names)
}

/// A family or `/BaseFont` name reduced for comparison: no subset prefix,
/// no style suffix after `-` or `,`, no spaces, lower case. So
/// `BAAAAA+Carlito-Bold` and `Carlito` agree, and `AptosDisplay` and
/// `Aptos` do not.
pub fn font_key(name: &str) -> String {
    let name = match name.split_once('+') {
        Some((prefix, rest))
            if prefix.len() == 6 && prefix.bytes().all(|b| b.is_ascii_uppercase()) =>
        {
            rest
        }
        _ => name,
    };
    let family = name.split(['-', ',']).next().unwrap_or(name);
    family
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two pages, two fonts: a standard font and a subset name.
    const PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 4 0 R /F2 5 0 R >> >> >>\nendobj\n\
4 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n\
5 0 obj\n<< /Type /Font /Subtype /TrueType /BaseFont /BAAAAA+Carlito-Bold >>\nendobj\n\
trailer\n<< /Size 6 /Root 1 0 R >>\n%%EOF\n";

    #[test]
    fn base_fonts_lists_every_name() {
        let names = base_fonts(PDF).expect("parse");
        assert_eq!(
            names,
            BTreeSet::from(["BAAAAA+Carlito-Bold".to_string(), "Helvetica".to_string()])
        );
    }

    #[test]
    fn a_broken_pdf_is_an_error() {
        assert!(base_fonts(b"not a pdf").is_err());
    }

    #[test]
    fn font_keys_drop_prefix_style_and_spaces() {
        assert_eq!(font_key("BAAAAA+Carlito-Bold"), "carlito");
        assert_eq!(
            font_key("Liberation Serif"),
            font_key("LiberationSerif,Italic")
        );
        assert_ne!(font_key("AptosDisplay"), font_key("Aptos"));
        assert_eq!(font_key("Aptos Display"), font_key("AptosDisplay"));
    }
}
