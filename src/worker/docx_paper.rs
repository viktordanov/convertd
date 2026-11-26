//! `DEFAULT_PAPER` for a Word package whose sections name no page size.
//!
//! LibreOffice 26.8.3's DOCX importer starts every section on Letter
//! (`sw/source/writerfilter/dmapper/PropertyMap.cxx`, `SectionPropertyMap`)
//! and reads no locale, so `LC_PAPER` cannot reach it. Before the load, the
//! office worker therefore gives each section without a `w:pgSz` one with
//! the `DEFAULT_PAPER` size, in a copy of the package:
//!
//! - the sections are the `w:sectPr` of the body (the last section) and of
//!   a paragraph's `w:pPr` (a section break). A body without one gets one;
//! - `w:pgSz` goes where the schema puts it: after the header and footer
//!   references, `w:footnotePr`, `w:endnotePr` and `w:type`;
//! - a section that has a `w:pgSz` keeps it, and a package in which every
//!   section has one is not copied;
//! - only the main part changes, with its own compression method. Every
//!   other entry is copied byte for byte, data descriptors included, and
//!   the central directory keeps its order.
//!
//! Bounds, as in the pre-flight: the main part is read under the
//! pre-flight's per-request cap, the archive under detection's caps, and
//! the copy may be at most [`copy_cap`] bytes, which the supervisor
//! reserves. A package outside them (ZIP64, encrypted entries, a part in
//! another encoding than UTF-8, deep nesting) is loaded as it came, on
//! Letter. Nothing here panics or fails the job: the worst case is the
//! original file.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::DeflateEncoder;
use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{Namespace, ResolveResult};

use crate::detect::zip_scan::{self, METHOD_DEFLATED, METHOD_STORED};
use crate::domain::limits::MIB;
use crate::engines::config::Paper;

const W_NS: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const W_STRICT_NS: &[u8] = b"http://purl.oclc.org/ooxml/wordprocessingml/main";

/// Inflated bytes of the main part that are read, the pre-flight's
/// per-request cap.
pub const MAIN_PART_CAP: u64 = crate::preflight::REQUEST_CAP;
/// Bytes of central directory that are read.
const DIRECTORY_CAP: u64 = 16 * MIB;
/// Elements open at once.
const MAX_DEPTH: usize = 4096;
/// The `w:sectPr` children that come before `w:pgSz` in the schema.
const BEFORE_PAGE_SIZE: [&[u8]; 5] = [
    b"headerReference",
    b"footerReference",
    b"footnotePr",
    b"endnotePr",
    b"type",
];

/// The source extensions whose packages LibreOffice reads with its DOCX
/// importer: the detected format's canonical extension.
pub const WORD_EXTENSIONS: [&str; 3] = ["docx", "docm", "dotx"];

/// The bytes the copy of a `source_bytes` package may take: the source
/// plus 1 MiB, for the new `w:pgSz` elements and a main part that
/// compresses a little worse than before. The supervisor reserves it.
pub const fn copy_cap(source_bytes: u64) -> u64 {
    crate::domain::limits::paper_copy_bytes(source_bytes)
}

/// A replacement of the main part's bytes `at..end` (inflated offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub at: u64,
    pub end: u64,
    pub text: Vec<u8>,
}

/// Why a package is loaded as it came.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Every section names its page size: nothing to do.
    Sized,
    /// Not a ZIP package this code can rewrite (ZIP64, an encrypted or
    /// unusually compressed main part, an odd layout).
    Unsupported,
    /// The main part is not well-formed UTF-8 XML, or over a cap.
    Unreadable,
}

