//! DOCX family (docx, docm, dotx): the requested Latin families (V2).
//! V1 is off for DOCX (005 §5: `docProps/app.xml` is not a reliable gate).
//!
//! Families come from `w:rFonts@w:ascii|w:hAnsi` in the main part and its
//! headers, footers, footnotes and endnotes; from `styles.xml`, only the
//! document defaults, the default paragraph and character styles, and the
//! styles the scanned parts reference, with their `w:basedOn` chain. Theme
//! references (`minorHAnsi`, `majorAscii`, …) resolve through the theme's
//! `a:minorFont|a:majorFont/a:latin@typeface`.
//!
//! Embedded families come from the font table that the main part links
//! (`word/fontTable.xml`): a requested family whose `w:font` has a
//! `w:embedRegular`, `w:embedBold`, `w:embedItalic` or `w:embedBoldItalic`
//! that points, through the table's relationships, at a font part the
//! package holds. Obfuscated (`w:fontKey`) and plain parts count alike.

use std::collections::{BTreeSet, HashMap, HashSet};

use quick_xml::events::BytesStart;

use super::package::{Package, XmlEvent, attr, attr_prefixed, rels_path, resolve};
use super::{FONT_SCAN_CAP, PART_CAP, add_font};
use crate::domain::verification::{Check, SkipReason, SourceFacts};

/// At most this many styles are kept.
const MAX_STYLES: usize = 4096;
/// At most this many header and footer parts are read.
const MAX_HEADERS_FOOTERS: usize = 64;

/// The major and minor Latin typefaces of a theme.
#[derive(Debug, Default, Clone)]
pub(super) struct ThemeFonts {
    pub major: Option<String>,
    pub minor: Option<String>,
}

/// Reads `a:majorFont/a:latin` and `a:minorFont/a:latin` from a theme part.
pub(super) fn theme_fonts(package: &mut Package, part: &str) -> ThemeFonts {
    let mut theme = ThemeFonts::default();
    let mut group: Option<bool> = None; // Some(true): major
    let _ = package.xml(part, PART_CAP, |e| {
        match e.local_name().as_ref() {
            b"majorFont" => group = Some(true),
            b"minorFont" => group = Some(false),
            b"latin" => {
                if let Some(major) = group.take() {
                    let face = attr(e, b"typeface");
                    if major {
                        theme.major = face;
                    } else {
                        theme.minor = face;
                    }
                }
            }
            _ => {}
        }
        theme.major.is_none() || theme.minor.is_none()
    });
    theme
}

/// The Latin families an `w:rFonts` element names: `ascii` and `hAnsi`,
/// each replaced by its theme font when `asciiTheme` or `hAnsiTheme` is
/// set.
fn rfonts(e: &BytesStart<'_>, theme: &ThemeFonts) -> Vec<String> {
    let mut out = Vec::with_capacity(2);
    for (literal, themed) in [
        (&b"ascii"[..], &b"asciiTheme"[..]),
        (b"hAnsi", b"hAnsiTheme"),
    ] {
        let value = match attr(e, themed) {
            Some(reference) => match reference.as_str() {
                "majorHAnsi" | "majorAscii" => theme.major.clone(),
                "minorHAnsi" | "minorAscii" => theme.minor.clone(),
                _ => None,
            },
            None => attr(e, literal),
        };
        out.extend(value);
    }
    out
}

#[derive(Debug, Default)]
struct Style {
    based_on: Option<String>,
    fonts: Vec<String>,
}

