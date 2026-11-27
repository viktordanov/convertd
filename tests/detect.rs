//! Detection from the bytes, with archives built here.

use std::io::{Cursor, Write};

use convertd::detect::detect_file;
use convertd::domain::detection::DetectError;
use convertd::domain::document::Format;
use flate2::Compression;
use flate2::write::DeflateEncoder;

/// One entry of a test archive.
struct Entry<'a> {
    name: &'a str,
    data: &'a [u8],
    deflate: bool,
    /// Sizes only in a data descriptor and the central directory, as Go's
    /// `archive/zip` writes them.
    descriptor: bool,
    /// Declared (compressed, uncompressed) sizes in the central directory,
    /// to fake a bomb without writing it.
    declared: Option<(u32, u32)>,
}

fn entry<'a>(name: &'a str, data: &'a [u8]) -> Entry<'a> {
    Entry {
        name,
        data,
        deflate: false,
        descriptor: false,
        declared: None,
    }
}

/// A minimal ZIP writer: local headers, data, central directory, EOCD.
fn zip(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for e in entries {
        let body = if e.deflate {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
            enc.write_all(e.data).unwrap();
            enc.finish().unwrap()
        } else {
            e.data.to_vec()
        };
        let crc = crc32fast::hash(e.data);
        let method: u16 = if e.deflate { 8 } else { 0 };
        let flags: u16 = if e.descriptor { 0x0008 } else { 0 };
        let (csize, usize_) = (body.len() as u32, e.data.len() as u32);
        let offset = out.len() as u32;
        let local_sizes = if e.descriptor {
            (0, 0, 0)
        } else {
            (crc, csize, usize_)
        };
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0; 4]); // time, date
        out.extend_from_slice(&local_sizes.0.to_le_bytes());
        out.extend_from_slice(&local_sizes.1.to_le_bytes());
        out.extend_from_slice(&local_sizes.2.to_le_bytes());
        out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(&body);
        if e.descriptor {
            out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&csize.to_le_bytes());
            out.extend_from_slice(&usize_.to_le_bytes());
        }
        let (dc, du) = e.declared.unwrap_or((csize, usize_));
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&flags.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&[0; 4]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&dc.to_le_bytes());
        central.extend_from_slice(&du.to_le_bytes());
        central.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]); // extra, comment, disk, attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(e.name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    let count = entries.len().min(0xFFFF) as u16;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn detect(bytes: &[u8]) -> Result<Format, DetectError> {
    detect_file(&mut Cursor::new(bytes), bytes.len() as u64).map(|d| d.format)
}

fn unsupported(bytes: &[u8]) -> &'static str {
    match detect(bytes) {
        Err(DetectError::Unsupported { label }) => label,
        other => panic!("expected unsupported, got {other:?}"),
    }
}

fn limit(bytes: &[u8]) -> &'static str {
    match detect(bytes) {
        Err(DetectError::LimitExceeded { limit }) => limit,
        other => panic!("expected a limit, got {other:?}"),
    }
}

fn content_types(main: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/main.xml" ContentType="{main}"/></Types>"#
    )
}

const OOXML: &[(&str, Format)] = &[
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        Format::Docx,
    ),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml",
        Format::Dotx,
    ),
    (
        "application/vnd.ms-word.document.macroEnabled.main+xml",
        Format::Docm,
    ),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        Format::Pptx,
    ),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml",
        Format::Ppsx,
    ),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml",
        Format::Potx,
    ),
    (
        "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml",
        Format::Pptm,
    ),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
        Format::Xlsx,
    ),
    (
        "application/vnd.ms-excel.sheet.macroEnabled.main+xml",
        Format::Xlsm,
    ),
];

