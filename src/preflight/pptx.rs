//! PPTX family (pptx, pptm, ppsx, potx): the declared slide count (V1) and
//! the requested Latin families (V2).
//!
//! V1 counts `p:sldIdLst/p:sldId`, resolves each `r:id` through the
//! presentation's relationships and reads only the root start tag of each
//! slide: `show="0"` is a hidden slide. The declared count is the visible
//! slides. `SourceFacts::slides` keeps every slide and the positions of the
//! hidden ones, so V1 can compare with `slides.include_hidden` set too, and
//! the manifest can mark the pages of hidden slides.
//!
//! Only the `p:sldId` children of the root's `p:sldIdLst` count, and each
//! one's `id` in the relationship namespace (transitional or strict), under
//! any prefix. Another child of the list, such as `mc:AlternateContent`,
//! leaves the slides unknown: V1 is skipped as malformed, and there is no
//! slide list.
//!
//! V2 reads `a:latin@typeface` from the slides, and from only the layouts
//! and masters that a slide reaches (slide to layout to master).
//! `+mj-lt` and `+mn-lt` resolve through the theme of the reached master.

use std::collections::{BTreeSet, HashMap};

use super::docx::{ThemeFonts, main_part, theme_fonts};
use super::package::{Package, XmlEvent, attr};
use super::{FONT_SCAN_CAP, PART_CAP, add_font};
use crate::domain::verification::{Check, DeclaredPages, SkipReason, Slides, SourceFacts};

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

pub(super) fn scan(package: &mut Package, fonts: &mut BTreeSet<String>, facts: &mut SourceFacts) {
    let main = main_part(package).unwrap_or_else(|| SOURCE.to_owned());
    let rels = match package.rels(&main) {
        Ok(rels) => rels,
        Err(reason) => {
            facts.skipped.push((Check::PageCount, reason));
            facts.skipped.push((Check::Fonts, reason));
            return;
        }
    };
    let mut ids = Vec::new();
    let mut too_many = false;
    // Only `p:sldId` children of the root's `p:sldIdLst` are slides; an
    // extension can refer to a slide too. Their `id` is the one in the
    // relationship namespace, whatever its prefix.
    let mut depth = 0usize;
    let mut in_list = false;
    // Another element in the list, such as `mc:AlternateContent`: which
    // slides an exporter takes is unknown.
    let mut foreign = false;
    // The prefixes that the root and the list declare.
    let mut scopes: Vec<Vec<(Vec<u8>, bool)>> = Vec::new();
    if let Err(reason) = package.xml_events(&main, PART_CAP, |event| {
        match event {
            XmlEvent::Start(e) | XmlEvent::Empty(e) => {
                let local = e.local_name();
                let opens = matches!(event, XmlEvent::Start(_));
                if depth == 1 && local.as_ref() == b"sldIdLst" {
                    in_list = opens;
                } else if depth == 2 && in_list {
                    if local.as_ref() != b"sldId" {
                        foreign = true;
                    } else if let Some(id) = relationship_id(e, &scopes) {
                        if ids.len() >= MAX_SLIDES {
                            too_many = true;
                            return false;
                        }
                        ids.push(id);
                    }
                }
                if opens {
                    if depth < 2 {
                        scopes.push(declarations(e));
                    }
                    depth += 1;
                }
            }
            XmlEvent::End => {
                depth = depth.saturating_sub(1);
                if depth < 2 {
                    scopes.truncate(depth);
                }
                if depth == 1 {
                    in_list = false;
                }
            }
            XmlEvent::Text => {}
        }
        true
    }) {
        facts.skipped.push((Check::PageCount, reason));
        facts.skipped.push((Check::Fonts, reason));
        return;
    }

    // V1: the visible slides, from each slide's root start tag.
    let mut slides = Vec::with_capacity(ids.len());
    let mut page_skip = if too_many {
        Some(SkipReason::CapReached)
    } else {
        foreign.then_some(SkipReason::MalformedXml)
    };
    let mut visible = 0u32;
    let mut hidden_at = Vec::new();
    for (position, id) in (1u32..).zip(&ids) {
        let Some((_, target)) = rels.get(id) else {
            page_skip.get_or_insert(SkipReason::MissingPart);
            continue;
        };
        match package.root_attributes(target) {
            Ok(attributes) => {
                let hidden = attributes.iter().any(|(name, prefixed, value)| {
                    name == b"show" && !prefixed && (value == "0" || value == "false")
                });
                if hidden {
                    hidden_at.push(position);
                } else {
                    visible += 1;
                }
                slides.push(target.clone());
            }
            Err(reason) => {
                page_skip.get_or_insert(reason);
            }
        }
    }
    match page_skip {
        Some(reason) => facts.skipped.push((Check::PageCount, reason)),
        None => {
            facts.declared_pages = Some(DeclaredPages {
                count: visible,
                source: SOURCE,
            });
            facts.slides = Some(Slides {
                count: ids.len().try_into().unwrap_or(u32::MAX),
                hidden: hidden_at,
            });
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
                match latin_fonts(package, &part, budget) {
                    Ok((part_fonts, read)) => {
                        budget = budget.saturating_sub(read);
                        cache.insert(part.clone(), part_fonts);
                    }
                    Err(SkipReason::MissingPart) => {
                        cache.insert(part.clone(), PartFonts::default());
                    }
                    Err(reason) => {
                        facts.skipped.push((Check::Fonts, reason));
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

/// The relationship namespaces of transitional and strict OOXML.
const RELATIONSHIPS: [&[u8]; 2] = [
    b"http://schemas.openxmlformats.org/officeDocument/2006/relationships",
    b"http://purl.oclc.org/ooxml/officeDocument/relationships",
];

/// The prefixes that `e` declares, each with whether it names the
/// relationship namespace.
fn declarations(e: &quick_xml::events::BytesStart<'_>) -> Vec<(Vec<u8>, bool)> {
    e.attributes()
        .flatten()
        .filter(|a| a.key.prefix().map(|p| p.into_inner()) == Some(b"xmlns".as_slice()))
        .map(|a| {
            let rel = RELATIONSHIPS.contains(&a.value.as_ref());
            (a.key.local_name().as_ref().to_vec(), rel)
        })
        .collect()
}

/// The relationship-namespace `id` of a `p:sldId`: the prefix resolves
/// through the element's own declarations, then the list's, then the root's.
fn relationship_id(
    e: &quick_xml::events::BytesStart<'_>,
    scopes: &[Vec<(Vec<u8>, bool)>],
) -> Option<String> {
    let own = declarations(e);
    e.attributes().flatten().find_map(|a| {
        let prefix = a.key.prefix()?;
        if a.key.local_name().as_ref() != b"id" {
            return None;
        }
        let rel = own
            .iter()
            .chain(scopes.iter().rev().flatten())
            .find(|(p, _)| p.as_slice() == prefix.as_ref())
            .is_some_and(|&(_, rel)| rel);
        if !rel {
            return None;
        }
        a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .ok()
            .map(|v| v.into_owned())
    })
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
/// the theme's major (`+mj-lt`) or minor (`+mn-lt`) font.
fn latin_fonts(
    package: &mut Package,
    part: &str,
    budget: u64,
) -> Result<(PartFonts, u64), SkipReason> {
    let mut out = PartFonts::default();
    let read = package.xml(part, budget, |e| {
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
