//! XLSX family (xlsx, xlsm): the requested Latin families (V2) from
//! `xl/styles.xml` `font/name@val`, with `scheme=minor|major` mapped to the
//! workbook theme. No declared page count.
//!
//! The text is the inline strings of the worksheets and the shared strings
//! that their cells reference, without phonetic runs (`rPh`). Formula
//! results are not read.

use std::collections::BTreeSet;

use super::docx::{main_part, theme_fonts};
use super::package::{Package, XmlEvent, attr, rels_path};
use super::{FONT_SCAN_CAP, PART_CAP, TextCollector, TextScope, add_font};
use crate::domain::verification::{Check, SkipReason, SourceFacts};

/// At most this many worksheets are read for inline strings.
pub const MAX_SHEETS: usize = 1024;

pub(super) fn scan(
    package: &mut Package,
    fonts: &mut BTreeSet<String>,
    text: &mut TextCollector,
    facts: &mut SourceFacts,
) {
    let workbook = main_part(package).unwrap_or_else(|| "xl/workbook.xml".to_owned());
    // The glyph check needs every worksheet, so it takes the complete read
    // of the relationships; the font check keeps the lenient one.
    let mut rels_budget = PART_CAP;
    let (rels, complete) = if package.has(&rels_path(&workbook)) {
        match package.rels_complete(&workbook, None, &mut rels_budget) {
            Ok(rels) => (rels, Ok(())),
            Err(reason) => (package.rels(&workbook).unwrap_or_default(), Err(reason)),
        }
    } else {
        (Default::default(), Ok(()))
    };
    let target = |suffix: &str, default: &str| {
        let mut targets: Vec<&String> = rels
            .values()
            .filter(|(kind, _)| kind.ends_with(suffix))
            .map(|(_, target)| target)
            .collect();
        targets.sort();
        targets
            .first()
            .map(|t| (*t).clone())
            .unwrap_or_else(|| default.to_owned())
    };
    let styles = target("/styles", "xl/styles.xml");
    let shared = target("/sharedStrings", "xl/sharedStrings.xml");
    let mut sheets: Vec<String> = rels
        .values()
        .filter(|(kind, _)| kind.ends_with("/worksheet"))
        .map(|(_, target)| target.clone())
        .collect();
    sheets.sort();
    sheets.dedup();
    let theme = theme_fonts(package, &target("/theme", "xl/theme/theme1.xml"));

    // One <font> at a time: its <name> and <scheme> children.
    let mut found = BTreeSet::new();
    let mut pending: Option<(Option<String>, Option<String>)> = None;
    let commit = |pending: &mut Option<(Option<String>, Option<String>)>,
                  found: &mut BTreeSet<String>| {
        if let Some((name, scheme)) = pending.take() {
            let resolved = match scheme.as_deref() {
                Some("minor") => theme.minor.clone().or(name),
                Some("major") => theme.major.clone().or(name),
                _ => name,
            };
            if let Some(name) = resolved {
                add_font(found, &name);
            }
        }
    };
    let result = package.xml(&styles, PART_CAP, |e| {
        match e.local_name().as_ref() {
            b"font" => {
                commit(&mut pending, &mut found);
                pending = Some((None, None));
            }
            b"name" => {
                if let Some((name, _)) = pending.as_mut() {
                    *name = attr(e, b"val");
                }
            }
            b"scheme" => {
                if let Some((_, scheme)) = pending.as_mut() {
                    *scheme = attr(e, b"val");
                }
            }
            _ => {}
        }
        true
    });
    commit(&mut pending, &mut found);
    match result {
        Ok(_) => fonts.extend(found),
        Err(SkipReason::MissingPart) => {}
        Err(reason) => facts.skipped.push((Check::Fonts, reason)),
    }
    if let Err(reason) = complete.and_then(|()| strings(package, &shared, &sheets, text)) {
        facts.skipped.push((Check::Glyphs, reason));
    }
}

/// The highest shared-string index tracked one by one; a cell that names
/// a higher one makes every shared string count.
const MAX_SHARED_INDEX: u32 = 1 << 24;

/// The shared-string indices that the cells reference, one bit each.
#[derive(Default)]
struct Referenced {
    bits: Vec<u64>,
    all: bool,
}

impl Referenced {
    fn insert(&mut self, index: u32) {
        if index >= MAX_SHARED_INDEX {
            self.all = true;
            return;
        }
        let word = (index / 64) as usize;
        if self.bits.len() <= word {
            self.bits.resize(word + 1, 0);
        }
        self.bits[word] |= 1 << (index % 64);
    }

    fn any(&self) -> bool {
        self.all || self.bits.iter().any(|word| *word != 0)
    }