/// The namespace of a Word element, when it is in one of the two.
fn word_ns(ns: &ResolveResult<'_>) -> Option<&'static [u8]> {
    match ns {
        ResolveResult::Bound(Namespace(n)) if *n == W_NS => Some(W_NS),
        ResolveResult::Bound(Namespace(n)) if *n == W_STRICT_NS => Some(W_STRICT_NS),
        _ => None,
    }
}

/// `<p:pgSz p:w=".." p:h=".."/>` for an element written with `prefix` in
/// namespace `ns`. Without a prefix, the attributes need one of their own.
fn page_size(prefix: &[u8], ns: &[u8], paper: Paper) -> Vec<u8> {
    let (w, h) = paper.twips();
    let mut out = Vec::with_capacity(160);
    if prefix.is_empty() {
        out.extend_from_slice(b"<pgSz xmlns:rscp=\"");
        out.extend_from_slice(ns);
        out.extend_from_slice(format!("\" rscp:w=\"{w}\" rscp:h=\"{h}\"/>").as_bytes());
    } else {
        let p = prefix;
        out.extend_from_slice(b"<");
        out.extend_from_slice(p);
        out.extend_from_slice(b":pgSz ");
        out.extend_from_slice(p);
        out.extend_from_slice(format!(":w=\"{w}\" ").as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(format!(":h=\"{h}\"/>").as_bytes());
    }
    out
}

fn qualified(prefix: &[u8], local: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    if !prefix.is_empty() {
        out.push(b':');
    }
    out.extend_from_slice(local);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Body,
    P,
    PPr,
    SectPr,
    Other,
}

/// The `w:sectPr` being read.
struct Section {
    /// Elements open outside it.
    depth: usize,
    prefix: Vec<u8>,
    ns: &'static [u8],
    sized: bool,
    /// Before the first child that comes after `w:pgSz` in the schema.
    insert_at: Option<u64>,
}

/// The edits that give every section a page size, from the main part's
/// inflated bytes. At most `cap` bytes are read.
pub fn plan(part: impl BufRead, paper: Paper, cap: u64) -> Result<Vec<Edit>, Skip> {
    let mut part = part.take(cap.saturating_add(1));
    // quick-xml drops a UTF-8 byte order mark without counting it in its
    // positions, so it never sees one: `skipped` puts it back.
    let mut head = [0u8; 3];
    let mut read = 0;
    while read < head.len() {
        match part.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return Err(Skip::Unreadable),
        }
    }
    let (skipped, head) = if head == *b"\xEF\xBB\xBF" {
        (3u64, &head[..0])
    } else {
        (0, &head[..read])
    };
    let mut xml = NsReader::from_reader(head.chain(part));
    let mut buf = Vec::with_capacity(4096);
    let mut stack: Vec<Tag> = Vec::with_capacity(64);
    let mut edits = Vec::new();
    let mut section: Option<Section> = None;
    // The body: its depth, prefix and namespace.
    let mut body: Option<(usize, Vec<u8>, &'static [u8])> = None;
    let mut body_has_section = false;
    let mut before = skipped + xml.buffer_position();
    loop {
        let (ns, event) = xml
            .read_resolved_event_into(&mut buf)
            .map_err(|_| Skip::Unreadable)?;
        let ns = word_ns(&ns);
        let after = skipped + xml.buffer_position();
        if after > cap {
            return Err(Skip::Unreadable);
        }
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                let local = e.local_name();
                let local = local.as_ref();
                let prefix = e
                    .name()
                    .prefix()
                    .map(|p| p.as_ref().to_vec())
                    .unwrap_or_default();
                let tag = match (ns, local) {
                    (Some(_), b"body") => Tag::Body,
                    (Some(_), b"p") => Tag::P,
                    (Some(_), b"pPr") => Tag::PPr,
                    (Some(_), b"sectPr") => Tag::SectPr,
                    _ => Tag::Other,
                };
                let depth = stack.len();
                if let Some(s) = section.as_mut()
                    && depth == s.depth + 1
                {
                    if ns.is_some() && local == b"pgSz" {
                        s.sized = true;
                    } else if s.insert_at.is_none()
                        && !(ns.is_some() && BEFORE_PAGE_SIZE.contains(&local))
                    {
                        s.insert_at = Some(before);
                    }
                }
                let parent = stack.last().copied();
                let grandparent = depth.checked_sub(2).map(|i| stack[i]);
                let hosted = match parent {
                    Some(Tag::Body) => {
                        body_has_section |= tag == Tag::SectPr;
                        true
                    }
                    Some(Tag::PPr) => grandparent == Some(Tag::P),
                    _ => false,
                };
                if tag == Tag::SectPr
                    && hosted
                    && section.is_none()
                    && let Some(ns) = ns
                {
                    if empty {
                        edits.push(Edit {
                            at: before,
                            end: after,
                            text: expanded(e, &prefix, ns, paper),
                        });
                    } else {
                        section = Some(Section {
                            depth,
                            prefix: prefix.clone(),
                            ns,
                            sized: false,
                            insert_at: None,
                        });
                    }
                }
                if tag == Tag::Body
                    && body.is_none()
                    && let Some(ns) = ns
                {
                    if empty {
                        // <w:body/>: nothing to lay out, nothing to size.
                    } else {
                        body = Some((depth, prefix, ns));
                    }
                }
                if !empty {
                    stack.push(tag);
                    if stack.len() > MAX_DEPTH {
                        return Err(Skip::Unreadable);
                    }
                }
            }
            Event::End(_) => {
                let closed = stack.pop();
                let depth = stack.len();
                if let Some(s) = section.take_if(|s| s.depth == depth)
                    && !s.sized
                {
                    let at = s.insert_at.unwrap_or(before);
                    edits.push(Edit {
                        at,
                        end: at,
                        text: page_size(&s.prefix, s.ns, paper),
                    });
                }
                if closed == Some(Tag::Body)
                    && let Some((body_depth, prefix, ns)) = &body
                    && *body_depth == depth
                {
                    if !body_has_section {
                        let mut text = b"<".to_vec();
                        text.extend(qualified(prefix, b"sectPr"));
                        text.push(b'>');
                        text.extend(page_size(prefix, ns, paper));
                        text.extend_from_slice(b"</");
                        text.extend(qualified(prefix, b"sectPr"));
                        text.push(b'>');
                        edits.push(Edit {
                            at: before,
                            end: before,
                            text,
                        });
                    }
                    // Only the first body counts.
                    body_has_section = true;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
        before = after;
    }
    if !stack.is_empty() {
        return Err(Skip::Unreadable);
    }
    Ok(edits)
}

/// `<w:sectPr …/>` as `<w:sectPr …><w:pgSz …/></w:sectPr>`.
fn expanded(e: &BytesStart<'_>, prefix: &[u8], ns: &[u8], paper: Paper) -> Vec<u8> {
    let raw: &[u8] = e;
    let raw = raw.trim_ascii_end();
    let mut text = Vec::with_capacity(raw.len() + 200);
    text.push(b'<');
    text.extend_from_slice(raw);
    text.push(b'>');
    text.extend(page_size(prefix, ns, paper));
    text.extend_from_slice(b"</");
    text.extend(qualified(prefix, b"sectPr"));
    text.push(b'>');
    text
}

/// Writes `part` with `edits` applied. The edits are sorted and do not
/// overlap ([`plan`] makes them so).
pub fn apply(mut part: impl Read, edits: &[Edit], out: &mut impl Write) -> io::Result<()> {
    let mut at = 0u64;
    for edit in edits {
        if edit.at < at || edit.end < edit.at {
            return Err(io::Error::other("edits out of order"));
        }
        let copied = io::copy(&mut (&mut part).take(edit.at - at), out)?;
        if copied != edit.at - at {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        out.write_all(&edit.text)?;
        let skipped = io::copy(&mut (&mut part).take(edit.end - edit.at), &mut io::sink())?;
        if skipped != edit.end - edit.at {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        at = edit.end;
    }
    io::copy(&mut part, out)?;
    Ok(())
}

/// `xml` with every section sized, or `None` when nothing changes. For
/// tests and for callers that hold the part in memory.
pub fn fix_part(xml: &[u8], paper: Paper) -> Result<Option<Vec<u8>>, Skip> {
    let edits = plan(xml, paper, MAIN_PART_CAP)?;
    if edits.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(xml.len() + 256);
    apply(xml, &edits, &mut out).map_err(|_| Skip::Unreadable)?;
    Ok(Some(out))
}

/// One central-directory record, as found in the directory bytes.
struct Record {
    /// Its bytes in the directory.
    start: usize,
    len: usize,
    flags: u16,
    method: u16,
    local_offset: u64,
    is_main: bool,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// The records of a central directory, or `None` when one is malformed or
/// uses ZIP64 fields.
fn records(directory: &[u8], entries: u64, main: &str) -> Option<Vec<Record>> {
    let mut out = Vec::with_capacity(usize::try_from(entries).ok()?);
    let mut at = 0usize;
    for _ in 0..entries {
        let header = directory.get(at..at + 46)?;
        if u32_at(header, 0) != 0x0201_4b50 {
            return None;
        }
        let compressed = u32_at(header, 20);
        let uncompressed = u32_at(header, 24);
        let local_offset = u32_at(header, 42);
        if [compressed, uncompressed, local_offset].contains(&0xFFFF_FFFF) {
            return None;
        }
        let name_len = usize::from(u16_at(header, 28));
        let len = 46 + name_len + usize::from(u16_at(header, 30)) + usize::from(u16_at(header, 32));
        let name = directory.get(at + 46..at + 46 + name_len)?;
        directory.get(at..at + len)?;
        out.push(Record {
            start: at,
            len,
            flags: u16_at(header, 8),
            method: u16_at(header, 10),
            local_offset: u64::from(local_offset),
            is_main: name == main.as_bytes(),
        });
        at += len;
    }
    Some(out)
}

/// Counts what goes through and fails past `cap`.
struct Capped<W> {
    inner: W,
    written: u64,
    cap: u64,
}

impl<W: Write> Write for Capped<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(buf.len() as u64) > self.cap {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Counts the inflated bytes and their CRC-32.
struct Crc<W> {
    inner: W,
    crc: crc32fast::Hasher,
    len: u64,
}

impl<W: Write> Write for Crc<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Writes a copy of the Word package `source` to `out` in which every
/// section names a page size of `paper`. `Ok(true)` when the copy was
/// written; `Ok(false)` or an error leaves no file at `out`, and the
/// caller loads `source`.
pub fn repack(source: &Path, out: &Path, paper: Paper) -> Result<bool, Skip> {
    let result = write_copy(source, out, paper);
    if !matches!(result, Ok(true)) {
        let _ = std::fs::remove_file(out);
    }
    result
}

fn write_copy(source: &Path, out: &Path, paper: Paper) -> Result<bool, Skip> {
    let unsupported = |_| Skip::Unsupported;
    let mut file = File::open(source).map_err(|_| Skip::Unsupported)?;
    let size = file.metadata().map_err(|_| Skip::Unsupported)?.len();
    let mut head = [0u8; 4];
    if file.read_exact(&mut head).is_err() || head != *b"PK\x03\x04" {
        return Err(Skip::Unsupported);
    }
    let main = crate::preflight::word_main_part(source).ok_or(Skip::Unsupported)?;
    let entries = zip_scan::list(&mut file, size).map_err(|_| Skip::Unsupported)?;
    let entry = entries
        .iter()
        .find(|(name, _)| *name == main)
        .map(|(_, entry)| *entry)
        .ok_or(Skip::Unsupported)?;
    if entry.method != METHOD_STORED && entry.method != METHOD_DEFLATED {
        return Err(Skip::Unsupported);
    }

    // The edits, from a first pass over the main part.
    let edits = {
        let reader = zip_scan::open_entry(&mut file, &entry)
            .map_err(|_| Skip::Unreadable)?
            .ok_or(Skip::Unsupported)?;
        plan(
            BufReader::with_capacity(64 * 1024, reader),
            paper,
            MAIN_PART_CAP,
        )?
    };
    if edits.is_empty() {
        return Err(Skip::Sized);
    }

    // The central directory, whole: at most DIRECTORY_CAP bytes.
    let (count, cd_offset, cd_size) =
        zip_scan::directory(&mut file, size).map_err(|_| Skip::Unsupported)?;
    if cd_size > DIRECTORY_CAP || cd_offset > u64::from(u32::MAX) {
        return Err(Skip::Unsupported);
    }
    let mut directory = vec![0u8; cd_size as usize];
    file.seek(SeekFrom::Start(cd_offset)).map_err(unsupported)?;
    file.read_exact(&mut directory).map_err(unsupported)?;
    let records = records(&directory, count, &main).ok_or(Skip::Unsupported)?;
    if records.iter().filter(|r| r.is_main).count() != 1
        || records.iter().any(|r| r.is_main && r.flags & 1 != 0)
    {
        return Err(Skip::Unsupported);
    }
    // Each entry's local record runs to the next one, the last to the
    // directory: the first starts at 0 and none overlap.
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by_key(|&i| records[i].local_offset);
    if order.first().is_none_or(|&i| records[i].local_offset != 0)
        || order
            .windows(2)
            .any(|w| records[w[0]].local_offset >= records[w[1]].local_offset)
        || order
            .last()
            .is_some_and(|&i| records[i].local_offset >= cd_offset)
    {
        return Err(Skip::Unsupported);
    }

    let target = File::create_new(out).map_err(unsupported)?;
    let mut writer = Capped {
        inner: BufWriter::with_capacity(64 * 1024, target),
        written: 0,
        cap: copy_cap(size).min(u64::from(u32::MAX)),
    };
    let mut new_offsets = vec![0u64; records.len()];
    let mut main_fields = None;
    for (k, &i) in order.iter().enumerate() {
        let record = &records[i];
        let end = order
            .get(k + 1)
            .map_or(cd_offset, |&j| records[j].local_offset);
        new_offsets[i] = writer.written;
        if record.is_main {
            main_fields = Some(write_main(&mut file, &mut writer, record, &entry, &edits)?);
        } else {
            file.seek(SeekFrom::Start(record.local_offset))
                .map_err(unsupported)?;
            let span = end - record.local_offset;
            let copied = io::copy(&mut (&mut file).take(span), &mut writer).map_err(unsupported)?;
            if copied != span {
                return Err(Skip::Unsupported);
            }
        }
    }
    let (crc, compressed, uncompressed) = main_fields.ok_or(Skip::Unsupported)?;

    let new_cd_offset = writer.written;
    for (record, &offset) in records.iter().zip(&new_offsets) {
        let mut bytes = directory[record.start..record.start + record.len].to_vec();
        let offset = u32::try_from(offset).map_err(|_| Skip::Unsupported)?;
        put_u32(&mut bytes, 42, offset);
        if record.is_main {
            put_u16(&mut bytes, 8, record.flags & !0x0008);
            put_u32(&mut bytes, 16, crc);
            put_u32(&mut bytes, 20, compressed);
            put_u32(&mut bytes, 24, uncompressed);
        }
        writer.write_all(&bytes).map_err(unsupported)?;
    }
    let new_cd_size = writer.written - new_cd_offset;
    let count = u16::try_from(records.len()).map_err(|_| Skip::Unsupported)?;
    let mut eocd = [0u8; 22];
    put_u32(&mut eocd, 0, 0x0605_4b50);
    put_u16(&mut eocd, 8, count);
    put_u16(&mut eocd, 10, count);
    put_u32(
        &mut eocd,
        12,
        u32::try_from(new_cd_size).map_err(|_| Skip::Unsupported)?,
    );
    put_u32(
        &mut eocd,
        16,
        u32::try_from(new_cd_offset).map_err(|_| Skip::Unsupported)?,
    );
    writer.write_all(&eocd).map_err(unsupported)?;
    writer.flush().map_err(unsupported)?;
    Ok(true)
}

/// Writes the main part's local record: its header with the new CRC and
/// sizes and no data descriptor, its name and extra field as they were,
/// and the edited content with its own method. Returns the CRC and sizes.
fn write_main(
    file: &mut File,
    writer: &mut Capped<BufWriter<File>>,
    record: &Record,
    entry: &zip_scan::Entry,
    edits: &[Edit],
) -> Result<(u32, u32, u32), Skip> {
    let unsupported = |_| Skip::Unsupported;
    file.seek(SeekFrom::Start(record.local_offset))
        .map_err(unsupported)?;
    let mut header = [0u8; 30];
    file.read_exact(&mut header).map_err(unsupported)?;
    if u32_at(&header, 0) != 0x0403_4b50 || u16_at(&header, 8) != record.method {
        return Err(Skip::Unsupported);
    }
    let mut name_extra =
        vec![0u8; usize::from(u16_at(&header, 26)) + usize::from(u16_at(&header, 28))];
    file.read_exact(&mut name_extra).map_err(unsupported)?;
    let extra = &name_extra[usize::from(u16_at(&header, 26))..];
    let mut at = 0;
    while at + 4 <= extra.len() {
        if u16_at(extra, at) == 0x0001 {
            return Err(Skip::Unsupported);
        }
        at += 4 + usize::from(u16_at(extra, at + 2));
    }

    // The header goes first with zeros, and is written again at the end.
    let header_at = writer.written;
    writer.write_all(&header).map_err(unsupported)?;
    writer.write_all(&name_extra).map_err(unsupported)?;
    let data_at = writer.written;
    let (crc, uncompressed) = {
        let part = zip_scan::open_entry(file, entry)
            .map_err(|_| Skip::Unreadable)?
            .ok_or(Skip::Unsupported)?;
        let part = part.take(MAIN_PART_CAP.saturating_add(1));
        match record.method {
            METHOD_DEFLATED => {
                let mut sink = Crc {
                    inner: DeflateEncoder::new(&mut *writer, Compression::default()),
                    crc: crc32fast::Hasher::new(),
                    len: 0,
                };
                apply(part, edits, &mut sink).map_err(|_| Skip::Unreadable)?;
                let (crc, len) = (sink.crc.finalize(), sink.len);
                sink.inner.finish().map_err(unsupported)?;
                (crc, len)
            }
            _ => {
                let mut sink = Crc {
                    inner: &mut *writer,
                    crc: crc32fast::Hasher::new(),
                    len: 0,
                };
                apply(part, edits, &mut sink).map_err(|_| Skip::Unreadable)?;
                (sink.crc.finalize(), sink.len)
            }
        }
    };
    let compressed = writer.written - data_at;
    let compressed = u32::try_from(compressed).map_err(|_| Skip::Unsupported)?;
    let uncompressed = u32::try_from(uncompressed).map_err(|_| Skip::Unsupported)?;
    let flags = u16_at(&header, 6) & !0x0008;
    put_u16(&mut header, 6, flags);
    put_u32(&mut header, 14, crc);
    put_u32(&mut header, 18, compressed);
    put_u32(&mut header, 22, uncompressed);
    let end = writer.written;
    let inner = &mut writer.inner;
    inner
        .seek(SeekFrom::Start(header_at))
        .map_err(unsupported)?;
    inner.write_all(&header).map_err(unsupported)?;
    inner.seek(SeekFrom::Start(end)).map_err(unsupported)?;
    Ok((crc, compressed, uncompressed))
}

/// Where the repacked copy of `source` goes: a `paper` directory next to
/// it, under the same file name, so LibreOffice sees the same name.
pub fn copy_path(source: &Path) -> Option<PathBuf> {
    Some(source.parent()?.join("paper").join(source.file_name()?))
}

/// Whether `source` is a package that LibreOffice reads with its DOCX
/// importer, from the detected format's extension.
pub fn is_word_package(source: &Path) -> bool {
    source
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| WORD_EXTENSIONS.contains(&e))
}

#[cfg(test)]
mod tests;
