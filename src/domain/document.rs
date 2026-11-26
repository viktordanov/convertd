//! Input formats and families. `FORMATS` is the one source for detection,
//! capabilities, the `Convert-Source-Format` header and the
//! `source.<ext>` rename.

/// The broad kind of input, which decides how it is routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Document,
    Presentation,
    Spreadsheet,
    Pdf,
    Image,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Document => "document",
            Family::Presentation => "presentation",
            Family::Spreadsheet => "spreadsheet",
            Family::Pdf => "pdf",
            Family::Image => "image",
        }
    }

    /// Office families go through the office engine.
    pub fn is_office(self) -> bool {
        matches!(
            self,
            Family::Document | Family::Presentation | Family::Spreadsheet
        )
    }
}

/// Which engine turns a format into a PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// LibreOffice.
    LibreOffice,
    /// The file already is a PDF.
    Pdfium,
    /// An image wrapped into a PDF.
    Image,
}

impl EngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineKind::LibreOffice => "libreoffice",
            EngineKind::Pdfium => "pdfium",
            EngineKind::Image => "image",
        }
    }
}

/// The document type LibreOffice reports after loading. For encrypted
/// OOXML it is the only source of the family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocType {
    Text,
    Spreadsheet,
    Presentation,
    Drawing,
    Other,
}

impl DocType {
    /// The family this document type belongs to, if any.
    pub fn family(self) -> Option<Family> {
        match self {
            DocType::Text => Some(Family::Document),
            DocType::Spreadsheet => Some(Family::Spreadsheet),
            DocType::Presentation => Some(Family::Presentation),
            DocType::Drawing | DocType::Other => None,
        }
    }
}

/// Every accepted input format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Docx,
    Docm,
    Dotx,
    Doc,
    Rtf,
    Odt,
    Pptx,
    Pptm,
    Ppsx,
    Potx,
    Ppt,
    Pps,
    Odp,
    Xlsx,
    Xlsm,
    Xls,
    Ods,
    Pdf,
    Png,
    Jpeg,
    Tiff,
    Gif,
    Bmp,
    Webp,
}

/// One row of [`FORMATS`].
#[derive(Debug)]
pub struct FormatSpec {
    pub format: Format,
    /// The short identifier in `Convert-Source-Format` and the manifest.
    pub id: &'static str,
    pub family: Family,
    /// The first one is canonical: it names `source.<ext>`.
    pub extensions: &'static [&'static str],
    pub media_types: &'static [&'static str],
    pub engine: EngineKind,
}

macro_rules! row {
    ($format:ident, $id:literal, $family:ident, [$($ext:literal),+], [$($media:literal),+], $engine:ident) => {
        FormatSpec {
            format: Format::$format,
            id: $id,
            family: Family::$family,
            extensions: &[$($ext),+],
            media_types: &[$($media),+],
            engine: EngineKind::$engine,
        }
    };
}

