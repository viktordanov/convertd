//! The `images` and `bundle` body: a ustar stream in one zstd frame
//! (INTERFACES.md §9, owner 2026-11-26).
//!
//! - Entries in order: `manifest.json`, `document.pdf` (bundle only), then
//!   `pages/NNNN.<ext>` in page order. 004's contract transport and 005's
//!   check both enforce that order.
//! - Regular files only, mode 0644, mtime 0, uid and gid 0, empty user and
//!   group names, so the bytes are a pure function of the content.
//! - Sizes come from the supervisor's own `fstat` at plan time, never from
//!   an engine. The writer copies exactly that many bytes per file and fails
//!   if a file is shorter.
//! - zstd level 1, one frame. The length is not known in advance, so the
//!   response is chunked.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use bytes::Bytes;

/// The ustar block size.
const BLOCK: usize = 512;
/// zstd's level 1: fast, and the pages are already compressed images.
pub const ZSTD_LEVEL: i32 = 1;
/// The longest name a ustar header holds without its prefix field.
const MAX_NAME: usize = 100;
/// The largest size the 11 octal digits of a ustar size field hold.
const MAX_SIZE: u64 = 0o777_7777_7777;

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
}

/// Why a plan cannot be built or sent.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// An entry over the ustar size field (8 GiB).
    #[error("a file of the response exceeds the 8 GiB tar entry limit")]
    TooLarge,
    /// A name over 100 bytes; the service's own names never are.
    #[error("the tar entry name {0} is over {MAX_NAME} bytes")]
    NameTooLong(String),
    #[error("failed to plan the tar entry {name}: {source}")]
    Io {
        name: String,
        #[source]
        source: io::Error,
    },
}

/// The entries of one tar.zst response, in order, with their sizes.
#[derive(Debug, Clone, Default)]
pub struct TarPlan {
    entries: Vec<Entry>,
}

impl TarPlan {
    pub fn new() -> Self {
        Self::default()
    }

    /// An entry from bytes in memory, such as `manifest.json`.
    pub fn add_bytes(&mut self, name: impl Into<String>, bytes: Bytes) -> Result<(), PlanError> {
        let name = checked_name(name.into())?;
        self.entries.push(Entry {
            name,
            size: bytes.len() as u64,
            source: Source::Bytes(bytes),
        });
        Ok(())
    }

    /// An entry from a file; its size is the file's own `fstat`.
    pub fn add_file(&mut self, name: impl Into<String>, path: PathBuf) -> Result<(), PlanError> {
        let name = checked_name(name.into())?;
        let size = std::fs::metadata(&path)
            .map_err(|source| PlanError::Io {
                name: name.clone(),
                source,
            })?
            .len();
        self.push_file(name, path, size)
    }

    /// An entry from a file whose size the caller measured, with its own
    /// stat, after the last write to it. The writer copies exactly `size`
    /// bytes and fails on a file that is shorter.
    pub fn add_file_sized(
        &mut self,
        name: impl Into<String>,
        path: PathBuf,
        size: u64,
    ) -> Result<(), PlanError> {
        let name = checked_name(name.into())?;
        self.push_file(name, path, size)
    }

    fn push_file(&mut self, name: String, path: PathBuf, size: u64) -> Result<(), PlanError> {
        if size > MAX_SIZE {
            return Err(PlanError::TooLarge);
        }
        self.entries.push(Entry {
            name,
            source: Source::File(path),
            size,
        });
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The length of the tar stream before compression.
    pub fn tar_length(&self) -> u64 {
        let entries: u64 = self
            .entries
            .iter()
            .map(|e| BLOCK as u64 + e.size.div_ceil(BLOCK as u64) * BLOCK as u64)
            .sum();
        entries + 2 * BLOCK as u64
    }

    /// Writes the tar stream, compressed into one zstd frame, to `out`.
    /// Fails if a file is shorter than planned. Blocks: call it on a
    /// blocking thread.
    pub fn write_to<W: Write>(&self, out: W) -> io::Result<W> {
        let mut encoder = zstd::stream::Encoder::new(out, ZSTD_LEVEL)?;
        self.write_tar(&mut encoder)?;
        encoder.finish()
    }

    /// Writes the uncompressed tar stream.
    pub fn write_tar<W: Write>(&self, out: &mut W) -> io::Result<()> {
        let padding = [0u8; BLOCK];
        for entry in &self.entries {
            out.write_all(&header(&entry.name, entry.size))?;
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
            let rest = (entry.size % BLOCK as u64) as usize;
            if rest != 0 {
                out.write_all(&padding[..BLOCK - rest])?;
            }
        }
        // The end of the archive: two zero blocks.
        out.write_all(&padding)?;
        out.write_all(&padding)?;
        Ok(())
    }
}

fn checked_name(name: String) -> Result<String, PlanError> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(PlanError::NameTooLong(name));
    }
    Ok(name)
}

