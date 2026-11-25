//! XLSX family (xlsx, xlsm): the requested Latin families (V2) from
//! `xl/styles.xml` `font/name@val`, with `scheme=minor|major` mapped to the
//! workbook theme. No declared page count.

use std::collections::BTreeSet;

use super::docx::{main_part, theme_fonts};
use super::package::{Package, attr};
use super::{PART_CAP, add_font};
use crate::domain::verification::{Check, SkipReason, SourceFacts};

pub(super) fn scan(package: &mut Package, fonts: &mut BTreeSet<String>, facts: &mut SourceFacts) {
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
}
