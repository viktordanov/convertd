//! Conversions between `domain::verification::SourceFacts` and its wire form.
//!
//! `DeclaredPages::source` is a `&'static str` in the frozen domain type, so
//! the supervisor maps the wire string back to a static one: the two
//! sources that the scanners name map to themselves, and any other source
//! reads as `"other"` (the lead's ruling for Round A; the domain type
//! becomes a `String` in Round B).

use crate::domain::text::needs_glyph;
use crate::domain::verification::{
    Check, DeclaredPages, MAX_CODE_POINTS, MacroFact, MacroKind, SkipReason, SourceFacts,
};
use crate::ipc::protocol::{
    CheckWire, DeclaredPagesWire, MacroFactWire, MacroKindWire, SkipReasonWire, SourceFactsWire,
};

/// The page-count sources the pre-flight scanners name (005 §5).
pub const KNOWN_SOURCES: [&str; 2] = ["ppt/presentation.xml", "docProps/app.xml"];

/// The static form of `source`: a known source, or `"other"`.
pub fn static_source(source: &str) -> &'static str {
    KNOWN_SOURCES
        .iter()
        .find(|known| **known == source)
        .copied()
        .unwrap_or("other")
}

impl From<&SourceFacts> for SourceFactsWire {
    fn from(facts: &SourceFacts) -> Self {
        Self {
            declared_pages: facts.declared_pages.as_ref().map(|d| DeclaredPagesWire {
                count: d.count,
                source: d.source.to_owned(),
            }),
            requested_fonts: facts.requested_fonts.clone(),
            embedded_fonts: facts.embedded_fonts.clone(),
            text_code_points: facts.text_code_points.clone(),
            macros: facts.macros.as_ref().map(|m| MacroFactWire {
                kind: match m.kind {
                    MacroKind::Vba => MacroKindWire::Vba,
                    MacroKind::Xlm => MacroKindWire::Xlm,
                    MacroKind::Basic => MacroKindWire::Basic,
                    MacroKind::Script => MacroKindWire::Script,
                },
                part: m.part.clone(),
            }),
            skipped: facts
                .skipped
                .iter()
                .map(|&(check, reason)| {
                    let check = match check {
                        Check::PageCount => CheckWire::PageCount,
                        Check::Fonts => CheckWire::Fonts,
                        Check::Macros => CheckWire::Macros,
                        Check::Glyphs => CheckWire::Glyphs,
                    };
                    let reason = match reason {
                        SkipReason::CapReached => SkipReasonWire::CapReached,
                        SkipReason::MissingPart => SkipReasonWire::MissingPart,
                        SkipReason::MalformedXml => SkipReasonWire::MalformedXml,
                    };
                    (check, reason)
                })
                .collect(),
        }
    }
}

impl From<&SourceFactsWire> for SourceFacts {
    fn from(wire: &SourceFactsWire) -> Self {
        Self {
            declared_pages: wire.declared_pages.as_ref().map(|d| DeclaredPages {
                count: d.count,
                source: static_source(&d.source),
            }),
            requested_fonts: wire.requested_fonts.clone(),
            embedded_fonts: wire.embedded_fonts.clone(),
            text_code_points: code_points(&wire.text_code_points),
            macros: wire.macros.as_ref().map(|m| MacroFact {
                kind: match m.kind {
                    MacroKindWire::Vba => MacroKind::Vba,
                    MacroKindWire::Xlm => MacroKind::Xlm,
                    MacroKindWire::Basic => MacroKind::Basic,
                    MacroKindWire::Script => MacroKind::Script,
                },
                part: m.part.clone(),
            }),
            skipped: wire
                .skipped
                .iter()
                .map(|&(check, reason)| {
                    let check = match check {
                        CheckWire::PageCount => Check::PageCount,
                        CheckWire::Fonts => Check::Fonts,
                        CheckWire::Macros => Check::Macros,
                        CheckWire::Glyphs => Check::Glyphs,
                    };
                    let reason = match reason {
                        SkipReasonWire::CapReached => SkipReason::CapReached,
                        SkipReasonWire::MissingPart => SkipReason::MissingPart,
                        SkipReasonWire::MalformedXml => SkipReason::MalformedXml,
                    };
                    (check, reason)
                })
                .collect(),
        }
    }
}

/// The worker parses hostile files, so the supervisor keeps its own bound:
/// only code points that need a glyph, sorted, distinct, at most
/// [`MAX_CODE_POINTS`].
fn code_points(wire: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = wire.iter().copied().filter(|&cp| needs_glyph(cp)).collect();
    out.sort_unstable();
    out.dedup();
    out.truncate(MAX_CODE_POINTS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_round_trip_through_the_wire_form() {
        let facts = SourceFacts {
            declared_pages: Some(DeclaredPages {
                count: 12,
                source: "ppt/presentation.xml",
            }),
            requested_fonts: vec!["Aptos".into(), "Calibri".into()],
            embedded_fonts: vec!["Aptos".into()],
            text_code_points: vec![0x41, 0x0e01],
            macros: Some(MacroFact {
                kind: MacroKind::Basic,
                part: "Basic/".into(),
            }),
            skipped: vec![
                (Check::Fonts, SkipReason::CapReached),
                (Check::Macros, SkipReason::MalformedXml),
                (Check::Glyphs, SkipReason::CapReached),
            ],
        };
        let wire = SourceFactsWire::from(&facts);
        let json = serde_json::to_string(&wire).unwrap();
        let back: SourceFactsWire = serde_json::from_str(&json).unwrap();
        assert_eq!(SourceFacts::from(&back), facts);
        assert_eq!(
            SourceFacts::from(&SourceFactsWire::default()),
            SourceFacts::default()
        );
    }

    #[test]
    fn the_supervisor_bounds_the_code_points_it_receives() {
        // A compromised worker can send anything; the supervisor sorts,
        // dedups and caps.
        let wire = SourceFactsWire {
            text_code_points: (0x4E00..0x4E00 + 10_000u32)
                .rev()
                .chain([0x4E05, 0x4E05, 0x0A, 0xE000, 0x11_0000, u32::MAX])
                .collect(),
            ..Default::default()
        };
        let facts = SourceFacts::from(&wire);
        assert_eq!(facts.text_code_points.len(), MAX_CODE_POINTS);
        assert!(facts.text_code_points.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(facts.text_code_points[0], 0x4E00);
        assert!(facts.text_code_points.iter().all(|&cp| cp < 0x11_0000));
    }

    #[test]
    fn known_sources_map_back_and_others_read_as_other() {
        assert_eq!(
            static_source("ppt/presentation.xml"),
            "ppt/presentation.xml"
        );
        assert_eq!(static_source("docProps/app.xml"), "docProps/app.xml");
        assert_eq!(static_source("word/settings.xml"), "other");
    }
}