pub(super) fn scan(package: &mut Package, fonts: &mut BTreeSet<String>, facts: &mut SourceFacts) {
    let main = main_part(package).unwrap_or_else(|| "word/document.xml".to_owned());
    // An unreadable relationships part leaves the theme and styles to their
    // usual names, but the font table cannot be found without it.
    let (rels, rels_error) = match package.rels(&main) {
        Ok(rels) => (rels, None),
        Err(SkipReason::MissingPart) if !package.has(&rels_path(&main)) => {
            (Default::default(), None)
        }
        Err(reason) => (Default::default(), Some(reason)),
    };
    let target = |suffix: &str| {
        rels.values()
            .filter(|(kind, _)| kind.ends_with(suffix))
            .map(|(_, target)| target.clone())
            .collect::<Vec<_>>()
    };
    let theme_part = target("/theme")
        .into_iter()
        .next()
        .unwrap_or_else(|| "word/theme/theme1.xml".to_owned());
    let styles_part = target("/styles")
        .into_iter()
        .next()
        .unwrap_or_else(|| "word/styles.xml".to_owned());
    // Only a table the main part links by the font-table type: a leftover
    // table is not loaded. Sorted, so that the choice never depends on map
    // order.
    let font_table_part = {
        let mut tables: Vec<&String> = rels
            .values()
            .filter(|(kind, _)| FONT_TABLE_TYPES.contains(&kind.as_str()))
            .map(|(_, target)| target)
            .collect();
        tables.sort();
        tables.first().map(|target| (*target).clone())
    };
    let theme = theme_fonts(package, &theme_part);

    // The parts with runs: the main part, then headers, footers, notes.
    let mut parts = vec![main.clone()];
    let mut headers_footers: Vec<String> = target("/header");
    headers_footers.extend(target("/footer"));
    headers_footers.sort();
    headers_footers.truncate(MAX_HEADERS_FOOTERS);
    parts.extend(headers_footers);
    parts.extend(target("/footnotes"));
    parts.extend(target("/endnotes"));

    let mut found = BTreeSet::new();
    let mut referenced: HashSet<String> = HashSet::new();
    let mut budget = FONT_SCAN_CAP;
    for part in &parts {
        let result = package.xml(part, budget, |e| {
            match e.local_name().as_ref() {
                b"rFonts" => {
                    for name in rfonts(e, &theme) {
                        add_font(&mut found, &name);
                    }
                }
                b"pStyle" | b"rStyle" | b"tblStyle" => {
                    if let Some(id) = attr(e, b"val")
                        && referenced.len() < MAX_STYLES
                    {
                        referenced.insert(id);
                    }
                }
                _ => {}
            }
            true
        });
        match result {
            Ok(read) => budget = budget.saturating_sub(read),
            // Headers and notes are optional; the main part is not.
            Err(SkipReason::MissingPart) if part != &main => {}
            Err(reason) => {
                facts.skipped.push((Check::Fonts, reason));
                return;
            }
        }
    }

    match read_styles(package, &styles_part, &theme) {
        Ok((defaults, styles)) => {
            for name in defaults {
                add_font(&mut found, &name);
            }
            let mut wanted: Vec<String> = styles
                .iter()
                .filter(|(_, (_, is_default))| *is_default)
                .map(|(id, _)| id.clone())
                .collect();
            wanted.extend(referenced);
            let mut seen = HashSet::new();
            for id in wanted {
                // Follow basedOn, at most MAX_STYLES steps, never in a loop.
                let mut next = Some(id);
                while let Some(id) = next.take() {
                    if !seen.insert(id.clone()) || seen.len() > MAX_STYLES {
                        break;
                    }
                    if let Some((style, _)) = styles.get(&id) {
                        for name in &style.fonts {
                            add_font(&mut found, name);
                        }
                        next = style.based_on.clone();
                    }
                }
            }
        }
        Err(SkipReason::MissingPart) => {}
        Err(reason) => {
            facts.skipped.push((Check::Fonts, reason));
            return;
        }
    }
    if let Some(reason) = rels_error {
        facts.skipped.push((Check::Fonts, reason));
        return;
    }
    match embedded_fonts(package, font_table_part.as_deref(), &found) {
        Ok(embedded) => facts.embedded_fonts = embedded.into_iter().collect(),
        Err(reason) => {
            facts.skipped.push((Check::Fonts, reason));
            return;
        }
    }
    fonts.extend(found);
}

