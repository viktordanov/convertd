//! Which characters of a document's text need a glyph, and the script a
//! character belongs to, for `GLYPHS_MISSING`.
//!
//! The data is a small table of Unicode 16.0 ranges, not the full
//! character database: the rule needs to know which characters a font
//! must draw, and to name the script of a missing one well enough for a
//! person to act on it.

/// Whether a font must draw `cp`. Not for controls, white space,
/// default-ignorable characters (ZWJ, ZWNJ, variation selectors, U+FEFF
/// and the others of `DerivedCoreProperties.txt`), private-use characters
/// (LibreOffice recodes Symbol and Wingdings text to OpenSymbol),
/// surrogates and noncharacters.
pub fn needs_glyph(cp: u32) -> bool {
    !(in_ranges(cp, CONTROLS)
        || in_ranges(cp, WHITE_SPACE)
        || in_ranges(cp, DEFAULT_IGNORABLE)
        || in_ranges(cp, PRIVATE_USE)
        || (0xD800..=0xDFFF).contains(&cp)
        || (0xFDD0..=0xFDEF).contains(&cp)
        || cp & 0xFFFE == 0xFFFE
        || cp > 0x10FFFF)
}

/// The name of the script `cp` belongs to, by block: "Thai", "Han",
/// "Ethiopic". A character outside the table is "Other".
pub fn script_name(cp: u32) -> &'static str {
    let at = SCRIPTS.partition_point(|&(_, end, _)| end < cp);
    match SCRIPTS.get(at) {
        Some(&(start, _, name)) if start <= cp => name,
        _ => "Other",
    }
}

fn in_ranges(cp: u32, ranges: &[(u32, u32)]) -> bool {
    let at = ranges.partition_point(|&(_, end)| end < cp);
    ranges.get(at).is_some_and(|&(start, _)| start <= cp)
}

/// General category Cc.
const CONTROLS: &[(u32, u32)] = &[(0x0000, 0x001F), (0x007F, 0x009F)];

/// `White_Space` (PropList.txt), outside the controls.
const WHITE_SPACE: &[(u32, u32)] = &[
    (0x0020, 0x0020),
    (0x0085, 0x0085),
    (0x00A0, 0x00A0),
    (0x1680, 0x1680),
    (0x2000, 0x200A),
    (0x2028, 0x2029),
    (0x202F, 0x202F),
    (0x205F, 0x205F),
    (0x3000, 0x3000),
];

/// `Default_Ignorable_Code_Point` (DerivedCoreProperties.txt, 16.0).
const DEFAULT_IGNORABLE: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFF8),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE0FFF),
];

/// General category Co.
const PRIVATE_USE: &[(u32, u32)] = &[(0xE000, 0xF8FF), (0xF0000, 0xFFFFD), (0x100000, 0x10FFFD)];

