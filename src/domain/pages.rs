//! The `pages` option: `3`, `1-5`, `10-`, comma-separated, 1-based.

use super::conversion::{ExportOptions, SlideNotes};
use super::conversion_service::ConversionError;
use super::verification::Slides;

/// The longest `pages` value the spec allows.
pub const PAGES_MAX_LEN: usize = 256;

/// A parsed `pages` value. Resolved against the page count only after the
/// PDF exists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PageSelection {
    /// No `pages` option: every page.
    #[default]
    All,
    /// Inclusive ranges; `None` as the end means "to the last page".
    Ranges(Vec<(u32, Option<u32>)>),
}

impl PageSelection {
    /// Parses `^[0-9]+(-[0-9]*)?(,[0-9]+(-[0-9]*)?)*$`, at most 256
    /// characters. Page 0 and reversed ranges are rejected.
    pub fn parse(text: &str) -> Result<Self, ConversionError> {
        let invalid = || {
            ConversionError::Validation(
                "pages must be a comma-separated list of pages and ranges, such as 1-5,8,10-"
                    .into(),
            )
        };
        if text.is_empty() || text.len() > PAGES_MAX_LEN {
            return Err(invalid());
        }
        let mut ranges = Vec::new();
        for item in text.split(',') {
            let (start, end) = match item.split_once('-') {
                None => (item, Some(item)),
                Some((start, "")) => (start, None),
                Some((start, end)) => (start, Some(end)),
            };
            let start = parse_page(start).ok_or_else(invalid)?;
            let end = match end {
                None => None,
                Some(end) => Some(parse_page(end).ok_or_else(invalid)?),
            };
            if start == 0 || end == Some(0) {
                return Err(ConversionError::Validation(
                    "pages are numbered from 1".into(),
                ));
            }
            if let Some(end) = end
                && end < start
            {
                return Err(ConversionError::Validation(format!(
                    "pages range {start}-{end} runs backwards"
                )));
            }
            ranges.push((start, end));
        }
        Ok(PageSelection::Ranges(ranges))
    }

    /// The selected pages that exist, sorted and without duplicates.
    /// Nothing left gives `VALIDATION_ERROR`; more than `max_render_pages`
    /// gives `LIMIT_EXCEEDED`. Never allocates more than the result.
    pub fn resolve(
        &self,
        page_count: u32,
        max_render_pages: u32,
    ) -> Result<Vec<u32>, ConversionError> {
        let mut ranges: Vec<(u32, u32)> = match self {
            PageSelection::All => vec![(1, page_count)],
            PageSelection::Ranges(ranges) => ranges
                .iter()
                .map(|&(start, end)| (start, end.unwrap_or(page_count).min(page_count)))
                .filter(|&(start, end)| start <= end)
                .collect(),
        };
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        let total: u64 = merged
            .iter()
            .map(|&(start, end)| u64::from(end - start) + 1)
            .sum();
        if total == 0 {
            return Err(ConversionError::Validation(
                "pages selects no page of the document".into(),
            ));
        }
        if total > u64::from(max_render_pages) {
            return Err(ConversionError::LimitExceeded(format!(
                "{total} pages selected, more than the {max_render_pages} page render limit"
            )));
        }
        let mut pages = Vec::with_capacity(total as usize);
        for (start, end) in merged {
            pages.extend(start..=end);
        }
        Ok(pages)
    }
}

