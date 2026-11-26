//! Pre-flight on fixtures built here, after the corpus cases of
//! `corpus/expected.json` (docx-small, docx-headings, pptx-hidden,
//! pptx-pages-mismatch, potx, xlsx-sheet3, odt, odt-basic-canary,
//! docm-novba, docx-encrypted) and on broken inputs, which must never
//! panic.

use std::io::{Cursor, Write};
use std::path::PathBuf;

use super::package::Package;
use super::*;
use crate::domain::verification::{DeclaredPages, Slides};

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
    docx_linking(extra_entries, document, true)
}

/// `docx`, with or without the main part's relationship to
/// `word/fontTable.xml`.
fn docx_linking(extra_entries: &[(&str, &[u8])], document: &str, font_table: bool) -> Vec<u8> {
    let content_types = types("");
    let root = rels(&[("rId1", "officeDocument", "word/document.xml")]);
    let mut doc_rels = vec![
        ("rId1", "styles", "styles.xml"),
        ("rId2", "theme", "theme/theme1.xml"),
        ("rId3", "header", "header1.xml"),
    ];
    if font_table {
        doc_rels.push(("rId4", "fontTable", "fontTable.xml"));
    }
    let doc_rels = rels(&doc_rels);
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

/// The `w:fontKey` of the test fonts, as Word writes it.
const KEY: &str = "{5F3C1A2B-9D4E-4C6F-8A7B-0E1D2C3B4A59}";

/// A font part that starts with the sfnt version `magic`: an offset
/// table with one table record, then filler.
fn sfnt(magic: &[u8; 4]) -> Vec<u8> {
    sfnt_with(magic, &FONT_TABLES)
}

/// The tables of a minimal TrueType font.
const FONT_TABLES: [&[u8; 4]; 7] = [
    b"cmap", b"glyf", b"head", b"hhea", b"hmtx", b"loca", b"maxp",
];

/// A font part with an offset table listing `tables`, each 64 bytes long
/// after the directory, then filler.
fn sfnt_with(magic: &[u8; 4], tables: &[&[u8; 4]]) -> Vec<u8> {
    let count = u16::try_from(tables.len()).unwrap();
    let mut out = magic.to_vec();
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&[0, 16, 0, 0, 0, 0]);
    let start = 12 + 16 * u32::from(count);
    for (i, tag) in tables.iter().enumerate() {
        out.extend_from_slice(*tag);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(start + 64 * u32::try_from(i).unwrap()).to_be_bytes());
        out.extend_from_slice(&64u32.to_be_bytes());
    }
    out.extend_from_slice(&vec![0x5a; 64 * tables.len()]);
    out
}

/// ECMA-376 Part 1, 17.8.1, written independently of the scanner: the
/// first 32 bytes XORed with the key's GUID bytes, read from the end.
fn obfuscate(data: &[u8], key: &str) -> Vec<u8> {
    let hex: String = key.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let k: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&hex[30 - 2 * i..32 - 2 * i], 16).unwrap())
        .collect();
    let mut out = data.to_vec();
    for (i, byte) in out.iter_mut().take(32).enumerate() {
        *byte ^= k[i % 16];
    }
    out
}

/// One embed element of a test font: (element, relationship id, font key).
type Embed<'a> = (&'a str, &'a str, Option<&'a str>);

