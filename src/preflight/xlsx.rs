//! XLSX family (xlsx, xlsm): the requested Latin families (V2) from
//! `xl/styles.xml` `font/name@val`, with `scheme=minor|major` mapped to the
//! workbook theme. No declared page count.
//!
//! The text is the inline strings of the worksheets that the workbook
//! lists as visible, which LibreOffice prints, and the shared strings that
//! their cells reference, without phonetic runs (`rPh`). Formula results
//! are not read.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::docx::{main_part, theme_fonts};
use super::package::{Package, XmlEvent, attr, attr_prefixed, rels_path};
use super::{FONT_SCAN_CAP, PART_CAP, Printed, TextCollector, TextScope, add_font};
use crate::domain::verification::{Check, SkipReason, SourceFacts};

/// At most this many sheets are listed; more skip the glyph check.
pub const MAX_SHEETS: usize = 1024;

pub(super) fn scan(
    package: &mut Package,
    fonts: &mut BTreeSet<String>,
    text: &mut TextCollector,
    printed: Printed,
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
    let complete = complete.and_then(|()| printed_sheets(package, &workbook, &rels, printed));
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
    if let Err(reason) = complete.and_then(|sheets| strings(package, &shared, &sheets, text)) {
        facts.skipped.push((Check::Glyphs, reason));
    }
}

