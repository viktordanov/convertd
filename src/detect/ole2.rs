//! OLE2 (Compound File Binary): legacy Office files and encrypted OOXML,
//! told apart by the names of the streams in the root storage. No stream
//! content is read.
//!
//! The `cfb` crate checks every sector index against the file length, so
//! what it allocates is bounded by the upload. At most [`MAX_ROOT_ENTRIES`]
//! names are looked at.

use std::io::{Read, Seek};

use crate::domain::detection::{DetectError, Detection};
use crate::domain::document::Format;

/// The root entries detection looks at.
pub const MAX_ROOT_ENTRIES: usize = 1024;

/// The format of an OLE2 file.
///
/// - `EncryptionInfo` and `EncryptedPackage`: encrypted OOXML. Its type is
///   inside the encrypted package, so the format is provisional (`docx`)
///   until the office engine reports the document type.
/// - `WordDocument`: `doc`. `Workbook` or `Book`: `xls`. `PowerPoint
///   Document`: `ppt` (a `.pps` looks the same).
/// - `__substg1.0_*`: an Outlook message, refused as `msg`.
/// - Anything else, or a file `cfb` cannot read, is refused as `ole2`.
pub fn detect<R: Read + Seek>(file: &mut R) -> Result<Detection, DetectError> {
    let unsupported = |label| DetectError::Unsupported { label };
    let compound = cfb::CompoundFile::open(file).map_err(|_| unsupported("ole2"))?;
    let names: Vec<String> = compound
        .read_root_storage()
        .take(MAX_ROOT_ENTRIES)
        .map(|entry| entry.name().to_owned())
        .collect();
    let has = |wanted: &str| names.iter().any(|name| name == wanted);
    let detection = |format| Detection {
        format,
        encrypted: false,
    };
    if has("EncryptionInfo") && has("EncryptedPackage") {
        return Ok(Detection {
            format: Format::Docx,
            encrypted: true,
        });
    }
    if names.iter().any(|name| name.starts_with("__substg1.0_")) {
        return Err(unsupported("msg"));
    }
    if has("WordDocument") {
        return Ok(detection(Format::Doc));
    }
    if has("Workbook") || has("Book") {
        return Ok(detection(Format::Xls));
    }
    if has("PowerPoint Document") {
        return Ok(detection(Format::Ppt));
    }
    Err(unsupported("ole2"))
}
