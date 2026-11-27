//! OpenDocument (odt, odp, ods): the requested families (V2) from
//! `office:font-face-decls/style:font-face@svg:font-family` in
//! `content.xml` and `styles.xml`, and the macro storage (V4). No declared
//! page count.
//!
//! For a presentation, the same pass lists the slides: each `draw:page`
//! child of `office:body/office:presentation` in `content.xml`, in order.
//! A page is hidden when its drawing-page style, or a parent of that style,
//! has `presentation:visibility="hidden"`. A page style resolves among the
//! automatic styles of `content.xml`, then the common styles of
//! `styles.xml`; a parent resolves among the common styles. The automatic
//! styles of `styles.xml` belong to master pages and never apply. A page
//! whose style is defined in neither scope leaves the list unknown (`None`).
//! Elements and attributes match by namespace URI, whatever the prefix,
//! and a property applies only inside the open style that holds it. Past
//! 256 open namespace declarations the list is unknown, so a name lookup
//! stays bounded.
//!
//! The same pass reads the text: `office:body` in `content.xml` and
//! `office:master-styles` (headers and footers) in `styles.xml`, without
//! annotations, tracked changes and the alternative text of frames
//! (`svg:title`, `svg:desc` by namespace), which are not printed.
//! Presentation notes count when the request exports notes pages. A page
//! that the slide list marks hidden counts only when the export includes
//! hidden slides: `content.xml` is then read again for its text, since the
//! list is known only after `styles.xml`; when that read does not find the
//! same pages, or there is no list, every page counts. A sheet with
//! `table:display="false"` on its element is not printed.

use std::collections::{BTreeSet, HashMap};

use quick_xml::events::BytesStart;

use super::package::{Package, XmlEvent, attr};
use super::pptx::MAX_SLIDES;
use super::{PART_CAP, Printed, TextCollector, TextScope, add_font};
use crate::domain::document::Family;
use crate::domain::verification::{Check, MacroFact, MacroKind, SkipReason, Slides, SourceFacts};

/// The longest chain of parent styles that is followed.
const MAX_STYLE_DEPTH: usize = 16;
/// Deeper elements are not tracked by name; none of them is a slide or a
/// slide style.
const MAX_DEPTH: usize = 64;

/// A drawing-page style: its own visibility, if it sets one, and its parent.
#[derive(Debug, Default)]
struct PageStyle {
    hidden: Option<bool>,
    parent: Option<String>,
}

/// Where a style definition applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// `content.xml`, `office:automatic-styles`.
    Automatic,
    /// `styles.xml`, `office:styles`.
    Common,
}

/// The open namespace declarations that are kept; past this many the slide
/// list is unknown. LibreOffice declares about 40 on the root.
const MAX_BINDINGS: usize = 256;

/// The namespaces the slide scan reads. A declaration is classified once,
/// so a long URI costs nothing per use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ns {
    Office,
    Style,
    Draw,
    Presentation,
    Other,
}

impl Ns {
    fn of(uri: &[u8]) -> Ns {
        match uri {
            b"urn:oasis:names:tc:opendocument:xmlns:office:1.0" => Ns::Office,
            b"urn:oasis:names:tc:opendocument:xmlns:style:1.0" => Ns::Style,
            b"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" => Ns::Draw,
            b"urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" => Ns::Presentation,
            _ => Ns::Other,
        }
    }
}

/// The elements that the path checks name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    AutomaticStyles,
    Styles,
    Body,
    Presentation,
    Style,
    Other,
}

/// An open element: what it is, and the prefixes it declares.
#[derive(Debug)]
struct Open {
    kind: Kind,
    bindings: Vec<(Vec<u8>, Ns)>,
}

/// The pages of a presentation and the drawing-page styles that can apply
/// to them. Elements and attributes match by namespace URI and local name,
/// whatever prefix the document binds.
#[derive(Debug, Default)]
struct Deck {
    /// The style name of each slide, in order.
    pages: Vec<Option<String>>,
    automatic: HashMap<String, PageStyle>,
    common: HashMap<String, PageStyle>,
    /// The open elements of the part being read.
    open: Vec<Open>,
    /// Elements opened past `MAX_DEPTH`, not kept in `open`.
    deeper: usize,
    /// The declarations in `open`.
    bound: usize,
    /// The slide style that is open, and its index in `open`.
    /// The visibility its properties set so far goes to the style when it
    /// closes, so no name is copied per properties element.
    current: Option<(Scope, String, usize, Option<bool>)>,
    /// Too many slides or declarations: the list is unknown.
    unknown: bool,
}

