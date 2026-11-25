//! The office warm-up: three tiny documents, one per family, that the
//! office worker converts before `Ready`, so the first real job does not pay
//! for loading the import filters and the PDF export.
//!
//! The files are built in memory as stored (uncompressed) ZIP archives, so
//! no binary fixture is committed. Each holds only the parts LibreOffice
//! needs to open it.

use crate::ipc::Family;

/// One warm-up document.
pub struct WarmupFile {
    pub name: &'static str,
    pub family: Family,
    pub bytes: Vec<u8>,
}

/// The warm-up documents: a DOCX, an XLSX and a PPTX.
pub fn files() -> Vec<WarmupFile> {
    vec![
        WarmupFile {
            name: "warmup.docx",
            family: Family::Document,
            bytes: docx(TEXT),
        },
        WarmupFile {
            name: "warmup.xlsx",
            family: Family::Spreadsheet,
            bytes: xlsx(TEXT),
        },
        WarmupFile {
            name: "warmup.pptx",
            family: Family::Presentation,
            bytes: pptx(TEXT),
        },
    ]
}

/// The text of every warm-up document.
const TEXT: &str = "Warm-up.";

const XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
const RELS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const REL_DOC: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";

fn content_types(overrides: &[(&str, &str)]) -> String {
    let overrides: String = overrides
        .iter()
        .map(|(part, kind)| format!(r#"<Override PartName="{part}" ContentType="{kind}"/>"#))
        .collect();
    format!(
        r#"{XML}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>{overrides}</Types>"#
    )
}

fn rels(entries: &[(&str, &str, &str)]) -> String {
    let entries: String = entries
        .iter()
        .map(|(id, kind, target)| {
            format!(r#"<Relationship Id="{id}" Type="{kind}" Target="{target}"/>"#)
        })
        .collect();
    format!(r#"{XML}<Relationships xmlns="{RELS_NS}">{entries}</Relationships>"#)
}

/// A one-paragraph DOCX holding `text` (XML-escaped by the caller).
pub fn docx(text: &str) -> Vec<u8> {
    stored_zip(&[
        (
            "[Content_Types].xml",
            content_types(&[(
                "/word/document.xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            )]),
        ),
        (
            "_rels/.rels",
            rels(&[("rId1", REL_DOC, "word/document.xml")]),
        ),
        (
            "word/document.xml",
            format!(
                r#"{XML}<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#
            ),
        ),
    ])
}

/// A one-sheet XLSX with `text` in A1 and 1 in B1.
pub fn xlsx(text: &str) -> Vec<u8> {
    stored_zip(&[
        (
            "[Content_Types].xml",
            content_types(&[
                (
                    "/xl/workbook.xml",
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
                ),
                (
                    "/xl/worksheets/sheet1.xml",
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml",
                ),
            ]),
        ),
        ("_rels/.rels", rels(&[("rId1", REL_DOC, "xl/workbook.xml")])),
        (
            "xl/workbook.xml",
            format!(
                r#"{XML}<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#
            ),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            rels(&[(
                "rId1",
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet",
                "worksheets/sheet1.xml",
            )]),
        ),
        (
            "xl/worksheets/sheet1.xml",
            format!(
                r#"{XML}<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>{text}</t></is></c><c r="B1"><v>1</v></c></row></sheetData></worksheet>"#
            ),
        ),
    ])
}

/// A one-slide PPTX with `text` in a text box.
pub fn pptx(text: &str) -> Vec<u8> {
    const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
    const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    let rel = |name: &str| format!("{R}/{name}");
    let shape_tree = r#"<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld>"#;
    let slide_tree = format!(
        r#"<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id="2" name="Text"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="914400" y="914400"/><a:ext cx="7315200" cy="914400"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>"#
    );
    let theme = format!(
        r#"{XML}<a:theme xmlns:a="{A}" name="Warm-up"><a:themeElements><a:clrScheme name="Warm-up"><a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="1F497D"/></a:dk2><a:lt2><a:srgbClr val="EEECE1"/></a:lt2><a:accent1><a:srgbClr val="4F81BD"/></a:accent1><a:accent2><a:srgbClr val="C0504D"/></a:accent2><a:accent3><a:srgbClr val="9BBB59"/></a:accent3><a:accent4><a:srgbClr val="8064A2"/></a:accent4><a:accent5><a:srgbClr val="4BACC6"/></a:accent5><a:accent6><a:srgbClr val="F79646"/></a:accent6><a:hlink><a:srgbClr val="0000FF"/></a:hlink><a:folHlink><a:srgbClr val="800080"/></a:folHlink></a:clrScheme><a:fontScheme name="Warm-up"><a:majorFont><a:latin typeface="DejaVu Sans"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="DejaVu Sans"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Warm-up"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#
    );
    let kind = |name: &str| {
        format!("application/vnd.openxmlformats-officedocument.presentationml.{name}+xml")
    };
    stored_zip(&[
        (
            "[Content_Types].xml",
            content_types(&[
                ("/ppt/presentation.xml", &kind("presentation.main")),
                ("/ppt/slides/slide1.xml", &kind("slide")),
                ("/ppt/slideLayouts/slideLayout1.xml", &kind("slideLayout")),
                ("/ppt/slideMasters/slideMaster1.xml", &kind("slideMaster")),
                (
                    "/ppt/theme/theme1.xml",
                    "application/vnd.openxmlformats-officedocument.theme+xml",
                ),
            ]),
        ),
        (
            "_rels/.rels",
            rels(&[("rId1", REL_DOC, "ppt/presentation.xml")]),
        ),
        (
            "ppt/presentation.xml",
            format!(
                r#"{XML}<p:presentation xmlns:a="{A}" xmlns:r="{R}" xmlns:p="{P}"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#
            ),
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            rels(&[
                ("rId1", &rel("slideMaster"), "slideMasters/slideMaster1.xml"),
                ("rId2", &rel("slide"), "slides/slide1.xml"),
                ("rId3", &rel("theme"), "theme/theme1.xml"),
            ]),
        ),
        (
            "ppt/slideMasters/slideMaster1.xml",
            format!(
                r#"{XML}<p:sldMaster xmlns:a="{A}" xmlns:r="{R}" xmlns:p="{P}">{shape_tree}<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>"#
            ),
        ),
        (
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            rels(&[
                (
                    "rId1",
                    &rel("slideLayout"),
                    "../slideLayouts/slideLayout1.xml",
                ),
                ("rId2", &rel("theme"), "../theme/theme1.xml"),
            ]),
        ),
        (
            "ppt/slideLayouts/slideLayout1.xml",
            format!(
                r#"{XML}<p:sldLayout xmlns:a="{A}" xmlns:r="{R}" xmlns:p="{P}" type="blank">{shape_tree}<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#
            ),
        ),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            rels(&[(
                "rId1",
                &rel("slideMaster"),
                "../slideMasters/slideMaster1.xml",
            )]),
        ),
        (
            "ppt/slides/slide1.xml",
            format!(
                r#"{XML}<p:sld xmlns:a="{A}" xmlns:r="{R}" xmlns:p="{P}">{slide_tree}<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#
            ),
        ),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            rels(&[(
                "rId1",
                &rel("slideLayout"),
                "../slideLayouts/slideLayout1.xml",
            )]),
        ),
        ("ppt/theme/theme1.xml", theme),
    ])
}

/// A ZIP archive of `entries`, every one stored without compression.
pub fn stored_zip(entries: &[(&str, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let data = data.as_bytes();
        let offset = out.len() as u32;
        let crc = crc32fast::hash(data);
        let size = (data.len() as u32).to_le_bytes();
        let name_len = (name.len() as u16).to_le_bytes();
        // Local file header: version 2.0, no flags, stored, no time.
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size);
        out.extend_from_slice(&size);
        out.extend_from_slice(&name_len);
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        // Central directory header.
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size);
        central.extend_from_slice(&size);
        central.extend_from_slice(&name_len);
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let count = (entries.len() as u16).to_le_bytes();
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&count);
    out.extend_from_slice(&count);
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_reads_each_warmup_file_as_its_family() {
        use crate::domain::document::Family as DomainFamily;

        for file in files() {
            let size = file.bytes.len() as u64;
            let detection =
                crate::detect::detect_file(&mut std::io::Cursor::new(&file.bytes), size)
                    .unwrap_or_else(|err| panic!("{}: {err:?}", file.name));
            let expected = match file.family {
                Family::Document => DomainFamily::Document,
                Family::Spreadsheet => DomainFamily::Spreadsheet,
                Family::Presentation => DomainFamily::Presentation,
            };
            assert_eq!(detection.format.family(), expected, "{}", file.name);
        }
    }
}