/// The worksheets that the workbook's `sheets` list names, in order and
/// each once: the ones the PDF prints. A hidden sheet (`state` `hidden` or
/// `veryHidden`) counts only when the export puts each sheet on one page,
/// which prints hidden sheets too. A listed sheet without its relationship,
/// and a workbook part that does not read as a whole, are errors.
fn printed_sheets(
    package: &mut Package,
    workbook: &str,
    rels: &HashMap<String, (String, String)>,
    printed: Printed,
) -> Result<Vec<String>, SkipReason> {
    let mut ids: Vec<Option<String>> = Vec::new();
    let mut too_many = false;
    let mut scope = TextScope::new(&[], None, &[]);
    let mut depth = 0usize;
    let mut in_list = false;
    package.xml_events(workbook, PART_CAP, |event| {
        scope.on(&event);
        match event {
            XmlEvent::Start(e) | XmlEvent::Empty(e) => {
                let local = e.local_name();
                if depth == 1 && local.as_ref() == b"sheets" {
                    in_list = matches!(event, XmlEvent::Start(_));
                } else if depth == 2 && in_list && local.as_ref() == b"sheet" {
                    let state = attr(e, b"state");
                    let hidden = matches!(state.as_deref(), Some("hidden" | "veryHidden"));
                    if !hidden || printed.hidden_sheets {
                        if ids.len() >= MAX_SHEETS {
                            too_many = true;
                            return false;
                        }
                        ids.push(attr_prefixed(e, b"id"));
                    }
                }
                if matches!(event, XmlEvent::Start(_)) {
                    depth += 1;
                }
            }
            XmlEvent::End => {
                depth = depth.saturating_sub(1);
                if depth == 1 {
                    in_list = false;
                }
            }
            _ => {}
        }
        true
    })?;
    if too_many {
        return Err(SkipReason::CapReached);
    }
    scope.complete()?;
    // Each relationship once: many sheet entries may name one target.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut sheets = Vec::new();
    for id in &ids {
        let (kind, target) = id
            .as_ref()
            .and_then(|id| rels.get(id))
            .ok_or(SkipReason::MissingPart)?;
        // A chart sheet has no cells.
        if kind.ends_with("/worksheet") && seen.insert(target.as_str()) {
            sheets.push(target.clone());
        }
    }
    Ok(sheets)
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

/// Reads the inline strings of the printed worksheets and the
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
    for part in sheets {
        let mut scope = TextScope::new(&[b"t"], None, &[b"rPh"]);
        let mut shared_cell = false;
        // The digits of a shared cell's <v>; None past 10 bytes.
        let mut value: Option<Option<String>> = None;
        let mut unescape = Unescape::default();
        let read = package.xml_events(part, budget, |event| {
            if let Some(drawn) = scope.on(&event) {
                unescape.feed(drawn, text);
            } else if !matches!(event, XmlEvent::Text { .. }) {
                unescape.flush(text);
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
    // No cell uses the table: it is not read, so a leftover table can
    // neither cost the budget nor fail the check.
    if !referenced.any() {
        return Ok(());
    }
    let mut scope = TextScope::new(&[b"t"], None, &[b"rPh"]);
    let mut index: Option<u32> = None;
    let mut unescape = Unescape::default();
    let result = package.xml_events(shared, budget, |event| {
        if !matches!(event, XmlEvent::Text { .. }) {
            unescape.flush(text);
        }
        if let XmlEvent::Start(e) | XmlEvent::Empty(e) = &event
            && e.local_name().as_ref() == b"si"
        {
            index = Some(index.map_or(0, |i| i.saturating_add(1)));
        }
        if let Some(drawn) = scope.on(&event)
            && index.is_some_and(|i| referenced.contains(i))
        {
            unescape.feed(drawn, text);
        }
        true
    });
    unescape.flush(text);
    result?;
    scope.complete()
}

/// Decodes SpreadsheetML text, whose `_xHHHH_` escapes stand for one
/// UTF-16 code unit each (`_x005F_` is a literal `_`), into the collector.
/// The value of a `t` element can come in several fragments (a reference
/// splits it), so an escape cut at a fragment's end waits for the next
/// one; any markup ends the value. A surrogate pair is two escapes; a lone
/// surrogate stands for nothing.
#[derive(Default)]
struct Unescape {
    /// The start of an escape that the last fragment cut, at most 6 bytes.
    pending: String,
    high: Option<u16>,
}

impl Unescape {
    fn feed(&mut self, fragment: &str, text: &mut TextCollector) {
        let joined;
        let raw: &str = if self.pending.is_empty() {
            fragment
        } else {
            joined = std::mem::take(&mut self.pending) + fragment;
            &joined
        };
        if !raw.contains('_') {
            self.high = None;
            text.add(raw);
            return;
        }
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(at) = rest.find('_') {
            if at > 0 {
                self.high = None;
                out.push_str(&rest[..at]);
            }
            let tail = &rest[at..];
            if tail.len() < 7 && is_escape_prefix(tail) {
                self.pending = tail.to_owned();
                rest = "";
                break;
            }
            let unit = tail
                .get(1..7)
                .filter(|t| t.starts_with('x') && t.ends_with('_'))
                .and_then(|t| u16::from_str_radix(&t[1..5], 16).ok());
            match unit {
                Some(unit) => {
                    self.unit(unit, &mut out);
                    rest = &tail[7..];
                }
                None => {
                    self.high = None;
                    out.push('_');
                    rest = &tail[1..];
                }
            }
        }
        if !rest.is_empty() {
            self.high = None;
            out.push_str(rest);
        }
        text.add(&out);
    }

    fn unit(&mut self, unit: u16, out: &mut String) {
        if let Some(high) = self.high.take()
            && (0xDC00..0xE000).contains(&unit)
        {
            out.extend(char::decode_utf16([high, unit]).flatten());
        } else if (0xD800..0xDC00).contains(&unit) {
            self.high = Some(unit);
        } else if let Some(c) = char::from_u32(u32::from(unit)) {
            out.push(c);
        }
    }

    /// Ends the value: a cut escape was literal text.
    fn flush(&mut self, text: &mut TextCollector) {
        self.high = None;
        if !self.pending.is_empty() {
            text.add(&std::mem::take(&mut self.pending));
        }
    }
}

/// Whether `tail`, shorter than an escape, can still become one.
fn is_escape_prefix(tail: &str) -> bool {
    tail.bytes().enumerate().all(|(i, b)| match i {
        0 => b == b'_',
        1 => b == b'x',
        2..=5 => b.is_ascii_hexdigit(),
        _ => false,
    })
}