/// The v1 input formats (`PLAN.md`, accepted inputs), in capabilities order.
pub static FORMATS: &[FormatSpec] = &[
    row!(
        Docx,
        "docx",
        Document,
        ["docx"],
        ["application/vnd.openxmlformats-officedocument.wordprocessingml.document"],
        LibreOffice
    ),
    row!(
        Docm,
        "docm",
        Document,
        ["docm"],
        ["application/vnd.ms-word.document.macroEnabled.12"],
        LibreOffice
    ),
    row!(
        Dotx,
        "dotx",
        Document,
        ["dotx"],
        ["application/vnd.openxmlformats-officedocument.wordprocessingml.template"],
        LibreOffice
    ),
    row!(
        Doc,
        "doc",
        Document,
        ["doc"],
        ["application/msword"],
        LibreOffice
    ),
    row!(
        Rtf,
        "rtf",
        Document,
        ["rtf"],
        ["application/rtf", "text/rtf"],
        LibreOffice
    ),
    row!(
        Odt,
        "odt",
        Document,
        ["odt"],
        ["application/vnd.oasis.opendocument.text"],
        LibreOffice
    ),
    row!(
        Pptx,
        "pptx",
        Presentation,
        ["pptx"],
        ["application/vnd.openxmlformats-officedocument.presentationml.presentation"],
        LibreOffice
    ),
    row!(
        Pptm,
        "pptm",
        Presentation,
        ["pptm"],
        ["application/vnd.ms-powerpoint.presentation.macroEnabled.12"],
        LibreOffice
    ),
    row!(
        Ppsx,
        "ppsx",
        Presentation,
        ["ppsx"],
        ["application/vnd.openxmlformats-officedocument.presentationml.slideshow"],
        LibreOffice
    ),
    row!(
        Potx,
        "potx",
        Presentation,
        ["potx"],
        ["application/vnd.openxmlformats-officedocument.presentationml.template"],
        LibreOffice
    ),
    row!(
        Ppt,
        "ppt",
        Presentation,
        ["ppt"],
        ["application/vnd.ms-powerpoint"],
        LibreOffice
    ),
    row!(
        Pps,
        "pps",
        Presentation,
        ["pps"],
        ["application/vnd.ms-powerpoint"],
        LibreOffice
    ),
    row!(
        Odp,
        "odp",
        Presentation,
        ["odp"],
        ["application/vnd.oasis.opendocument.presentation"],
        LibreOffice
    ),
    row!(
        Xlsx,
        "xlsx",
        Spreadsheet,
        ["xlsx"],
        ["application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"],
        LibreOffice
    ),
    row!(
        Xlsm,
        "xlsm",
        Spreadsheet,
        ["xlsm"],
        ["application/vnd.ms-excel.sheet.macroEnabled.12"],
        LibreOffice
    ),
    row!(
        Xls,
        "xls",
        Spreadsheet,
        ["xls"],
        ["application/vnd.ms-excel"],
        LibreOffice
    ),
    row!(
        Ods,
        "ods",
        Spreadsheet,
        ["ods"],
        ["application/vnd.oasis.opendocument.spreadsheet"],
        LibreOffice
    ),
    row!(Pdf, "pdf", Pdf, ["pdf"], ["application/pdf"], Pdfium),
    row!(Png, "png", Image, ["png"], ["image/png"], Image),
    row!(Jpeg, "jpeg", Image, ["jpg", "jpeg"], ["image/jpeg"], Image),
    row!(Tiff, "tiff", Image, ["tiff", "tif"], ["image/tiff"], Image),
    row!(Gif, "gif", Image, ["gif"], ["image/gif"], Image),
    row!(Bmp, "bmp", Image, ["bmp"], ["image/bmp"], Image),
    row!(Webp, "webp", Image, ["webp"], ["image/webp"], Image),
];

impl Format {
    /// This format's row in [`FORMATS`].
    pub fn spec(self) -> &'static FormatSpec {
        // FORMATS has one row per variant, in declaration order (tested).
        &FORMATS[self as usize]
    }

    pub fn id(self) -> &'static str {
        self.spec().id
    }

    pub fn family(self) -> Family {
        self.spec().family
    }

    /// The canonical extension, without the dot.
    pub fn extension(self) -> &'static str {
        self.spec().extensions[0]
    }

    /// Whether v1 converts this format. Image inputs are out of v1 (owner,
    /// 2026-11-26): detection still names them, so that a PNG is never
    /// taken for the PDF it may contain, and then refuses them with 415.
    /// Their rows stay so that detection can name them.
    pub fn accepted(self) -> bool {
        self.family() != Family::Image
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_follow_the_enum_order() {
        for (index, row) in FORMATS.iter().enumerate() {
            assert_eq!(row.format as usize, index, "{}", row.id);
            assert_eq!(row.format.spec().id, row.id);
            assert!(!row.extensions.is_empty() && !row.media_types.is_empty());
        }
        assert_eq!(FORMATS.len(), Format::Webp as usize + 1);
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<_> = FORMATS.iter().map(|row| row.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), FORMATS.len());
    }

    #[test]
    fn canonical_extension_and_family() {
        assert_eq!(Format::Jpeg.extension(), "jpg");
        assert_eq!(Format::Pptm.family(), Family::Presentation);
        assert!(Format::Xlsm.family().is_office());
        assert!(!Format::Pdf.family().is_office());
        assert_eq!(DocType::Text.family(), Some(Family::Document));
        assert_eq!(DocType::Drawing.family(), None);
    }
}