    fn contains(&self, index: u32) -> bool {
        self.all
            || self
                .bits
                .get((index / 64) as usize)
                .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }
}

/// Reads the inline strings of the first [`MAX_SHEETS`] worksheets and the
/// shared strings that their cells (`c t="s"`) reference, under one
/// [`FONT_SCAN_CAP`] budget. A shared string that no cell uses is not
/// drawn. A listed worksheet that is missing, and a shared-string table
/// that cells use but the package lacks, are errors, as is any part that
/// does not read as a whole.
fn strings(
    package: &mut Package,
    shared: &str,
    sheets: &[String],
    text: &mut TextCollector,
) -> Result<(), SkipReason> {
    let mut budget = FONT_SCAN_CAP;
    let mut referenced = Referenced::default();
    // Every listed worksheet must be read: a missing one is not optional.
    for part in sheets.iter().take(MAX_SHEETS) {
        let mut scope = TextScope::new(&[b"t"], None, &[b"rPh"]);
        let mut shared_cell = false;
        // The digits of a shared cell's <v>; None past 10 bytes.
        let mut value: Option<Option<String>> = None;
        let read = package.xml_events(part, budget, |event| {
            if let Some(drawn) = scope.on(&event) {
                add_escaped(text, drawn);
            }
            match event {
                XmlEvent::Start(e) | XmlEvent::Empty(e) if e.local_name().as_ref() == b"c" => {
                    shared_cell = attr(e, b"t").as_deref() == Some("s");
                }
                XmlEvent::Start(e) if shared_cell && e.local_name().as_ref() == b"v" => {
                    value = Some(Some(String::new()));
                }
                XmlEvent::Text { text, .. } => {
                    if let Some(digits) = value.as_mut() {
                        match digits {
                            Some(d) if d.len() + text.len() <= 10 => d.push_str(text),
                            _ => *digits = None,
                        }
                    }
                }
                XmlEvent::Unresolved => {
                    if value.is_some() {
                        value = Some(None);
                    }
                }
                XmlEvent::End => match value.take() {
                    Some(Some(digits)) => match digits.trim().parse::<u32>() {
                        Ok(index) => referenced.insert(index),
                        Err(_) => referenced.all = true,
                    },
                    // An index that cannot be read may name any string.
                    Some(None) => referenced.all = true,
                    None => {}
                },
                _ => {}
            }
            true
        })?;
        scope.complete()?;
        budget = budget.saturating_sub(read);
    }
    if sheets.len() > MAX_SHEETS {
        return Err(SkipReason::CapReached);
    }

    let mut scope = TextScope::new(&[b"t"], None, &[b"rPh"]);
    let mut index: Option<u32> = None;
    let result = package.xml_events(shared, budget, |event| {
        if let XmlEvent::Start(e) | XmlEvent::Empty(e) = &event
            && e.local_name().as_ref() == b"si"
        {
            index = Some(index.map_or(0, |i| i.saturating_add(1)));
        }
        if let Some(drawn) = scope.on(&event)
            && index.is_some_and(|i| referenced.contains(i))
        {
            add_escaped(text, drawn);
        }
        true
    });
    match result {
        Ok(_) => scope.complete(),
        // The table is optional only when no cell uses it.
        Err(SkipReason::MissingPart) if !referenced.any() => Ok(()),
        Err(reason) => Err(reason),
    }
}

/// Adds SpreadsheetML text, whose `_xHHHH_` escapes stand for one UTF-16
/// code unit each (`_x005F_` is a literal `_`). A surrogate pair is two
/// escapes; a lone surrogate stands for nothing.
fn add_escaped(text: &mut TextCollector, raw: &str) {
    if !raw.contains("_x") {
        text.add(raw);
        return;
    }
    let mut out = String::with_capacity(raw.len());
    let mut high: Option<u16> = None;
    let mut rest = raw;
    while let Some(at) = rest.find("_x") {
        out.push_str(&rest[..at]);
        let unit = rest
            .get(at + 2..at + 7)
            .filter(|tail| tail.ends_with('_'))
            .and_then(|tail| u16::from_str_radix(&tail[..4], 16).ok());
        match unit {
            Some(unit) => {
                if let Some(h) = high.take()
                    && (0xDC00..0xE000).contains(&unit)
                {
                    out.extend(char::decode_utf16([h, unit]).flatten());
                } else if (0xD800..0xDC00).contains(&unit) {
                    high = Some(unit);
                } else if let Some(c) = char::from_u32(u32::from(unit)) {
                    out.push(c);
                }
                rest = &rest[at + 7..];
            }
            None => {
                out.push_str("_x");
                rest = &rest[at + 2..];
            }
        }
    }
    out.push_str(rest);
    text.add(&out);
}
