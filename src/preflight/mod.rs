//! Pure pre-flight scanners over an office source.
//!
//! The office worker calls [`scan`] on the source file before LibreOffice
//! loads it and returns the facts in `Converted.facts`. It reads the
//! declared slide count
//! (V1, PPTX), the requested font families (V2), the macro storage (V4)
//! and the distinct code points of the text (`GLYPHS_MISSING`). No tokio,
//! no network, no engine calls.
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
use crate::domain::text::needs_glyph;
use crate::domain::verification::{
    Check, MAX_CODE_POINTS, MacroFact, MacroKind, SkipReason, SourceFacts,
};

use package::{Package, XmlEvent};
use quick_xml::events::BytesStart;

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

/// What the export prints beyond the visible slides, so that the text the
/// glyph check reads is the text of the PDF.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Printed {
    /// The notes pages (`slides.notes` is `append` or `only`).
    pub notes: bool,
    /// The hidden slides (`slides.include_hidden`).
    pub hidden_slides: bool,
    /// The hidden sheets: `sheets.one_page_per_sheet` exports whole
    /// sheets, hidden ones included.
    pub hidden_sheets: bool,
}

/// Reads the facts that the warning rules need from an office source.
///
/// Never fails: a part that is missing, malformed or over a cap is recorded
/// in `SourceFacts::skipped`, and the check it feeds is skipped.
pub fn scan(source: &Path, family: Family, printed: Printed) -> SourceFacts {
    let mut facts = SourceFacts::default();
    let Ok(mut file) = File::open(source) else {
        skip_all(&mut facts, SkipReason::MissingPart);
        return facts;
    };
    let mut head = [0u8; 8];
    let read = read_head(&mut file, &mut head);
    if head[..read].starts_with(b"PK\x03\x04") {
        match Package::open(file) {
            Some(package) => scan_package(package, family, printed, &mut facts),
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
    for check in [Check::PageCount, Check::Fonts, Check::Glyphs, Check::Macros] {
        facts.skipped.push((check, reason));
    }
}

fn scan_package(mut package: Package, family: Family, printed: Printed, facts: &mut SourceFacts) {
    let mut text = TextCollector::new();
    if package.has("mimetype") {
        odf::scan(&mut package, family, &mut text, printed, facts);
        finish(text, facts);
        return;
    }
    if !package.has("[Content_Types].xml") {
        skip_all(facts, SkipReason::MissingPart);
        return;
    }
    facts.macros = ooxml_macros(&mut package, facts);
    let mut fonts = BTreeSet::new();
    match family {
        Family::Document => docx::scan(&mut package, &mut fonts, &mut text, facts),
        Family::Presentation => pptx::scan(&mut package, &mut fonts, &mut text, printed, facts),
        Family::Spreadsheet => xlsx::scan(&mut package, &mut fonts, &mut text, printed, facts),
        Family::Pdf | Family::Image => {}
    }
    facts.requested_fonts = fonts.into_iter().collect();
    finish(text, facts);
}

/// Moves the code points into `facts`, sorted; past the cap the glyph
/// check is skipped with `cap_reached` and nothing is kept. When a text part
/// could not be read, the scanner has recorded a glyphs skip and nothing
/// is kept: the check does not run on a failed scan.
fn finish(text: TextCollector, facts: &mut SourceFacts) {
    if facts
        .skipped
        .iter()
        .any(|(check, _)| *check == Check::Glyphs)
    {
        facts.text_code_points.clear();
        return;
    }
    // Past the cap the check is skipped, not run on the code points that
    // happened to come first.
    if text.full {
        facts.skipped.push((Check::Glyphs, SkipReason::CapReached));
        return;
    }
    facts.text_code_points = text.into_sorted();
}

/// The distinct code points of the text read so far that need a glyph
/// ([`needs_glyph`]), at most [`MAX_CODE_POINTS`]. A bit per code point,
/// so a character costs the same whatever the text holds.
struct TextCollector {
    seen: Vec<u64>,
    kept: Vec<u32>,
    full: bool,
}

impl TextCollector {
    fn new() -> Self {
        Self {
            seen: vec![0; 0x11_0000 / 64],
            kept: Vec::new(),
            full: false,
        }
    }

    /// Adds the characters of `text`. A character is looked up once, needed
    /// or not; after the first one past the cap, nothing can change the
    /// result, so the rest is not looked at.
    fn add(&mut self, text: &str) {
        for c in text.chars() {
            if !self.add_cp(u32::from(c)) {
                return;
            }
        }
    }

    /// Adds one code point; false once the cap is passed.
    fn add_cp(&mut self, cp: u32) -> bool {
        if self.full {
            return false;
        }
        let (word, bit) = ((cp / 64) as usize, 1u64 << (cp % 64));
        let Some(slot) = self.seen.get_mut(word) else {
            return true;
        };
        if *slot & bit != 0 {
            return true;
        }
        if !needs_glyph(cp) {
            *slot |= bit;
            return true;
        }
        if self.kept.len() >= MAX_CODE_POINTS {
            self.full = true;
            return false;
        }
        *slot |= bit;
        self.kept.push(cp);
        true
    }

    /// Adds what `other` collected, and its cap.
    fn absorb(&mut self, other: TextCollector) {
        for &cp in &other.kept {
            if !self.add_cp(cp) {
                return;
            }
        }
        self.full |= other.full;
    }

    fn into_sorted(self) -> Vec<u32> {
        let mut kept = self.kept;
        kept.sort_unstable();
        kept
    }
}

/// The most qualified names that [`TextScope`] keeps for `skip_ns`.
const MAX_ALIASES: usize = 8;

/// Picks the drawn text out of one part's events: the text of an element
/// named in `text` (by local name), or with `text` empty of any element
/// inside `within`, and never inside an element named in `skip` (by local
/// name) or in `skip_ns` (by namespace and local name; the prefixes come
/// from the `xmlns` declarations of the root, where OOXML and ODF declare
/// them).
struct TextScope {
    text: &'static [&'static [u8]],
    within: Option<&'static [u8]>,
    skip: &'static [&'static [u8]],
    skip_ns: &'static [(&'static [u8], &'static [u8])],
    /// The qualified names of `skip_ns` under the root's prefixes.
    skip_qualified: Vec<Vec<u8>>,
    depth: usize,
    text_at: Option<usize>,
    within_at: Option<usize>,
    skip_at: Option<usize>,
    /// A root element was seen.
    rooted: bool,
    /// Markup or character data outside the one root.
    outside: bool,
    /// Drawn character data whose text is unknown.
    unresolved: bool,
}

