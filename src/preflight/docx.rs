//! DOCX family (docx, docm, dotx): the requested Latin families (V2).
//! V1 is off for DOCX (005 §5: `docProps/app.xml` is not a reliable gate).
//!
//! Families come from `w:rFonts@w:ascii|w:hAnsi` in the main part and its
//! headers, footers, footnotes and endnotes; from `styles.xml`, only the
//! document defaults, the default paragraph and character styles, and the
//! styles the scanned parts reference, with their `w:basedOn` chain. Theme
//! references (`minorHAnsi`, `majorAscii`, …) resolve through the theme's
//! `a:minorFont|a:majorFont/a:latin@typeface`.

use std::collections::{BTreeSet, HashMap, HashSet};

use quick_xml::events::BytesStart;

use super::package::{Package, attr, resolve};
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
    let rels = package.rels(&main).unwrap_or_default();
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
    fonts.extend(found);
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
