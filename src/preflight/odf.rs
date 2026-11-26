//! OpenDocument (odt, odp, ods): the requested families (V2) from
//! `office:font-face-decls/style:font-face@svg:font-family` in
//! `content.xml` and `styles.xml`, and the macro storage (V4). No declared
//! page count.
//!
//! The same pass reads the text: `office:body` in `content.xml` and
//! `office:master-styles` (headers and footers) in `styles.xml`, without
//! annotations, tracked changes and presentation notes, which are not
//! printed.

use std::collections::BTreeSet;

use super::package::{Package, XmlEvent, attr};
use super::{PART_CAP, TextCollector, TextScope, add_font};
use crate::domain::verification::{Check, MacroFact, MacroKind, SkipReason, SourceFacts};

/// Elements whose text is not printed.
const NOT_PRINTED: &[&[u8]] = &[b"annotation", b"tracked-changes", b"notes"];

pub(super) fn scan(package: &mut Package, text: &mut TextCollector, facts: &mut SourceFacts) {
    facts.macros = macros(package);
    let mut found = BTreeSet::new();
    for (part, within) in [
        ("content.xml", &b"body"[..]),
        ("styles.xml", &b"master-styles"[..]),
    ] {
        let mut scope = TextScope::new(&[], Some(within), NOT_PRINTED);
        let result = package.xml_events(part, PART_CAP, |event| {
            if let Some(drawn) = scope.on(&event) {
                text.add(drawn);
            }
            let (XmlEvent::Start(e) | XmlEvent::Empty(e)) = event else {
                return true;
            };
            if e.local_name().as_ref() == b"font-face"
                && let Some(family) = attr(e, b"font-family")
            {
                // LibreOffice quotes names with spaces: 'Liberation Serif'.
                add_font(&mut found, family.trim().trim_matches(['\'', '"']));
            }
            true
        });
        match result {
            Ok(_) | Err(SkipReason::MissingPart) => {}
            Err(reason) => {
                facts.skipped.push((Check::Fonts, reason));
                facts.skipped.push((Check::Glyphs, reason));
                return;
            }
        }
    }
    facts.requested_fonts = found.into_iter().collect();
}

/// A `Basic/<lib>/<module>.xml` entry other than `script-lb.xml`,
/// `script-lc.xml` and `dialog-l*.xml` gives `basic`; a `Scripts/` entry
/// gives `script`.
fn macros(package: &Package) -> Option<MacroFact> {
    let mut names: Vec<&str> = package.names().collect();
    names.sort_unstable();
    let basic = names.iter().find(|name| {
        let parts: Vec<&str> = name.split('/').collect();
        matches!(parts.as_slice(), ["Basic", lib, module]
            if !lib.is_empty()
                && module.ends_with(".xml")
                && *module != "script-lb.xml"
                && *module != "script-lc.xml"
                && !module.starts_with("dialog-l"))
    });
    if let Some(part) = basic {
        return Some(MacroFact {
            kind: MacroKind::Basic,
            part: (*part).to_owned(),
        });
    }
    names
        .iter()
        .find(|name| name.starts_with("Scripts/") && name.len() > "Scripts/".len())
        .map(|part| MacroFact {
            kind: MacroKind::Script,
            part: (*part).to_owned(),
        })
}
