//! Which characters of a document's text need a glyph, and the script a
//! character belongs to, for `GLYPHS_MISSING`. The data is the Unicode
//! character database that `icu_properties` compiles in.

use icu_properties::props::{
    DefaultIgnorableCodePoint, GeneralCategory, NoncharacterCodePoint, Script, WhiteSpace,
};
use icu_properties::{CodePointMapData, CodePointSetData, PropertyNamesLong};

/// Whether a font must draw `cp`. Not for controls, white space,
/// default-ignorable characters (ZWJ, ZWNJ, variation selectors, U+FEFF
/// and the others of `Default_Ignorable_Code_Point`), private-use
/// characters (LibreOffice recodes Symbol and Wingdings text to
/// OpenSymbol), surrogates, noncharacters, or values past U+10FFFF.
pub fn needs_glyph(cp: u32) -> bool {
    if cp > 0x10_FFFF {
        return false;
    }
    let category = CodePointMapData::<GeneralCategory>::new().get32(cp);
    !(matches!(
        category,
        GeneralCategory::Control | GeneralCategory::PrivateUse | GeneralCategory::Surrogate
    ) || CodePointSetData::new::<WhiteSpace>().contains32(cp)
        || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains32(cp)
        || CodePointSetData::new::<NoncharacterCodePoint>().contains32(cp))
}

/// The long name of `cp`'s `Script` property: "Thai", "Han", "Ethiopic",
/// "Common" for shared punctuation and symbols, "Unknown" when unassigned.
pub fn script_name(cp: u32) -> &'static str {
    let script = CodePointMapData::<Script>::new().get32(cp);
    PropertyNamesLong::<Script>::new()
        .get(script)
        .unwrap_or("Unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_that_need_no_glyph() {
        for cp in [
            0x0009,
            0x000A,
            0x0020,
            0x00A0,
            0x0085,
            0x00AD,
            0x200B,
            0x200C,
            0x200D,
            0x2060,
            0xFE0F,
            0xFEFF,
            0xE000,
            0xF8FF,
            0xF0000,
            0x10FFFD,
            0xE0001,
            0xFFFE,
            0x1FFFF,
            0xFDD0,
            0x2028,
            0x3000,
            0xD800,
            0x11_0000,
            u32::MAX,
        ] {
            assert!(!needs_glyph(cp), "U+{cp:04X} needs no glyph");
        }
        for cp in [
            0x41, 0xE9, 0x0E01, 0x1200, 0x4E2D, 0x20AC, 0x1F600, 0x0300, 0xFFFD, 0x12000,
        ] {
            assert!(needs_glyph(cp), "U+{cp:04X} needs a glyph");
        }
    }

    #[test]
    fn scripts_from_the_script_property() {
        assert_eq!(script_name(0x41), "Latin");
        assert_eq!(script_name(0x0E01), "Thai");
        assert_eq!(script_name(0x1200), "Ethiopic");
        assert_eq!(script_name(0x4E2D), "Han");
        assert_eq!(script_name(0x3042), "Hiragana");
        assert_eq!(script_name(0xAC00), "Hangul");
        assert_eq!(script_name(0x0915), "Devanagari");
        assert_eq!(script_name(0x12000), "Cuneiform");
        // By character, not by block: Hebrew presentation forms.
        assert_eq!(script_name(0xFB2A), "Hebrew");
        assert_eq!(script_name(0x0300), "Inherited");
        assert_eq!(script_name(0x1F600), "Common");
        assert_eq!(script_name(0x0378), "Unknown");
    }
}