/// A ustar header for a regular file: mode 0644, mtime 0, uid and gid 0,
/// empty user and group names.
fn header(name: &str, size: u64) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], 0o644); // mode
    octal(&mut h[108..116], 0); // uid
    octal(&mut h[116..124], 0); // gid
    octal(&mut h[124..136], size); // size
    octal(&mut h[136..148], 0); // mtime
    h[148..156].copy_from_slice(b"        "); // checksum, spaces while summing
    h[156] = b'0'; // typeflag: regular file
    h[257..263].copy_from_slice(b"ustar\0"); // magic
    h[263..265].copy_from_slice(b"00"); // version
    octal(&mut h[329..337], 0); // devmajor
    octal(&mut h[337..345], 0); // devminor
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    // Six octal digits, NUL, space.
    let digits = format!("{sum:06o}");
    h[148..154].copy_from_slice(&digits.as_bytes()[digits.len() - 6..]);
    h[154] = 0;
    h[155] = b' ';
    h
}

/// Zero-padded octal digits followed by NUL, filling `field`.
fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let digits = format!("{value:0width$o}");
    field[..width].copy_from_slice(&digits.as_bytes()[digits.len() - width..]);
    field[width] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("convertd-tar-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plan(dir: &std::path::Path) -> TarPlan {
        std::fs::write(dir.join("document.pdf"), b"%PDF-1.4 fake pdf bytes").unwrap();
        std::fs::write(dir.join("0001.png"), vec![7u8; 100_000]).unwrap();
        let mut plan = TarPlan::new();
        plan.add_bytes(
            "manifest.json",
            Bytes::from_static(br#"{"manifest_version":1}"#),
        )
        .unwrap();
        plan.add_file("document.pdf", dir.join("document.pdf"))
            .unwrap();
        plan.add_file("pages/0001.png", dir.join("0001.png"))
            .unwrap();
        plan
    }

    /// (name, mode, size, data) of each entry of an uncompressed tar.
    pub(crate) fn read_tar(tar: &[u8]) -> Vec<(String, u64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + BLOCK <= tar.len() && tar[at..at + BLOCK].iter().any(|&b| b != 0) {
            let h = &tar[at..at + BLOCK];
            let name_end = h[..100].iter().position(|&b| b == 0).unwrap_or(100);
            let name = String::from_utf8(h[..name_end].to_vec()).unwrap();
            let size_text = std::str::from_utf8(&h[124..135]).unwrap();
            let size = u64::from_str_radix(size_text, 8).unwrap();
            // The checksum matches.
            let mut copy = h.to_vec();
            copy[148..156].copy_from_slice(b"        ");
            let sum: u32 = copy.iter().map(|&b| u32::from(b)).sum();
            let stored =
                u32::from_str_radix(std::str::from_utf8(&h[148..154]).unwrap(), 8).unwrap();
            assert_eq!(sum, stored, "{name}");
            assert_eq!(&h[100..107], b"0000644");
            assert_eq!(&h[136..147], b"00000000000", "mtime 0");
            assert_eq!(h[156], b'0');
            assert_eq!(&h[257..263], b"ustar\0");
            let data = tar[at + BLOCK..at + BLOCK + size as usize].to_vec();
            out.push((name, size, data));
            at += BLOCK + (size as usize).div_ceil(BLOCK) * BLOCK;
        }
        out
    }

    #[test]
    fn entries_read_back_in_order() {
        let dir = scratch("read");
        let plan = plan(&dir);
        let compressed = plan.write_to(Vec::new()).unwrap();
        let tar = zstd::stream::decode_all(&compressed[..]).unwrap();
        assert_eq!(tar.len() as u64, plan.tar_length());
        let entries = read_tar(&tar);
        let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, ["manifest.json", "document.pdf", "pages/0001.png"]);
        assert_eq!(entries[1].2, b"%PDF-1.4 fake pdf bytes");
        assert_eq!(entries[2].1, 100_000);
        assert!(tar[tar.len() - 2 * BLOCK..].iter().all(|&b| b == 0));
        // One zstd frame: magic at the start, and no second frame.
        assert_eq!(&compressed[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bytes_are_a_function_of_the_content() {
        let dir = scratch("same");
        let a = plan(&dir).write_to(Vec::new()).unwrap();
        let b = plan(&dir).write_to(Vec::new()).unwrap();
        assert_eq!(a, b);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_cut_after_planning_aborts() {
        let dir = scratch("cut");
        let plan = plan(&dir);
        std::fs::write(dir.join("0001.png"), vec![7u8; 10]).unwrap();
        let err = plan.write_to(Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_sized_file_is_not_stat_again_and_must_hold_its_size() {
        let dir = scratch("sized");
        std::fs::write(dir.join("a.png"), vec![7u8; 10]).unwrap();
        let mut plan = TarPlan::new();
        // No stat at planning: a missing file plans, and fails when written.
        plan.add_file_sized("missing", dir.join("missing"), 5)
            .unwrap();
        assert_eq!(
            plan.write_to(Vec::new()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let mut plan = TarPlan::new();
        plan.add_file_sized("a.png", dir.join("a.png"), 11).unwrap();
        assert_eq!(
            plan.write_to(Vec::new()).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let mut plan = TarPlan::new();
        plan.add_file_sized("a.png", dir.join("a.png"), 10).unwrap();
        assert_eq!(plan.tar_length(), 3 * BLOCK as u64 + BLOCK as u64);
        assert!(matches!(
            plan.add_file_sized("big", dir.join("a.png"), MAX_SIZE + 1),
            Err(PlanError::TooLarge)
        ));
        plan.write_to(Vec::new()).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_and_missing_files_are_refused() {
        let mut plan = TarPlan::new();
        let long = "p".repeat(101);
        assert!(matches!(
            plan.add_bytes(long, Bytes::new()),
            Err(PlanError::NameTooLong(_))
        ));
        assert!(matches!(
            plan.add_file("x", PathBuf::from("/nonexistent/x")),
            Err(PlanError::Io { .. })
        ));
        assert!(plan.is_empty());
    }

    #[test]
    fn headers_match_the_ustar_layout() {
        let h = header("pages/0001.jpg", 1234);
        assert_eq!(&h[124..136], b"00000002322\0");
        assert_eq!(&h[108..116], b"0000000\0");
        assert_eq!(&h[265..297], &[0u8; 32], "empty user name");
        assert_eq!(&h[297..329], &[0u8; 32], "empty group name");
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "a test runs zstd and tar directly; no reaper exists here"
    )]
    fn zstd_and_tar_read_it_when_present() {
        use std::process::{Command, Stdio};
        let tools = ["zstd", "tar"].iter().all(|tool| {
            Command::new(tool)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        });
        if !tools {
            eprintln!("zstd or tar is not installed; skipped");
            return;
        }
        let dir = scratch("tools");
        let path = dir.join("out.tar.zst");
        std::fs::write(&path, plan(&dir).write_to(Vec::new()).unwrap()).unwrap();
        let script = format!("zstd -dc '{}' | tar -tvf -", path.display());
        let output = Command::new("sh").args(["-c", &script]).output().unwrap();
        let listing = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let names: Vec<&str> = listing
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .collect();
        assert_eq!(
            names,
            ["manifest.json", "document.pdf", "pages/0001.png"],
            "{listing}"
        );
        assert!(listing.contains("100000"), "{listing}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
