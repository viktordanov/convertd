//! Pure pre-flight scanners over an office source (lane 001).
//!
//! The office worker (lane 002) calls [`scan`] on the source file before
//! LibreOffice loads it and returns the facts in `Converted.facts`
//! (INTERFACES.md §13). The algorithms are 005 §5: the declared slide count
//! (V1, PPTX), the requested font families (V2) and the macro storage
//! (V4). No tokio, no network, no engine calls.
//!
//! It must never panic: the worker's panic hook ends the process, which
//! would turn a good conversion into `CONVERSION_FAILED`. A part that is
//! missing, malformed or over a cap skips the check it feeds, and the skip
//! is recorded in `SourceFacts::skipped`.
//!
//! Caps:
//! - 4 MiB inflated per small part;
//! - 32 MiB for the parts one font scan reads together;
//! - 64 MiB inflated per request across all parts;
//! - at most 64 header and footer parts and 10 000 slides;
//! - the zip-bomb caps of detection.
//!
//! Every part streams through one `DeflateDecoder` into quick-xml with a
//! reused buffer; nothing is inflated whole. quick-xml expands no DTD
//! entity, so an entity bomb is malformed XML, not memory.

mod docx;
mod odf;
mod package;
mod pptx;
mod xlsx;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::domain::document::Family;
use crate::domain::verification::{Check, MacroFact, MacroKind, SkipReason, SourceFacts};

use package::Package;

/// Inflated bytes per small part.
pub const PART_CAP: u64 = 4 * 1024 * 1024;
/// Inflated bytes for the parts one font scan reads together.
pub const FONT_SCAN_CAP: u64 = 32 * 1024 * 1024;
/// Inflated bytes per request.
pub const REQUEST_CAP: u64 = 64 * 1024 * 1024;
/// Requested families kept.
pub const MAX_FONTS: usize = 256;
/// The longest family name kept, in bytes. OOXML and ODF names are far
/// shorter (Word allows 31 characters); a longer one is not a font name,
/// and it would only grow the facts that cross IPC.
pub const MAX_FONT_NAME: usize = 256;

/// Reads the facts that the warning rules need from an office source.
///
/// Never fails: a part that is missing, malformed or over a cap is recorded
/// in `SourceFacts::skipped`, and the check it feeds is skipped.
pub fn scan(source: &Path, family: Family) -> SourceFacts {
    let mut facts = SourceFacts::default();
    let Ok(mut file) = File::open(source) else {
        skip_all(&mut facts, SkipReason::MissingPart);
        return facts;
    };
    let mut head = [0u8; 8];
    let read = read_head(&mut file, &mut head);
    if head[..read].starts_with(b"PK\x03\x04") {
        match Package::open(file) {
            Some(package) => scan_package(package, family, &mut facts),
            None => skip_all(&mut facts, SkipReason::MalformedXml),
        }
    } else if head[..read].starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        scan_ole2(file, &mut facts);
    }
    // RTF and others: no declared count, fonts or macro storage to read.
    facts
}

fn read_head(file: &mut File, head: &mut [u8]) -> usize {
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => read += n,
        }
    }
    read
}

fn skip_all(facts: &mut SourceFacts, reason: SkipReason) {
    for check in [Check::PageCount, Check::Fonts, Check::Macros] {
        facts.skipped.push((check, reason));
    }
}

fn scan_package(mut package: Package, family: Family, facts: &mut SourceFacts) {
    if package.has("mimetype") {
        odf::scan(&mut package, facts);
        return;
    }
    if !package.has("[Content_Types].xml") {
        skip_all(facts, SkipReason::MissingPart);
        return;
    }
    facts.macros = ooxml_macros(&mut package, facts);
    let mut fonts = BTreeSet::new();
    match family {
        Family::Document => docx::scan(&mut package, &mut fonts, facts),
        Family::Presentation => pptx::scan(&mut package, &mut fonts, facts),
        Family::Spreadsheet => xlsx::scan(&mut package, &mut fonts, facts),
        Family::Pdf | Family::Image => {}
    }
    facts.requested_fonts = fonts.into_iter().collect();
}

/// Adds a family name read from a part, unless it is empty, a theme
/// reference such as `+mn-lt`, longer than [`MAX_FONT_NAME`], or over
/// [`MAX_FONTS`].
fn add_font(fonts: &mut BTreeSet<String>, name: &str) {
    let name = name.trim();
    if !name.is_empty()
        && !name.starts_with('+')
        && name.len() <= MAX_FONT_NAME
        && fonts.len() < MAX_FONTS
    {
        fonts.insert(name.to_owned());
    }
}

/// V4 for OOXML: a `{word,xl,ppt}/vbaProject.bin` part or the vbaProject
/// content type gives `vba`; a macrosheet content type gives `xlm`. Never
/// the main content type alone (a `docm` may hold no VBA).
fn ooxml_macros(package: &mut Package, facts: &mut SourceFacts) -> Option<MacroFact> {
    for part in [
        "word/vbaProject.bin",
        "xl/vbaProject.bin",
        "ppt/vbaProject.bin",
    ] {
        if package.has(part) {
            return Some(MacroFact {
                kind: MacroKind::Vba,
                part: part.to_owned(),
            });
        }
    }
    let mut found = None;
    let result = package.xml("[Content_Types].xml", PART_CAP, |e| {
        let local = e.local_name();
        if local.as_ref() != b"Override" && local.as_ref() != b"Default" {
            return true;
        }
        let content_type = package::attr(e, b"ContentType").unwrap_or_default();
        let kind = if content_type == "application/vnd.ms-office.vbaProject" {
            MacroKind::Vba
        } else if content_type.ends_with("ms-excel.macrosheet+xml")
            || content_type.ends_with("ms-excel.intlmacrosheet+xml")
        {
            MacroKind::Xlm
        } else {
            return true;
        };
        let part = package::attr(e, b"PartName")
            .map(|name| name.trim_start_matches('/').to_owned())
            .unwrap_or_else(|| "[Content_Types].xml".to_owned());
        found = Some(MacroFact { kind, part });
        false
    });
    if let Err(reason) = result {
        facts.skipped.push((Check::Macros, reason));
    }
    found
}

/// V4 for OLE2: a root storage `Macros` (`.doc`) or `_VBA_PROJECT_CUR`
/// (`.xls`) gives `vba`. Encrypted OOXML cannot be read: every check is
/// skipped.
fn scan_ole2(mut file: File, facts: &mut SourceFacts) {
    if file.seek(SeekFrom::Start(0)).is_err() {
        skip_all(facts, SkipReason::MissingPart);
        return;
    }
    // The supervisor's bounded reader: a hostile FAT costs the worker nothing.
    let Some(names) = crate::detect::ole2::root_names(&mut file) else {
        skip_all(facts, SkipReason::MalformedXml);
        return;
    };
    if names.iter().any(|name| name == "EncryptedPackage") {
        skip_all(facts, SkipReason::MissingPart);
        return;
    }
    facts.macros = ["Macros", "_VBA_PROJECT_CUR"]
        .into_iter()
        .find(|storage| names.iter().any(|name| name == storage))
        .map(|storage| MacroFact {
            kind: MacroKind::Vba,
            part: storage.to_owned(),
        });
}

#[cfg(test)]
mod tests;
