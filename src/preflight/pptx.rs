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
//! one's `id` in the relationship namespace. Both match by namespace URI,
//! transitional or strict, under any prefix. The slides are unknown when
//! the list holds another element (an extension's `sldId`,
//! `mc:AlternateContent`), an entry has no relationship `id`, or an element
//! declares more than 64 namespaces. Then V1 and V2 are skipped as
//! malformed, and there is no slide list: a partial list gives wrong marks
//! and misses font warnings.
//!
//! V2 reads `a:latin@typeface` from the slides, and from only the layouts
//! and masters that a slide reaches (slide to layout to master).
//! `+mj-lt` and `+mn-lt` resolve through the theme of the reached master.

use std::collections::{BTreeSet, HashMap};

use super::docx::{ThemeFonts, main_part, theme_fonts};
use quick_xml::events::BytesStart;

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
    let mut list = SlideList::default();
    if let Err(reason) = package.xml_events(&main, PART_CAP, |event| list.on_event(&event)) {
        facts.skipped.push((Check::PageCount, reason));
        facts.skipped.push((Check::Fonts, reason));
        return;
    }
    if list.unknown {
        // A partial list gives wrong marks and misses font warnings.
        facts
            .skipped
            .push((Check::PageCount, SkipReason::MalformedXml));
        facts.skipped.push((Check::Fonts, SkipReason::MalformedXml));
        return;
    }
    let SlideList { ids, too_many, .. } = list;

    // V1: the visible slides, from each slide's root start tag.
    let mut slides = Vec::with_capacity(ids.len());
    let mut page_skip = too_many.then_some(SkipReason::CapReached);
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

/// The PresentationML namespaces of transitional and strict OOXML.
const PRESENTATIONML: [&[u8]; 2] = [
    b"http://schemas.openxmlformats.org/presentationml/2006/main",
    b"http://purl.oclc.org/ooxml/presentationml/main",
];
/// The relationship namespaces of transitional and strict OOXML.
const RELATIONSHIPS: [&[u8]; 2] = [
    b"http://schemas.openxmlformats.org/officeDocument/2006/relationships",
    b"http://purl.oclc.org/ooxml/officeDocument/relationships",
];
/// The namespace declarations kept for the root and the slide list, and
/// for one slide entry. Past them the slide list is unknown, so a lookup
/// stays bounded.
const MAX_DECLARATIONS: usize = 256;
const MAX_OWN_DECLARATIONS: usize = 64;

/// The namespaces the slide list reads, classified once per declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ns {
    PresentationMl,
    Relationships,
    Other,
}

impl Ns {
    fn of(uri: &[u8]) -> Ns {
        if PRESENTATIONML.contains(&uri) {
            Ns::PresentationMl
        } else if RELATIONSHIPS.contains(&uri) {
            Ns::Relationships
        } else {
            Ns::Other
        }
    }
}

/// The `p:sldId` entries of the root's `p:sldIdLst`, in order. Anything the
/// scan cannot read for sure makes the list unknown: another element in the
/// list (an extension's `sldId`, `mc:AlternateContent`), an entry without a
/// relationship `id`, or too many namespace declarations.
#[derive(Debug, Default)]
struct SlideList {
    ids: Vec<String>,
    too_many: bool,
    unknown: bool,
    depth: usize,
    in_list: bool,
    /// A `p:sldIdLst` was read; a second one makes the list unknown.
    seen_list: bool,
    /// A root-level `mc:AlternateContent` is open.
    in_compat: bool,
    /// The prefixes in scope at the root, and inside the list.
    root: HashMap<Vec<u8>, Ns>,
    list: HashMap<Vec<u8>, Ns>,
}

/// The prefixes that `e` declares, or `None` past `cap`.
fn declarations(e: &BytesStart<'_>, cap: usize) -> Option<Vec<(Vec<u8>, Ns)>> {
    let mut out = Vec::new();
    for a in e.attributes().flatten() {
        let prefix = if a.key.as_ref() == b"xmlns" {
            Vec::new()
        } else if a.key.prefix().map(|p| p.into_inner()) == Some(b"xmlns".as_slice()) {
            a.key.local_name().as_ref().to_vec()
        } else {
            continue;
        };
        if out.len() >= cap {
            return None;
        }
        out.push((prefix, Ns::of(&a.value)));
    }
    Some(out)
}

impl SlideList {
    fn on_event(&mut self, event: &XmlEvent<'_, '_>) -> bool {
        match event {
            XmlEvent::Start(e) | XmlEvent::Empty(e) => {
                let opens = matches!(event, XmlEvent::Start(_));
                let local = e.local_name();
                if self.depth == 1 && local.as_ref() == b"AlternateContent" {
                    self.in_compat = opens;
                } else if self.in_compat && local.as_ref() == b"sldIdLst" {
                    // A list inside a compatibility block: the branch that
                    // an exporter takes is unknown.
                    self.unknown = true;
                    return false;
                }
                if self.depth <= 2 && !self.on_element(e, opens) {
                    return false;
                }
                if opens {
                    self.depth += 1;
                }
            }
            XmlEvent::End => {
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 1 {
                    self.in_list = false;
                    self.in_compat = false;
                }
            }
            XmlEvent::Text => {}
        }
        true
    }

    /// Reads an element at depth 0 to 2. `false` stops the scan.
    fn on_element(&mut self, e: &BytesStart<'_>, opens: bool) -> bool {
        let Some(own) = declarations(e, MAX_OWN_DECLARATIONS) else {
            self.unknown = true;
            return false;
        };
        let scope = if self.in_list { &self.list } else { &self.root };
        let resolve = |prefix: &[u8]| {
            own.iter()
                .find(|(p, _)| p == prefix)
                .map(|&(_, ns)| ns)
                .or_else(|| scope.get(prefix).copied())
                .unwrap_or(Ns::Other)
        };
        let name = e.name();
        let element_ns = resolve(name.prefix().map(|p| p.into_inner()).unwrap_or(b""));
        let local = e.local_name();
        match self.depth {
            0 => {
                self.root = own.into_iter().collect();
            }
            1 if local.as_ref() == b"sldIdLst" && element_ns == Ns::PresentationMl => {
                if self.seen_list {
                    self.unknown = true;
                    return false;
                }
                self.seen_list = true;
                if !opens {
                    return true;
                }
                let mut list = self.root.clone();
                list.extend(own);
                self.list = list;
                self.in_list = true;
            }
            2 if self.in_list => {
                if local.as_ref() != b"sldId" || element_ns != Ns::PresentationMl {
                    self.unknown = true;
                    return false;
                }
                let id = e.attributes().flatten().find_map(|a| {
                    let prefix = a.key.prefix()?;
                    if a.key.local_name().as_ref() != b"id"
                        || resolve(prefix.as_ref()) != Ns::Relationships
                    {
                        return None;
                    }
                    a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .ok()
                        .map(|v| v.into_owned())
                });
                let Some(id) = id else {
                    self.unknown = true;
                    return false;
                };
                if self.ids.len() >= MAX_SLIDES {
                    self.too_many = true;
                    return false;
                }
                self.ids.push(id);
            }
            _ => {}
        }
        if self.root.len() + self.list.len() > 2 * MAX_DECLARATIONS
            || self.root.len() > MAX_DECLARATIONS
            || self.list.len() > MAX_DECLARATIONS
        {
            self.unknown = true;
            return false;
        }
        true
    }
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
