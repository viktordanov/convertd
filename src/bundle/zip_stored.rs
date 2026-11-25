//! A stored (method 0) ZIP whose bytes and length are known before the
//! first byte goes out.
//!
//! - Every CRC-32 is known before its local header, so there are no data
//!   descriptors and any streaming unzipper works.
//! - Sizes come from the supervisor's own `fstat` at plan time, never from
//!   an engine. The writer copies exactly that many bytes per file and fails
//!   if a file is shorter.
//! - A fixed date (1980-01-01 00:00), the UTF-8 name flag and the order of
//!   the entries make the bytes a pure function of the content.
//! - No ZIP64: a plan of 4 GiB or more, or of 65 535 entries or more, is
//!   refused before the response starts.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use bytes::Bytes;

const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const LOCAL_HEADER: u64 = 30;
const CENTRAL_HEADER: u64 = 46;
const EOCD: u64 = 22;
/// Version 2.0, the version that introduced stored and deflated entries.
const VERSION: u16 = 20;
/// Bit 11: names are UTF-8.
const FLAG_UTF8: u16 = 0x0800;
/// DOS date 1980-01-01; the time is 00:00:00.
const DOS_DATE: u16 = (1 << 5) | 1;
/// The largest offset or size without ZIP64.
const MAX_U32: u64 = u32::MAX as u64;
/// The largest entry count without ZIP64.
const MAX_ENTRIES: usize = 0xFFFF - 1;

/// Where an entry's bytes come from.
#[derive(Debug, Clone)]
pub enum Source {
    Bytes(Bytes),
    File(PathBuf),
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    source: Source,
    size: u64,
    crc32: u32,
}

/// Why a plan cannot be built or sent.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// The ZIP would need ZIP64.
    #[error("the response would exceed the ZIP limit of 4 GiB and 65 534 entries")]
    TooLarge,
    #[error("failed to plan the ZIP entry {name}: {source}")]
    Io {
        name: String,
        #[source]
        source: io::Error,
    },
}

/// The entries of one ZIP, in order, with their sizes and CRCs.
#[derive(Debug, Clone, Default)]
pub struct ZipPlan {
    entries: Vec<Entry>,
}

impl ZipPlan {
    pub fn new() -> Self {
        Self::default()
    }

    /// An entry from bytes in memory, such as `manifest.json`.
    pub fn add_bytes(&mut self, name: impl Into<String>, bytes: Bytes) {
        let crc32 = crc32fast::hash(&bytes);
        self.entries.push(Entry {
            name: name.into(),
            size: bytes.len() as u64,
            source: Source::Bytes(bytes),
            crc32,
        });
    }

    /// An entry from a file. The size is the file's own `fstat`. Without a
    /// known CRC, one read pass computes it (page cache, about 10 ms per
    /// 100 MB, estimated). Blocks: call it on a blocking thread.
    pub fn add_file(
        &mut self,
        name: impl Into<String>,
        path: PathBuf,
        crc32: Option<u32>,
    ) -> Result<(), PlanError> {
        let name = name.into();
        let io = |source| PlanError::Io {
            name: name.clone(),
            source,
        };
        let size = std::fs::metadata(&path).map_err(io)?.len();
        let crc32 = match crc32 {
            Some(crc32) => crc32,
            None => crc_of(&path, size).map_err(io)?,
        };
        self.entries.push(Entry {
            name,
            source: Source::File(path),
            size,
            crc32,
        });
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The exact length of the ZIP:
    /// `Σ(30 + name + size) + Σ(46 + name) + 22`.
    pub fn content_length(&self) -> u64 {
        let entries: u64 = self
            .entries
            .iter()
            .map(|e| LOCAL_HEADER + CENTRAL_HEADER + 2 * e.name.len() as u64 + e.size)
            .sum();
        entries + EOCD
    }

    /// Refuses a plan that would need ZIP64.
    pub fn check(&self) -> Result<(), PlanError> {
        let central_offset = self.content_length() - EOCD - self.central_size();
        let too_large = self.entries.len() > MAX_ENTRIES
            || self.content_length() > MAX_U32
            || central_offset > MAX_U32
            || self
                .entries
                .iter()
                .any(|e| e.size > MAX_U32 || e.name.len() > 0xFFFF);
        if too_large {
            Err(PlanError::TooLarge)
        } else {
            Ok(())
        }
    }

    fn central_size(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| CENTRAL_HEADER + e.name.len() as u64)
            .sum()
    }

