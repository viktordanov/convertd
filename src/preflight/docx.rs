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
    // Only a table the main part links: a leftover table is not loaded.
    let font_table_part = target("/fontTable").into_iter().next();
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
        .filter(|(_, (kind, target))| kind.ends_with("/font") && package.has(target))
        .map(|(id, (_, target))| (id, target))
        .collect();
    let wanted: HashMap<String, &String> =
        requested.iter().map(|name| (fold(name), name)).collect();

    // (family, font part, obfuscation key), at most four per family.
    let mut candidates: Vec<(&String, &String, Option<[u8; 16]>)> = Vec::new();
    let mut per_family: HashMap<&String, usize> = HashMap::new();
    // The requested family of the open `w:font`, and the depth below it.
    let mut current: Option<(Option<&String>, usize)> = None;
    // Elements open in the whole table: a table cut short reads as EOF
    // with some still open, and is malformed rather than complete.
    let mut open = 0usize;
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
        match current.as_mut() {
            None => {
                if opens && e.local_name().as_ref() == b"font" {
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
                    && per_family.get(family).copied().unwrap_or(0) < 4
                {
                    let target = attr_prefixed(e, b"id").and_then(|id| font_parts.get(&id));
                    let key = match attr(e, b"fontKey") {
                        None => Some(None),
                        Some(key) => font_key(&key).map(Some),
                    };
                    if let (Some(target), Some(key)) = (target, key) {
                        candidates.push((family, target, key));
                        *per_family.entry(family).or_default() += 1;
                    }
                }
                if opens {
                    *depth += 1;
                }
            }
        }
        true
    })?;
    if open != 0 {
        return Err(SkipReason::MalformedXml);
    }

    let mut embedded = BTreeSet::new();
    for (family, target, key) in candidates {
        if embedded.contains(family) {
            continue;
        }
        // The part is in the package: a read that fails is not evidence
        // that the family is missing, so it skips the check.
        let mut head = package.head(target, SFNT_HEADER)?;
        if let Some(key) = key {
            for (i, byte) in head.iter_mut().enumerate() {
                *byte ^= key[i % 16];
            }
        }
        if is_font(&head, package.size(target).unwrap_or(0)) {
            embedded.insert(family.clone());
        }
    }
    Ok(embedded)
}

/// Bytes read from a font part: the obfuscated span (ECMA-376 Part 1,
/// 17.8.1), which holds the sfnt version.
const SFNT_HEADER: usize = 32;

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

/// Whether `head`, the first bytes of a part of `size` bytes, starts a
/// font: an sfnt version (TrueType, OpenType with CFF, Apple TrueType) with
/// at least one table whose directory fits in the part, or a font
/// collection with at least one font whose offsets fit. This is a header
/// check, not a parse: it rejects empty, cut or unrelated parts.
fn is_font(head: &[u8], size: u64) -> bool {
    let u16_at = |at: usize| {
        head.get(at..at + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
    };
    let u32_at = |at: usize| {
        head.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    match head.get(..4) {
        Some([0x00, 0x01, 0x00, 0x00] | b"OTTO" | b"true") => {
            u16_at(4).is_some_and(|tables| tables > 0 && size >= 12 + 16 * u64::from(tables))
        }
        Some(b"ttcf") => {
            u32_at(8).is_some_and(|fonts| fonts > 0 && size >= 12 + 4 * u64::from(fonts))
        }
        _ => false,
    }
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
