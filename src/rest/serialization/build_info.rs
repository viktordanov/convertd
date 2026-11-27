//! `build-info.json`, written by the image build (INTERFACES.md §8).

use std::collections::BTreeMap;

use serde::Deserialize;

/// What the image provides about its engines and fonts. Every field is
/// optional, so a partial file still loads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BuildInfo {
    #[serde(default)]
    pub libreoffice: Option<String>,
    #[serde(default)]
    pub pdfium: Option<String>,
    #[serde(default)]
    pub fonts: Vec<String>,
    #[serde(default)]
    pub font_substitutes: BTreeMap<String, String>,
    #[serde(default)]
    pub default_substitute: Option<String>,
    /// The code point ranges that the installed faces cover, as
    /// `[[start, end], ...]` (inclusive), from `fc-list :charset` at build
    /// time. Empty when absent, as outside the image.
    #[serde(default)]
    pub coverage: Vec<[u32; 2]>,
}

impl BuildInfo {
    pub fn parse(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// `fonts.json` (lane 003): `{family: metric_compatible}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct FontsFile(pub BTreeMap<String, bool>);

impl FontsFile {
    pub fn parse(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::BuildInfo;

    #[test]
    fn parses_the_interfaces_example() {
        let info = BuildInfo::parse(
            br#"{ "libreoffice": "26.8.3.1", "pdfium": "8179",
                  "fonts": ["Caladea", "Carlito"],
                  "font_substitutes": { "Calibri": "Carlito" },
                  "default_substitute": "DejaVu Sans" }"#,
        )
        .unwrap();
        assert_eq!(info.libreoffice.as_deref(), Some("26.8.3.1"));
        assert_eq!(info.fonts, ["Caladea", "Carlito"]);
        assert_eq!(info.font_substitutes["Calibri"], "Carlito");
        assert!(BuildInfo::parse(b"{}").unwrap().fonts.is_empty());
    }

    #[test]
    fn parses_docker_fonts_json() {
        let fonts = super::FontsFile::parse(br#"{"Calibri": true, "Aptos": false}"#).unwrap();
        assert!(fonts.0["Calibri"]);
        assert!(super::FontsFile::parse(b"[]").is_err());
    }
}