/// Blocks by script, sorted and disjoint (Blocks.txt, 16.0). Symbols and
/// punctuation blocks are "Common".
const SCRIPTS: &[(u32, u32, &str)] = &[
    (0x0000, 0x007F, "Latin"),
    (0x0080, 0x024F, "Latin"),
    (0x0250, 0x02AF, "Latin"),
    (0x02B0, 0x02FF, "Common"),
    (0x0300, 0x036F, "Inherited"),
    (0x0370, 0x03FF, "Greek"),
    (0x0400, 0x052F, "Cyrillic"),
    (0x0530, 0x058F, "Armenian"),
    (0x0590, 0x05FF, "Hebrew"),
    (0x0600, 0x06FF, "Arabic"),
    (0x0700, 0x074F, "Syriac"),
    (0x0750, 0x077F, "Arabic"),
    (0x0780, 0x07BF, "Thaana"),
    (0x07C0, 0x07FF, "Nko"),
    (0x0800, 0x083F, "Samaritan"),
    (0x0840, 0x085F, "Mandaic"),
    (0x0860, 0x086F, "Syriac"),
    (0x0870, 0x08FF, "Arabic"),
    (0x0900, 0x097F, "Devanagari"),
    (0x0980, 0x09FF, "Bengali"),
    (0x0A00, 0x0A7F, "Gurmukhi"),
    (0x0A80, 0x0AFF, "Gujarati"),
    (0x0B00, 0x0B7F, "Oriya"),
    (0x0B80, 0x0BFF, "Tamil"),
    (0x0C00, 0x0C7F, "Telugu"),
    (0x0C80, 0x0CFF, "Kannada"),
    (0x0D00, 0x0D7F, "Malayalam"),
    (0x0D80, 0x0DFF, "Sinhala"),
    (0x0E00, 0x0E7F, "Thai"),
    (0x0E80, 0x0EFF, "Lao"),
    (0x0F00, 0x0FFF, "Tibetan"),
    (0x1000, 0x109F, "Myanmar"),
    (0x10A0, 0x10FF, "Georgian"),
    (0x1100, 0x11FF, "Hangul"),
    (0x1200, 0x139F, "Ethiopic"),
    (0x13A0, 0x13FF, "Cherokee"),
    (0x1400, 0x167F, "Canadian Aboriginal"),
    (0x1680, 0x169F, "Ogham"),
    (0x16A0, 0x16FF, "Runic"),
    (0x1700, 0x171F, "Tagalog"),
    (0x1780, 0x17FF, "Khmer"),
    (0x1800, 0x18AF, "Mongolian"),
    (0x18B0, 0x18FF, "Canadian Aboriginal"),
    (0x1900, 0x194F, "Limbu"),
    (0x1950, 0x197F, "Tai Le"),
    (0x1980, 0x19DF, "New Tai Lue"),
    (0x19E0, 0x19FF, "Khmer"),
    (0x1A00, 0x1A1F, "Buginese"),
    (0x1A20, 0x1AAF, "Tai Tham"),
    (0x1AB0, 0x1AFF, "Inherited"),
    (0x1B00, 0x1B7F, "Balinese"),
    (0x1B80, 0x1BBF, "Sundanese"),
    (0x1BC0, 0x1BFF, "Batak"),
    (0x1C00, 0x1C4F, "Lepcha"),
    (0x1C50, 0x1C7F, "Ol Chiki"),
    (0x1C80, 0x1C8F, "Cyrillic"),
    (0x1C90, 0x1CBF, "Georgian"),
    (0x1CC0, 0x1CCF, "Sundanese"),
    (0x1CD0, 0x1CFF, "Inherited"),
    (0x1D00, 0x1DBF, "Latin"),
    (0x1DC0, 0x1DFF, "Inherited"),
    (0x1E00, 0x1EFF, "Latin"),
    (0x1F00, 0x1FFF, "Greek"),
    (0x2000, 0x2BFF, "Common"),
    (0x2C00, 0x2C5F, "Glagolitic"),
    (0x2C60, 0x2C7F, "Latin"),
    (0x2C80, 0x2CFF, "Coptic"),
    (0x2D00, 0x2D2F, "Georgian"),
    (0x2D30, 0x2D7F, "Tifinagh"),
    (0x2D80, 0x2DDF, "Ethiopic"),
    (0x2DE0, 0x2DFF, "Cyrillic"),
    (0x2E00, 0x2E7F, "Common"),
    (0x2E80, 0x2FDF, "Han"),
    (0x2FF0, 0x303F, "Common"),
    (0x3040, 0x309F, "Hiragana"),
    (0x30A0, 0x30FF, "Katakana"),
    (0x3100, 0x312F, "Bopomofo"),
    (0x3130, 0x318F, "Hangul"),
    (0x3190, 0x319F, "Common"),
    (0x31A0, 0x31BF, "Bopomofo"),
    (0x31C0, 0x31EF, "Common"),
    (0x31F0, 0x31FF, "Katakana"),
    (0x3200, 0x33FF, "Common"),
    (0x3400, 0x4DBF, "Han"),
    (0x4DC0, 0x4DFF, "Common"),
    (0x4E00, 0x9FFF, "Han"),
    (0xA000, 0xA4CF, "Yi"),
    (0xA4D0, 0xA4FF, "Lisu"),
    (0xA500, 0xA63F, "Vai"),
    (0xA640, 0xA69F, "Cyrillic"),
    (0xA6A0, 0xA6FF, "Bamum"),
    (0xA700, 0xA7FF, "Latin"),
    (0xA800, 0xA82F, "Syloti Nagri"),
    (0xA840, 0xA87F, "Phags Pa"),
    (0xA880, 0xA8DF, "Saurashtra"),
    (0xA8E0, 0xA8FF, "Devanagari"),
    (0xA900, 0xA92F, "Kayah Li"),
    (0xA930, 0xA95F, "Rejang"),
    (0xA960, 0xA97F, "Hangul"),
    (0xA980, 0xA9DF, "Javanese"),
    (0xA9E0, 0xA9FF, "Myanmar"),
    (0xAA00, 0xAA5F, "Cham"),
    (0xAA60, 0xAA7F, "Myanmar"),
    (0xAA80, 0xAADF, "Tai Viet"),
    (0xAAE0, 0xAAFF, "Meetei Mayek"),
    (0xAB00, 0xAB2F, "Ethiopic"),
    (0xAB30, 0xAB6F, "Latin"),
    (0xAB70, 0xABBF, "Cherokee"),
    (0xABC0, 0xABFF, "Meetei Mayek"),
    (0xAC00, 0xD7FF, "Hangul"),
    (0xF900, 0xFAFF, "Han"),
    (0xFB00, 0xFB12, "Latin"),
    (0xFB13, 0xFB1C, "Armenian"),
    (0xFB1D, 0xFB4F, "Hebrew"),
    (0xFB50, 0xFDFF, "Arabic"),
    (0xFE00, 0xFE6F, "Common"),
    (0xFE70, 0xFEFF, "Arabic"),
    (0xFF00, 0xFFEF, "Common"),
    (0xFFF0, 0xFFFF, "Common"),
    (0x10000, 0x1007F, "Linear B"),
    (0x10300, 0x1032F, "Old Italic"),
    (0x10330, 0x1034F, "Gothic"),
    (0x10380, 0x1039F, "Ugaritic"),
    (0x103A0, 0x103DF, "Old Persian"),
    (0x10400, 0x1044F, "Deseret"),
    (0x10450, 0x1047F, "Shavian"),
    (0x10480, 0x104AF, "Osmanya"),
    (0x10800, 0x1083F, "Cypriot"),
    (0x10900, 0x1091F, "Phoenician"),
    (0x10A00, 0x10A5F, "Kharoshthi"),
    (0x10C00, 0x10C4F, "Old Turkic"),
    (0x11000, 0x1107F, "Brahmi"),
    (0x12000, 0x1254F, "Cuneiform"),
    (0x13000, 0x1345F, "Egyptian Hieroglyphs"),
    (0x16800, 0x16A3F, "Bamum"),
    (0x16F00, 0x16F9F, "Miao"),
    (0x17000, 0x18AFF, "Tangut"),
    (0x1B000, 0x1B16F, "Kana"),
    (0x1D000, 0x1D24F, "Common"),
    (0x1D400, 0x1D7FF, "Common"),
    (0x1E800, 0x1E8DF, "Mende Kikakui"),
    (0x1E900, 0x1E95F, "Adlam"),
    (0x1EE00, 0x1EEFF, "Arabic"),
    (0x1F000, 0x1FAFF, "Common"),
    (0x20000, 0x323AF, "Han"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_that_need_no_glyph() {
        for cp in [
            0x0009, 0x000A, 0x0020, 0x00A0, 0x0085, 0x00AD, 0x200B, 0x200C, 0x200D, 0x2060, 0xFE0F,
            0xFEFF, 0xE000, 0xF8FF, 0xF0000, 0x10FFFD, 0xE0001, 0xFFFE, 0x1FFFF, 0xFDD0, 0x2028,
            0x3000,
        ] {
            assert!(!needs_glyph(cp), "U+{cp:04X} needs no glyph");
        }
        for cp in [
            0x41, 0xE9, 0x0E01, 0x1200, 0x4E2D, 0x20AC, 0x1F600, 0x0300, 0xFFFD,
        ] {
            assert!(needs_glyph(cp), "U+{cp:04X} needs a glyph");
        }
    }

    #[test]
    fn scripts_by_block() {
        assert_eq!(script_name(0x41), "Latin");
        assert_eq!(script_name(0x0E01), "Thai");
        assert_eq!(script_name(0x1200), "Ethiopic");
        assert_eq!(script_name(0x4E2D), "Han");
        assert_eq!(script_name(0x3042), "Hiragana");
        assert_eq!(script_name(0xAC00), "Hangul");
        assert_eq!(script_name(0x0915), "Devanagari");
        assert_eq!(script_name(0x12000), "Cuneiform");
        assert_eq!(script_name(0x10450), "Shavian");
        assert_eq!(script_name(0x1F600), "Common");
        assert_eq!(script_name(0xFB2A), "Hebrew");
        assert_eq!(script_name(0xFB01), "Latin");
        assert_eq!(script_name(0x10E60), "Other");
    }

    #[test]
    fn tables_are_sorted_and_disjoint() {
        for table in [CONTROLS, WHITE_SPACE, DEFAULT_IGNORABLE, PRIVATE_USE] {
            for pair in table.windows(2) {
                assert!(pair[0].0 <= pair[0].1 && pair[0].1 < pair[1].0, "{pair:?}");
            }
        }
        for pair in SCRIPTS.windows(2) {
            assert!(
                pair[0].0 <= pair[0].1 && pair[0].1 < pair[1].0,
                "{:?} {:?}",
                pair[0],
                pair[1]
            );
        }
    }
}