/// A `word/fontTable.xml`: each font with its embed elements.
fn font_table(fonts: &[(&str, &[Embed<'_>])]) -> String {
    let mut out = format!(
        r#"<w:fonts {W} xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#
    );
    for (name, embeds) in fonts {
        out.push_str(&format!(
            r#"<w:font w:name="{name}"><w:family w:val="swiss"/>"#
        ));
        for (element, id, key) in *embeds {
            let key = key
                .map(|k| format!(r#" w:fontKey="{k}""#))
                .unwrap_or_default();
            out.push_str(&format!(r#"<w:{element} r:id="{id}"{key}/>"#));
        }
        out.push_str("</w:font>");
    }
    out.push_str("</w:fonts>");
    out
}

/// A body whose runs request each of `families`.
fn runs(families: &[&str]) -> String {
    let runs: String = families
        .iter()
        .map(|f| format!(r#"<w:r><w:rPr><w:rFonts w:ascii="{f}" w:hAnsi="{f}"/></w:rPr></w:r>"#))
        .collect();
    body(&format!("<w:p>{runs}</w:p>"))
}

#[test]
fn docx_embedded_fonts_from_the_font_table() {
    // F1: a requested family counts as embedded only when one of its embed
    // elements points at a font part that the package holds and that is an
    // sfnt font once deobfuscated with the element's key.
    let other_key = "{00000000-1111-2222-3333-444444444444}";
    let table = font_table(&[
        ("Calibri", &[]),
        ("Aptos", &[("embedRegular", "rIdF1", Some(KEY))]),
        ("Embed Plain", &[("embedBold", "rIdF2", None)]),
        ("Gone Part", &[("embedItalic", "rIdF3", None)]),
        ("No Relationship", &[("embedBoldItalic", "rIdF9", None)]),
        ("Not Requested", &[("embedRegular", "rIdF1", Some(KEY))]),
        ("Empty Font", &[("embedRegular", "rIdF4", None)]),
        ("Junk Font", &[("embedRegular", "rIdF5", None)]),
        ("Wrong Key", &[("embedRegular", "rIdF1", Some(other_key))]),
        ("Bad Key", &[("embedRegular", "rIdF2", Some("not a guid"))]),
        (
            "Second Try",
            &[
                ("embedRegular", "rIdF4", None),
                ("embedBold", "rIdF2", None),
            ],
        ),
        ("Cut Font", &[("embedRegular", "rIdF6", None)]),
        ("No Tables", &[("embedRegular", "rIdF7", None)]),
    ]);
    let table_rels = rels(&[
        ("rIdF1", "font", "fonts/font1.odttf"),
        ("rIdF2", "font", "fonts/font2.otf"),
        ("rIdF3", "font", "fonts/font3.odttf"),
        ("rIdF4", "font", "fonts/font4.ttf"),
        ("rIdF5", "font", "fonts/font5.ttf"),
        ("rIdF6", "font", "fonts/font6.otf"),
        ("rIdF7", "font", "fonts/font7.ttf"),
    ]);
    let obfuscated = obfuscate(&sfnt(&[0, 1, 0, 0]), KEY);
    let plain = sfnt(b"OTTO");
    let mut no_tables = sfnt(b"true");
    no_tables[4..6].copy_from_slice(&[0, 0]);
    let bytes = docx(
        &[
            ("word/fontTable.xml", table.as_bytes()),
            ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
            ("word/fonts/font1.odttf", &obfuscated),
            ("word/fonts/font2.otf", &plain),
            ("word/fonts/font4.ttf", b""),
            (
                "word/fonts/font5.ttf",
                b"not a font at all, not a font at all",
            ),
            ("word/fonts/font6.otf", b"OTTO"),
            ("word/fonts/font7.ttf", &no_tables),
        ],
        &runs(&[
            "aptos",
            "Embed Plain",
            "Gone Part",
            "No Relationship",
            "Empty Font",
            "Junk Font",
            "Wrong Key",
            "Bad Key",
            "Second Try",
            "Cut Font",
            "No Tables",
        ]),
    );
    let facts = scan_bytes("embedded.docx", &bytes, Family::Document);
    // The run's spelling: names compare case-folded, as the catalog does.
    assert_eq!(facts.embedded_fonts, ["Embed Plain", "Second Try", "aptos"]);
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
}

#[test]
fn docx_embed_elements_count_only_inside_their_font() {
    // An embed element after a `w:font` closed, after a self-closing
    // `w:font`, or below another child is not that family's embed.
    let table = format!(
        r#"<w:fonts {W} xmlns:r="r"><w:font w:name="Aptos"><w:family w:val="swiss"/></w:font><w:embedRegular r:id="rIdF1"/><w:font w:name="Georgia"/><w:embedRegular r:id="rIdF1"/><w:font w:name="Verdana"><w:sig><w:embedRegular r:id="rIdF1"/></w:sig></w:font></w:fonts>"#
    );
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    let font = sfnt(&[0, 1, 0, 0]);
    let bytes = docx(
        &[
            ("word/fontTable.xml", table.as_bytes()),
            ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
            ("word/fonts/font1.ttf", &font),
        ],
        &runs(&["Aptos", "Georgia", "Verdana"]),
    );
    let facts = scan_bytes("scoped.docx", &bytes, Family::Document);
    assert!(
        facts.embedded_fonts.is_empty(),
        "{:?}",
        facts.embedded_fonts
    );
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
}

#[test]
fn docx_font_table_cut_at_a_tag_boundary_skips_the_font_check() {
    // The XML ends after a complete tag with elements still open: quick-xml
    // reports EOF, and the scan must not take the partial table.
    let table = format!(
        r#"<w:fonts {W} xmlns:r="r"><w:font w:name="Aptos"><w:embedRegular r:id="rIdF1"/>"#
    );
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    let font = sfnt(&[0, 1, 0, 0]);
    let bytes = docx(
        &[
            ("word/fontTable.xml", table.as_bytes()),
            ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
            ("word/fonts/font1.ttf", &font),
        ],
        &runs(&["Aptos"]),
    );
    let facts = scan_bytes("cut.docx", &bytes, Family::Document);
    assert!(
        facts.requested_fonts.is_empty(),
        "{:?}",
        facts.requested_fonts
    );
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
}

#[test]
fn docx_unreadable_font_part_skips_the_font_check() {
    // The part is present but in a compression method the reader does not
    // support: not evidence that the family is missing.
    let table = font_table(&[("Aptos", &[("embedRegular", "rIdF1", None)])]);
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    let font = sfnt(&[0, 1, 0, 0]);
    let mut bytes = docx(
        &[
            ("word/fontTable.xml", table.as_bytes()),
            ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
            ("word/fonts/font1.ttf", &font),
        ],
        &runs(&["Aptos"]),
    );
    set_method(&mut bytes, "word/fonts/font1.ttf", 12);
    let facts = scan_bytes("unreadable.docx", &bytes, Family::Document);
    assert!(
        facts.requested_fonts.is_empty(),
        "{:?}",
        facts.requested_fonts
    );
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MissingPart)]);
}

/// A DOCX whose runs ask for Aptos and whose linked font table embeds it
/// through `table_rels` with `font` as `word/fonts/font1.ttf`.
fn aptos_embedded(table_rels: &str, font: &[u8]) -> SourceFacts {
    let table = font_table(&[("Aptos", &[("embedRegular", "rIdF1", None)])]);
    let bytes = docx(
        &[
            ("word/fontTable.xml", table.as_bytes()),
            ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
            ("word/fonts/font1.ttf", font),
        ],
        &runs(&["Aptos"]),
    );
    scan_bytes("aptos.docx", &bytes, Family::Document)
}

#[test]
fn docx_font_whose_tables_leave_the_part_is_not_embedded() {
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    let font = sfnt(&[0, 1, 0, 0]);
    assert_eq!(aptos_embedded(&table_rels, &font).embedded_fonts, ["Aptos"]);
    // Cut right after the directory: the one table it lists is not there.
    let facts = aptos_embedded(&table_rels, &font[..28]);
    assert!(
        facts.embedded_fonts.is_empty(),
        "{:?}",
        facts.embedded_fonts
    );
    // A collection: Word embeds single fonts, so it does not count.
    let mut collection = b"ttcf\0\x01\0\0\0\0\0\x01\0\0\x10\0".to_vec();
    collection.extend_from_slice(&[0; 16]);
    let facts = aptos_embedded(&table_rels, &collection);
    assert!(
        facts.embedded_fonts.is_empty(),
        "{:?}",
        facts.embedded_fonts
    );
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
}

#[test]
fn docx_font_without_a_required_table_is_not_embedded() {
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    // Only `head`: no character map, no metrics, no outlines.
    let facts = aptos_embedded(&table_rels, &sfnt_with(&[0, 1, 0, 0], &[b"head"]));
    assert!(
        facts.embedded_fonts.is_empty(),
        "{:?}",
        facts.embedded_fonts
    );
    // CFF outlines instead of glyf and loca count.
    let cff = sfnt_with(
        b"OTTO",
        &[b"CFF ", b"cmap", b"head", b"hhea", b"hmtx", b"maxp"],
    );
    assert_eq!(aptos_embedded(&table_rels, &cff).embedded_fonts, ["Aptos"]);
}

#[test]
fn docx_font_table_needs_a_fonts_root() {
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    let font = sfnt(&[0, 1, 0, 0]);
    let with_table = |table: &str| {
        let bytes = docx(
            &[
                ("word/fontTable.xml", table.as_bytes()),
                ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
                ("word/fonts/font1.ttf", &font),
            ],
            &runs(&["Aptos"]),
        );
        scan_bytes("root.docx", &bytes, Family::Document)
    };
    // Empty, or only the declaration: malformed, not an empty table.
    for table in ["", r#"<?xml version="1.0"?>"#] {
        let facts = with_table(table);
        assert_eq!(
            facts.skipped,
            [(Check::Fonts, SkipReason::MalformedXml)],
            "{table:?}"
        );
    }
    // A `w:font` that is not a child of the `w:fonts` root does not count.
    let nested = format!(
        r#"<w:fonts {W} xmlns:r="r"><w:ext><w:font w:name="Aptos"><w:embedRegular r:id="rIdF1"/></w:font></w:ext></w:fonts>"#
    );
    let other_root = format!(
        r#"<w:other {W} xmlns:r="r"><w:font w:name="Aptos"><w:embedRegular r:id="rIdF1"/></w:font></w:other>"#
    );
    for table in [nested, other_root] {
        let facts = with_table(&table);
        assert!(facts.embedded_fonts.is_empty(), "{table}");
        assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
    }
    // An empty but well-formed table embeds nothing and runs the check.
    let facts = with_table(&format!("<w:fonts {W}/>"));
    assert!(facts.embedded_fonts.is_empty());
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
}

#[test]
fn docx_font_table_with_a_second_root_skips_the_font_check() {
    let table = format!(
        r#"<w:fonts {W}/><w:x {W} xmlns:r="r"><w:font w:name="Aptos"><w:embedRegular r:id="rIdF1"/></w:font></w:x>"#
    );
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    let font = sfnt(&[0, 1, 0, 0]);
    let bytes = docx(
        &[
            ("word/fontTable.xml", table.as_bytes()),
            ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
            ("word/fonts/font1.ttf", &font),
        ],
        &runs(&["Aptos"]),
    );
    let facts = scan_bytes("roots.docx", &bytes, Family::Document);
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
}

#[test]
fn docx_font_with_a_short_required_table_is_not_embedded() {
    // `head` is 54 bytes at least; 16 is a cut table.
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    let mut font = sfnt(&[0, 1, 0, 0]);
    // The `head` record is the third; its length is at 12 + 16 * 2 + 12.
    font[56..60].copy_from_slice(&16u32.to_be_bytes());
    let facts = aptos_embedded(&table_rels, &font);
    assert!(
        facts.embedded_fonts.is_empty(),
        "{:?}",
        facts.embedded_fonts
    );
}

#[test]
fn docx_font_table_relationships_over_the_cap_skip_the_font_check() {
    // More relationships than the reader keeps: a dropped one would read
    // as one never made, so the check is skipped.
    let entries: Vec<(String, &str, &str)> = (0..20_001)
        .map(|i| (format!("rIdX{i}"), "font", "fonts/font1.ttf"))
        .collect();
    let entries: Vec<(&str, &str, &str)> = entries
        .iter()
        .map(|(id, kind, target)| (id.as_str(), *kind, *target))
        .collect();
    let facts = aptos_embedded(&rels(&entries), &sfnt(&[0, 1, 0, 0]));
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::CapReached)]);
}

#[test]
fn docx_font_table_relationships_with_a_second_root_skip_the_font_check() {
    // Only the direct children of the one `Relationships` root count.
    let two_roots = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdF1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/font" Target="fonts/font1.ttf"/></Relationships>"#;
    let facts = aptos_embedded(two_roots, &sfnt(&[0, 1, 0, 0]));
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
    // A part whose root is not `Relationships` is malformed, not empty.
    let wrong_root = r#"<?xml version="1.0"?><Other><Relationship Id="rIdF1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/font" Target="fonts/font1.ttf"/></Other>"#;
    let facts = aptos_embedded(wrong_root, &sfnt(&[0, 1, 0, 0]));
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
    // A relationship nested below another element is not one.
    let nested = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><x><Relationship Id="rIdF1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/font" Target="fonts/font1.ttf"/></x></Relationships>"#;
    let facts = aptos_embedded(nested, &sfnt(&[0, 1, 0, 0]));
    assert!(
        facts.embedded_fonts.is_empty(),
        "{:?}",
        facts.embedded_fonts
    );
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
}

#[test]
fn docx_family_names_over_256_bytes_are_not_kept() {
    // Such a name is not a font name; kept, it would only grow the facts
    // that cross IPC, twice when it is also embedded.
    let long = "A".repeat(257);
    let facts = scan_bytes(
        "long.docx",
        &docx(&[], &runs(&[&long, "Georgia"])),
        Family::Document,
    );
    assert!(facts.requested_fonts.contains(&"Georgia".to_owned()));
    assert!(!facts.requested_fonts.contains(&long));
}

#[test]
fn docx_text_after_the_root_skips_the_font_check() {
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    let font = sfnt(&[0, 1, 0, 0]);
    let table = font_table(&[("Aptos", &[("embedRegular", "rIdF1", None)])]) + "stray";
    let bytes = docx(
        &[
            ("word/fontTable.xml", table.as_bytes()),
            ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
            ("word/fonts/font1.ttf", &font),
        ],
        &runs(&["Aptos"]),
    );
    let facts = scan_bytes("text.docx", &bytes, Family::Document);
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
    // The same in the font table's relationships.
    let facts = aptos_embedded(&(table_rels.clone() + "stray"), &font);
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
    // A character reference after the root is character data too.
    let facts = aptos_embedded(&(table_rels.clone() + "&#65;"), &font);
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
    // White space after the root is fine.
    let facts = aptos_embedded(&(table_rels + "\n  \n"), &font);
    assert_eq!(facts.embedded_fonts, ["Aptos"]);
}

#[test]
fn docx_malformed_attribute_in_the_font_table_skips_the_font_check() {
    // The name parses before the broken attribute; the tag is still broken.
    let table = format!(
        r#"<w:fonts {W} xmlns:r="r"><w:font w:name="Aptos" broken><w:embedRegular r:id="rIdF1"/></w:font></w:fonts>"#
    );
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.ttf")]);
    let font = sfnt(&[0, 1, 0, 0]);
    let bytes = docx(
        &[
            ("word/fontTable.xml", table.as_bytes()),
            ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
            ("word/fonts/font1.ttf", &font),
        ],
        &runs(&["Aptos"]),
    );
    let facts = scan_bytes("attr.docx", &bytes, Family::Document);
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
}

#[test]
fn docx_undecodable_relationship_target_skips_the_font_check() {
    // An undeclared entity in Target: the relationship cannot be read, so
    // it is not taken as absent.
    let bad = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdF1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/font" Target="fonts/&nope;.ttf"/></Relationships>"#;
    let facts = aptos_embedded(bad, &sfnt(&[0, 1, 0, 0]));
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
}

#[test]
fn docx_font_relationships_need_the_font_type() {
    // A relationship whose type only ends in "/font" is not a font part.
    let custom = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdF1" Type="http://example.com/custom/font" Target="fonts/font1.ttf"/></Relationships>"#;
    let facts = aptos_embedded(custom, &sfnt(&[0, 1, 0, 0]));
    assert!(
        facts.embedded_fonts.is_empty(),
        "{:?}",
        facts.embedded_fonts
    );
    // The strict OOXML type counts.
    let strict = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdF1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/font" Target="fonts/font1.ttf"/></Relationships>"#;
    assert_eq!(
        aptos_embedded(strict, &sfnt(&[0, 1, 0, 0])).embedded_fonts,
        ["Aptos"]
    );
}

#[test]
fn docx_font_table_relationships_cut_short_skip_the_font_check() {
    // Cut at a tag boundary with `Relationships` still open.
    let cut = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdF9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/font" Target="fonts/font9.ttf"/>"#;
    let facts = aptos_embedded(cut, &sfnt(&[0, 1, 0, 0]));
    assert!(
        facts.requested_fonts.is_empty(),
        "{:?}",
        facts.requested_fonts
    );
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
}

#[test]
fn docx_unreadable_main_relationships_skip_the_font_check() {
    // Without the main part's relationships the font table cannot be found,
    // so the check does not run as if nothing were embedded.
    let mut bytes = docx(&[], &runs(&["Aptos"]));
    set_method(&mut bytes, "word/_rels/document.xml.rels", 12);
    let facts = scan_bytes("norels.docx", &bytes, Family::Document);
    assert!(
        facts.requested_fonts.is_empty(),
        "{:?}",
        facts.requested_fonts
    );
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MissingPart)]);
}