/// One run of ASCII digits as a page number; `None` for anything else.
fn parse_page(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The PDF pages that come from hidden slides, ascending.
///
/// Only with `slides.include_hidden`: without it, LibreOffice exports no
/// hidden slide. The slide list must predict the page count exactly:
/// `count` pages for slides only or for notes pages only, `2 × count` for
/// slides followed by their notes pages. When the PDF has any other count,
/// or the list is unknown, no page is marked: a wrong mark is worse than
/// none.
pub fn hidden_pages(slides: Option<&Slides>, export: &ExportOptions, page_count: u32) -> Vec<u32> {
    let Some(slides) = slides else {
        return Vec::new();
    };
    if !export.include_hidden_slides || slides.hidden.is_empty() {
        return Vec::new();
    }
    match export.notes {
        SlideNotes::None | SlideNotes::Only if page_count == slides.count => slides.hidden.clone(),
        SlideNotes::Append if Some(page_count) == slides.count.checked_mul(2) => slides
            .hidden
            .iter()
            .copied()
            .chain(slides.hidden.iter().map(|n| n + slides.count))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(include_hidden_slides: bool, notes: SlideNotes) -> ExportOptions {
        ExportOptions {
            notes,
            include_hidden_slides,
            one_page_per_sheet: false,
        }
    }

    #[test]
    fn hidden_pages_follow_the_slides_when_the_count_matches() {
        let three = Slides {
            count: 3,
            hidden: vec![2],
        };
        let on = export(true, SlideNotes::None);
        assert_eq!(hidden_pages(Some(&three), &on, 3), [2]);
        // Without the option LibreOffice drops the hidden slide.
        let off = export(false, SlideNotes::None);
        assert!(hidden_pages(Some(&three), &off, 2).is_empty());
        // A count the slides do not predict: no mark.
        assert!(hidden_pages(Some(&three), &on, 2).is_empty());
        assert!(hidden_pages(Some(&three), &on, 4).is_empty());
        // Unknown slides (a .ppt, an unread part): no mark.
        assert!(hidden_pages(None, &on, 3).is_empty());
        // Notes appended: the slide page and its notes page.
        let append = export(true, SlideNotes::Append);
        assert_eq!(hidden_pages(Some(&three), &append, 6), [2, 5]);
        assert!(hidden_pages(Some(&three), &append, 3).is_empty());
        // Notes only: one notes page per slide.
        let only = export(true, SlideNotes::Only);
        assert_eq!(hidden_pages(Some(&three), &only, 3), [2]);
        // No hidden slide: nothing to mark, whatever the option.
        let none = Slides {
            count: 3,
            hidden: vec![],
        };
        assert!(hidden_pages(Some(&none), &on, 3).is_empty());
    }

    fn resolve(text: &str, count: u32) -> Result<Vec<u32>, ConversionError> {
        PageSelection::parse(text)?.resolve(count, 500)
    }

    #[test]
    fn grammar_matches_the_spec_pattern() {
        // The cases of scripts/check_spec.py.
        for (text, ok) in [
            ("1", true),
            ("1-5,8,10-", true),
            ("1-", true),
            ("-3", false),
            ("a", false),
            ("1,,2", false),
            ("", false),
        ] {
            assert_eq!(PageSelection::parse(text).is_ok(), ok, "{text:?}");
        }
        for text in ["1 ", " 1", "1-2-3", "+1", "1,", ",1", "1--"] {
            assert!(PageSelection::parse(text).is_err(), "{text:?}");
        }
        let long = format!("1{}", ",1".repeat(127));
        assert_eq!(long.len(), 255);
        assert!(PageSelection::parse(&long).is_ok());
        assert!(PageSelection::parse(&format!("{long}0")).is_ok());
        assert!(PageSelection::parse(&format!("{long},1")).is_err());
    }

    #[test]
    fn zero_and_backwards_are_rejected() {
        for text in ["0", "0-3", "2-0", "5-3"] {
            let err = PageSelection::parse(text).unwrap_err();
            assert!(matches!(err, ConversionError::Validation(_)), "{text}");
        }
        assert!(PageSelection::parse("99999999999").is_err());
    }

    #[test]
    fn resolves_against_the_page_count() {
        assert_eq!(
            resolve("1-5,8,10-", 12).unwrap(),
            [1, 2, 3, 4, 5, 8, 10, 11, 12]
        );
        assert_eq!(resolve("3", 5).unwrap(), [3]);
        assert_eq!(resolve("2-", 4).unwrap(), [2, 3, 4]);
        assert_eq!(resolve("4-100", 5).unwrap(), [4, 5]);
        assert_eq!(PageSelection::All.resolve(3, 500).unwrap(), [1, 2, 3]);
    }

    #[test]
    fn duplicates_are_merged() {
        assert_eq!(resolve("2,1-3,3,2-2", 10).unwrap(), [1, 2, 3]);
        assert_eq!(resolve("1-2,3-4", 10).unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    fn nothing_left_is_a_validation_error() {
        let err = resolve("10-", 3).unwrap_err();
        assert!(matches!(err, ConversionError::Validation(_)), "{err:?}");
        let err = resolve("7,9", 3).unwrap_err();
        assert!(matches!(err, ConversionError::Validation(_)), "{err:?}");
    }

    #[test]
    fn over_the_render_limit() {
        let err = PageSelection::parse("1-")
            .unwrap()
            .resolve(501, 500)
            .unwrap_err();
        assert!(matches!(err, ConversionError::LimitExceeded(_)), "{err:?}");
        assert_eq!(PageSelection::All.resolve(500, 500).unwrap().len(), 500);
        // A huge range is counted, not expanded.
        let err = PageSelection::parse("1-4000000000")
            .unwrap()
            .resolve(u32::MAX, 500)
            .unwrap_err();
        assert!(matches!(err, ConversionError::LimitExceeded(_)));
    }
}
