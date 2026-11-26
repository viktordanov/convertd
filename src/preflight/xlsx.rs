//! XLSX family (xlsx, xlsm): the requested Latin families (V2) from
//! `xl/styles.xml` `font/name@val`, with `scheme=minor|major` mapped to the
//! workbook theme. No declared page count.
//!
//! The text is the shared strings and the inline strings of the
//! worksheets, without phonetic runs (`rPh`). Formula results are not
//! read.

use std::collections::BTreeSet;

use super::docx::{main_part, theme_fonts};
use super::package::{Package, attr};
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
    let rels = package.rels(&workbook).unwrap_or_default();
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
    if let Err(reason) = strings(package, &shared, &sheets, text) {
        facts.skipped.push((Check::Glyphs, reason));
    }
}

/// Reads the text of the shared strings, then the inline strings of the
/// first [`MAX_SHEETS`] worksheets, under one [`FONT_SCAN_CAP`] budget. A missing part has no
/// text; the first other error ends the read.
fn strings(
    package: &mut Package,
    shared: &str,
    sheets: &[String],
    text: &mut TextCollector,
) -> Result<(), SkipReason> {
    let mut budget = FONT_SCAN_CAP;
    let listed = sheets.iter().take(MAX_SHEETS).map(String::as_str);
    for part in std::iter::once(shared).chain(listed) {
        let mut scope = TextScope::new(&[b"t"], None, &[b"rPh"]);
        let result = package.xml_events(part, budget, |event| {
            if let Some(drawn) = scope.on(&event) {
                text.add(drawn);
            }
            true
        });
        match result {
            Ok(read) => budget = budget.saturating_sub(read),
            Err(SkipReason::MissingPart) => {}
            Err(reason) => return Err(reason),
        }
    }
    if sheets.len() > MAX_SHEETS {
        return Err(SkipReason::CapReached);
    }
    Ok(())
}
