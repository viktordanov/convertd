//! Pre-flight on fixtures built here, after the corpus cases of
//! `corpus/expected.json` (docx-small, docx-headings, pptx-hidden,
//! pptx-pages-mismatch, potx, xlsx-sheet3, odt, odt-basic-canary,
//! docm-novba, docx-encrypted) and on broken inputs, which must never
//! panic.

use std::io::{Cursor, Write};
use std::path::PathBuf;

use super::package::Package;
use super::*;
use crate::domain::verification::DeclaredPages;

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
const A: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main""#;
const P: &str = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut out));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zip.start_file(*name, stored).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }
    out
}

fn rels(entries: &[(&str, &str, &str)]) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    for (id, kind, target) in entries {
        out.push_str(&format!(
            r#"<Relationship Id="{id}" Type="{REL}/{kind}" Target="{target}"/>"#
        ));
    }
    out.push_str("</Relationships>");
    out
}

fn types(extra: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/>{extra}</Types>"#
    )
}

fn theme(major: &str, minor: &str) -> String {
    format!(
        r#"<a:theme {A}><a:themeElements><a:fontScheme name="x"><a:majorFont><a:latin typeface="{major}"/><a:ea typeface=""/></a:majorFont><a:minorFont><a:latin typeface="{minor}"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#
    )
}