/// The prefixes that `e` declares (`xmlns` binds the empty prefix), or
/// `None` when they would take the open declarations past `room`. Nothing
/// past the cap is copied.
fn bindings(e: &BytesStart<'_>, room: usize) -> Option<Vec<(Vec<u8>, Ns)>> {
    let mut out = Vec::new();
    for a in e.attributes().flatten() {
        let prefix = if a.key.as_ref() == b"xmlns" {
            Vec::new()
        } else if a.key.prefix().map(|p| p.into_inner()) == Some(b"xmlns".as_slice()) {
            a.key.local_name().as_ref().to_vec()
        } else {
            continue;
        };
        if out.len() >= room {
            return None;
        }
        out.push((prefix, Ns::of(&a.value)));
    }
    Some(out)
}

impl Deck {
    fn start_part(&mut self) {
        self.open.clear();
        self.deeper = 0;
        self.bound = 0;
        self.current = None;
    }

    /// The namespace bound to `prefix`, by `own` (the element's own
    /// declarations) or an open element, innermost first. At most
    /// `MAX_BINDINGS` declarations are searched.
    fn resolve(&self, own: &[(Vec<u8>, Ns)], prefix: &[u8]) -> Ns {
        own.iter()
            .chain(self.open.iter().rev().flat_map(|o| o.bindings.iter()))
            .find(|(p, _)| p == prefix)
            .map_or(Ns::Other, |&(_, ns)| ns)
    }

