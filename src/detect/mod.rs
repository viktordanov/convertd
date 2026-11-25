//! Format detection over the uploaded file, safe and bounded (lane 001).
//!
//! Runs in the supervisor at the tail of the upload sink, while the file
//! is page-cache hot: the signature, then for a ZIP its central directory
//! and either `[Content_Types].xml` (OOXML) or `mimetype` (ODF). The type
//! comes from the bytes, never from the file name. OLE2 (legacy Office,
//! encrypted OOXML) arrives in plan step 10; until then it is 415.

pub mod odf;
pub mod ooxml;
pub mod signature;
pub mod zip_scan;

use std::io::{Read, Seek, SeekFrom};

use signature::Sniff;
use zip_scan::Entry;

use crate::domain::detection::{DetectError, Detection};

const CONTENT_TYPES: &[u8] = b"[Content_Types].xml";
const MIMETYPE: &[u8] = b"mimetype";

/// Detects the format of `file`, which is `size` bytes long.
pub fn detect_file<R: Read + Seek>(file: &mut R, size: u64) -> Result<Detection, DetectError> {
    file.seek(SeekFrom::Start(0))?;
    let mut head = Vec::with_capacity(signature::HEAD_LEN);
    (&mut *file)
        .take(signature::HEAD_LEN as u64)
        .read_to_end(&mut head)?;
    let format = match signature::sniff(&head) {
        Sniff::Format(format) => format,
        Sniff::Zip => zip_format(file, size)?,
        Sniff::Ole2 => return Err(DetectError::Unsupported { label: "ole2" }),
        Sniff::Unsupported(label) => return Err(DetectError::Unsupported { label }),
    };
    Ok(Detection {
        format,
        encrypted: false,
    })
}

fn zip_format<R: Read + Seek>(
    file: &mut R,
    size: u64,
) -> Result<crate::domain::document::Format, DetectError> {
    let mut found: [Option<Entry>; 2] = [None, None];
    zip_scan::scan(file, size, &[CONTENT_TYPES, MIMETYPE], &mut found)?;
    match found {
        [Some(_), Some(_)] => Err(DetectError::Unsupported { label: "zip" }),
        [Some(types), None] => {
            let bytes = zip_scan::read_entry(file, &types, ooxml::CONTENT_TYPES_CAP)?.ok_or(
                DetectError::LimitExceeded {
                    limit: "content types size",
                },
            )?;
            ooxml::format(&bytes).ok_or(DetectError::Unsupported { label: "ooxml" })
        }
        [None, Some(mimetype)] => {
            if mimetype.method != zip_scan::METHOD_STORED {
                return Err(DetectError::Unsupported { label: "odf" });
            }
            let bytes = zip_scan::read_entry(file, &mimetype, odf::MIMETYPE_CAP)?
                .ok_or(DetectError::Unsupported { label: "odf" })?;
            odf::format(&bytes)
        }
        [None, None] => Err(DetectError::Unsupported { label: "zip" }),
    }
}