/// Writes `bytes` to a scratch file in a directory of its own, scans it,
/// and removes the directory. One directory per call, so tests that run in
/// parallel in one process never remove each other's files.
fn scan_bytes(name: &str, bytes: &[u8], family: Family) -> SourceFacts {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("convertd-preflight-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    let facts = scan(&path, family);
    std::fs::remove_dir_all(&dir).unwrap();
    facts
}

fn docx(extra_entries: &[(&str, &[u8])], document: &str) -> Vec<u8> {
    let content_types = types("");
    let root = rels(&[("rId1", "officeDocument", "word/document.xml")]);
    let doc_rels = rels(&[
        ("rId1", "styles", "styles.xml"),
        ("rId2", "theme", "theme/theme1.xml"),
        ("rId3", "header", "header1.xml"),
    ]);
    let styles = format!(
        r#"<w:styles {W}><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:asciiTheme="minorHAnsi" w:hAnsiTheme="minorHAnsi" w:eastAsiaTheme="minorEastAsia"/></w:rPr></w:rPrDefault></w:docDefaults><w:latentStyles/><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:basedOn w:val="Normal"/><w:rPr><w:rFonts w:asciiTheme="majorHAnsi" w:hAnsiTheme="majorHAnsi"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:basedOn w:val="Heading1"/><w:rPr><w:rFonts w:ascii="Calibri Light" w:hAnsi="Calibri Light"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Unused"><w:rPr><w:rFonts w:ascii="Zzyzx" w:hAnsi="Zzyzx"/></w:rPr></w:style></w:styles>"#
    );
    let theme = theme("Aptos Display", "Cambria");
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", root.as_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes()),
        ("word/document.xml", document.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
        ("word/theme/theme1.xml", theme.as_bytes()),
    ];
    entries.extend_from_slice(extra_entries);
    zip(&entries)
}

fn body(inner: &str) -> String {
    format!(r#"<w:document {W}><w:body>{inner}</w:body></w:document>"#)
}

#[test]
fn docx_defaults_and_the_normal_style() {
    // docx-small: Normal names Calibri, docDefaults use the minor theme font.
    let bytes = docx(&[], &body("<w:p><w:r><w:t>hi</w:t></w:r></w:p>"));
    let facts = scan_bytes("small.docx", &bytes, Family::Document);
    assert_eq!(facts.requested_fonts, ["Calibri", "Cambria"]);
    assert_eq!(facts.declared_pages, None, "V1 is off for DOCX");
    assert_eq!(facts.macros, None);
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
}

#[test]
fn docx_referenced_styles_their_chain_runs_and_headers() {
    // docx-headings: Heading 1 takes the major theme font, Heading 2 a
    // literal Calibri Light; an unreferenced style is not read.
    let document = body(
        r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr></w:p><w:p><w:r><w:rPr><w:rFonts w:ascii="Georgia" w:hAnsi="Georgia"/></w:rPr></w:r></w:p>"#,
    );
    let header =
        format!(r#"<w:hdr {W}><w:r><w:rPr><w:rFonts w:hAnsi="Verdana"/></w:rPr></w:r></w:hdr>"#);
    let bytes = docx(&[("word/header1.xml", header.as_bytes())], &document);
    let facts = scan_bytes("headings.docx", &bytes, Family::Document);
    assert_eq!(
        facts.requested_fonts,
        [
            "Aptos Display",
            "Calibri",
            "Calibri Light",
            "Cambria",
            "Georgia",
            "Verdana"
        ]
    );
}

#[test]
fn docm_macros_only_with_a_vba_project() {
    let document = body("<w:p/>");
    let facts = scan_bytes("novba.docm", &docx(&[], &document), Family::Document);
    assert_eq!(facts.macros, None, "docm-novba");
    let bytes = docx(&[("word/vbaProject.bin", b"\x00vba")], &document);
    let facts = scan_bytes("vba.docm", &bytes, Family::Document);
    assert_eq!(
        facts.macros,
        Some(MacroFact {
            kind: MacroKind::Vba,
            part: "word/vbaProject.bin".into()
        })
    );
}

/// A deck: one entry per slide, (root tag with attributes, inner XML).
fn pptx(slides: &[(&str, &str)], with_list: bool) -> Vec<u8> {
    let content_types = types("");
    let root = rels(&[("rId1", "officeDocument", "ppt/presentation.xml")]);
    let mut pres_rels: Vec<(String, String, String)> = vec![(
        "rIdM".into(),
        "slideMaster".into(),
        "slideMasters/slideMaster1.xml".into(),
    )];
    let mut list = String::new();
    for i in 0..slides.len() {
        pres_rels.push((
            format!("rId{}", i + 2),
            "slide".into(),
            format!("slides/slide{}.xml", i + 1),
        ));
        list.push_str(&format!(
            r#"<p:sldId id="{}" r:id="rId{}"/>"#,
            256 + i,
            i + 2
        ));
    }
    let presentation = if with_list {
        format!(r#"<p:presentation {P}><p:sldIdLst>{list}</p:sldIdLst></p:presentation>"#)
    } else {
        format!(r#"<p:presentation {P}/>"#)
    };
    let pres_rels: Vec<(&str, &str, &str)> = pres_rels
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
        .collect();
    let pres_rels = rels(&pres_rels);
    let slide_rels = rels(&[("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")]);
    let layout_rels = rels(&[("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")]);
    let master_rels = rels(&[("rId1", "theme", "../theme/theme1.xml")]);
    let layout = format!(r#"<p:sldLayout {P} {A}><a:latin typeface="+mj-lt"/></p:sldLayout>"#);
    let unused =
        format!(r#"<p:sldLayout {P} {A}><a:latin typeface="Zzyzx Grotesk"/></p:sldLayout>"#);
    let master = format!(
        r#"<p:sldMaster {P} {A}><p:txStyles><a:latin typeface="+mn-lt"/></p:txStyles></p:sldMaster>"#
    );
    let theme = theme("Aptos", "Calibri");
    let mut owned: Vec<(String, Vec<u8>)> = vec![
        ("[Content_Types].xml".into(), content_types.into_bytes()),
        ("_rels/.rels".into(), root.into_bytes()),
        ("ppt/presentation.xml".into(), presentation.into_bytes()),
        (
            "ppt/_rels/presentation.xml.rels".into(),
            pres_rels.into_bytes(),
        ),
        (
            "ppt/slideLayouts/slideLayout1.xml".into(),
            layout.into_bytes(),
        ),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(),
            layout_rels.into_bytes(),
        ),
        (
            "ppt/slideLayouts/slideLayout11.xml".into(),
            unused.into_bytes(),
        ),
        (
            "ppt/slideMasters/slideMaster1.xml".into(),
            master.into_bytes(),
        ),
        (
            "ppt/slideMasters/_rels/slideMaster1.xml.rels".into(),
            master_rels.into_bytes(),
        ),
        ("ppt/theme/theme1.xml".into(), theme.into_bytes()),
    ];
    for (i, (root_tag, inner)) in slides.iter().enumerate() {
        let name = root_tag.split(' ').next().unwrap_or_default();
        owned.push((
            format!("ppt/slides/slide{}.xml", i + 1),
            format!(r#"<{root_tag} {P} {A}>{inner}</{name}>"#).into_bytes(),
        ));
        owned.push((
            format!("ppt/slides/_rels/slide{}.xml.rels", i + 1),
            slide_rels.clone().into_bytes(),
        ));
    }
    let entries: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    zip(&entries)
}

#[test]
fn pptx_counts_visible_slides_and_follows_the_master_theme() {
    // pptx-hidden: 5 slides, 2 hidden; one slide names Georgia.
    let bytes = pptx(
        &[
            ("p:sld", r#"<a:latin typeface="Georgia"/>"#),
            ("p:sld show=\"0\"", ""),
            ("p:sld", ""),
            ("p:sld show=\"0\"", ""),
            ("p:sld", ""),
        ],
        true,
    );
    let facts = scan_bytes("hidden.pptx", &bytes, Family::Presentation);
    assert_eq!(
        facts.declared_pages,
        Some(DeclaredPages {
            count: 3,
            source: "ppt/presentation.xml"
        })
    );
    // +mj-lt (layout) and +mn-lt (master) resolve through the master's
    // theme; the unreached layout's Zzyzx Grotesk is not read.
    assert_eq!(facts.requested_fonts, ["Aptos", "Calibri", "Georgia"]);
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
}

#[test]
fn pptx_counts_a_listed_part_that_is_not_a_slide() {
    // pptx-pages-mismatch: the 4th part's root is p:notASlide; it counts.
    let slides = [
        ("p:sld", ""),
        ("p:sld", ""),
        ("p:sld", ""),
        ("p:notASlide", ""),
    ];
    let facts = scan_bytes("mismatch.pptx", &pptx(&slides, true), Family::Presentation);
    assert_eq!(facts.declared_pages.map(|d| d.count), Some(4));
}

#[test]
fn potx_without_a_slide_list() {
    let facts = scan_bytes("empty.potx", &pptx(&[], false), Family::Presentation);
    assert_eq!(facts.declared_pages.map(|d| d.count), Some(0));
    assert!(
        facts.requested_fonts.is_empty(),
        "no slide reaches a layout"
    );
}

#[test]
fn pptx_with_a_missing_slide_part_skips_v1() {
    let mut bytes = pptx(&[("p:sld", ""), ("p:sld", "")], true);
    // Rename the slide2.xml entry (local header and central directory) so
    // that its relationship target is missing.
    let needle = b"ppt/slides/slide2.xml";
    let hits: Vec<usize> = bytes
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| w == needle)
        .map(|(at, _)| at)
        .collect();
    assert_eq!(hits.len(), 2);
    for at in hits {
        bytes[at + 11..at + 16].copy_from_slice(b"slidX");
    }
    let facts = scan_bytes("missing.pptx", &bytes, Family::Presentation);
    assert_eq!(facts.declared_pages, None);
    assert!(
        facts
            .skipped
            .contains(&(Check::PageCount, SkipReason::MissingPart)),
        "{:?}",
        facts.skipped
    );
}

#[test]
fn xlsx_fonts_map_the_scheme_to_the_theme() {
    let content_types = types(
        r#"<Override PartName="/xl/macrosheets/sheet1.xml" ContentType="application/vnd.ms-excel.macrosheet+xml"/>"#,
    );
    let root = rels(&[("rId1", "officeDocument", "xl/workbook.xml")]);
    let wb_rels = rels(&[
        ("rId1", "styles", "styles.xml"),
        ("rId2", "theme", "theme/theme1.xml"),
    ]);
    let styles = r#"<styleSheet><fonts count="3"><font><sz val="11"/><name val="Calibri"/><scheme val="minor"/></font><font><name val="Arial"/></font><font/></fonts><dxfs><dxf><font><name val="Tahoma"/></font></dxf></dxfs></styleSheet>"#;
    let theme = theme("Cambria", "Carlito");
    let bytes = zip(&[
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", root.as_bytes()),
        ("xl/workbook.xml", b"<workbook/>"),
        ("xl/_rels/workbook.xml.rels", wb_rels.as_bytes()),
        ("xl/styles.xml", styles.as_bytes()),
        ("xl/theme/theme1.xml", theme.as_bytes()),
    ]);
    let facts = scan_bytes("book.xlsm", &bytes, Family::Spreadsheet);
    // scheme=minor takes the theme's minor font, Carlito, not the name.
    assert_eq!(facts.requested_fonts, ["Arial", "Carlito", "Tahoma"]);
    assert_eq!(
        facts.macros,
        Some(MacroFact {
            kind: MacroKind::Xlm,
            part: "xl/macrosheets/sheet1.xml".into()
        })
    );
}

fn odt(extra: &[(&str, &[u8])]) -> Vec<u8> {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"><office:font-face-decls><style:font-face style:name="Liberation Serif" svg:font-family="'Liberation Serif'"/></office:font-face-decls></office:document-content>"#;
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("mimetype", b"application/vnd.oasis.opendocument.text"),
        ("content.xml", content.as_bytes()),
    ];
    entries.extend_from_slice(extra);
    zip(&entries)
}

#[test]
fn odf_fonts_and_basic_macros() {
    let index: (&str, &[u8]) = ("Basic/Standard/script-lb.xml", b"<x/>");
    let facts = scan_bytes("plain.odt", &odt(&[index]), Family::Document);
    assert_eq!(facts.requested_fonts, ["Liberation Serif"]);
    assert_eq!(
        facts.macros, None,
        "script-lb.xml is only the library index"
    );
    let module: (&str, &[u8]) = ("Basic/Standard/Module1.xml", b"<x/>");
    let facts = scan_bytes("basic.odt", &odt(&[index, module]), Family::Document);
    assert_eq!(
        facts.macros,
        Some(MacroFact {
            kind: MacroKind::Basic,
            part: "Basic/Standard/Module1.xml".into()
        })
    );
    let script: (&str, &[u8]) = ("Scripts/python/x.py", b"x");
    let facts = scan_bytes("script.odt", &odt(&[script]), Family::Document);
    assert_eq!(facts.macros.map(|m| m.kind), Some(MacroKind::Script));
}

fn ole2(names: &[&str]) -> Vec<u8> {
    let mut compound = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    for name in names {
        match name.strip_suffix('/') {
            Some(storage) => compound.create_storage(format!("/{storage}")).unwrap(),
            None => compound
                .create_stream(format!("/{name}"))
                .unwrap()
                .write_all(b"x")
                .unwrap(),
        }
    }
    compound.flush().unwrap();
    compound.into_inner().into_inner()
}

#[test]
fn ole2_macro_storages() {
    let facts = scan_bytes(
        "m.doc",
        &ole2(&["WordDocument", "Macros/"]),
        Family::Document,
    );
    assert_eq!(facts.macros.map(|m| m.part), Some("Macros".into()));
    let bytes = ole2(&["Workbook", "_VBA_PROJECT_CUR/"]);
    let facts = scan_bytes("m.xls", &bytes, Family::Spreadsheet);
    assert_eq!(facts.macros.map(|m| m.kind), Some(MacroKind::Vba));
    let facts = scan_bytes("plain.doc", &ole2(&["WordDocument"]), Family::Document);
    assert_eq!(facts, SourceFacts::default());
    // docx-encrypted: nothing can be read.
    let bytes = ole2(&["EncryptionInfo", "EncryptedPackage"]);
    let facts = scan_bytes("e.docx", &bytes, Family::Document);
    assert!(facts.requested_fonts.is_empty() && facts.macros.is_none());
    assert_eq!(facts.skipped.len(), 3);
}

#[test]
fn rtf_and_missing_files() {
    let facts = scan_bytes("a.rtf", b"{\\rtf1 hello}", Family::Document);
    assert_eq!(facts, SourceFacts::default());
    let facts = scan(
        std::path::Path::new("/nonexistent/source.docx"),
        Family::Document,
    );
    assert_eq!(facts.skipped.len(), 3);
}

#[test]
fn malformed_xml_skips_the_check() {
    let document = format!("<w:document {W}><w:body></w:p>");
    let facts = scan_bytes("bad.docx", &docx(&[], &document), Family::Document);
    assert!(
        facts
            .skipped
            .contains(&(Check::Fonts, SkipReason::MalformedXml)),
        "{:?}",
        facts.skipped
    );
    assert!(
        facts.requested_fonts.is_empty(),
        "a skipped check reports no fonts"
    );
}

#[test]
fn an_entity_bomb_is_never_expanded() {
    // xml-entities: quick-xml reads the DTD as one event and expands no
    // entity; the scan reads the styles as usual.
    let mut entities = String::from(r#"<!ENTITY a "aaaaaaaaaa">"#);
    for (name, inner) in [
        ("b", "a"),
        ("c", "b"),
        ("d", "c"),
        ("e", "d"),
        ("f", "e"),
        ("g", "f"),
        ("h", "g"),
        ("i", "h"),
    ] {
        entities.push_str(&format!(
            r#"<!ENTITY {name} "{}">"#,
            format!("&{inner};").repeat(10)
        ));
    }
    let document = format!(
        r#"<?xml version="1.0"?><!DOCTYPE w [{entities}]><w:document {W}><w:body><w:p><w:r><w:t>&i;</w:t></w:r></w:p></w:body></w:document>"#
    );
    let started = std::time::Instant::now();
    let facts = scan_bytes("laughs.docx", &docx(&[], &document), Family::Document);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(facts.requested_fonts, ["Calibri", "Cambria"]);
}

#[test]
fn a_part_over_its_cap_is_cap_reached() {
    let big = vec![b' '; 10_000];
    let bytes = zip(&[("big.xml", &big), ("fits.xml", &big[..4096])]);
    let dir = std::env::temp_dir().join(format!("convertd-preflight-cap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cap.zip");
    std::fs::write(&path, &bytes).unwrap();
    let mut package = Package::open(std::fs::File::open(&path).unwrap()).unwrap();
    assert_eq!(
        package.xml("big.xml", 4096, |_| true),
        Err(SkipReason::CapReached)
    );
    assert_eq!(package.xml("fits.xml", 4096, |_| true), Ok(4096));
    assert_eq!(
        package.xml("nope.xml", 4096, |_| true),
        Err(SkipReason::MissingPart)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn broken_packages_never_panic() {
    let good = docx(&[], &body("<w:p/>"));
    let deck = pptx(&[("p:sld", ""), ("p:sld show=\"0\"", "")], true);
    for (bytes, family) in [(good, Family::Document), (deck, Family::Presentation)] {
        for cut in [10, 100, bytes.len() / 2, bytes.len() - 1] {
            let _ = scan_bytes("cut", &bytes[..cut], family);
        }
        for at in (0..bytes.len()).step_by(37) {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x5A;
            let _ = scan_bytes("flipped", &flipped, family);
        }
    }
}