/// The requested families that `part`, the font table linked from the
/// main part, embeds. A family counts when one of its embed elements, a
/// direct child of its `w:font`, points through the table's relationships
/// at a font part whose first bytes, deobfuscated with the element's
/// `w:fontKey`, are an sfnt header. Only requested families are kept, so
/// the result is bounded by the requested set and an unrelated family
/// cannot crowd one out; no name is copied per element. No linked table,
/// or a linked table absent from the package, embeds nothing; a table or
/// relationships part that is present but cannot be read is an error,
/// because the font check must not run without knowing what is embedded.
fn embedded_fonts(
    package: &mut Package,
    part: Option<&str>,
    requested: &BTreeSet<String>,
) -> Result<BTreeSet<String>, SkipReason> {
    let Some(part) = part else {
        return Ok(BTreeSet::new());
    };
    if !package.has(part) {
        return Ok(BTreeSet::new());
    }
    let rels = if package.has(&rels_path(part)) {
        package.rels(part)?
    } else {
        Default::default()
    };
    // Relationship id to font part, for the parts the package holds.
    let font_parts: HashMap<String, String> = rels
        .into_iter()
        .filter(|(_, (kind, target))| FONT_TYPES.contains(&kind.as_str()) && package.has(target))
        .map(|(id, (_, target))| (id, target))
        .collect();
    // Requested families by index, so that an embed element costs no hash
    // of a family name.
    let families: Vec<&String> = requested.iter().collect();
    let wanted: HashMap<String, usize> = families
        .iter()
        .enumerate()
        .map(|(index, name)| (fold(name), index))
        .collect();

    // (family index, font part, obfuscation key), at most four per family.
    let mut candidates: Vec<(usize, &String, Option<[u8; 16]>)> = Vec::new();
    let mut per_family = vec![0u8; families.len()];
    // The requested family of the open `w:font`, and the depth below it.
    let mut current: Option<(Option<usize>, usize)> = None;
    // Elements open in the whole table: a table cut short reads as EOF
    // with some still open, and is malformed rather than complete.
    let mut open = 0usize;
    // Whether the root element is `w:fonts`; `None` until one is seen. A
    // part with no element at all is malformed, not an empty table.
    let mut root: Option<bool> = None;
    let mut second_root = false;
    package.xml_events(part, PART_CAP, |event| {
        let (e, opens) = match event {
            XmlEvent::Start(e) => {
                open += 1;
                (e, true)
            }
            XmlEvent::Empty(e) => (e, false),
            XmlEvent::End => {
                open = open.saturating_sub(1);
                current = match current {
                    Some((family, depth)) if depth > 0 => Some((family, depth - 1)),
                    _ => None,
                };
                return true;
            }
        };
        if root.is_none() {
            root = Some(e.local_name().as_ref() == b"fonts");
            return true;
        }
        // A second top-level element after the root closed: malformed.
        if open == usize::from(opens) {
            second_root = true;
            return false;
        }
        match current.as_mut() {
            None => {
                // Only a `w:font` that is a child of the `w:fonts` root.
                if opens && open == 2 && root == Some(true) && e.local_name().as_ref() == b"font" {
                    let family =
                        attr(e, b"name").and_then(|name| wanted.get(&fold(&name)).copied());
                    current = Some((family, 0));
                }
            }
            Some((family, depth)) => {
                if *depth == 0
                    && let Some(family) = *family
                    && matches!(
                        e.local_name().as_ref(),
                        b"embedRegular" | b"embedBold" | b"embedItalic" | b"embedBoldItalic"
                    )
                    && per_family[family] < 4
                {
                    let target = attr_prefixed(e, b"id").and_then(|id| font_parts.get(&id));
                    let key = match attr(e, b"fontKey") {
                        None => Some(None),
                        Some(key) => font_key(&key).map(Some),
                    };
                    if let (Some(target), Some(key)) = (target, key) {
                        candidates.push((family, target, key));
                        per_family[family] += 1;
                    }
                }
                if opens {
                    *depth += 1;
                }
            }
        }
        true
    })?;
    if open != 0 || root.is_none() || second_root {
        return Err(SkipReason::MalformedXml);
    }

    let mut found = vec![false; families.len()];
    for (family, target, key) in candidates {
        if found[family] {
            continue;
        }
        // The part is in the package: a read that fails is not evidence
        // that the family is missing, so it skips the check.
        found[family] = is_font(package, target, key)?;
    }
    Ok(families
        .iter()
        .zip(found)
        .filter(|(_, found)| *found)
        .map(|(name, _)| (*name).clone())
        .collect())
}

/// The relationship types of a font table, transitional and strict.
const FONT_TABLE_TYPES: [&str; 2] = [
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/fontTable",
    "http://purl.oclc.org/ooxml/officeDocument/relationships/fontTable",
];

/// The relationship types of an embedded font part, transitional and strict.
const FONT_TYPES: [&str; 2] = [
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/font",
    "http://purl.oclc.org/ooxml/officeDocument/relationships/font",
];

/// The span that obfuscation covers (ECMA-376 Part 1, 17.8.1).
const OBFUSCATED: usize = 32;

/// At most this many table records or collection entries are read.
const MAX_DIRECTORY: u64 = 1024;