#[test]
fn docx_font_table_counts_only_when_the_main_part_links_it() {
    // A leftover table that no relationship of the main part names is not
    // loaded by LibreOffice, so it embeds nothing.
    let table = font_table(&[("Aptos", &[("embedRegular", "rIdF1", Some(KEY))])]);
    let table_rels = rels(&[("rIdF1", "font", "fonts/font1.odttf")]);
    let font = obfuscate(&sfnt(&[0, 1, 0, 0]), KEY);
    let entries: [(&str, &[u8]); 3] = [
        ("word/fontTable.xml", table.as_bytes()),
        ("word/_rels/fontTable.xml.rels", table_rels.as_bytes()),
        ("word/fonts/font1.odttf", &font),
    ];
    let unlinked = docx_linking(&entries, &runs(&["Aptos"]), false);
    let facts = scan_bytes("stale.docx", &unlinked, Family::Document);
    assert!(
        facts.embedded_fonts.is_empty(),
        "{:?}",
        facts.embedded_fonts
    );
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
    // The same parts, linked: Aptos is embedded.
    let facts = scan_bytes(
        "linked.docx",
        &docx(&entries, &runs(&["Aptos"])),
        Family::Document,
    );
    assert_eq!(facts.embedded_fonts, ["Aptos"]);
}

