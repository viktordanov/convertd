use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::detect::zip_scan::Entry;

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const A4: &str = r#"<w:pgSz w:w="11906" w:h="16838"/>"#;

fn doc(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W}" xmlns:r="r-ns"><w:body>{body}</w:body></w:document>"#
    )
}

fn fixed(xml: &str, paper: Paper) -> Option<String> {
    fix_part(xml.as_bytes(), paper)
        .unwrap()
        .map(|bytes| String::from_utf8(bytes).unwrap())
}

#[test]
fn a_body_without_a_section_gets_one() {
    let out = fixed(&doc("<w:p><w:r><w:t>x</w:t></w:r></w:p>"), Paper::A4).unwrap();
    assert_eq!(
        out,
        doc(&format!(
            "<w:p><w:r><w:t>x</w:t></w:r></w:p><w:sectPr>{A4}</w:sectPr>"
        ))
    );
}

#[test]
fn the_page_size_goes_after_the_references_and_before_the_margins() {
    let body = r#"<w:p/><w:sectPr w:rsidR="00A1"><w:headerReference w:type="default" r:id="rId7"/><w:type w:val="nextPage"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/><w:cols w:space="720"/></w:sectPr>"#;
    let out = fixed(&doc(body), Paper::Letter).unwrap();
    assert_eq!(
        out,
        doc(
            r#"<w:p/><w:sectPr w:rsidR="00A1"><w:headerReference w:type="default" r:id="rId7"/><w:type w:val="nextPage"/><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/><w:cols w:space="720"/></w:sectPr>"#
        )
    );
    // Only references: the size goes last.
    let out = fixed(
        &doc(r#"<w:sectPr><w:footerReference w:type="default" r:id="rId8"/></w:sectPr>"#),
        Paper::A4,
    )
    .unwrap();
    assert_eq!(
        out,
        doc(&format!(
            r#"<w:sectPr><w:footerReference w:type="default" r:id="rId8"/>{A4}</w:sectPr>"#
        ))
    );
}

#[test]
fn a_section_with_a_page_size_is_left_alone() {
    let body =
        r#"<w:p/><w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1"/></w:sectPr>"#;
    assert_eq!(fixed(&doc(body), Paper::A4), None);
    // A page size with only one dimension is still the document's choice.
    let body = r#"<w:sectPr><w:pgSz w:w="12240"/></w:sectPr>"#;
    assert_eq!(fixed(&doc(body), Paper::A4), None);
}

#[test]
fn an_empty_section_is_expanded() {
    let out = fixed(&doc(r#"<w:p/><w:sectPr w:rsidR="1" />"#), Paper::A4).unwrap();
    assert_eq!(
        out,
        doc(&format!(r#"<w:p/><w:sectPr w:rsidR="1">{A4}</w:sectPr>"#))
    );
}

#[test]
fn every_section_break_is_sized_and_a_sized_one_kept() {
    let body = r#"<w:p><w:pPr><w:sectPr><w:pgMar w:top="1"/></w:sectPr></w:pPr></w:p><w:p><w:pPr><w:sectPr><w:pgSz w:w="15840" w:h="12240" w:orient="landscape"/></w:sectPr></w:pPr></w:p><w:p/><w:sectPr/>"#;
    let out = fixed(&doc(body), Paper::A4).unwrap();
    assert_eq!(
        out,
        doc(&format!(
            r#"<w:p><w:pPr><w:sectPr>{A4}<w:pgMar w:top="1"/></w:sectPr></w:pPr></w:p><w:p><w:pPr><w:sectPr><w:pgSz w:w="15840" w:h="12240" w:orient="landscape"/></w:sectPr></w:pPr></w:p><w:p/><w:sectPr>{A4}</w:sectPr>"#
        ))
    );
}

#[test]
fn a_tracked_former_section_is_not_a_section() {
    let body = r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:sectPrChange w:id="1"><w:sectPr><w:pgMar w:top="1"/></w:sectPr></w:sectPrChange></w:sectPr>"#;
    assert_eq!(fixed(&doc(body), Paper::A4), None);
    // Without a size, the size goes before the change record.
    let body = r#"<w:sectPr><w:sectPrChange w:id="1"><w:sectPr/></w:sectPrChange></w:sectPr>"#;
    assert_eq!(
        fixed(&doc(body), Paper::A4).unwrap(),
        doc(&format!(
            r#"<w:sectPr>{A4}<w:sectPrChange w:id="1"><w:sectPr/></w:sectPrChange></w:sectPr>"#
        ))
    );
}

#[test]
fn elements_of_other_namespaces_are_not_sections() {
    let body = r#"<x:sectPr xmlns:x="other"/><w:sectPr><w:pgSz w:w="1" w:h="2"/></w:sectPr>"#;
    assert_eq!(fixed(&doc(body), Paper::A4), None);
}

#[test]
fn the_prefix_of_the_document_is_used() {
    let xml =
        format!(r#"<ns0:document xmlns:ns0="{W}"><ns0:body><ns0:p/></ns0:body></ns0:document>"#);
    assert_eq!(
        fixed(&xml, Paper::A4).unwrap(),
        format!(
            r#"<ns0:document xmlns:ns0="{W}"><ns0:body><ns0:p/><ns0:sectPr><ns0:pgSz ns0:w="11906" ns0:h="16838"/></ns0:sectPr></ns0:body></ns0:document>"#
        )
    );
}

#[test]
fn a_default_namespace_gets_a_prefix_for_the_attributes() {
    let xml = format!(r#"<document xmlns="{W}"><body><sectPr/></body></document>"#);
    assert_eq!(
        fixed(&xml, Paper::Letter).unwrap(),
        format!(
            r#"<document xmlns="{W}"><body><sectPr><pgSz xmlns:rscp="{W}" rscp:w="12240" rscp:h="15840"/></sectPr></body></document>"#
        )
    );
}

#[test]
fn the_strict_namespace_counts() {
    let strict = "http://purl.oclc.org/ooxml/wordprocessingml/main";
    let xml = format!(r#"<w:document xmlns:w="{strict}"><w:body><w:p/></w:body></w:document>"#);
    assert!(fixed(&xml, Paper::A4).unwrap().contains(A4));
}

#[test]
fn a_byte_order_mark_and_comments_keep_the_offsets() {
    let mut xml = b"\xEF\xBB\xBF".to_vec();
    xml.extend_from_slice(
        doc("<!-- a <w:sectPr/> in a comment --><w:p><w:r><w:t>&lt;&amp;</w:t></w:r></w:p><![CDATA[<w:sectPr/>]]>")
            .as_bytes(),
    );
    let out = fix_part(&xml, Paper::A4).unwrap().unwrap();
    let mut want = b"\xEF\xBB\xBF".to_vec();
    want.extend_from_slice(
        doc(&format!(
            "<!-- a <w:sectPr/> in a comment --><w:p><w:r><w:t>&lt;&amp;</w:t></w:r></w:p><![CDATA[<w:sectPr/>]]><w:sectPr>{A4}</w:sectPr>"
        ))
        .as_bytes(),
    );
    assert_eq!(
        String::from_utf8(out).unwrap(),
        String::from_utf8(want).unwrap()
    );
}

#[test]
fn malformed_or_oversized_parts_are_skipped() {
    assert_eq!(
        fix_part(b"<w:document><w:body>", Paper::A4),
        Err(Skip::Unreadable)
    );
    assert_eq!(
        fix_part(doc("<w:p></w:r>").as_bytes(), Paper::A4),
        Err(Skip::Unreadable)
    );
    let xml = doc("<w:p/>");
    assert_eq!(
        plan(xml.as_bytes(), Paper::A4, xml.len() as u64 - 1),
        Err(Skip::Unreadable)
    );
    assert!(plan(xml.as_bytes(), Paper::A4, xml.len() as u64).is_ok());
    let deep = doc(&("<w:p>".repeat(MAX_DEPTH) + &"</w:p>".repeat(MAX_DEPTH)));
    assert_eq!(fix_part(deep.as_bytes(), Paper::A4), Err(Skip::Unreadable));
}

/// One entry of a test archive: name, content, deflated, with a data
/// descriptor.
type TestEntry<'a> = (&'a str, &'a [u8], bool, bool);

/// A ZIP archive written by hand, so the test controls the methods and
/// the data descriptors.
fn zip(entries: &[TestEntry<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for &(name, data, deflated, descriptor) in entries {
        let body = if deflated {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        } else {
            data.to_vec()
        };
        let crc = crc32fast::hash(data);
        let offset = out.len() as u32;
        let flags: u16 = if descriptor { 0x0808 } else { 0x0800 };
        let method: u16 = if deflated { 8 } else { 0 };
        let mut header = Vec::new();
        header.extend_from_slice(&20u16.to_le_bytes());
        header.extend_from_slice(&flags.to_le_bytes());
        header.extend_from_slice(&method.to_le_bytes());
        header.extend_from_slice(&[0x21, 0x43, 0x65, 0x57]); // time, date
        let sizes = |crc: u32, c: u32, u: u32| {
            [crc.to_le_bytes(), c.to_le_bytes(), u.to_le_bytes()].concat()
        };
        let real = sizes(crc, body.len() as u32, data.len() as u32);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(&if descriptor {
            sizes(0, 0, 0)
        } else {
            real.clone()
        });
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&[0xCA, 0xFE, 0, 0]); // an extra block of length 0
        out.extend_from_slice(&body);
        if descriptor {
            out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
            out.extend_from_slice(&real);
        }
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&header);
        central.extend_from_slice(&real);
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 4]); // extra, comment
        central.extend_from_slice(&[0; 8]); // disk, attributes
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

const CONTENT_TYPES: &[u8] = br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#;
const RELS: &[u8] = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

fn entries(bytes: &[u8]) -> Vec<(String, Entry)> {
    zip_scan::list(&mut std::io::Cursor::new(bytes), bytes.len() as u64).unwrap()
}

fn read(bytes: &[u8], name: &str) -> Vec<u8> {
    let (_, entry) = entries(bytes).into_iter().find(|(n, _)| n == name).unwrap();
    zip_scan::read_entry(&mut std::io::Cursor::new(bytes), &entry, 1 << 20)
        .unwrap()
        .unwrap()
}

/// The local record of `name`: from its offset to the next entry's, or to
/// the central directory.
fn local_record<'a>(bytes: &'a [u8], name: &str) -> &'a [u8] {
    let mut list = entries(bytes);
    list.sort_by_key(|(_, e)| e.local_offset);
    let (_, cd, _) =
        zip_scan::directory(&mut std::io::Cursor::new(bytes), bytes.len() as u64).unwrap();
    let at = list.iter().position(|(n, _)| n == name).unwrap();
    let start = list[at].1.local_offset as usize;
    let end = list
        .get(at + 1)
        .map_or(cd as usize, |(_, e)| e.local_offset as usize);
    &bytes[start..end]
}

/// A fresh directory under the system temp dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "convertd-docx-paper-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn repacked(source: &[u8], paper: Paper) -> (Result<bool, Skip>, Option<Vec<u8>>) {
    let dir = Scratch::new();
    let input = dir.path().join("source.docx");
    std::fs::write(&input, source).unwrap();
    let out = dir.path().join("copy.docx");
    let result = repack(&input, &out, paper);
    (result, std::fs::read(&out).ok())
}

#[test]
fn a_deflated_main_part_is_rewritten_and_every_other_entry_kept_byte_for_byte() {
    let main = doc("<w:p><w:r><w:t>Hello</w:t></w:r></w:p>");
    let image = vec![7u8; 5000];
    let source = zip(&[
        ("[Content_Types].xml", CONTENT_TYPES, true, false),
        ("_rels/.rels", RELS, false, true),
        ("word/document.xml", main.as_bytes(), true, true),
        ("word/media/image1.png", &image, false, false),
    ]);
    let (result, copy) = repacked(&source, Paper::A4);
    assert_eq!(result, Ok(true));
    let copy = copy.unwrap();
    for name in [
        "[Content_Types].xml",
        "_rels/.rels",
        "word/media/image1.png",
    ] {
        assert_eq!(
            local_record(&copy, name),
            local_record(&source, name),
            "{name}"
        );
    }
    let list = entries(&copy);
    let names: Vec<&str> = list.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "[Content_Types].xml",
            "_rels/.rels",
            "word/document.xml",
            "word/media/image1.png"
        ]
    );
    let (_, entry) = list.iter().find(|(n, _)| n == "word/document.xml").unwrap();
    assert_eq!(entry.method, METHOD_DEFLATED);
    assert_eq!(
        String::from_utf8(read(&copy, "word/document.xml")).unwrap(),
        fixed(&main, Paper::A4).unwrap()
    );
    // No data descriptor on the new record, and its local header agrees with
    // the directory.
    let record = local_record(&copy, "word/document.xml");
    assert_eq!(u16_at(record, 6) & 0x0008, 0);
    assert_eq!(u32_at(record, 18) as u64, entry.compressed);
    assert_eq!(u32_at(record, 22) as u64, entry.uncompressed);
    assert_eq!(
        u32_at(record, 14),
        crc32fast::hash(&read(&copy, "word/document.xml"))
    );
    // The time, the date and the extra block stay.
    assert_eq!(&record[10..14], &[0x21, 0x43, 0x65, 0x57]);
    assert_eq!(&record[30 + 17..30 + 21], &[0xCA, 0xFE, 0, 0]);
    // The pre-flight reads the copy.
    let dir = Scratch::new();
    let path = dir.path().join("copy.docx");
    std::fs::write(&path, &copy).unwrap();
    assert_eq!(
        crate::preflight::word_main_part(&path).as_deref(),
        Some("word/document.xml")
    );
}

#[test]
fn a_stored_main_part_stays_stored() {
    let source = crate::worker::warmup::docx("Stored.");
    let (result, copy) = repacked(&source, Paper::Letter);
    assert_eq!(result, Ok(true));
    let copy = copy.unwrap();
    let (_, entry) = entries(&copy)
        .into_iter()
        .find(|(n, _)| n == "word/document.xml")
        .unwrap();
    assert_eq!(entry.method, METHOD_STORED);
    let part = String::from_utf8(read(&copy, "word/document.xml")).unwrap();
    assert!(part.contains(r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr></w:body>"#));
    for name in ["[Content_Types].xml", "_rels/.rels"] {
        assert_eq!(local_record(&copy, name), local_record(&source, name));
    }
}

#[test]
fn a_sized_package_or_another_file_is_not_copied() {
    let sized = doc(r#"<w:p/><w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>"#);
    let source = zip(&[
        ("[Content_Types].xml", CONTENT_TYPES, true, false),
        ("_rels/.rels", RELS, true, false),
        ("word/document.xml", sized.as_bytes(), true, false),
    ]);
    assert_eq!(repacked(&source, Paper::A4), (Err(Skip::Sized), None));
    assert_eq!(
        repacked(b"{\\rtf1 hello}", Paper::A4),
        (Err(Skip::Unsupported), None)
    );
    // An OLE2 file, as an encrypted DOCX arrives.
    assert_eq!(
        repacked(
            &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0],
            Paper::A4
        ),
        (Err(Skip::Unsupported), None)
    );
    // A malformed main part: no copy either.
    let source = zip(&[
        ("[Content_Types].xml", CONTENT_TYPES, true, false),
        ("_rels/.rels", RELS, true, false),
        ("word/document.xml", b"<w:document><w:body>", true, false),
    ]);
    assert_eq!(repacked(&source, Paper::A4), (Err(Skip::Unreadable), None));
}

#[test]
fn the_copy_is_capped() {
    // A main part that compresses far better at the best level than at the
    // default: the copy outgrows nothing here, so the cap is checked with a
    // small one directly.
    let mut writer = Capped {
        inner: Vec::new(),
        written: 0,
        cap: 4,
    };
    assert!(writer.write_all(b"1234").is_ok());
    assert_eq!(
        writer.write_all(b"5").unwrap_err().kind(),
        io::ErrorKind::FileTooLarge
    );
    assert_eq!(copy_cap(10), 10 + MIB);
}

#[test]
fn word_packages_are_told_by_extension() {
    assert!(is_word_package(Path::new("/j/source.docx")));
    assert!(is_word_package(Path::new("/j/source.docm")));
    assert!(is_word_package(Path::new("/j/source.dotx")));
    assert!(!is_word_package(Path::new("/j/source.doc")));
    assert!(!is_word_package(Path::new("/j/source.rtf")));
    assert_eq!(
        copy_path(Path::new("/j/source.docx")).unwrap(),
        Path::new("/j/paper/source.docx")
    );
}