    /// The value of the attribute `ns`:`local` of `e`.
    fn attr_ns(
        &self,
        e: &BytesStart<'_>,
        own: &[(Vec<u8>, Ns)],
        ns: Ns,
        local: &[u8],
    ) -> Option<String> {
        e.attributes().flatten().find_map(|a| {
            let prefix = a.key.prefix()?;
            if a.key.local_name().as_ref() != local || self.resolve(own, prefix.as_ref()) != ns {
                return None;
            }
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
    }

    fn on_event(&mut self, content: bool, event: &XmlEvent<'_, '_>) {
        // An unknown list stays unknown: the scan stops tracking the deck.
        if self.unknown {
            return;
        }
        match event {
            XmlEvent::Start(e) => {
                if self.deeper > 0 || self.open.len() >= MAX_DEPTH {
                    self.deeper += 1;
                    return;
                }
                let Some(own) = bindings(e, MAX_BINDINGS - self.bound) else {
                    self.unknown = true;
                    return;
                };
                self.bound += own.len();
                let kind = self.on_element(content, e, &own, true);
                self.open.push(Open {
                    kind,
                    bindings: own,
                });
            }
            XmlEvent::Empty(e) => {
                if self.deeper == 0 && self.open.len() < MAX_DEPTH {
                    match bindings(e, MAX_BINDINGS - self.bound) {
                        Some(own) => {
                            self.on_element(content, e, &own, false);
                        }
                        None => self.unknown = true,
                    }
                }
            }
            XmlEvent::End => {
                if self.deeper > 0 {
                    self.deeper -= 1;
                } else if let Some(open) = self.open.pop() {
                    self.bound -= open.bindings.len();
                    if matches!(self.current, Some((_, _, at, _)) if at >= self.open.len())
                        && let Some((scope, name, _, hidden)) = self.current.take()
                        && hidden.is_some()
                    {
                        let styles = match scope {
                            Scope::Automatic => &mut self.automatic,
                            Scope::Common => &mut self.common,
                        };
                        if let Some(style) = styles.get_mut(&name) {
                            style.hidden = hidden;
                        }
                    }
                }
            }
            XmlEvent::Text { .. } | XmlEvent::Unresolved => {}
        }
    }

    /// The namespace of `e`; only for the local names the scan reads, so
    /// other elements cost no lookup.
    fn element_ns(&self, e: &BytesStart<'_>, own: &[(Vec<u8>, Ns)]) -> Ns {
        let name = e.name();
        let prefix = name.prefix().map(|p| p.into_inner()).unwrap_or(b"");
        self.resolve(own, prefix)
    }

    /// Whether the open elements, below the root, are exactly `path`.
    fn under(&self, path: &[Kind]) -> bool {
        self.open.len() == path.len() + 1
            && self.open[1..]
                .iter()
                .zip(path)
                .all(|(o, kind)| o.kind == *kind)
    }

    /// Reads one element, and gives its kind for the path checks.
    fn on_element(
        &mut self,
        content: bool,
        e: &BytesStart<'_>,
        own: &[(Vec<u8>, Ns)],
        opens: bool,
    ) -> Kind {
        let local = e.local_name();
        let wanted = matches!(
            local.as_ref(),
            b"automatic-styles"
                | b"styles"
                | b"body"
                | b"presentation"
                | b"style"
                | b"drawing-page-properties"
                | b"page"
        );
        if !wanted {
            return Kind::Other;
        }
        match (self.element_ns(e, own), local.as_ref()) {
            (Ns::Office, b"automatic-styles") => Kind::AutomaticStyles,
            (Ns::Office, b"styles") => Kind::Styles,
            (Ns::Office, b"body") => Kind::Body,
            (Ns::Office, b"presentation") => Kind::Presentation,
            (Ns::Style, b"style") => {
                let scope = if content && self.under(&[Kind::AutomaticStyles]) {
                    Scope::Automatic
                } else if !content && self.under(&[Kind::Styles]) {
                    Scope::Common
                } else {
                    return Kind::Style;
                };
                if self.attr_ns(e, own, Ns::Style, b"family").as_deref() != Some("drawing-page") {
                    return Kind::Style;
                }
                let Some(name) = self.attr_ns(e, own, Ns::Style, b"name") else {
                    return Kind::Style;
                };
                let parent = self.attr_ns(e, own, Ns::Style, b"parent-style-name");
                let styles = match scope {
                    Scope::Automatic => &mut self.automatic,
                    Scope::Common => &mut self.common,
                };
                styles.entry(name.clone()).or_default().parent = parent;
                // A self-closing style has no properties to read.
                if opens {
                    self.current = Some((scope, name, self.open.len(), None));
                }
                Kind::Style
            }
            (Ns::Style, b"drawing-page-properties") => {
                let direct =
                    matches!(&self.current, Some((_, _, at, _)) if *at + 1 == self.open.len());
                if direct
                    && let Some(visibility) = self.attr_ns(e, own, Ns::Presentation, b"visibility")
                    && let Some(current) = self.current.as_mut()
                {
                    current.3 = Some(visibility == "hidden");
                }
                Kind::Other
            }
            (Ns::Draw, b"page") => {
                if content && self.under(&[Kind::Body, Kind::Presentation]) {
                    if self.pages.len() >= MAX_SLIDES {
                        self.unknown = true;
                    } else {
                        let style = self.attr_ns(e, own, Ns::Draw, b"style-name");
                        self.pages.push(style);
                    }
                }
                Kind::Other
            }
            _ => Kind::Other,
        }
    }

    /// Whether the slide style `name` hides its page; `None` when a style
    /// in the chain is not defined. `common` caches the common styles that
    /// were resolved.
    fn hides<'a>(
        &'a self,
        name: &'a str,
        common: &mut HashMap<&'a str, Option<bool>>,
    ) -> Option<bool> {
        let (style, from_common) = match self.automatic.get(name) {
            Some(style) => (style, false),
            None => (self.common.get(name)?, true),
        };
        if from_common {
            return self.common_hides(name, common);
        }
        match (style.hidden, &style.parent) {
            (Some(hidden), _) => Some(hidden),
            (None, Some(parent)) => self.common_hides(parent, common),
            (None, None) => Some(false),
        }
    }

    /// `hides` for a common style, once per name.
    fn common_hides<'a>(
        &'a self,
        name: &'a str,
        cache: &mut HashMap<&'a str, Option<bool>>,
    ) -> Option<bool> {
        if let Some(&known) = cache.get(name) {
            return known;
        }
        let mut result = None;
        let mut at = name;
        for _ in 0..MAX_STYLE_DEPTH {
            let Some(style) = self.common.get(at) else {
                break;
            };
            if let Some(hidden) = style.hidden {
                result = Some(hidden);
                break;
            }
            match &style.parent {
                Some(parent) => at = parent,
                None => {
                    result = Some(false);
                    break;
                }
            }
        }
        cache.insert(name, result);
        result
    }

    fn slides(&self) -> Option<Slides> {
        if self.unknown {
            return None;
        }
        // Each page style is resolved once, whatever the number of pages.
        let mut pages: HashMap<&str, Option<bool>> = HashMap::new();
        let mut common = HashMap::new();
        let mut hidden = Vec::new();
        for (position, style) in (1u32..).zip(&self.pages) {
            let is_hidden = match style {
                Some(name) => match pages.get(name.as_str()) {
                    Some(&known) => known?,
                    None => {
                        let known = self.hides(name, &mut common);
                        pages.insert(name, known);
                        known?
                    }
                },
                None => false,
            };
            if is_hidden {
                hidden.push(position);
            }
        }
        Some(Slides {
            count: self.pages.len().try_into().ok()?,
            hidden,
        })
    }
}

/// Elements whose text is not printed, notes pages included.
const NOT_PRINTED: &[&[u8]] = &[b"annotation", b"tracked-changes", b"notes"];
/// The same when the request exports notes pages.
const NOT_PRINTED_WITH_NOTES: &[&[u8]] = &[b"annotation", b"tracked-changes"];
/// The alternative text of frames, by namespace.
const SVG: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0";
const ALT_TEXT: &[(&[u8], &[u8])] = &[(SVG, b"title"), (SVG, b"desc")];