#[test]
fn docx_without_a_font_table_embeds_nothing() {
    let facts = scan_bytes("small.docx", &docx(&[], &body("<w:p/>")), Family::Document);
    assert!(facts.embedded_fonts.is_empty());
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
}

/// Sets the compression method of entry `name` in both its local and its
/// central header, to make an entry that is present but cannot be read.
fn set_method(zip: &mut [u8], name: &str, method: u16) {
    let mut patched = 0;
    for at in 0..zip.len().saturating_sub(46) {
        let signature = &zip[at..at + 4];
        let (method_at, name_at) = match signature {
            b"PK\x03\x04" => (at + 8, at + 30),
            b"PK\x01\x02" => (at + 10, at + 46),
            _ => continue,
        };
        if zip.get(name_at..name_at + name.len()) == Some(name.as_bytes()) {
            zip[method_at..method_at + 2].copy_from_slice(&method.to_le_bytes());
            patched += 1;
        }
    }
    assert_eq!(patched, 2, "{name} not found");
}

#[test]
fn docx_font_table_present_but_unreadable_skips_the_font_check() {
    // An entry in a compression method the reader does not support is not
    // absent: the check is skipped instead of running without the table.
    let table = font_table(&[("Aptos", &[("embedRegular", "rIdF1", Some(KEY))])]);
    let mut bytes = docx(
        &[("word/fontTable.xml", table.as_bytes())],
        &runs(&["Aptos"]),
    );
    set_method(&mut bytes, "word/fontTable.xml", 12);
    let facts = scan_bytes("bzip2.docx", &bytes, Family::Document);
    assert!(
        facts.requested_fonts.is_empty(),
        "{:?}",
        facts.requested_fonts
    );
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MissingPart)]);
}

