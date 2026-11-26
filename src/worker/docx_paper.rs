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
//! - a package in which any section has a `w:pgSz` is not copied: its
//!   other sections may take their size from that one (a continuous
//!   break), so adding sizes there would be a guess;
//! - only the main part changes, with its own compression method. Every
//!   other entry is copied byte for byte, data descriptors included, and
//!   the central directory keeps its order.
//!
//! Only `DEFAULT_PAPER=A4` needs it: Letter is the importer's own default,
//! so with Letter there is no copy and no reservation.
//!
//! Bounds: the main part is inflated at most twice, a plan and a rewrite,
//! each under [`MAIN_PART_CAP`] (the size of the pre-flight's per-request
//! cap, apart from it). The archive is read under detection's caps, the
//! plan holds at most [`MAX_EDITS`] edits, and the copy may be at most
//! [`copy_cap`] bytes, which the supervisor reserves. A package outside
//! them (ZIP64, encrypted entries, a part in another encoding than UTF-8,
//! deep nesting, a section in a markup compatibility branch) is loaded as
//! it came, on Letter. Nothing here panics. Only a failed write of the copy
//! ([`Skip::Io`]) fails the job, so that a busy disk never changes the
//! paper.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::DeflateEncoder;
use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::{Namespace, ResolveResult};

use crate::detect::zip_scan::{self, METHOD_DEFLATED, METHOD_STORED};
use crate::domain::limits::MIB;
use crate::engines::config::Paper;

const W_NS: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const W_STRICT_NS: &[u8] = b"http://purl.oclc.org/ooxml/wordprocessingml/main";
const MC_NS: &[u8] = b"http://schemas.openxmlformats.org/markup-compatibility/2006";

/// Inflated bytes of the main part that are read, the pre-flight's
/// per-request cap.
pub const MAIN_PART_CAP: u64 = crate::preflight::REQUEST_CAP;
/// Bytes of central directory that are read.
const DIRECTORY_CAP: u64 = 16 * MIB;
/// Elements open at once.
const MAX_DEPTH: usize = 4096;
/// Namespace declarations in scope at once. A Word main part declares
/// about 40, all on its root.
const MAX_BINDINGS: usize = 512;
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

/// A replacement of the main part's bytes `at..end` (inflated offsets) by
/// `Plan::texts[text]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit {
    pub at: u64,
    pub end: u64,
    pub text: u32,
}

/// The edits that size every section. The texts are shared, so the plan of
/// a part with many sections stays small.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    pub edits: Vec<Edit>,
    pub texts: Vec<Vec<u8>>,
}

/// At most this many edits: a part with more sections loads as it came.
pub const MAX_EDITS: usize = 100_000;
/// At most this many distinct texts (one per prefix and kind of edit).
const MAX_TEXTS: usize = 16;
/// The longest prefix that is copied into a text.
const MAX_PREFIX: usize = 64;

impl Plan {
    fn push(&mut self, at: u64, end: u64, text: Vec<u8>) -> Result<(), Skip> {
        if self.edits.len() >= MAX_EDITS {
            return Err(Skip::Unsupported);
        }
        let id = match self.texts.iter().position(|t| *t == text) {
            Some(id) => id,
            None if self.texts.len() < MAX_TEXTS => {
                self.texts.push(text);
                self.texts.len() - 1
            }
            None => return Err(Skip::Unsupported),
        };
        self.edits.push(Edit {
            at,
            end,
            text: id as u32,
        });
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }
}

/// Why a package is loaded as it came, or why its copy failed.
#[derive(Debug)]
pub enum Skip {
    /// Every section names its page size: nothing to do.
    Sized,
    /// Not a package this code rewrites: ZIP64, an encrypted or unusually
    /// compressed main part, an odd layout, a section in a markup
    /// compatibility branch, more than [`MAX_EDITS`] sections, or a copy
    /// over its cap.
    Unsupported,
    /// The main part is not well-formed UTF-8 XML, or over a cap.
    Unreadable,
    /// Writing the copy failed (a full disk, for example). Not a property of
    /// the document: the job fails, so that the paper never depends on how
    /// busy the disk was.
    Io(io::Error),
}

impl PartialEq for Skip {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Skip::Io(a), Skip::Io(b)) => a.kind() == b.kind(),
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

/// A failed write of the copy: [`Skip::Io`], except at the copy's own cap.
fn written(err: io::Error) -> Skip {
    if err.raw_os_error().is_none() && err.kind() == io::ErrorKind::FileTooLarge {
        Skip::Unsupported
    } else {
        Skip::Io(err)
    }
}