impl TextScope {
    fn new(
        text: &'static [&'static [u8]],
        within: Option<&'static [u8]>,
        skip: &'static [&'static [u8]],
    ) -> Self {
        Self {
            text,
            within,
            skip,
            skip_ns: &[],
            skip_qualified: Vec::new(),
            depth: 0,
            text_at: None,
            within_at: None,
            skip_at: None,
            rooted: false,
            outside: false,
            unresolved: false,
        }
    }

    /// Also skips these `(namespace, local name)` elements.
    fn skipping_ns(mut self, skip_ns: &'static [(&'static [u8], &'static [u8])]) -> Self {
        self.skip_ns = skip_ns;
        self
    }

    /// Whether the part read as a whole: one root that closed, nothing
    /// outside it, and no drawn text left unknown. quick-xml ends a part
    /// cut after a complete tag without an error, so a text scan checks
    /// this after the read.
    fn complete(&self) -> Result<(), SkipReason> {
        if self.rooted && self.depth == 0 && !self.outside && !self.unresolved {
            Ok(())
        } else {
            Err(SkipReason::MalformedXml)
        }
    }

    /// Skips the element whose start tag was the last event.
    fn skip_current(&mut self) {
        if let Some(depth) = self.depth.checked_sub(1) {
            self.skip_at.get_or_insert(depth);
        }
    }

    fn drawn(&self) -> bool {
        let within = self.within.is_none() || self.within_at.is_some();
        let named = self.text.is_empty() || self.text_at.is_some();
        within && named && self.skip_at.is_none()
    }

    /// Records the prefixes that the root binds to the namespaces of
    /// `skip_ns`, at most [`MAX_ALIASES`] names: a part that binds one
    /// namespace to more prefixes keeps the first ones, and an element under
    /// another alias counts as printed text.
    fn bind(&mut self, root: &BytesStart<'_>) {
        for attribute in root.attributes().with_checks(false).flatten() {
            let key = attribute.key.as_ref();
            let prefix: &[u8] = if key == b"xmlns" {
                b""
            } else if let Some(prefix) = key.strip_prefix(b"xmlns:") {
                prefix
            } else {
                continue;
            };
            for (namespace, local) in self.skip_ns {
                if attribute.value.as_ref() == *namespace && self.skip_qualified.len() < MAX_ALIASES
                {
                    let mut name = prefix.to_vec();
                    if !name.is_empty() {
                        name.push(b':');
                    }
                    name.extend_from_slice(local);
                    if !self.skip_qualified.contains(&name) {
                        self.skip_qualified.push(name);
                    }
                }
            }
        }
    }

    /// The drawn text that `event` carries, if any.
    fn on<'e>(&mut self, event: &XmlEvent<'e, '_>) -> Option<&'e str> {
        match event {
            XmlEvent::Start(e) | XmlEvent::Empty(e) if self.depth == 0 => {
                if self.rooted {
                    self.outside = true;
                    return None;
                }
                self.rooted = true;
                self.bind(e);
                self.open(e, matches!(event, XmlEvent::Start(_)));
                None
            }
            XmlEvent::Start(e) => {
                self.open(e, true);
                None
            }
            XmlEvent::Empty(_) => None,
            XmlEvent::End => {
                self.depth = self.depth.saturating_sub(1);
                for at in [&mut self.text_at, &mut self.within_at, &mut self.skip_at] {
                    if *at == Some(self.depth) {
                        *at = None;
                    }
                }
                None
            }
            XmlEvent::Text { text, stray } => {
                if self.depth == 0 {
                    self.outside |= *stray;
                    return None;
                }
                self.drawn().then_some(*text)
            }
            XmlEvent::Unresolved => {
                if self.depth == 0 {
                    self.outside = true;
                }
                self.unresolved |= self.drawn();
                None
            }
        }
    }

    fn open(&mut self, e: &BytesStart<'_>, has_content: bool) {
        if !has_content {
            return;
        }
        let local = e.local_name();
        let name = local.as_ref();
        let depth = self.depth;
        let qualified = e.name();
        // The aliases are looked at only for an element whose local name
        // is one of `skip_ns`, so the cost per element stays constant.
        let by_namespace = self.skip_ns.iter().any(|(_, local)| *local == name)
            && self
                .skip_qualified
                .iter()
                .any(|skip| skip.as_slice() == qualified.as_ref());
        if self.skip.contains(&name) || by_namespace {
            self.skip_at.get_or_insert(depth);
        }
        if self.within == Some(name) {
            self.within_at.get_or_insert(depth);
        }
        if self.text.contains(&name) {
            self.text_at.get_or_insert(depth);
        }
        self.depth += 1;
    }
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