#[test]
fn docx_malformed_font_table_skips_the_font_check() {
    // The table feeds the font check: unreadable, the check is skipped
    // rather than run without knowing which families are embedded.
    let bytes = docx(
        &[(
            "word/fontTable.xml",
            b"<w:fonts><w:font w:name=\"A\"><broken",
        )],
        &body("<w:p/>"),
    );
    let facts = scan_bytes("broken.docx", &bytes, Family::Document);
    assert!(
        facts.requested_fonts.is_empty(),
        "{:?}",
        facts.requested_fonts
    );
    assert!(facts.embedded_fonts.is_empty());
    assert_eq!(facts.skipped, [(Check::Fonts, SkipReason::MalformedXml)]);
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
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 5,
            hidden: vec![2, 4]
        })
    );
    // +mj-lt (layout) and +mn-lt (master) resolve through the master's
    // theme; the unreached layout's Zzyzx Grotesk is not read.
    assert_eq!(facts.requested_fonts, ["Aptos", "Calibri", "Georgia"]);
    assert!(facts.skipped.is_empty(), "{:?}", facts.skipped);
}

#[test]
fn pptx_lists_the_hidden_slides_in_slide_order() {
    // pptx-hidden3: 3 slides, the 2nd hidden; `show="false"` is hidden too.
    let slides = [("p:sld", ""), ("p:sld show=\"0\"", ""), ("p:sld", "")];
    let facts = scan_bytes("hidden3.pptx", &pptx(&slides, true), Family::Presentation);
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 3,
            hidden: vec![2]
        })
    );
    let slides = [("p:sld show=\"false\"", ""), ("p:sld show=\"1\"", "")];
    let facts = scan_bytes("false.pptx", &pptx(&slides, true), Family::Presentation);
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 2,
            hidden: vec![1]
        })
    );
    // No hidden slide: the list is empty, not absent.
    let slides = [("p:sld", ""), ("p:sld", "")];
    let facts = scan_bytes("plain.pptx", &pptx(&slides, true), Family::Presentation);
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 2,
            hidden: vec![]
        })
    );
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