/// The 16 bytes of a `w:fontKey` GUID, read from its end (ECMA-376 Part 1,
/// 17.8.1). `None` for a malformed key.
fn font_key(key: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = key
        .bytes()
        .filter(|b| !matches!(b, b'{' | b'}' | b'-'))
        .collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        let at = 30 - 2 * i;
        let pair = std::str::from_utf8(&hex[at..at + 2]).ok()?;
        *slot = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// The tables a font needs, with the least length each can have (the fixed
/// part of its format): the character map and the metrics. The outlines,
/// TrueType glyphs or CFF, are checked apart.
const REQUIRED_TABLES: [(&[u8; 4], u64); 5] = [
    (b"cmap", 4),
    (b"head", 54),
    (b"hhea", 36),
    (b"hmtx", 4),
    (b"maxp", 6),
];

/// Whether the font part `target`, deobfuscated with `key`, starts a font:
/// an sfnt version (TrueType, OpenType with CFF, Apple TrueType) with 1 to
/// 1024 table records, each pointing inside the part, that list the
/// character map, the metrics and the outlines, each at least as long as
/// the fixed part of its format. This reads the directory only; it is not
/// a parse, but it rejects empty, cut, unrelated and incomplete parts. Word embeds single TrueType fonts, so a collection
/// (`ttcf`) does not count.
fn is_font(package: &mut Package, target: &str, key: Option<[u8; 16]>) -> Result<bool, SkipReason> {
    let size = package.size(target).unwrap_or(0);
    let read = |package: &mut Package, len: u64| -> Result<Vec<u8>, SkipReason> {
        let mut bytes = package.head(target, usize::try_from(len).unwrap_or(usize::MAX))?;
        if let Some(key) = key {
            for (i, byte) in bytes.iter_mut().take(OBFUSCATED).enumerate() {
                *byte ^= key[i % 16];
            }
        }
        Ok(bytes)
    };
    let head = read(package, OBFUSCATED as u64)?;
    if !matches!(
        head.get(..4),
        Some([0x00, 0x01, 0x00, 0x00] | b"OTTO" | b"true")
    ) {
        return Ok(false);
    }
    let Some(count) = head
        .get(4..6)
        .map(|b| u64::from(u16::from_be_bytes([b[0], b[1]])))
        .filter(|n| (1..=MAX_DIRECTORY).contains(n))
    else {
        return Ok(false);
    };
    let directory = 12 + 16 * count;
    if size < directory {
        return Ok(false);
    }
    let bytes = read(package, directory)?;
    if (bytes.len() as u64) < directory {
        return Ok(false);
    }
    let be32 = |at: usize| {
        bytes.get(at..at + 4).map_or(u64::MAX, |x| {
            u64::from(u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
        })
    };
    // Tag to length, for the records that point inside the part.
    let mut tables: HashMap<[u8; 4], u64> = HashMap::new();
    for i in 0..count {
        let at = usize::try_from(12 + 16 * i).unwrap_or(usize::MAX);
        let (offset, length) = (be32(at + 8), be32(at + 12));
        if offset < directory || offset.saturating_add(length) > size {
            return Ok(false);
        }
        if let Some(tag) = bytes.get(at..at + 4) {
            tables.insert([tag[0], tag[1], tag[2], tag[3]], length);
        }
    }
    let has = |tag: &[u8; 4], least: u64| tables.get(tag).is_some_and(|&length| length >= least);
    let outlines = (has(b"glyf", 1) && has(b"loca", 2)) || has(b"CFF ", 4) || has(b"CFF2", 5);
    Ok(outlines && REQUIRED_TABLES.iter().all(|(tag, least)| has(tag, *least)))
}

/// Trimmed, case-folded, runs of whitespace collapsed to one space: the
/// comparison the font catalog uses.
fn fold(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The document defaults' families and every style: id to (style, is the
/// default paragraph or character style).
#[allow(clippy::type_complexity)]
fn read_styles(
    package: &mut Package,
    part: &str,
    theme: &ThemeFonts,
) -> Result<(Vec<String>, HashMap<String, (Style, bool)>), SkipReason> {
    enum Context {
        None,
        Defaults,
        Style(String),
    }
    let mut context = Context::None;
    let mut defaults = Vec::new();
    let mut styles: HashMap<String, (Style, bool)> = HashMap::new();
    package.xml(part, PART_CAP, |e| {
        match e.local_name().as_ref() {
            b"docDefaults" => context = Context::Defaults,
            b"latentStyles" => context = Context::None,
            b"style" => {
                context = Context::None;
                if let Some(id) = attr(e, b"styleId")
                    && styles.len() < MAX_STYLES
                {
                    let kind = attr(e, b"type").unwrap_or_default();
                    let is_default = attr(e, b"default").is_some_and(|d| d == "1" || d == "true")
                        && (kind == "paragraph" || kind == "character");
                    styles.insert(id.clone(), (Style::default(), is_default));
                    context = Context::Style(id);
                }
            }
            b"basedOn" => {
                if let Context::Style(id) = &context
                    && let Some((style, _)) = styles.get_mut(id)
                {
                    style.based_on = attr(e, b"val");
                }
            }
            b"rFonts" => match &context {
                Context::Defaults => defaults.extend(rfonts(e, theme)),
                Context::Style(id) => {
                    if let Some((style, _)) = styles.get_mut(id) {
                        style.fonts.extend(rfonts(e, theme));
                    }
                }
                Context::None => {}
            },
            _ => {}
        }
        true
    })?;
    Ok((defaults, styles))
}

/// The main part named by the package relationships (`_rels/.rels`).
pub(super) fn main_part(package: &mut Package) -> Option<String> {
    let mut main = None;
    package
        .xml("_rels/.rels", PART_CAP, |e| {
            if e.local_name().as_ref() == b"Relationship"
                && attr(e, b"Type").is_some_and(|t| t.ends_with("/officeDocument"))
                && let Some(target) = attr(e, b"Target")
            {
                main = Some(resolve("", &target));
                return false;
            }
            true
        })
        .ok()?;
    main
}