/// A failed read of the source: [`Skip::Io`] for an error of the file
/// system (so that a transient one fails the job instead of changing the
/// paper), anything else the package's fault.
fn source_io(err: io::Error) -> Skip {
    if err.raw_os_error().is_some() {
        Skip::Io(err)
    } else {
        Skip::Unsupported
    }
}

fn scan_error(err: zip_scan::ScanError) -> Skip {
    match err {
        zip_scan::ScanError::Io(err) => source_io(err),
        _ => Skip::Unsupported,
    }
}

/// A failed parse: an error of the file system below it is [`Skip::Io`].
fn xml_error(err: quick_xml::Error) -> Skip {
    match err {
        quick_xml::Error::Io(err) => match err.raw_os_error() {
            Some(code) => Skip::Io(io::Error::from_raw_os_error(code)),
            None => Skip::Unreadable,
        },
        _ => Skip::Unreadable,
    }
}

/// A failed rewrite of the main part: a write error is [`Skip::Io`] (or
/// the cap, as in [`written`]); anything else is the inflated part's fault.
fn applied(err: io::Error) -> Skip {
    if err.raw_os_error().is_some() || err.kind() == io::ErrorKind::FileTooLarge {
        written(err)
    } else {
        Skip::Unreadable
    }
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

/// `<p:sectPr><p:pgSz …/></p:sectPr>`.
fn section(prefix: &[u8], ns: &[u8], paper: Paper) -> Vec<u8> {
    let mut text = b"<".to_vec();
    text.extend(qualified(prefix, b"sectPr"));
    text.push(b'>');
    text.extend(page_size(prefix, ns, paper));
    text.extend(closing(prefix, b"sectPr"));
    text
}

fn closing(prefix: &[u8], local: &[u8]) -> Vec<u8> {
    let mut text = b"</".to_vec();
    text.extend(qualified(prefix, local));
    text.push(b'>');
    text
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
    Document,
    Body,
    P,
    PPr,
    SectPr,
    SectPrChange,
    /// `mc:AlternateContent`, by local name.
    Alternate,
    /// An element in no Word namespace: an extension, or a drawing's markup.
    Foreign,
    Other,
}

/// The `w:sectPr` being read.
struct Section {
    /// Elements open outside it.
    depth: usize,
    prefix: Vec<u8>,
    ns: &'static [u8],
    /// After the last child that comes before `w:pgSz` in the schema, or
    /// after the start tag. Other children (extensions that a reader may
    /// ignore) do not move it, so the schema order holds either way.
    insert_at: u64,
    /// A child that comes before `w:pgSz` is open.
    in_before: bool,
}

/// The edits that give every section a page size, from the main part's
/// inflated bytes. At most `cap` bytes are read.
///
/// A `w:sectPr` anywhere but in the body, in a paragraph's `w:pPr` or in a
/// `w:sectPrChange`, a `w:sectPr` or `w:body` inside an
/// `mc:AlternateContent`, and an `mc:AlternateContent` inside a section,
/// make the section that LibreOffice uses ambiguous: the part loads as it
/// came.
pub fn plan(part: impl BufRead, paper: Paper, cap: u64) -> Result<Plan, Skip> {
    // Offsets are only safe in UTF-8: every byte is checked.
    let mut part = Utf8Reader {
        inner: part.take(cap.saturating_add(1)),
        carry: Vec::with_capacity(8),
        checked: Vec::new(),
    };
    // quick-xml drops a UTF-8 byte order mark without counting it in its
    // positions, so it never sees one: `skipped` puts it back.
    let mut head = [0u8; 3];
    let mut read = 0;
    while read < head.len() {
        match part.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(err) if err.raw_os_error().is_some() => return Err(Skip::Io(err)),
            Err(_) => return Err(Skip::Unreadable),
        }
    }
    let (skipped, head) = if head == *b"\xEF\xBB\xBF" {
        (3u64, &head[..0])
    } else {
        (0, &head[..read])
    };
    let mut xml = NsReader::from_reader(head.chain(BufReader::with_capacity(64 * 1024, part)));
    // The total in scope is checked below; one element may use all of it.
    xml.resolver_mut()
        .set_max_declarations_per_element(MAX_BINDINGS);
    let mut buf = Vec::with_capacity(4096);
    let mut stack: Vec<Tag> = Vec::with_capacity(64);
    // Namespace declarations per open element, and in scope in all:
    // quick-xml looks a prefix up through all of them.
    let mut declared: Vec<u32> = Vec::with_capacity(64);
    let mut bindings = 0usize;
    let mut plan = Plan::default();
    let mut open: Option<Section> = None;
    // The body: its depth, prefix and namespace.
    let mut body: Option<(usize, Vec<u8>, &'static [u8])> = None;
    let mut body_has_section = false;
    // Open `mc:AlternateContent` elements.
    let mut alternates = 0usize;
    // Open elements of other namespaces: a section in one is extension
    // content (often ignorable), not a section of the document.
    let mut foreign = 0usize;
    // Open elements with an mc:ProcessContent attribute: inside one, a
    // foreign wrapper may be transparent, and a section in it may be the
    // document's.
    let mut processing: Vec<bool> = Vec::with_capacity(64);
    let mut in_process = 0usize;
    // Open `w:sectPrChange` elements: a section's former properties.
    let mut history = 0usize;
    let mut before = skipped + xml.buffer_position();
    loop {
        let (ns, event) = xml.read_resolved_event_into(&mut buf).map_err(xml_error)?;
        let mc = matches!(ns, ResolveResult::Bound(Namespace(n)) if n == MC_NS);
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
                let prefix: &[u8] = e
                    .name()
                    .prefix()
                    .map(|p| p.into_inner())
                    .unwrap_or_default();
                let tag = match (ns, local) {
                    (Some(_), b"document") => Tag::Document,
                    (Some(_), b"body") => Tag::Body,
                    (Some(_), b"p") => Tag::P,
                    (Some(_), b"pPr") => Tag::PPr,
                    (Some(_), b"sectPr") => Tag::SectPr,
                    (Some(_), b"sectPrChange") => Tag::SectPrChange,
                    _ if mc && local == b"AlternateContent" => Tag::Alternate,
                    (None, _) => Tag::Foreign,
                    _ => Tag::Other,
                };
                let depth = stack.len();
                let mut declares = 0usize;
                for a in e.attributes().with_checks(false).flatten() {
                    let key = a.key.as_ref();
                    if key == b"xmlns" || key.starts_with(b"xmlns:") {
                        declares += 1;
                        // A namespace spelled with a reference (`&#97;`) is
                        // not compared as written: which elements are Word's
                        // would be a guess.
                        if a.value.contains(&b'&') {
                            return Err(Skip::Unsupported);
                        }
                    }
                }
                // The name first: a namespace lookup only for candidates.
                let processes = e.attributes().with_checks(false).flatten().any(|a| {
                    a.key.local_name().as_ref() == b"ProcessContent" && {
                        let (ns, _) = xml.resolver().resolve_attribute(a.key);
                        matches!(ns, ResolveResult::Bound(Namespace(n)) if n == MC_NS)
                    }
                });
                if bindings + declares > MAX_BINDINGS {
                    return Err(Skip::Unsupported);
                }
                if let Some(s) = open.as_mut() {
                    // A branch, or a page size or an earlier child inside a
                    // wrapper (such as one that mc:ProcessContent names):
                    // which size counts, or where it goes, is not plain.
                    if tag == Tag::Alternate
                        || (depth > s.depth + 1
                            && history == 0
                            && ns.is_some()
                            && (local == b"pgSz" || BEFORE_PAGE_SIZE.contains(&local)))
                    {
                        return Err(Skip::Unsupported);
                    }
                    if depth == s.depth + 1 {
                        if ns.is_some() && local == b"pgSz" {
                            // Then the others may take their size from it (a
                            // continuous break): adding sizes would be a
                            // guess, so the part loads as it came, and the
                            // rest need not be read.
                            return Err(Skip::Sized);
                        } else if ns.is_some() && BEFORE_PAGE_SIZE.contains(&local) {
                            if empty {
                                s.insert_at = after;
                            } else {
                                s.in_before = true;
                            }
                        }
                    }
                }
                let parent = stack.last().copied();
                let grandparent = depth.checked_sub(2).map(|i| stack[i]);
                if tag == Tag::SectPr && foreign > 0 && alternates == 0 && in_process > 0 {
                    return Err(Skip::Unsupported);
                }
                if tag == Tag::SectPr
                    && parent != Some(Tag::SectPrChange)
                    && (alternates > 0 || foreign == 0)
                {
                    let hosted = match parent {
                        Some(Tag::Body) => true,
                        Some(Tag::PPr) => grandparent == Some(Tag::P),
                        _ => false,
                    };
                    if !hosted || open.is_some() || alternates > 0 || prefix.len() > MAX_PREFIX {
                        return Err(Skip::Unsupported);
                    }
                    let ns = ns.unwrap_or(W_NS);
                    body_has_section |= parent == Some(Tag::Body);
                    if empty {
                        // `<w:sectPr …/>`: its `/>` becomes `>…</w:sectPr>`.
                        let mut text = b">".to_vec();
                        text.extend(page_size(prefix, ns, paper));
                        text.extend(closing(prefix, b"sectPr"));
                        plan.push(after - 2, after, text)?;
                    } else {
                        open = Some(Section {
                            depth,
                            prefix: prefix.to_vec(),
                            ns,
                            insert_at: after,
                            in_before: false,
                        });
                    }
                }
                if tag == Tag::Body
                    && body.is_none()
                    && let Some(ns) = ns
                {
                    // The body is the root w:document's child; one anywhere
                    // else (in an extension, or a body per branch) makes the
                    // one LibreOffice reads unclear.
                    let under_root = depth == 1 && stack.first() == Some(&Tag::Document);
                    if !under_root || alternates > 0 || prefix.len() > MAX_PREFIX {
                        return Err(Skip::Unsupported);
                    }
                    if empty {
                        // `<w:body/>` still lays out one empty page.
                        let mut text = b">".to_vec();
                        text.extend(section(prefix, ns, paper));
                        text.extend(closing(prefix, b"body"));
                        plan.push(after - 2, after, text)?;
                        body_has_section = true;
                    }
                    body = Some((depth, prefix.to_vec(), ns));
                }
                if !empty {
                    alternates += usize::from(tag == Tag::Alternate);
                    foreign += usize::from(tag == Tag::Foreign);
                    history += usize::from(tag == Tag::SectPrChange);
                    stack.push(tag);
                    declared.push(declares as u32);
                    processing.push(processes);
                    in_process += usize::from(processes);
                    bindings += declares;
                    if stack.len() > MAX_DEPTH {
                        return Err(Skip::Unreadable);
                    }
                }
            }
            Event::End(_) => {
                let closed = stack.pop();
                bindings -= declared.pop().unwrap_or(0) as usize;
                in_process -= usize::from(processing.pop().unwrap_or(false));
                alternates -= usize::from(closed == Some(Tag::Alternate));
                foreign -= usize::from(closed == Some(Tag::Foreign));
                history -= usize::from(closed == Some(Tag::SectPrChange));
                let depth = stack.len();
                if let Some(s) = open.as_mut()
                    && s.in_before
                    && depth == s.depth + 1
                {
                    s.insert_at = after;
                    s.in_before = false;
                }
                if let Some(s) = open.take_if(|s| s.depth == depth) {
                    let at = s.insert_at;
                    plan.push(at, at, page_size(&s.prefix, s.ns, paper))?;
                }
                if closed == Some(Tag::Body)
                    && let Some((body_depth, prefix, ns)) = &body
                    && *body_depth == depth
                {
                    if !body_has_section {
                        plan.push(before, before, section(prefix, ns, paper))?;
                    }
                    // Only the first body counts.
                    body_has_section = true;
                }
            }
            Event::Decl(ref d) => {
                let utf8 = match d.encoding() {
                    None => true,
                    Some(Ok(name)) => {
                        name.eq_ignore_ascii_case(b"UTF-8") || name.eq_ignore_ascii_case(b"UTF8")
                    }
                    Some(Err(_)) => false,
                };
                if !utf8 {
                    return Err(Skip::Unsupported);
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
    Ok(plan)
}

/// Writes `part` with the edits of `plan` applied. The edits are sorted
/// and do not overlap ([`plan`] makes them so).
pub fn apply(mut part: impl Read, plan: &Plan, out: &mut impl Write) -> io::Result<()> {
    let mut at = 0u64;
    for edit in &plan.edits {
        let text = plan
            .texts
            .get(edit.text as usize)
            .ok_or_else(|| io::Error::other("no such text"))?;
        if edit.at < at || edit.end < edit.at {
            return Err(io::Error::other("edits out of order"));
        }
        let copied = io::copy(&mut (&mut part).take(edit.at - at), out)?;
        if copied != edit.at - at {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        out.write_all(text)?;
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
    let plan = match plan(xml, paper, MAIN_PART_CAP) {
        Err(Skip::Sized) => return Ok(None),
        other => other?,
    };
    if plan.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(xml.len() + 256);
    apply(xml, &plan, &mut out).map_err(|_| Skip::Unreadable)?;
    Ok(Some(out))
}

/// The main part that `_rels/.rels` names, or `word/document.xml` when it
/// names none, as the pre-flight finds it. A package with a signature
/// origin is [`Skip::Unsupported`]: a changed main part would break its
/// signature, wherever the signature parts are.
pub fn main_part(file: &mut File, entries: &[(String, zip_scan::Entry)]) -> Result<String, Skip> {
    let mut found = entries
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("_rels/.rels"));
    let rels = match (found.next(), found.next()) {
        (None, _) => return Ok("word/document.xml".to_owned()),
        (Some((_, rels)), None) => rels,
        (Some(_), Some(_)) => return Err(Skip::Unsupported),
    };
    let bytes = zip_scan::read_entry(file, rels, crate::preflight::PART_CAP as usize)
        .map_err(scan_error)?
        .ok_or(Skip::Unsupported)?;
    // OPC allows UTF-16 here, whatever the main part uses; no offsets are
    // needed, so it is decoded first.
    let bytes = utf16_to_utf8(&bytes).unwrap_or(bytes);
    let mut xml = NsReader::from_reader(bytes.as_slice());
    let mut buf = Vec::new();
    let mut main = None;
    // Relationship parts are two levels deep; anything deeper, a root that
    // is not OPC's Relationships, or a namespace spelled with a reference
    // could hide a signature origin: the package loads as it came.
    let mut depth = 0usize;
    let mut root_seen = false;
    loop {
        let (ns, event) = xml
            .read_resolved_event_into(&mut buf)
            .map_err(|_| Skip::Unsupported)?;
        let in_rels = matches!(ns, ResolveResult::Bound(Namespace(n)) if n == RELS_NS);
        if let Event::Start(e) | Event::Empty(e) = &event {
            let escaped = e.attributes().with_checks(false).flatten().any(|a| {
                let key = a.key.as_ref();
                (key == b"xmlns" || key.starts_with(b"xmlns:")) && a.value.contains(&b'&')
            });
            let bad_root = !root_seen && !(in_rels && e.local_name().as_ref() == b"Relationships");
            // A branch could make another relationship the active one.
            let branch = e.local_name().as_ref() == b"AlternateContent";
            if escaped || bad_root || branch || depth >= 8 {
                return Err(Skip::Unsupported);
            }
            root_seen = true;
        }
        // Relationship elements count only as children of the root.
        let child_of_root = depth == 1;
        // A Relationship deeper down may be live (mc:ProcessContent can make
        // its wrapper transparent) and may be a signature origin.
        if let Event::Start(e) | Event::Empty(e) = &event
            && !child_of_root
            && in_rels
            && e.local_name().as_ref() == b"Relationship"
        {
            return Err(Skip::Unsupported);
        }
        match &event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        match event {
            Event::Start(e) | Event::Empty(e)
                if child_of_root && in_rels && e.local_name().as_ref() == b"Relationship" =>
            {
                // Only the unqualified attributes: `xmlns:Type` or `x:Type`
                // are not the relationship's type.
                let attr = |key: &[u8]| {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == key)
                        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
                        .map(|v| v.into_owned())
                };
                let kind = attr(b"Type").unwrap_or_default();
                if kind == SIGNATURE_ORIGIN {
                    return Err(Skip::Unsupported);
                }
                if main.is_none()
                    && OFFICE_DOCUMENT.contains(&kind.as_str())
                    && attr(b"TargetMode").is_none_or(|mode| mode == "Internal")
                    && let Some(target) = attr(b"Target")
                {
                    main = Some(normalize(&target));
                }
            }
            Event::Eof => return Ok(main.unwrap_or_else(|| "word/document.xml".to_owned())),
            _ => {}
        }
        buf.clear();
    }
}

/// The namespace of OPC relationship parts.
const RELS_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";

/// The relationship type of a package's digital-signature origin.
const SIGNATURE_ORIGIN: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin";

/// The relationship types of a main document, transitional and strict.
const OFFICE_DOCUMENT: [&str; 2] = [
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument",
    "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument",
];

/// A reader that fails with `InvalidData` on bytes that are not UTF-8,
/// across read boundaries.
struct Utf8Reader<R> {
    inner: R,
    /// The start of a character that the last read cut (at most 3 bytes).
    carry: Vec<u8>,
    checked: Vec<u8>,
}

impl<R: Read> Read for Utf8Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            return if self.carry.is_empty() {
                Ok(0)
            } else {
                Err(io::ErrorKind::InvalidData.into())
            };
        }
        let data: &[u8] = if self.carry.is_empty() {
            &buf[..n]
        } else {
            self.checked.clear();
            self.checked.extend_from_slice(&self.carry);
            self.checked.extend_from_slice(&buf[..n]);
            &self.checked
        };
        let carry = match std::str::from_utf8(data) {
            Ok(_) => Vec::new(),
            Err(err) if err.error_len().is_none() => data[err.valid_up_to()..].to_vec(),
            Err(_) => return Err(io::ErrorKind::InvalidData.into()),
        };
        self.carry = carry;
        Ok(n)
    }
}