/// An ODP whose pages use the given drawing-page styles. `styles` holds
/// the automatic styles of `content.xml`, `common` those of `styles.xml`.
fn odp(pages: &[Option<&str>], styles: &str, common: &str) -> Vec<u8> {
    let pages: String = pages
        .iter()
        .map(|style| match style {
            Some(name) => format!(r#"<draw:page draw:name="p" draw:style-name="{name}"/>"#),
            None => r#"<draw:page draw:name="p"/>"#.to_owned(),
        })
        .collect();
    odp_parts(&pages, styles, common, "")
}

/// An ODP from its parts: the children of `office:presentation`, the
/// automatic styles of `content.xml`, and the common and automatic styles
/// of `styles.xml` (the latter belong to master pages).
fn odp_parts(pages: &str, styles: &str, common: &str, master_automatic: &str) -> Vec<u8> {
    const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0""#;
    let content = format!(
        r#"<office:document-content {NS}><office:automatic-styles>{styles}</office:automatic-styles><office:body><office:presentation>{pages}</office:presentation></office:body></office:document-content>"#
    );
    let common = format!(
        r#"<office:document-styles {NS}><office:styles>{common}</office:styles><office:automatic-styles>{master_automatic}</office:automatic-styles></office:document-styles>"#
    );
    zip(&[
        (
            "mimetype",
            b"application/vnd.oasis.opendocument.presentation",
        ),
        ("content.xml", content.as_bytes()),
        ("styles.xml", common.as_bytes()),
    ])
}

#[test]
fn odp_styles_resolve_only_in_their_own_scope() {
    // styles.xml may reuse an automatic style name of content.xml for a
    // master page: that style never applies to a slide.
    let slide = page_style("dp1", None, None);
    let master = page_style("dp1", Some("hidden"), None);
    let pages = r#"<draw:page draw:style-name="dp1"/><draw:page/>"#;
    let facts = scan_bytes(
        "scope.odp",
        &odp_parts(pages, &slide, "", &master),
        Family::Presentation,
    );
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 2,
            hidden: vec![]
        })
    );
    // A slide style defined only among the master-page styles is unknown.
    let pages = r#"<draw:page draw:style-name="dpM"/>"#;
    let master = page_style("dpM", Some("hidden"), None);
    let facts = scan_bytes(
        "master-only.odp",
        &odp_parts(pages, "", "", &master),
        Family::Presentation,
    );
    assert_eq!(facts.slides, None);
}