    /// Writes the whole ZIP to `out` and returns the bytes written. Fails
    /// if a file is shorter than planned. Call [`ZipPlan::check`] first.
    pub fn write_to<W: Write>(&self, out: &mut W) -> io::Result<u64> {
        self.check().map_err(io::Error::other)?;
        let mut written = 0u64;
        let mut offsets = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            offsets.push(written);
            let header = local_header(entry);
            out.write_all(&header)?;
            written += header.len() as u64;
            match &entry.source {
                Source::Bytes(bytes) => out.write_all(bytes)?,
                Source::File(path) => {
                    let copied = io::copy(&mut File::open(path)?.take(entry.size), out)?;
                    if copied != entry.size {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("{} is shorter than planned", entry.name),
                        ));
                    }
                }
            }
            written += entry.size;
        }
        let central_offset = written;
        for (entry, offset) in self.entries.iter().zip(&offsets) {
            let header = central_header(entry, *offset);
            out.write_all(&header)?;
            written += header.len() as u64;
        }
        let central_size = written - central_offset;
        let count = self.entries.len() as u16;
        let mut eocd = Vec::with_capacity(EOCD as usize);
        eocd.extend_from_slice(&EOCD_SIG.to_le_bytes());
        eocd.extend_from_slice(&[0; 4]); // disk numbers
        eocd.extend_from_slice(&count.to_le_bytes());
        eocd.extend_from_slice(&count.to_le_bytes());
        eocd.extend_from_slice(&(central_size as u32).to_le_bytes());
        eocd.extend_from_slice(&(central_offset as u32).to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes()); // comment length
        out.write_all(&eocd)?;
        written += EOCD;
        Ok(written)
    }
}

/// The fields a local and a central header share, from "version needed".
fn common(entry: &Entry) -> Vec<u8> {
    let mut out = Vec::with_capacity(26);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&FLAG_UTF8.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
    out.extend_from_slice(&0u16.to_le_bytes()); // time 00:00:00
    out.extend_from_slice(&DOS_DATE.to_le_bytes());
    out.extend_from_slice(&entry.crc32.to_le_bytes());
    out.extend_from_slice(&(entry.size as u32).to_le_bytes()); // compressed
    out.extend_from_slice(&(entry.size as u32).to_le_bytes()); // uncompressed
    out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra length
    out
}

fn local_header(entry: &Entry) -> Vec<u8> {
    let mut out = Vec::with_capacity(LOCAL_HEADER as usize + entry.name.len());
    out.extend_from_slice(&LOCAL_SIG.to_le_bytes());
    out.extend_from_slice(&common(entry));
    out.extend_from_slice(entry.name.as_bytes());
    out
}

fn central_header(entry: &Entry, offset: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(CENTRAL_HEADER as usize + entry.name.len());
    out.extend_from_slice(&CENTRAL_SIG.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes()); // version made by
    out.extend_from_slice(&common(entry));
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    out.extend_from_slice(&0u16.to_le_bytes()); // disk number
    out.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
    out.extend_from_slice(&0u32.to_le_bytes()); // external attributes
    out.extend_from_slice(&(offset as u32).to_le_bytes());
    out.extend_from_slice(entry.name.as_bytes());
    out
}