/// `bytes` decoded from UTF-16 when they start with its byte order mark.
fn utf16_to_utf8(bytes: &[u8]) -> Option<Vec<u8>> {
    let (body, little) = match bytes {
        [0xFF, 0xFE, rest @ ..] => (rest, true),
        [0xFE, 0xFF, rest @ ..] => (rest, false),
        _ => return None,
    };
    let units = body.as_chunks::<2>().0.iter().map(|pair| {
        if little {
            u16::from_le_bytes(*pair)
        } else {
            u16::from_be_bytes(*pair)
        }
    });
    let text: String = char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    // The declaration still names UTF-16; quick-xml reads bytes as UTF-8
    // without the encoding feature, so it does not mind.
    Some(text.into_bytes())
}

/// A package-relative target with `.` and `..` resolved.
fn normalize(target: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// One central-directory record, as found in the directory bytes.
struct Record {
    /// Its bytes in the directory.
    start: usize,
    len: usize,
    name_len: usize,
    flags: u16,
    method: u16,
    crc: u32,
    compressed: u64,
    uncompressed: u64,
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
        // ZIP64 by the disk-start sentinel, or by an extra block.
        if u16_at(header, 34) == 0xFFFF {
            return None;
        }
        let name_len = usize::from(u16_at(header, 28));
        let extra_len = usize::from(u16_at(header, 30));
        let len = 46 + name_len + extra_len + usize::from(u16_at(header, 32));
        let name = directory.get(at + 46..at + 46 + name_len)?;
        directory.get(at..at + len)?;
        let extra = directory.get(at + 46 + name_len..at + 46 + name_len + extra_len)?;
        let mut block = 0;
        while block + 4 <= extra.len() {
            if u16_at(extra, block) == 0x0001 {
                return None;
            }
            block += 4 + usize::from(u16_at(extra, block + 2));
        }
        out.push(Record {
            start: at,
            len,
            name_len,
            flags: u16_at(header, 8),
            method: u16_at(header, 10),
            crc: u32_at(header, 16),
            compressed: u64::from(compressed),
            uncompressed: u64::from(uncompressed),
            local_offset: u64::from(local_offset),
            is_main: name == main.as_bytes(),
        });
        at += len;
    }
    // Bytes after the counted records (a directory signature, say) would
    // be dropped by the copy.
    (at == directory.len()).then_some(out)
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

/// Counts the bytes read and their CRC-32.
struct CrcReader<R> {
    inner: R,
    crc: crc32fast::Hasher,
    len: u64,
}

impl<R: Read> Read for CrcReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crc.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
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
/// section names a page size of `paper`. `Ok` when the copy was
/// written. An error leaves no file at `out`: the caller loads `source`,
/// except after [`Skip::Io`], which fails the job.
pub fn repack(source: &Path, out: &Path, paper: Paper) -> Result<(), Skip> {
    let mut created = false;
    let result = write_copy(source, out, paper, &mut created);
    // Only a file that this call created is removed.
    if result.is_err() && created {
        let _ = std::fs::remove_file(out);
    }
    result
}

fn write_copy(source: &Path, out: &Path, paper: Paper, created: &mut bool) -> Result<(), Skip> {
    let mut file = File::open(source).map_err(source_io)?;
    let size = file.metadata().map_err(source_io)?.len();
    let mut head = [0u8; 4];
    file.read_exact(&mut head).map_err(source_io)?;
    if head != *b"PK\x03\x04" {
        return Err(Skip::Unsupported);
    }
    // The central directory's size first: the listings below keep names.
    let (count, cd_offset, cd_size, zip64) =
        zip_scan::directory(&mut file, size).map_err(scan_error)?;
    if zip64 || cd_size > DIRECTORY_CAP || cd_offset > u64::from(u32::MAX) {
        return Err(Skip::Unsupported);
    }
    let entries = zip_scan::list(&mut file, size).map_err(scan_error)?;
    // A signed package: a changed main part would break its signature.
    if entries.iter().any(|(name, _)| {
        name.as_bytes()
            .get(..15)
            .is_some_and(|start| start.eq_ignore_ascii_case(b"_xmlsignatures/"))
    }) {
        return Err(Skip::Unsupported);
    }
    // OPC part names compare without ASCII case; one entry must match.
    let wanted = main_part(&mut file, &entries)?;
    let mut matching = entries
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(&wanted));
    let (Some((main, entry)), None) = (matching.next(), matching.next()) else {
        return Err(Skip::Unsupported);
    };
    let (main, entry) = (main.clone(), *entry);
    if entry.method != METHOD_STORED && entry.method != METHOD_DEFLATED {
        return Err(Skip::Unsupported);
    }
    // Declared too large: no pass could finish. The streaming cap still
    // catches a part that declares less than it holds.
    if entry.uncompressed > MAIN_PART_CAP {
        return Err(Skip::Unreadable);
    }

    // The edits, from a first pass over the main part. The pass also takes
    // the part's CRC and size, which must match its directory record
    // below: a damaged part loads as it came, so that the copy's new CRC
    // never hides the damage from LibreOffice.
    let (edits, crc, len) = {
        let reader = zip_scan::open_entry(&mut file, &entry)
            .map_err(scan_error)?
            .ok_or(Skip::Unsupported)?;
        let mut checked = CrcReader {
            inner: reader,
            crc: crc32fast::Hasher::new(),
            len: 0,
        };
        let edits = plan(
            BufReader::with_capacity(64 * 1024, &mut checked),
            paper,
            MAIN_PART_CAP,
        )?;
        // plan() read to the end of the part; anything left is damage too,
        // and so are compressed bytes that the stream did not use.
        let rest = io::copy(&mut (&mut checked).take(1), &mut io::sink()).map_err(applied)?;
        let consumed = match &checked.inner {
            zip_scan::EntryReader::Deflated(decoder) => decoder.total_in(),
            zip_scan::EntryReader::Stored(data) => entry.compressed - data.limit(),
        };
        if rest != 0 || consumed != entry.compressed {
            return Err(Skip::Unreadable);
        }
        (edits, checked.crc.finalize(), checked.len)
    };
    // Nothing to change: the directory is not read again.
    if edits.is_empty() {
        return Err(Skip::Sized);
    }

    // The central directory, whole: at most DIRECTORY_CAP bytes.
    let mut directory = vec![0u8; cd_size as usize];
    file.seek(SeekFrom::Start(cd_offset)).map_err(source_io)?;
    file.read_exact(&mut directory).map_err(source_io)?;
    let records = records(&directory, count, &main).ok_or(Skip::Unsupported)?;
    let mut mains = records.iter().filter(|r| r.is_main);
    let (Some(main_record), None) = (mains.next(), mains.next()) else {
        return Err(Skip::Unsupported);
    };
    if main_record.flags & 1 != 0 {
        return Err(Skip::Unsupported);
    }
    if len != entry.uncompressed || crc != main_record.crc {
        return Err(Skip::Unreadable);
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

    // Each local record (header, name, extra field and data) must fit
    // before the next one, or the spans below would cut or mix entries.
    for (k, &i) in order.iter().enumerate() {
        let record = &records[i];
        let end = order
            .get(k + 1)
            .map_or(cd_offset, |&j| records[j].local_offset);
        file.seek(SeekFrom::Start(record.local_offset))
            .map_err(source_io)?;
        let mut header = [0u8; 30];
        file.read_exact(&mut header).map_err(source_io)?;
        let needed = 30
            + u64::from(u16_at(&header, 26))
            + u64::from(u16_at(&header, 28))
            + record.compressed;
        if u32_at(&header, 0) != 0x0403_4b50 || record.local_offset + needed > end {
            return Err(Skip::Unsupported);
        }
        // No entry may use ZIP64 in its local header either.
        let name_len = usize::from(u16_at(&header, 26));
        let mut name_extra = vec![0u8; name_len + usize::from(u16_at(&header, 28))];
        file.read_exact(&mut name_extra).map_err(source_io)?;
        let extra = &name_extra[name_len..];
        let mut at = 0;
        while at + 4 <= extra.len() {
            if u16_at(extra, at) == 0x0001 {
                return Err(Skip::Unsupported);
            }
            at += 4 + usize::from(u16_at(extra, at + 2));
        }
        // The local header must say the same about a data descriptor, and
        // the descriptor (12 bytes, or 16 with its signature) must fit too.
        if (u16_at(&header, 6) ^ record.flags) & 0x0008 != 0 {
            return Err(Skip::Unreadable);
        }
        // The directory's CRC and sizes, which a descriptor must repeat: they
        // also tell the 16-byte layout (with a signature) from the 12-byte
        // one, whose CRC may look like a signature.
        let want = [
            record.crc,
            record.compressed as u32,
            record.uncompressed as u32,
        ];
        if record.flags & 0x0008 != 0 {
            let at = record.local_offset + needed;
            let room = usize::try_from((end - at).min(16)).unwrap_or(16);
            let mut descriptor = [0u8; 16];
            file.seek(SeekFrom::Start(at)).map_err(source_io)?;
            file.read_exact(&mut descriptor[..room])
                .map_err(source_io)?;
            let fields = |from: usize| {
                [
                    u32_at(&descriptor, from),
                    u32_at(&descriptor, from + 4),
                    u32_at(&descriptor, from + 8),
                ]
            };
            let signed = room >= 16 && u32_at(&descriptor, 0) == 0x0807_4b50 && fields(4) == want;
            let plain = room >= 12 && fields(0) == want;
            if !signed && !plain {
                return Err(Skip::Unsupported);
            }
        }
        // The main part's own name, flags, CRC and sizes are replaced or
        // kept beside new data: they must agree with the directory first,
        // so that the copy never repairs or hides what LibreOffice would see
        // in the source. With a descriptor, the local fields may be zero.
        if record.is_main {
            let cd_name = &directory[record.start + 46..record.start + 46 + record.name_len];
            if &name_extra[..name_len] != cd_name || u16_at(&header, 6) != record.flags {
                return Err(Skip::Unreadable);
            }
            let local = [
                u32_at(&header, 14),
                u32_at(&header, 18),
                u32_at(&header, 22),
            ];
            let placeholders = record.flags & 0x0008 != 0;
            let agrees = local
                .iter()
                .zip(&want)
                .all(|(have, want)| have == want || (placeholders && *have == 0));
            if !agrees {
                return Err(Skip::Unreadable);
            }
        }
    }

    // The copy's folder is made only now that there is a copy to write. An
    // existing one must be a real folder, not a link out of the job.
    if let Some(dir) = out.parent() {
        match std::fs::create_dir(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let meta = std::fs::symlink_metadata(dir).map_err(written)?;
                if !meta.file_type().is_dir() {
                    return Err(Skip::Io(io::Error::from_raw_os_error(libc::ENOTDIR)));
                }
            }
            Err(err) => return Err(written(err)),
        }
    }
    let target = File::create_new(out).map_err(written)?;
    *created = true;
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
                .map_err(source_io)?;
            let span = end - record.local_offset;
            let copied = io::copy(&mut (&mut file).take(span), &mut writer).map_err(written)?;
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
        writer.write_all(&bytes).map_err(written)?;
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
    writer.write_all(&eocd).map_err(written)?;
    writer.flush().map_err(written)?;
    Ok(())
}