#[test]
fn odp_properties_apply_only_inside_their_open_style() {
    // A self-closing slide style has no properties. The properties of a
    // style inside an inline object must not land on it.
    let styles = r#"<style:style style:name="dp1" style:family="drawing-page"/>"#;
    let pages = concat!(
        r#"<draw:page draw:style-name="dp1"><draw:frame><draw:object><office:document>"#,
        r#"<office:automatic-styles><style:style style:name="x" style:family="drawing-page"><style:drawing-page-properties presentation:visibility="hidden"/></style:style></office:automatic-styles>"#,
        r#"</office:document></draw:object></draw:frame></draw:page><draw:page/>"#,
    );
    let facts = scan_bytes(
        "empty-style.odp",
        &odp_parts(pages, styles, "", ""),
        Family::Presentation,
    );
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 2,
            hidden: vec![]
        })
    );
}

#[test]
fn odp_visibility_is_read_by_namespace() {
    // `ext:visibility` is another vocabulary's attribute; a prefix other
    // than `presentation` bound to the presentation namespace counts.
    let styles = concat!(
        r#"<style:style xmlns:ext="urn:example:ext" style:name="dp1" style:family="drawing-page"><style:drawing-page-properties ext:visibility="hidden"/></style:style>"#,
        r#"<style:style xmlns:pr="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" style:name="dp2" style:family="drawing-page"><style:drawing-page-properties pr:visibility="hidden"/></style:style>"#,
    );
    let pages = r#"<draw:page draw:style-name="dp1"/><draw:page draw:style-name="dp2"/>"#;
    let facts = scan_bytes(
        "namespaces.odp",
        &odp_parts(pages, styles, "", ""),
        Family::Presentation,
    );
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 2,
            hidden: vec![2]
        })
    );
}

