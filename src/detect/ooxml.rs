//! OOXML: the main part's content type in `[Content_Types].xml`.

use crate::domain::document::Format;

/// `[Content_Types].xml` is read up to this many inflated bytes.
pub const CONTENT_TYPES_CAP: usize = 1024 * 1024;

/// The main-part content types, one per accepted OOXML format.
const MAIN_PARTS: &[(&[u8], Format)] = &[
    (
        b"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        Format::Docx,
    ),
    (
        b"application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml",
        Format::Dotx,
    ),
    (
        b"application/vnd.ms-word.document.macroEnabled.main+xml",
        Format::Docm,
    ),
    (
        b"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        Format::Pptx,
    ),
    (
        b"application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml",
        Format::Ppsx,
    ),
    (
        b"application/vnd.openxmlformats-officedocument.presentationml.template.main+xml",
        Format::Potx,
    ),
    (
        b"application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml",
        Format::Pptm,
    ),
    (
        b"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
        Format::Xlsx,
    ),
    (
        b"application/vnd.ms-excel.sheet.macroEnabled.main+xml",
        Format::Xlsm,
    ),
];

/// The format whose main part `content_types` declares. `None` when it
/// declares none of ours, or more than one.
pub fn format(content_types: &[u8]) -> Option<Format> {
    let mut found = None;
    for &(content_type, format) in MAIN_PARTS {
        if memchr::memmem::find(content_types, content_type).is_some() {
            if found.is_some() {
                return None;
            }
            found = Some(format);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(main: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/x.xml" ContentType="{main}"/></Types>"#
        )
        .into_bytes()
    }

    #[test]
    fn each_main_part() {
        for &(content_type, want) in MAIN_PARTS {
            let text = std::str::from_utf8(content_type).unwrap();
            assert_eq!(format(&types(text)), Some(want), "{text}");
        }
    }

    #[test]
    fn none_or_several() {
        assert_eq!(
            format(&types(
                "application/vnd.ms-excel.sheet.binary.macroEnabled.main"
            )),
            None
        );
        let mut both = types(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        );
        both.extend_from_slice(b"application/vnd.ms-excel.sheet.macroEnabled.main+xml");
        assert_eq!(format(&both), None);
    }
}
