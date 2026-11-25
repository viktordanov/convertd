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
            bytes: warmup_docx(),
        },
        WarmupFile {
            name: "warmup.xlsx",
            family: Family::Spreadsheet,
            bytes: warmup_xlsx(),
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
    let theme = theme("Calibri Light", "Calibri");
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

/// A DrawingML theme with `major` (headings) and `minor` (body) Latin fonts.
fn theme(major: &str, minor: &str) -> String {
    const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    format!(
        r#"{XML}<a:theme xmlns:a="{A}" name="Warm-up"><a:themeElements><a:clrScheme name="Warm-up"><a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="1F497D"/></a:dk2><a:lt2><a:srgbClr val="EEECE1"/></a:lt2><a:accent1><a:srgbClr val="4F81BD"/></a:accent1><a:accent2><a:srgbClr val="C0504D"/></a:accent2><a:accent3><a:srgbClr val="9BBB59"/></a:accent3><a:accent4><a:srgbClr val="8064A2"/></a:accent4><a:accent5><a:srgbClr val="4BACC6"/></a:accent5><a:accent6><a:srgbClr val="F79646"/></a:accent6><a:hlink><a:srgbClr val="0000FF"/></a:hlink><a:folHlink><a:srgbClr val="800080"/></a:folHlink></a:clrScheme><a:fontScheme name="Warm-up"><a:majorFont><a:latin typeface="{major}"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="{minor}"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Warm-up"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#
    )
}

/// The warm-up DOCX: shaped like a python-docx or Word document, so the
/// first real job finds its import paths, fonts and layout code loaded. It
/// has a theme with Cambria headings and Calibri body text (Caladea and
/// Carlito in the image), styles with a heading, a bullet list, bold,
/// italic, Arial and Times New Roman runs, and a bordered table.
pub fn warmup_docx() -> Vec<u8> {
    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    let rel = |name: &str| format!("{R}/{name}");
    let kind = |name: &str| format!("application/vnd.openxmlformats-officedocument.{name}+xml");
    let run = |text: &str, props: &str| {
        format!("<w:r><w:rPr>{props}</w:rPr><w:t xml:space=\"preserve\">{text}</w:t></w:r>")
    };
    let cell = |text: &str| {
        format!(
            r#"<w:tc><w:tcPr><w:tcW w:w="4500" w:type="dxa"/></w:tcPr><w:p>{}</w:p></w:tc>"#,
            run(text, "")
        )
    };
    let body = format!(
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>{h}</w:p><w:p>{a}{b}{i}{arial}{times}</w:p><w:p><w:pPr><w:pStyle w:val="ListParagraph"/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr>{item}</w:p><w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="9000" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="4500"/><w:gridCol w:w="4500"/></w:tblGrid><w:tr>{c1}{c2}</w:tr><w:tr>{c3}{c4}</w:tr></w:tbl><w:p>{end}</w:p>"#,
        h = run("Warm-up heading", ""),
        a = run("Body text in the theme font. ", ""),
        b = run("Bold. ", "<w:b/>"),
        i = run("Italic. ", "<w:i/>"),
        arial = run("Arial. ", r#"<w:rFonts w:ascii="Arial" w:hAnsi="Arial"/>"#),
        times = run(
            "Times New Roman.",
            r#"<w:rFonts w:ascii="Times New Roman" w:hAnsi="Times New Roman"/>"#
        ),
        item = run("A list item.", ""),
        c1 = cell("A1"),
        c2 = cell("B1"),
        c3 = cell("A2"),
        c4 = cell("B2"),
        end = run("The end.", ""),
    );
    let border =
        |side: &str| format!(r#"<w:{side} w:val="single" w:sz="4" w:space="0" w:color="auto"/>"#);
    let borders: String = ["top", "left", "bottom", "right", "insideH", "insideV"]
        .iter()
        .map(|s| border(s))
        .collect();
    let styles = format!(
        r#"{XML}<w:styles xmlns:w="{W}"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:asciiTheme="minorHAnsi" w:hAnsiTheme="minorHAnsi" w:eastAsiaTheme="minorEastAsia" w:cstheme="minorBidi"/><w:sz w:val="22"/><w:szCs w:val="22"/><w:lang w:val="en-US"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="200" w:line="276" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="480" w:after="0"/><w:outlineLvl w:val="0"/></w:pPr><w:rPr><w:rFonts w:asciiTheme="majorHAnsi" w:hAnsiTheme="majorHAnsi"/><w:b/><w:color w:val="365F91"/><w:sz w:val="28"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="ListParagraph"><w:name w:val="List Paragraph"/><w:basedOn w:val="Normal"/><w:pPr><w:ind w:left="720"/></w:pPr></w:style><w:style w:type="table" w:default="1" w:styleId="TableNormal"><w:name w:val="Normal Table"/><w:tblPr><w:tblCellMar><w:left w:w="108" w:type="dxa"/><w:right w:w="108" w:type="dxa"/></w:tblCellMar></w:tblPr></w:style><w:style w:type="table" w:styleId="TableGrid"><w:name w:val="Table Grid"/><w:basedOn w:val="TableNormal"/><w:tblPr><w:tblBorders>{borders}</w:tblBorders></w:tblPr></w:style></w:styles>"#
    );
    let numbering = format!(
        r#"{XML}<w:numbering xmlns:w="{W}"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
    );
    let font = |name: &str| {
        format!(
            r#"<w:font w:name="{name}"><w:family w:val="auto"/><w:pitch w:val="variable"/></w:font>"#
        )
    };
    let fonts: String = ["Calibri", "Cambria", "Arial", "Times New Roman", "Symbol"]
        .iter()
        .map(|f| font(f))
        .collect();
    stored_zip(&[
        (
            "[Content_Types].xml",
            content_types(&[
                (
                    "/word/document.xml",
                    &kind("wordprocessingml.document.main"),
                ),
                ("/word/styles.xml", &kind("wordprocessingml.styles")),
                ("/word/numbering.xml", &kind("wordprocessingml.numbering")),
                ("/word/settings.xml", &kind("wordprocessingml.settings")),
                ("/word/fontTable.xml", &kind("wordprocessingml.fontTable")),
                (
                    "/word/theme/theme1.xml",
                    "application/vnd.openxmlformats-officedocument.theme+xml",
                ),
            ]),
        ),
        (
            "_rels/.rels",
            rels(&[("rId1", REL_DOC, "word/document.xml")]),
        ),
        (
            "word/_rels/document.xml.rels",
            rels(&[
                ("rId1", &rel("styles"), "styles.xml"),
                ("rId2", &rel("numbering"), "numbering.xml"),
                ("rId3", &rel("settings"), "settings.xml"),
                ("rId4", &rel("fontTable"), "fontTable.xml"),
                ("rId5", &rel("theme"), "theme/theme1.xml"),
            ]),
        ),
        (
            "word/document.xml",
            format!(
                r#"{XML}<w:document xmlns:w="{W}" xmlns:r="{R}"><w:body>{body}</w:body></w:document>"#
            ),
        ),
        ("word/styles.xml", styles),
        ("word/numbering.xml", numbering),
        (
            "word/settings.xml",
            format!(
                r#"{XML}<w:settings xmlns:w="{W}"><w:defaultTabStop w:val="720"/><w:compat><w:compatSetting w:name="compatibilityMode" w:uri="http://schemas.microsoft.com/office/word" w:val="15"/></w:compat></w:settings>"#
            ),
        ),
        (
            "word/fontTable.xml",
            format!(r#"{XML}<w:fonts xmlns:w="{W}">{fonts}</w:fonts>"#),
        ),
        ("word/theme/theme1.xml", theme("Cambria", "Calibri")),
    ])
}

/// The warm-up XLSX: a styled workbook with the theme, Calibri cells, a
/// bold header, numbers, a formula and two sheets, as openpyxl and Excel
/// write them.
pub fn warmup_xlsx() -> Vec<u8> {
    const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    let rel = |name: &str| format!("{R}/{name}");
    let kind = |name: &str| format!("application/vnd.openxmlformats-officedocument.{name}+xml");
    let sheet = |name: &str| {
        format!(
            r#"{XML}<worksheet xmlns="{S}"><sheetData><row r="1"><c r="A1" t="inlineStr" s="1"><is><t>{name}</t></is></c><c r="B1" t="inlineStr" s="1"><is><t>Value</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>One</t></is></c><c r="B2"><v>1.5</v></c></row><row r="3"><c r="A3" t="inlineStr"><is><t>Two</t></is></c><c r="B3"><f>B2*2</f><v>3</v></c></row></sheetData></worksheet>"#
        )
    };
    stored_zip(&[
        (
            "[Content_Types].xml",
            content_types(&[
                ("/xl/workbook.xml", &kind("spreadsheetml.sheet.main")),
                (
                    "/xl/worksheets/sheet1.xml",
                    &kind("spreadsheetml.worksheet"),
                ),
                (
                    "/xl/worksheets/sheet2.xml",
                    &kind("spreadsheetml.worksheet"),
                ),
                ("/xl/styles.xml", &kind("spreadsheetml.styles")),
                (
                    "/xl/theme/theme1.xml",
                    "application/vnd.openxmlformats-officedocument.theme+xml",
                ),
            ]),
        ),
        ("_rels/.rels", rels(&[("rId1", REL_DOC, "xl/workbook.xml")])),
        (
            "xl/workbook.xml",
            format!(
                r#"{XML}<workbook xmlns="{S}" xmlns:r="{R}"><sheets><sheet name="First" sheetId="1" r:id="rId1"/><sheet name="Second" sheetId="2" r:id="rId2"/></sheets></workbook>"#
            ),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            rels(&[
                ("rId1", &rel("worksheet"), "worksheets/sheet1.xml"),
                ("rId2", &rel("worksheet"), "worksheets/sheet2.xml"),
                ("rId3", &rel("styles"), "styles.xml"),
                ("rId4", &rel("theme"), "theme/theme1.xml"),
            ]),
        ),
        ("xl/worksheets/sheet1.xml", sheet("First")),
        ("xl/worksheets/sheet2.xml", sheet("Second")),
        (
            "xl/styles.xml",
            format!(
                r#"{XML}<styleSheet xmlns="{S}"><fonts count="2"><font><sz val="11"/><name val="Calibri"/><family val="2"/><scheme val="minor"/></font><font><b/><sz val="11"/><name val="Calibri"/><family val="2"/><scheme val="minor"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
            ),
        ),
        ("xl/theme/theme1.xml", theme("Cambria", "Calibri")),
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
    fn every_part_of_the_warmup_files_is_well_formed_xml() {
        use std::io::Read;

        for file in files() {
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&file.bytes))
                .unwrap_or_else(|err| panic!("{}: {err}", file.name));
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).unwrap();
                let name = format!("{:?}", entry.name());
                let mut xml = String::new();
                entry.read_to_string(&mut xml).unwrap();
                let mut reader = quick_xml::Reader::from_str(&xml);
                let mut depth = 0i32;
                loop {
                    match reader.read_event() {
                        Ok(quick_xml::events::Event::Start(_)) => depth += 1,
                        Ok(quick_xml::events::Event::End(_)) => depth -= 1,
                        Ok(quick_xml::events::Event::Eof) => break,
                        Ok(_) => {}
                        Err(err) => panic!("{} {name}: {err}", file.name),
                    }
                }
                assert_eq!(depth, 0, "{} {name}: unbalanced", file.name);
            }
        }
    }

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