#[test]
fn plain_signatures() {
    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&[0; 12]);
    bmp.extend_from_slice(&124u32.to_le_bytes());
    let cases: [(&[u8], Format); 2] = [
        (b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n1 0 obj", Format::Pdf),
        (b"{\\rtf1\\ansi\\deff0 hello}", Format::Rtf),
    ];
    for (bytes, want) in cases {
        assert_eq!(detect(bytes).unwrap(), want);
    }
    // Image inputs are out of v1: named by their signature, then refused.
    // A PNG that holds `%PDF-` later is still a PNG, never a PDF.
    let images: [(&[u8], &str); 8] = [
        (b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", "png"),
        (b"\x89PNG\r\n\x1a\n%PDF-1.4", "png"),
        (b"\xFF\xD8\xFF\xDB\0C", "jpeg"),
        (b"GIF87a\x01\0\x01\0", "gif"),
        (b"II*\0\x08\0\0\0", "tiff"),
        (b"RIFF\x1a\0\0\0WEBPVP8L", "webp"),
        (&bmp, "bmp"),
        (b"MM\0*\0\0\0\x08", "tiff"),
    ];
    for (bytes, label) in images {
        assert_eq!(unsupported(bytes), label);
    }
}

#[test]
fn every_ooxml_variant_stored_and_deflated() {
    for &(main, want) in OOXML {
        let types = content_types(main);
        for (deflate, descriptor) in [(false, false), (true, false), (true, true), (false, true)] {
            let bytes = zip(&[
                Entry {
                    deflate,
                    descriptor,
                    ..entry("[Content_Types].xml", types.as_bytes())
                },
                Entry {
                    deflate,
                    descriptor,
                    ..entry("main.xml", b"<w:document/>")
                },
            ]);
            assert_eq!(
                detect(&bytes).unwrap(),
                want,
                "{main} deflate={deflate} descriptor={descriptor}"
            );
        }
    }
}

#[test]
fn the_three_odf_types() {
    for (mimetype, want) in [
        ("application/vnd.oasis.opendocument.text", Format::Odt),
        (
            "application/vnd.oasis.opendocument.presentation",
            Format::Odp,
        ),
        (
            "application/vnd.oasis.opendocument.spreadsheet",
            Format::Ods,
        ),
    ] {
        let bytes = zip(&[
            entry("mimetype", mimetype.as_bytes()),
            entry("content.xml", b"<x/>"),
        ]);
        assert_eq!(detect(&bytes).unwrap(), want, "{mimetype}");
    }
    let base = zip(&[entry(
        "mimetype",
        b"application/vnd.oasis.opendocument.base",
    )]);
    assert_eq!(unsupported(&base), "odb");
    let deflated = zip(&[Entry {
        deflate: true,
        ..entry("mimetype", b"application/vnd.oasis.opendocument.text")
    }]);
    assert_eq!(unsupported(&deflated), "odf");
}

#[test]
fn bytes_decide_not_names() {
    // A DOCX whose entries say nothing of its name, and a PDF that a client
    // would call deck.pptx: there is no name to read.
    let types = content_types(OOXML[0].0);
    let docx = zip(&[entry("[Content_Types].xml", types.as_bytes())]);
    assert_eq!(detect(&docx).unwrap(), Format::Docx);
    assert_eq!(
        detect(b"%PDF-1.7 pretending to be a pptx").unwrap(),
        Format::Pdf
    );
}

#[test]
fn a_docx_that_mentions_pdf_is_a_docx() {
    let types = content_types(OOXML[0].0);
    let bytes = zip(&[
        entry("[Content_Types].xml", types.as_bytes()),
        entry("word/document.xml", b"<w:t>%PDF-1.7 is a file format</w:t>"),
    ]);
    assert!(memchr::memmem::find(&bytes[..bytes.len().min(1024)], b"%PDF-").is_some());
    assert_eq!(detect(&bytes).unwrap(), Format::Docx);
}

#[test]
fn ambiguous_and_unknown_archives() {
    let types = content_types(OOXML[0].0);
    let both = zip(&[
        entry("mimetype", b"application/vnd.oasis.opendocument.text"),
        entry("[Content_Types].xml", types.as_bytes()),
    ]);
    assert_eq!(unsupported(&both), "zip");
    assert_eq!(unsupported(&zip(&[entry("readme.txt", b"hi")])), "zip");
    let xlsb = content_types("application/vnd.ms-excel.sheet.binary.macroEnabled.main");
    assert_eq!(
        unsupported(&zip(&[entry("[Content_Types].xml", xlsb.as_bytes())])),
        "ooxml"
    );
}

#[test]
fn labels_for_what_is_refused() {
    assert_eq!(
        unsupported(b"<!DOCTYPE html>\n<html><body>hi</body></html>"),
        "html"
    );
    assert_eq!(unsupported(b"  <HTML>"), "html");
    assert_eq!(unsupported(b"<?xml version=\"1.0\"?><x/>"), "xml");
    assert_eq!(unsupported(b"just some text\n"), "text");
    assert_eq!(unsupported(&[0u8, 159, 146, 150, 7, 7, 7]), "unknown");
    assert_eq!(unsupported(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1rest"), "ole2");
}

#[test]
fn truncated_and_garbage_archives_are_unknown() {
    let types = content_types(OOXML[0].0);
    let good = zip(&[entry("[Content_Types].xml", types.as_bytes())]);
    // EOCD cut off.
    assert_eq!(unsupported(&good[..good.len() - 10]), "unknown");
    // Only the local header.
    assert_eq!(unsupported(&good[..40]), "unknown");
    // A ZIP signature followed by noise.
    let mut garbage = b"PK\x03\x04".to_vec();
    garbage.extend((0..5000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
    assert_eq!(unsupported(&garbage), "unknown");
    // A central directory that points past the end.
    let mut bad = good.clone();
    let at = bad.len() - 6;
    bad[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(unsupported(&bad), "unknown");
}

#[test]
fn too_many_entries() {
    let names: Vec<String> = (0..10_001).map(|i| format!("f{i}")).collect();
    let entries: Vec<Entry> = names.iter().map(|n| entry(n, b"")).collect();
    assert_eq!(limit(&zip(&entries)), "entry count");
    // 10 000 is allowed (and is just a ZIP).
    assert_eq!(unsupported(&zip(&entries[..10_000])), "zip");
}

#[test]
fn declared_expansion_over_two_gib() {
    let types = content_types(OOXML[0].0);
    let half = 1_610_612_736u32; // 1.5 GiB, twice
    let bytes = zip(&[
        entry("[Content_Types].xml", types.as_bytes()),
        Entry {
            declared: Some((half, half)),
            ..entry("a.bin", b"x")
        },
        Entry {
            declared: Some((half, half)),
            ..entry("b.bin", b"x")
        },
    ]);
    assert_eq!(limit(&bytes), "expanded size");
}

#[test]
fn declared_ratio_of_one_thousand() {
    let types = content_types(OOXML[0].0);
    let bomb = Entry {
        deflate: true,
        declared: Some((2_000, 2_000_000)),
        ..entry("bomb.xml", b"x")
    };
    let bytes = zip(&[entry("[Content_Types].xml", types.as_bytes()), bomb]);
    assert_eq!(limit(&bytes), "compression ratio");
    // A small, very repetitive part is fine.
    let zeros = vec![0u8; 512 * 1024];
    let small = Entry {
        deflate: true,
        ..entry("zeros.bin", &zeros)
    };
    let bytes = zip(&[entry("[Content_Types].xml", types.as_bytes()), small]);
    assert_eq!(detect(&bytes).unwrap(), Format::Docx);
}

#[test]
fn oversized_content_types_is_refused() {
    let mut types = content_types(OOXML[0].0).into_bytes();
    types.resize(1024 * 1024 + 1, b' ');
    // Stored: deflating this much padding would trip the ratio cap first.
    let bytes = zip(&[entry("[Content_Types].xml", &types)]);
    assert_eq!(limit(&bytes), "content types size");
}

/// An OLE2 file whose root storage holds these streams (and storages for
/// names ending in `/`).
fn ole2(names: &[&str]) -> Vec<u8> {
    let mut compound = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    for name in names {
        match name.strip_suffix('/') {
            Some(storage) => compound.create_storage(format!("/{storage}")).unwrap(),
            None => {
                let mut stream = compound.create_stream(format!("/{name}")).unwrap();
                stream.write_all(b"\x00stream bytes").unwrap();
            }
        }
    }
    compound.flush().unwrap();
    compound.into_inner().into_inner()
}

#[test]
fn legacy_office_from_its_streams() {
    for (names, want) in [
        (
            &["WordDocument", "1Table", "\u{5}SummaryInformation"][..],
            Format::Doc,
        ),
        (&["WordDocument", "Macros/"][..], Format::Doc),
        (&["Workbook"][..], Format::Xls),
        (&["Book"][..], Format::Xls),
        (&["PowerPoint Document", "Current User"][..], Format::Ppt),
    ] {
        let detection = detect_file(&mut Cursor::new(ole2(names)), 0).unwrap();
        assert_eq!(detection.format, want, "{names:?}");
        assert!(!detection.encrypted);
    }
}

#[test]
fn encrypted_ooxml_is_found_without_its_type() {
    let bytes = ole2(&["EncryptionInfo", "EncryptedPackage", "\u{6}DataSpaces/"]);
    let detection = detect_file(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap();
    assert!(detection.encrypted);
    // Provisional: the engine's document type decides the real one.
    assert_eq!(detection.format, Format::Docx);
    // One of the two streams alone is not encrypted OOXML.
    assert_eq!(unsupported(&ole2(&["EncryptedPackage"])), "ole2");
}

#[test]
fn refused_ole2_files() {
    assert_eq!(
        unsupported(&ole2(&["__substg1.0_0037001F", "__properties_version1.0"])),
        "msg"
    );
    assert_eq!(unsupported(&ole2(&["Contents", "Equation Native"])), "ole2");
    assert_eq!(unsupported(&ole2(&[])), "ole2");
}

#[test]
fn broken_ole2_is_refused_not_a_panic() {
    let good = ole2(&["WordDocument"]);
    // The signature alone, a truncated file, and noise after the header.
    let mut noise = good[..512].to_vec();
    noise.extend((0..8192u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8));
    for bytes in [
        &good[..8],
        &good[..600],
        &noise[..],
        &good[..good.len() / 2],
    ] {
        assert_eq!(unsupported(bytes), "ole2", "{} bytes", bytes.len());
    }
    // Flipped bytes across the whole header and directory.
    for at in (8..good.len().min(4096)).step_by(7) {
        let mut flipped = good.clone();
        flipped[at] ^= 0xFF;
        let _ = detect(&flipped);
    }
}

/// The bounded reader follows version 3 (512-byte sectors) as well as 4,
/// and looks only at the root's own children: a `WordDocument` embedded in
/// a sub-storage of a workbook does not make it a `doc`.
#[test]
fn ole2_version_3_and_nested_names() {
    let mut v3 =
        cfb::CompoundFile::create_with_version(cfb::Version::V3, Cursor::new(Vec::new())).unwrap();
    v3.create_stream("/WordDocument")
        .unwrap()
        .write_all(&vec![7u8; 6000])
        .unwrap();
    let v3 = v3.into_inner().into_inner();
    assert_eq!(
        detect_file(&mut Cursor::new(&v3), v3.len() as u64)
            .unwrap()
            .format,
        Format::Doc
    );

    let mut nested = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    nested
        .create_stream("/Workbook")
        .unwrap()
        .write_all(b"book")
        .unwrap();
    nested.create_storage("/MBD0001").unwrap();
    nested
        .create_stream("/MBD0001/WordDocument")
        .unwrap()
        .write_all(b"embedded")
        .unwrap();
    let nested = nested.into_inner().into_inner();
    assert_eq!(
        detect_file(&mut Cursor::new(&nested), nested.len() as u64)
            .unwrap()
            .format,
        Format::Xls
    );
}

/// The bounded reader against `cfb` on compound files that Office wrote,
/// not ones `cfb` made: both must give the same root names.
#[test]
fn ole2_root_names_match_cfb_on_real_files() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/fixtures");
    let mut checked = 0;
    for name in [
        "vbaProject-word.bin",
        "vbaProject-xl.bin",
        "vbaProject-ppt.bin",
    ] {
        let bytes = std::fs::read(fixtures.join(name)).unwrap();
        let mut ours = convertd::detect::ole2::root_names(&mut Cursor::new(&bytes))
            .unwrap_or_else(|| panic!("{name}: the reader could not follow it"));
        let compound = cfb::CompoundFile::open(Cursor::new(&bytes)).unwrap();
        let mut theirs: Vec<String> = compound
            .read_root_storage()
            .map(|entry| entry.name().to_owned())
            .collect();
        ours.sort();
        theirs.sort();
        assert_eq!(ours, theirs, "{name}");
        checked += 1;
    }
    assert_eq!(checked, 3);
}
