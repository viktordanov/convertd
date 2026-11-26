//! PPTX family (pptx, pptm, ppsx, potx): the declared slide count (V1) and
//! the requested Latin families (V2).
//!
//! V1 counts `p:sldIdLst/p:sldId`, resolves each `r:id` through the
//! presentation's relationships and reads only the root start tag of each
//! slide: `show="0"` is a hidden slide. The declared count is the visible
//! slides. The hidden count has no place in `SourceFacts` this round, so
//! verification skips V1 when hidden slides are included.
//!
//! V2 reads `a:latin@typeface` from the slides, and from only the layouts
//! and masters that a slide reaches (slide to layout to master).
//! `+mj-lt` and `+mn-lt` resolve through the theme of the reached master.
//!
//! The text is the `a:t` of the slides, hidden ones included. Layout and
//! master text is mostly placeholder prompts, which are not drawn.

use std::collections::{BTreeSet, HashMap};

use super::docx::{ThemeFonts, main_part, theme_fonts};
use super::package::{Package, XmlEvent, attr, attr_prefixed};
use super::{FONT_SCAN_CAP, PART_CAP, TextCollector, TextScope, add_font};
use crate::domain::verification::{Check, DeclaredPages, SkipReason, SourceFacts};

/// At most this many slides are counted and scanned.
pub const MAX_SLIDES: usize = 10_000;
/// Where V1 reads the declared count.
pub const SOURCE: &str = "ppt/presentation.xml";

/// What one slide, layout or master part asks for.
#[derive(Debug, Default, Clone)]
struct PartFonts {
    fonts: Vec<String>,
    major: bool,
    minor: bool,
}

pub(super) fn scan(
    package: &mut Package,
    fonts: &mut BTreeSet<String>,
    text: &mut TextCollector,
    facts: &mut SourceFacts,
) {
    let main = main_part(package).unwrap_or_else(|| SOURCE.to_owned());
    let rels = match package.rels(&main) {
        Ok(rels) => rels,
        Err(reason) => {
            facts.skipped.push((Check::PageCount, reason));
            facts.skipped.push((Check::Fonts, reason));
            facts.skipped.push((Check::Glyphs, reason));
            return;
        }
    };
    let mut ids = Vec::new();
    let mut too_many = false;
    if let Err(reason) = package.xml(&main, PART_CAP, |e| {
        if e.local_name().as_ref() == b"sldId"
            && let Some(id) = attr_prefixed(e, b"id")
        {
            if ids.len() >= MAX_SLIDES {
                too_many = true;
                return false;
            }
            ids.push(id);
        }
        true
    }) {
        facts.skipped.push((Check::PageCount, reason));
        facts.skipped.push((Check::Fonts, reason));
        facts.skipped.push((Check::Glyphs, reason));
        return;
    }

    // V1: the visible slides, from each slide's root start tag.
    let mut slides = Vec::with_capacity(ids.len());
    let mut page_skip = too_many.then_some(SkipReason::CapReached);
    let mut visible = 0u32;
    for id in &ids {
        let Some((_, target)) = rels.get(id) else {
            page_skip.get_or_insert(SkipReason::MissingPart);
            continue;
        };
        match package.root_attributes(target) {
            Ok(attributes) => {
                let hidden = attributes.iter().any(|(name, prefixed, value)| {
                    name == b"show" && !prefixed && (value == "0" || value == "false")
                });
                if !hidden {
                    visible += 1;
                }
                slides.push(target.clone());
            }
            Err(reason) => {
                page_skip.get_or_insert(reason);
            }
        }
    }
    // A slide that was not counted is not read for text either.
    if let Some(reason) = page_skip {
        facts.skipped.push((Check::Glyphs, reason));
    }
    match page_skip {
        Some(reason) => facts.skipped.push((Check::PageCount, reason)),
        None => {
            facts.declared_pages = Some(DeclaredPages {
                count: visible,
                source: SOURCE,
            })
        }
    }

    // V2: slides, and the layouts, masters and themes they reach.
    let mut cache: HashMap<String, PartFonts> = HashMap::new();
    let mut themes: HashMap<String, ThemeFonts> = HashMap::new();
    let mut budget = FONT_SCAN_CAP;
    let mut found = BTreeSet::new();
    for slide in &slides {
        let layout = related(package, slide, "/slideLayout");
        let master = layout
            .as_deref()
            .and_then(|l| related(package, l, "/slideMaster"));
        let theme_part = master
            .as_deref()
            .and_then(|m| related(package, m, "/theme"));
        let theme = match theme_part {
            Some(part) => themes
                .entry(part.clone())
                .or_insert_with(|| theme_fonts(package, &part))
                .clone(),
            None => ThemeFonts::default(),
        };
        for part in [Some(slide.clone()), layout, master].into_iter().flatten() {
            if !cache.contains_key(&part) {
                let slide_text = (part == *slide).then_some(&mut *text);
                match latin_fonts(package, &part, budget, slide_text) {
                    Ok((part_fonts, read)) => {
                        budget = budget.saturating_sub(read);
                        cache.insert(part.clone(), part_fonts);
                    }
                    Err(SkipReason::MissingPart) => {
                        cache.insert(part.clone(), PartFonts::default());
                    }
                    Err(reason) => {
                        facts.skipped.push((Check::Fonts, reason));
                        facts.skipped.push((Check::Glyphs, reason));
                        return;
                    }
                }
            }
            if let Some(part_fonts) = cache.get(&part) {
                for name in &part_fonts.fonts {
                    add_font(&mut found, name);
                }
                if part_fonts.major
                    && let Some(name) = &theme.major
                {
                    add_font(&mut found, name);
                }
                if part_fonts.minor
                    && let Some(name) = &theme.minor
                {
                    add_font(&mut found, name);
                }
            }
        }
    }
    fonts.extend(found);
}

/// The first target of `part`'s relationships whose type ends with `kind`.
fn related(package: &mut Package, part: &str, kind: &str) -> Option<String> {
    let rels = package.rels(part).ok()?;
    let mut targets: Vec<&(String, String)> =
        rels.values().filter(|(t, _)| t.ends_with(kind)).collect();
    targets.sort();
    targets.first().map(|(_, target)| target.clone())
}

/// The literal `a:latin@typeface` values of one part, and whether it uses
/// the theme's major (`+mj-lt`) or minor (`+mn-lt`) font. With `text`, the
/// same pass collects the part's `a:t` text.
fn latin_fonts(
    package: &mut Package,
    part: &str,
    budget: u64,
    mut text: Option<&mut TextCollector>,
) -> Result<(PartFonts, u64), SkipReason> {
    let mut out = PartFonts::default();
    let mut scope = TextScope::new(&[b"t"], None, &[]);
    let read = package.xml_events(part, budget, |event| {
        if let Some(drawn) = scope.on(&event)
            && let Some(text) = text.as_deref_mut()
        {
            text.add(drawn);
        }
        let (XmlEvent::Start(e) | XmlEvent::Empty(e)) = event else {
            return true;
        };
        if e.local_name().as_ref() == b"latin"
            && let Some(face) = attr(e, b"typeface")
        {
            match face.as_str() {
                "+mj-lt" => out.major = true,
                "+mn-lt" => out.minor = true,
                other if out.fonts.len() < super::MAX_FONTS => out.fonts.push(other.to_owned()),
                _ => {}
            }
        }
        true
    })?;
    Ok((out, read))
}
