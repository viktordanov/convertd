//! OpenDocument: the stored `mimetype` entry.

use crate::domain::detection::DetectError;
use crate::domain::document::Format;

/// The `mimetype` entry is read up to this many bytes.
pub const MIMETYPE_CAP: usize = 128;

/// The format the `mimetype` entry names. `base` is labelled `odb` and
/// anything else `odf`, both unsupported.
pub fn format(mimetype: &[u8]) -> Result<Format, DetectError> {
    let mimetype = mimetype.trim_ascii();
    match mimetype {
        b"application/vnd.oasis.opendocument.text" => Ok(Format::Odt),
        b"application/vnd.oasis.opendocument.presentation" => Ok(Format::Odp),
        b"application/vnd.oasis.opendocument.spreadsheet" => Ok(Format::Ods),
        b"application/vnd.oasis.opendocument.base" => {
            Err(DetectError::Unsupported { label: "odb" })
        }
        _ => Err(DetectError::Unsupported { label: "odf" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_types_and_the_rest() {
        assert_eq!(
            format(b"application/vnd.oasis.opendocument.text").unwrap(),
            Format::Odt
        );
        assert_eq!(
            format(b"application/vnd.oasis.opendocument.presentation").unwrap(),
            Format::Odp
        );
        assert_eq!(
            format(b"application/vnd.oasis.opendocument.spreadsheet\n").unwrap(),
            Format::Ods
        );
        for (mimetype, label) in [
            (&b"application/vnd.oasis.opendocument.base"[..], "odb"),
            (b"application/vnd.oasis.opendocument.graphics", "odf"),
            (b"application/epub+zip", "odf"),
        ] {
            match format(mimetype) {
                Err(DetectError::Unsupported { label: got }) => assert_eq!(got, label),
                other => panic!("{other:?}"),
            }
        }
    }
}