#[test]
fn odp_counts_only_the_slides_of_the_presentation() {
    // An inline object inside a slide holds a draw:page of its own; it is
    // not a slide. A style inside it is not a slide style either.
    let styles = page_style("dp2", Some("hidden"), None);
    let pages = concat!(
        r#"<draw:page/>"#,
        r#"<draw:page draw:style-name="dp2"><draw:frame><draw:object><office:document>"#,
        r#"<office:automatic-styles><style:style style:name="dp2" style:family="drawing-page"><style:drawing-page-properties presentation:visibility="visible"/></style:style></office:automatic-styles>"#,
        r#"<office:body><office:presentation><draw:page draw:name="inner"/></office:presentation></office:body>"#,
        r#"</office:document></draw:object></draw:frame></draw:page>"#,
    );
    let facts = scan_bytes(
        "nested.odp",
        &odp_parts(pages, &styles, "", ""),
        Family::Presentation,
    );
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 2,
            hidden: vec![2]
        })
    );
}

fn page_style(name: &str, visibility: Option<&str>, parent: Option<&str>) -> String {
    let parent = parent
        .map(|p| format!(r#" style:parent-style-name="{p}""#))
        .unwrap_or_default();
    let props = visibility
        .map(|v| format!(r#"<style:drawing-page-properties presentation:visibility="{v}"/>"#))
        .unwrap_or_default();
    format!(
        r#"<style:style style:name="{name}" style:family="drawing-page"{parent}>{props}</style:style>"#
    )
}

#[test]
fn odp_lists_the_pages_whose_style_hides_them() {
    let styles = [
        page_style("dp1", None, None),
        page_style("dp2", Some("hidden"), None),
        page_style("dp3", Some("visible"), None),
    ]
    .concat();
    let pages = [Some("dp1"), Some("dp2"), None, Some("dp3"), Some("dp2")];
    let facts = scan_bytes("deck.odp", &odp(&pages, &styles, ""), Family::Presentation);
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 5,
            hidden: vec![2, 5]
        })
    );
    // A style inherits the visibility of its parent, here a common style
    // in styles.xml.
    let styles = page_style("dp1", None, Some("Hide"));
    let common = page_style("Hide", Some("hidden"), None);
    let pages = [Some("dp1"), None];
    let facts = scan_bytes(
        "inherit.odp",
        &odp(&pages, &styles, &common),
        Family::Presentation,
    );
    assert_eq!(
        facts.slides,
        Some(Slides {
            count: 2,
            hidden: vec![1]
        })
    );
    // A page whose style is defined nowhere: the visibility is unknown, so
    // no slide list.
    let facts = scan_bytes(
        "unknown.odp",
        &odp(&[Some("dp9")], "", ""),
        Family::Presentation,
    );
    assert_eq!(facts.slides, None);
}

#[test]
fn only_presentations_list_slides() {
    let facts = scan_bytes("plain.odt", &odt(&[]), Family::Document);
    assert_eq!(facts.slides, None);
    // A legacy .ppt: no slide list is read from the binary format.
    let bytes = ole2(&["PowerPoint Document", "Current User"]);
    let facts = scan_bytes("deck.ppt", &bytes, Family::Presentation);
    assert_eq!(facts.slides, None);
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