/// Writes the main part's local record: its header with the new CRC and
/// sizes and no data descriptor, its name and extra field as they were,
/// and the edited content with its own method. Returns the CRC and sizes.
fn write_main(
    file: &mut File,
    writer: &mut Capped<BufWriter<File>>,
    record: &Record,
    entry: &zip_scan::Entry,
    edits: &Plan,
) -> Result<(u32, u32, u32), Skip> {
    file.seek(SeekFrom::Start(record.local_offset))
        .map_err(source_io)?;
    let mut header = [0u8; 30];
    file.read_exact(&mut header).map_err(source_io)?;
    if u32_at(&header, 0) != 0x0403_4b50 || u16_at(&header, 8) != record.method {
        return Err(Skip::Unsupported);
    }
    let mut name_extra =
        vec![0u8; usize::from(u16_at(&header, 26)) + usize::from(u16_at(&header, 28))];
    file.read_exact(&mut name_extra).map_err(source_io)?;
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
    writer.write_all(&header).map_err(written)?;
    writer.write_all(&name_extra).map_err(written)?;
    let data_at = writer.written;
    let (crc, uncompressed) = {
        let part = zip_scan::open_entry(file, entry)
            .map_err(scan_error)?
            .ok_or(Skip::Unsupported)?;
        let part = part.take(MAIN_PART_CAP.saturating_add(1));
        match record.method {
            METHOD_DEFLATED => {
                let mut sink = Crc {
                    inner: DeflateEncoder::new(&mut *writer, Compression::default()),
                    crc: crc32fast::Hasher::new(),
                    len: 0,
                };
                apply(part, edits, &mut sink).map_err(applied)?;
                let (crc, len) = (sink.crc.finalize(), sink.len);
                sink.inner.finish().map_err(written)?;
                (crc, len)
            }
            _ => {
                let mut sink = Crc {
                    inner: &mut *writer,
                    crc: crc32fast::Hasher::new(),
                    len: 0,
                };
                apply(part, edits, &mut sink).map_err(applied)?;
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
    inner.seek(SeekFrom::Start(header_at)).map_err(written)?;
    inner.write_all(&header).map_err(written)?;
    inner.seek(SeekFrom::Start(end)).map_err(written)?;
    Ok((crc, compressed, uncompressed))
}

/// The folder of the copies, next to the source.
pub const COPY_DIR: &str = ".convertd-paper";

/// The paths of a swap for `source`: where the copy is written (`fresh`),
/// and where the source waits while the copy is in its place (`backup`).
/// Both are in [`COPY_DIR`] next to the source, on its file system, so
/// each move is a rename.
pub fn swap_paths(source: &Path) -> Option<(PathBuf, PathBuf)> {
    let dir = source.parent()?.join(COPY_DIR);
    let name = source.file_name()?;
    let mut fresh = name.to_os_string();
    fresh.push(".paper");
    Some((dir.join(fresh), dir.join(name)))
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
