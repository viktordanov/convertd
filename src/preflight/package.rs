//! A ZIP package for pre-flight: its entries, a per-request inflate
//! budget, and streaming XML over one part at a time.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::rc::Rc;

use quick_xml::events::{BytesStart, Event};

use super::REQUEST_CAP;
use crate::detect::zip_scan::{self, Entry};
use crate::domain::verification::SkipReason;

/// The bytes a slide's root start tag may take.
pub const ROOT_TAG_CAP: u64 = 4 * 1024;

/// One event of [`Package::xml_events`].
pub enum XmlEvent<'a, 'b> {
    Start(&'a BytesStart<'b>),
    Empty(&'a BytesStart<'b>),
    End,
}

pub struct Package {
    file: File,
    entries: HashMap<String, Entry>,
    /// Inflated bytes left for this request.
    left: u64,
}

impl Package {
    /// Lists the entries under detection's zip-bomb caps; `None` when the
    /// archive cannot be read.
    pub fn open(mut file: File) -> Option<Self> {
        let size = file.metadata().ok()?.len();
        let entries = zip_scan::list(&mut file, size).ok()?.into_iter().collect();
        Some(Self {
            file,
            entries,
            left: REQUEST_CAP,
        })
    }

    pub fn has(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// The inflated size of `name` that the archive declares.
    pub fn size(&self, name: &str) -> Option<u64> {
        self.entries.get(name).map(|entry| entry.uncompressed)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Streams `name` through quick-xml, calling `on_start` for each start
    /// or empty tag until it returns `false`. At most `cap` inflated bytes
    /// are read (less when the request budget is lower). Returns the bytes
    /// read.
    pub fn xml(
        &mut self,
        name: &str,
        cap: u64,
        mut on_start: impl FnMut(&BytesStart<'_>) -> bool,
    ) -> Result<u64, SkipReason> {
        self.xml_events(name, cap, |event| match event {
            XmlEvent::Start(e) | XmlEvent::Empty(e) => on_start(e),
            XmlEvent::End => true,
        })
    }

    /// [`Package::xml`] with end tags too, for a scan that must know where
    /// an element ends. `Start` opens an element with content, `Empty` is a
    /// self-closing one, and `End` closes the innermost open element.
    pub fn xml_events(
        &mut self,
        name: &str,
        cap: u64,
        mut on_event: impl FnMut(XmlEvent<'_, '_>) -> bool,
    ) -> Result<u64, SkipReason> {
        let entry = *self.entries.get(name).ok_or(SkipReason::MissingPart)?;
        let cap = cap.min(self.left);
        let reader = zip_scan::open_entry(&mut self.file, &entry)
            .map_err(|_| SkipReason::MalformedXml)?
            .ok_or(SkipReason::MissingPart)?;
        let count = Rc::new(Cell::new(0u64));
        let counted = Counted {
            inner: reader,
            count: Rc::clone(&count),
            cap,
        };
        let mut xml = quick_xml::Reader::from_reader(BufReader::with_capacity(64 * 1024, counted));
        let mut buf = Vec::with_capacity(4 * 1024);
        let result = loop {
            let go_on = match xml.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => on_event(XmlEvent::Start(&e)),
                Ok(Event::Empty(e)) => on_event(XmlEvent::Empty(&e)),
                Ok(Event::End(_)) => on_event(XmlEvent::End),
                Ok(Event::Eof) => break Ok(()),
                Ok(_) => true,
                Err(_) if count.get() > cap => break Err(SkipReason::CapReached),
                Err(_) => break Err(SkipReason::MalformedXml),
            };
            if !go_on {
                break Ok(());
            }
            buf.clear();
        };
        let read = count.get().min(cap);
        self.left = self.left.saturating_sub(read);
        result.map(|()| read)
    }

    /// The first `len` inflated bytes of `name`, or fewer when it is
    /// shorter. They count against the request budget.
    pub fn head(&mut self, name: &str, len: usize) -> Result<Vec<u8>, SkipReason> {
        let entry = *self.entries.get(name).ok_or(SkipReason::MissingPart)?;
        let reader = zip_scan::open_entry(&mut self.file, &entry)
            .map_err(|_| SkipReason::MalformedXml)?
            .ok_or(SkipReason::MissingPart)?;
        let cap = (len as u64).min(self.left);
        let mut out = Vec::with_capacity(len);
        reader
            .take(cap)
            .read_to_end(&mut out)
            .map_err(|_| SkipReason::MalformedXml)?;
        self.left = self.left.saturating_sub(out.len() as u64);
        Ok(out)
    }

    /// The attributes of the root element of `name`, read from its first
    /// [`ROOT_TAG_CAP`] bytes, as `(local name, has prefix, value)`.
    pub fn root_attributes(
        &mut self,
        name: &str,
    ) -> Result<Vec<(Vec<u8>, bool, String)>, SkipReason> {
        let mut attributes = None;
        let result = self.xml(name, ROOT_TAG_CAP, |e| {
            attributes = Some(
                e.attributes()
                    .flatten()
                    .filter_map(|a| {
                        let value = a
                            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .ok()?
                            .into_owned();
                        Some((
                            a.key.local_name().as_ref().to_vec(),
                            a.key.prefix().is_some(),
                            value,
                        ))
                    })
                    .collect(),
            );
            false
        });
        match (result, attributes) {
            (_, Some(attributes)) => Ok(attributes),
            (Err(reason), None) => Err(reason),
            (Ok(_), None) => Err(SkipReason::MalformedXml),
        }
    }

    /// The relationships of `part` (`<dir>/_rels/<file>.rels`): id to
    /// (type, resolved target). External targets are left out.
    pub fn rels(&mut self, part: &str) -> Result<HashMap<String, (String, String)>, SkipReason> {
        let rels_name = rels_path(part);
        let mut map = HashMap::new();
        // A part cut at a tag boundary reads as EOF with elements still
        // open: a partial map would pass for the whole one.
        let mut open = 0usize;
        self.xml_events(&rels_name, super::PART_CAP, |event| {
            let e = match event {
                XmlEvent::Start(e) => {
                    open += 1;
                    e
                }
                XmlEvent::Empty(e) => e,
                XmlEvent::End => {
                    open = open.saturating_sub(1);
                    return true;
                }
            };
            if e.local_name().as_ref() == b"Relationship"
                && attr(e, b"TargetMode").as_deref() != Some("External")
                && let (Some(id), Some(kind), Some(target)) =
                    (attr(e, b"Id"), attr(e, b"Type"), attr(e, b"Target"))
                && map.len() < 20_000
            {
                map.insert(id, (kind, resolve(part, &target)));
            }
            true
        })?;
        if open != 0 {
            return Err(SkipReason::MalformedXml);
        }
        Ok(map)
    }
}

/// An attribute by local name, whatever its prefix.
pub fn attr(e: &BytesStart<'_>, local: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == local)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// An attribute by local name that has a prefix, such as `r:id`.
pub fn attr_prefixed(e: &BytesStart<'_>, local: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.prefix().is_some() && a.key.local_name().as_ref() == local)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// `ppt/slides/slide1.xml` gives `ppt/slides/_rels/slide1.xml.rels`.
pub fn rels_path(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// A relationship target relative to the part that names it, with `..`
/// resolved. An absolute target (`/ppt/...`) is relative to the package.
pub fn resolve(part: &str, target: &str) -> String {
    let mut segments: Vec<&str> = if let Some(absolute) = target.strip_prefix('/') {
        return normalize(absolute.split('/').collect());
    } else {
        part.rsplit_once('/')
            .map(|(dir, _)| dir.split('/').collect())
            .unwrap_or_default()
    };
    segments.extend(target.split('/'));
    normalize(segments)
}

fn normalize(segments: Vec<&str>) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(segments.len());
    for segment in segments {
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

/// Counts inflated bytes and fails once more than `cap` have been read.
struct Counted<R> {
    inner: R,
    count: Rc<Cell<u64>>,
    cap: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let used = self.count.get();
        if used >= self.cap {
            // At the cap: the part fits only if nothing follows.
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => {
                    self.count.set(self.cap + 1);
                    Err(io::ErrorKind::FileTooLarge.into())
                }
            };
        }
        let room = usize::try_from(self.cap - used).unwrap_or(usize::MAX);
        let len = buf.len().min(room);
        let n = self.inner.read(&mut buf[..len])?;
        self.count.set(used + n as u64);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_resolve_against_their_part() {
        assert_eq!(
            resolve("ppt/presentation.xml", "slides/slide1.xml"),
            "ppt/slides/slide1.xml"
        );
        assert_eq!(
            resolve("ppt/slideMasters/slideMaster1.xml", "../theme/theme1.xml"),
            "ppt/theme/theme1.xml"
        );
        assert_eq!(
            resolve("ppt/slides/slide1.xml", "/ppt/slideLayouts/l.xml"),
            "ppt/slideLayouts/l.xml"
        );
        assert_eq!(resolve("word/document.xml", "../../../../x.xml"), "x.xml");
        assert_eq!(
            rels_path("ppt/slides/slide1.xml"),
            "ppt/slides/_rels/slide1.xml.rels"
        );
        assert_eq!(rels_path("x.xml"), "_rels/x.xml.rels");
    }
}