/// CRC-32 of the first `size` bytes of `path`.
fn crc_of(path: &PathBuf, size: u64) -> io::Result<u32> {
    let mut file = File::open(path)?.take(size);
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finalize());
        }
        hasher.update(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("convertd-zip-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plan(dir: &std::path::Path) -> ZipPlan {
        std::fs::write(dir.join("document.pdf"), b"%PDF-1.4 fake pdf bytes").unwrap();
        std::fs::write(dir.join("0001.png"), vec![7u8; 100_000]).unwrap();
        let mut plan = ZipPlan::new();
        plan.add_bytes(
            "manifest.json",
            Bytes::from_static(br#"{"manifest_version":1}"#),
        );
        plan.add_file("document.pdf", dir.join("document.pdf"), None)
            .unwrap();
        let crc = crc32fast::hash(&vec![7u8; 100_000]);
        plan.add_file("pages/0001.png", dir.join("0001.png"), Some(crc))
            .unwrap();
        plan
    }

    #[test]
    fn length_is_exact_and_entries_read_back() {
        let dir = scratch("read");
        let plan = plan(&dir);
        let mut out = Vec::new();
        let written = plan.write_to(&mut out).unwrap();
        assert_eq!(written, out.len() as u64);
        assert_eq!(plan.content_length(), written);

        let mut archive = zip::ZipArchive::new(Cursor::new(&out)).unwrap();
        let names: Vec<String> = archive
            .file_names()
            .map(|name| name.unwrap().into_owned())
            .collect();
        assert_eq!(names, ["manifest.json", "document.pdf", "pages/0001.png"]);
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            assert_eq!(entry.compression(), zip::CompressionMethod::Stored);
            let mut data = Vec::new();
            // Reading to the end checks the CRC.
            entry.read_to_end(&mut data).unwrap();
            assert_eq!(data.len() as u64, entry.size());
        }
        // No data descriptors: bit 3 is clear in every local header.
        assert_eq!(&out[..4], &LOCAL_SIG.to_le_bytes());
        assert_eq!(u16::from_le_bytes([out[6], out[7]]) & 0x0008, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bytes_are_a_function_of_the_content() {
        let dir = scratch("same");
        let (mut a, mut b) = (Vec::new(), Vec::new());
        plan(&dir).write_to(&mut a).unwrap();
        plan(&dir).write_to(&mut b).unwrap();
        assert_eq!(a, b);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_cut_after_planning_aborts() {
        let dir = scratch("cut");
        let plan = plan(&dir);
        std::fs::write(dir.join("0001.png"), vec![7u8; 10]).unwrap();
        let err = plan.write_to(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_crc_pass_matches() {
        let dir = scratch("crc");
        std::fs::write(dir.join("x"), b"hello world").unwrap();
        assert_eq!(
            crc_of(&dir.join("x"), 11).unwrap(),
            crc32fast::hash(b"hello world")
        );
        let mut plan = ZipPlan::new();
        let err = plan
            .add_file("missing", dir.join("missing"), None)
            .unwrap_err();
        assert!(matches!(err, PlanError::Io { .. }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn four_gib_and_too_many_entries_are_refused() {
        // A plan of declared sizes only: nothing is written.
        let big = Entry {
            name: "pages/0001.png".into(),
            source: Source::Bytes(Bytes::new()),
            size: 3 * 1024 * 1024 * 1024,
            crc32: 0,
        };
        let mut plan = ZipPlan::new();
        plan.entries.push(big.clone());
        assert!(plan.check().is_ok());
        plan.entries.push(big);
        assert!(matches!(plan.check(), Err(PlanError::TooLarge)));
        assert!(plan.write_to(&mut io::sink()).is_err());

        let mut many = ZipPlan::new();
        for i in 0..=MAX_ENTRIES {
            many.add_bytes(format!("{i}"), Bytes::new());
        }
        assert!(matches!(many.check(), Err(PlanError::TooLarge)));
    }

    #[test]
    fn python_zipfile_accepts_it_when_present() {
        let dir = scratch("python");
        let path = dir.join("bundle.zip");
        let mut file = File::create(&path).unwrap();
        plan(&dir).write_to(&mut file).unwrap();
        drop(file);
        #[allow(
            clippy::disallowed_methods,
            reason = "a test runs python directly; no reaper exists here"
        )]
        let status = std::process::Command::new("python3")
            .args(["-I", "-m", "zipfile", "-t"])
            .arg(&path)
            .output();
        match status {
            Ok(output) => assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            ),
            Err(_) => eprintln!("python3 is not installed; skipped"),
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
