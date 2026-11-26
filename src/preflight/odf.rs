//! OpenDocument (odt, odp, ods): the requested families (V2) from
//! `office:font-face-decls/style:font-face@svg:font-family` in
//! `content.xml` and `styles.xml`, and the macro storage (V4). No declared
//! page count.
//!
//! For a presentation, the same pass lists the slides: each `draw:page` of
//! `content.xml` in order, hidden when its drawing-page style, or a parent
//! of that style, has `presentation:visibility="hidden"`. A page whose style
//! is defined in neither part leaves the list unknown (`None`).

use std::collections::{BTreeSet, HashMap};

use super::package::{Package, attr};
use super::pptx::MAX_SLIDES;
use super::{PART_CAP, add_font};
use crate::domain::document::Family;
use crate::domain::verification::{Check, MacroFact, MacroKind, SkipReason, Slides, SourceFacts};

/// The longest chain of parent styles that is followed.
const MAX_STYLE_DEPTH: usize = 16;

/// A drawing-page style: its own visibility, if it sets one, and its parent.
#[derive(Debug, Default)]
struct PageStyle {
    hidden: Option<bool>,
    parent: Option<String>,
}

/// The pages of a presentation and the drawing-page styles both parts define.
#[derive(Debug, Default)]
struct Deck {
    /// The style name of each `draw:page`, in order.
    pages: Vec<Option<String>>,
    styles: HashMap<String, PageStyle>,
    /// The drawing-page style whose properties come next.
    current: Option<String>,
    too_many: bool,
}

impl Deck {
    fn on_start(&mut self, local: &[u8], e: &quick_xml::events::BytesStart<'_>) {
        match local {
            b"style" | b"default-style" => {
                self.current = None;
                if local == b"style"
                    && attr(e, b"family").as_deref() == Some("drawing-page")
                    && let Some(name) = attr(e, b"name")
                {
                    let style = self.styles.entry(name.clone()).or_default();
                    style.parent = attr(e, b"parent-style-name");
                    self.current = Some(name);
                }
            }
            b"drawing-page-properties" => {
                if let Some(name) = &self.current
                    && let Some(visibility) = attr(e, b"visibility")
                    && let Some(style) = self.styles.get_mut(name)
                {
                    style.hidden = Some(visibility == "hidden");
                }
            }
            b"page" => {
                if self.pages.len() >= MAX_SLIDES {
                    self.too_many = true;
                } else {
                    self.pages.push(attr(e, b"style-name"));
                }
            }
            _ => {}
        }
    }

    /// Whether the style `name` hides its page; `None` when a style in the
    /// chain is not defined.
    fn hides(&self, name: &str) -> Option<bool> {
        let mut name = name;
        for _ in 0..MAX_STYLE_DEPTH {
            let style = self.styles.get(name)?;
            if let Some(hidden) = style.hidden {
                return Some(hidden);
            }
            match &style.parent {
                Some(parent) => name = parent,
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
        let result = package.xml(part, PART_CAP, |e| {
            let local = e.local_name();
            if local.as_ref() == b"font-face"
                && let Some(family) = attr(e, b"font-family")
            {
                // LibreOffice quotes names with spaces: 'Liberation Serif'.
                add_font(&mut found, family.trim().trim_matches(['\'', '"']));
            }
            if let Some(deck) = deck.as_mut() {
                // Pages live in content.xml only; styles.xml has master pages.
                if part == "content.xml" || local.as_ref() != b"page" {
                    deck.on_start(local.as_ref(), e);
                }
            }
            true
        });
        match result {
            Ok(_) => {}
            Err(SkipReason::MissingPart) if part == "styles.xml" => {}
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