pub(super) fn scan(
    package: &mut Package,
    family: Family,
    text: &mut TextCollector,
    printed: Printed,
    facts: &mut SourceFacts,
) {
    facts.macros = macros(package);
    let mut found = BTreeSet::new();
    let mut deck = (family == Family::Presentation).then(Deck::default);
    let mut read_all = true;
    // The text of content.xml, apart: it may be read again without the
    // hidden pages.
    let mut body = TextCollector::new();
    for part in ["content.xml", "styles.xml"] {
        let content = part == "content.xml";
        if let Some(deck) = deck.as_mut() {
            deck.start_part();
        }
        let collector = if content { &mut body } else { &mut *text };
        let result = read_text(package, part, printed, &[], collector, |event| {
            if let XmlEvent::Start(e) | XmlEvent::Empty(e) = event
                && e.local_name().as_ref() == b"font-face"
                && let Some(family) = attr(e, b"font-family")
            {
                // LibreOffice quotes names with spaces: 'Liberation Serif'.
                add_font(&mut found, family.trim().trim_matches(['\'', '"']));
            }
            if let Some(deck) = deck.as_mut() {
                deck.on_event(content, event);
            }
        });
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(reason)) => facts.skipped.push((Check::Glyphs, reason)),
            Err(SkipReason::MissingPart) if !content => {}
            Err(SkipReason::MissingPart) => read_all = false,
            Err(reason) => {
                facts.skipped.push((Check::Fonts, reason));
                facts.skipped.push((Check::Glyphs, reason));
                return;
            }
        }
    }
    facts.requested_fonts = found.into_iter().collect();
    if read_all {
        facts.slides = deck.and_then(|deck| deck.slides());
    }
    if !printed.hidden_slides
        && let Some(slides) = facts.slides.as_ref()
        && !slides.hidden.is_empty()
    {
        let mut visible = TextCollector::new();
        match read_text(
            package,
            "content.xml",
            printed,
            &slides.hidden,
            &mut visible,
            |_| {},
        ) {
            Ok(Ok(pages)) if pages == slides.count => body = visible,
            // Other pages than the list's: every page counts.
            Ok(Ok(_)) => {}
            Ok(Err(reason)) | Err(reason) => facts.skipped.push((Check::Glyphs, reason)),
        }
    }
    text.absorb(body);
}

/// Reads the printed text of `part` into `text`, leaving out the pages at
/// the 1-based positions `hidden` (sorted), and passes every event to
/// `also`. The outer error is a part that cannot be read; the inner one a
/// part that does not read as a whole for the text. Returns the number of
/// `draw:page` children of `office:body/office:presentation`.
fn read_text(
    package: &mut Package,
    part: &str,
    printed: Printed,
    hidden: &[u32],
    text: &mut TextCollector,
    mut also: impl FnMut(&XmlEvent<'_, '_>),
) -> Result<Result<u32, SkipReason>, SkipReason> {
    let content = part == "content.xml";
    // The master pages' notes are a template, never printed as such.
    let skip = if printed.notes && content {
        NOT_PRINTED_WITH_NOTES
    } else {
        NOT_PRINTED
    };
    let within: &'static [u8] = if content { b"body" } else { b"master-styles" };
    let mut scope = TextScope::new(&[], Some(within), skip).skipping_ns(ALT_TEXT);
    let (mut depth, mut in_body, mut in_deck, mut pages) = (0usize, false, false, 0u32);
    package.xml_events(part, PART_CAP, |event| {
        if let Some(drawn) = scope.on(&event) {
            text.add(drawn);
        }
        also(&event);
        match event {
            XmlEvent::Start(e) | XmlEvent::Empty(e) => {
                let opens = matches!(event, XmlEvent::Start(_));
                match (depth, e.local_name().as_ref()) {
                    (1, b"body") => in_body = opens,
                    (2, b"presentation") if in_body => in_deck = opens,
                    (3, b"page") if in_deck => {
                        pages = pages.saturating_add(1);
                        if opens && hidden.binary_search(&pages).is_ok() {
                            scope.skip_current();
                        }
                    }
                    (_, b"table") if opens && attr(e, b"display").as_deref() == Some("false") => {
                        scope.skip_current();
                    }
                    _ => {}
                }
                if opens {
                    depth += 1;
                }
            }
            XmlEvent::End => {
                depth = depth.saturating_sub(1);
                match depth {
                    2 => in_deck = false,
                    1 => in_body = false,
                    _ => {}
                }
            }
            _ => {}
        }
        true
    })?;
    Ok(scope.complete().map(|()| pages))
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
