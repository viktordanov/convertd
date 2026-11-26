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

use std::collections::{BTreeSet, HashMap};

use quick_xml::events::BytesStart;

use super::package::{Package, XmlEvent, attr};
use super::pptx::MAX_SLIDES;
use super::{PART_CAP, add_font};
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

/// The pages of a presentation and the drawing-page styles that can apply
/// to them.
#[derive(Debug, Default)]
struct Deck {
    /// The style name of each slide, in order.
    pages: Vec<Option<String>>,
    automatic: HashMap<String, PageStyle>,
    common: HashMap<String, PageStyle>,
    /// The local names of the open elements of the part being read.
    open: Vec<Vec<u8>>,
    /// Elements opened past `MAX_DEPTH`, not kept in `open`.
    deeper: usize,
    /// The slide style whose properties come next.
    current: Option<(Scope, String)>,
    too_many: bool,
}

impl Deck {
    fn start_part(&mut self) {
        self.open.clear();
        self.deeper = 0;
        self.current = None;
    }

    fn on_event(&mut self, content: bool, event: &XmlEvent<'_, '_>) {
        match event {
            XmlEvent::Start(e) => {
                self.on_element(content, e);
                if self.deeper > 0 || self.open.len() >= MAX_DEPTH {
                    self.deeper += 1;
                } else {
                    self.open.push(e.local_name().as_ref().to_vec());
                }
            }
            XmlEvent::Empty(e) => self.on_element(content, e),
            XmlEvent::End => {
                if self.deeper > 0 {
                    self.deeper -= 1;
                } else if self.open.pop().as_deref() == Some(b"style") {
                    self.current = None;
                }
            }
            XmlEvent::Text => {}
        }
    }

    /// Whether the open elements, below the root, are exactly `path`.
    fn under(&self, path: &[&[u8]]) -> bool {
        self.deeper == 0
            && self.open.len() == path.len() + 1
            && self.open[1..].iter().zip(path).all(|(a, b)| a == b)
    }

    fn on_element(&mut self, content: bool, e: &BytesStart<'_>) {
        match e.local_name().as_ref() {
            b"style" => {
                let scope = if content && self.under(&[b"automatic-styles"]) {
                    Scope::Automatic
                } else if !content && self.under(&[b"styles"]) {
                    Scope::Common
                } else {
                    return;
                };
                if attr(e, b"family").as_deref() != Some("drawing-page") {
                    return;
                }
                let Some(name) = attr(e, b"name") else {
                    return;
                };
                let styles = match scope {
                    Scope::Automatic => &mut self.automatic,
                    Scope::Common => &mut self.common,
                };
                let style = styles.entry(name.clone()).or_default();
                style.parent = attr(e, b"parent-style-name");
                self.current = Some((scope, name));
            }
            b"drawing-page-properties" => {
                if self.open.last().map(Vec::as_slice) != Some(b"style") {
                    return;
                }
                if let Some((scope, name)) = &self.current
                    && let Some(visibility) = attr(e, b"visibility")
                {
                    let styles = match scope {
                        Scope::Automatic => &mut self.automatic,
                        Scope::Common => &mut self.common,
                    };
                    if let Some(style) = styles.get_mut(name) {
                        style.hidden = Some(visibility == "hidden");
                    }
                }
            }
            b"page" if content && self.under(&[b"body", b"presentation"]) => {
                if self.pages.len() >= MAX_SLIDES {
                    self.too_many = true;
                } else {
                    self.pages.push(attr(e, b"style-name"));
                }
            }
            _ => {}
        }
    }

    /// Whether the slide style `name` hides its page; `None` when a style
    /// in the chain is not defined.
    fn hides(&self, name: &str) -> Option<bool> {
        let mut style = self.automatic.get(name).or_else(|| self.common.get(name))?;
        for _ in 0..MAX_STYLE_DEPTH {
            if let Some(hidden) = style.hidden {
                return Some(hidden);
            }
            match &style.parent {
                Some(parent) => style = self.common.get(parent)?,
                None => return Some(false),
            }
        }
        None
    }

    fn slides(&self) -> Option<Slides> {
        if self.too_many {
            return None;
        }
        let mut hidden = Vec::new();
        for (position, style) in (1u32..).zip(&self.pages) {
            let is_hidden = match style {
                Some(name) => self.hides(name)?,
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

pub(super) fn scan(package: &mut Package, family: Family, facts: &mut SourceFacts) {
    facts.macros = macros(package);
    let mut found = BTreeSet::new();
    let mut deck = (family == Family::Presentation).then(Deck::default);
    let mut read_all = true;
    for part in ["content.xml", "styles.xml"] {
        let content = part == "content.xml";
        if let Some(deck) = deck.as_mut() {
            deck.start_part();
        }
        let result = package.xml_events(part, PART_CAP, |event| {
            if let XmlEvent::Start(e) | XmlEvent::Empty(e) = &event
                && e.local_name().as_ref() == b"font-face"
                && let Some(family) = attr(e, b"font-family")
            {
                // LibreOffice quotes names with spaces: 'Liberation Serif'.
                add_font(&mut found, family.trim().trim_matches(['\'', '"']));
            }
            if let Some(deck) = deck.as_mut() {
                deck.on_event(content, &event);
            }
            true
        });
        match result {
            Ok(_) => {}
            Err(SkipReason::MissingPart) if !content => {}
            Err(SkipReason::MissingPart) => read_all = false,
            Err(reason) => {
                facts.skipped.push((Check::Fonts, reason));
                return;
            }
        }
    }
    facts.requested_fonts = found.into_iter().collect();
    if read_all {
        facts.slides = deck.and_then(|deck| deck.slides());
    }
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
