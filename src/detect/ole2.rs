//! OLE2 (Compound File Binary): legacy Office files and encrypted OOXML,
//! told apart by the names of the streams in the root storage. No stream
//! content is read.
//!
//! The reader is our own and reads only what the root's names need: the
//! header, the directory sectors that hold the root's children, and the one
//! FAT entry (or DIFAT entry) that links each directory sector to the next.
//! It never builds the FAT or the DIFAT, so a header that claims a huge FAT
//! costs nothing. Its memory is bounded by its caps, not by the file:
//! [`MAX_ROOT_ENTRIES`] names, [`MAX_DIRECTORY_INDEX`] addressable directory
//! entries, and a visited set on every chain and on the directory tree.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

use crate::domain::detection::{DetectError, Detection};
use crate::domain::document::Format;

/// The root entries detection looks at.
pub const MAX_ROOT_ENTRIES: usize = 1024;

/// The highest directory entry index the reader follows. Real files have a
/// few hundred entries; an Outlook message with many attachments a few
/// thousand.
pub const MAX_DIRECTORY_INDEX: u32 = 65_536;

const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const HEADER_LEN: usize = 512;
const HEADER_DIFAT: usize = 109;
const ENTRY_LEN: u64 = 128;
const MAX_REGULAR_SECTOR: u32 = 0xFFFF_FFFA;
const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
/// `NOSTREAM` in a directory entry's sibling or child field.
const NO_STREAM: u32 = 0xFFFF_FFFF;
const TYPE_UNALLOCATED: u8 = 0;
const TYPE_STREAM: u8 = 2;
const TYPE_ROOT: u8 = 5;

/// The format of an OLE2 file.
///
/// - `EncryptionInfo` and `EncryptedPackage`: encrypted OOXML. Its type is
///   inside the encrypted package, so the format is provisional (`docx`)
///   until the office engine reports the document type.
/// - `WordDocument`: `doc`. `Workbook` or `Book`: `xls`. `PowerPoint
///   Document`: `ppt` (a `.pps` looks the same).
/// - `__substg1.0_*`: an Outlook message, refused as `msg`.
/// - Anything else, or a file the reader cannot follow, is refused as `ole2`.
pub fn detect<R: Read + Seek>(file: &mut R) -> Result<Detection, DetectError> {
    let unsupported = |label| DetectError::Unsupported { label };
    let names = root_names(file).ok_or(unsupported("ole2"))?;
    let has = |wanted: &str| names.iter().any(|name| name == wanted);
    let detection = |format| Detection {
        format,
        encrypted: false,
    };
    if has("EncryptionInfo") && has("EncryptedPackage") {
        return Ok(Detection {
            format: Format::Docx,
            encrypted: true,
        });
    }
    if names.iter().any(|name| name.starts_with("__substg1.0_")) {
        return Err(unsupported("msg"));
    }
    if has("WordDocument") {
        return Ok(detection(Format::Doc));
    }
    if has("Workbook") || has("Book") {
        return Ok(detection(Format::Xls));
    }
    if has("PowerPoint Document") {
        return Ok(detection(Format::Ppt));
    }
    Err(unsupported("ole2"))
}

/// The names of the root storage's children, at most [`MAX_ROOT_ENTRIES`],
/// in directory-tree order. `None` when the file is not a compound file the
/// reader can follow within its caps.
///
/// A cheap check stands in for validating the whole FAT: every stream among
/// those children must start inside the file. A large stream starts at a
/// sector; a small one lives in the mini stream (the root's own data) and
/// is mapped by the mini FAT, so both of those must start inside the file.
/// That refuses a truncated file, as a full reader would.
pub fn root_names<R: Read + Seek>(file: &mut R) -> Option<Vec<String>> {
    let mut reader = Reader::open(file)?;
    let root = reader.entry(0)?;
    if root.kind != TYPE_ROOT {
        return None;
    }
    let mut names = Vec::new();
    let mut small_streams = false;
    let mut visited = HashSet::new();
    let mut pending = vec![root.child];
    while let Some(index) = pending.pop() {
        if index == NO_STREAM || !visited.insert(index) {
            continue;
        }
        if names.len() >= MAX_ROOT_ENTRIES {
            break;
        }
        let entry = reader.entry(index)?;
        if entry.kind == TYPE_UNALLOCATED {
            continue;
        }
        if entry.kind == TYPE_STREAM && entry.size > 0 {
            if entry.size >= u64::from(reader.mini_cutoff) {
                reader.valid(entry.start)?;
            } else {
                small_streams = true;
            }
        }
        // The siblings are the root's other children; `child` would descend.
        pending.push(entry.right);
        pending.push(entry.left);
        names.push(entry.name);
    }
    if small_streams {
        if root.size == 0 {
            return None;
        }
        reader.valid(root.start)?;
        reader.valid(reader.first_minifat)?;
    }
    Some(names)
}

struct Entry {
    name: String,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

struct Reader<'a, R> {
    file: &'a mut R,
    sector_len: u64,
    /// Sectors in the file after the header.
    sectors: u32,
    header_difat: [u32; HEADER_DIFAT],
    first_difat: u32,
    first_dir: u32,
    first_minifat: u32,
    /// Streams smaller than this live in the mini stream.
    mini_cutoff: u32,
    /// Version 3 keeps only the low 32 bits of a stream size.
    version_3: bool,
    /// The DIFAT sectors found so far, in chain order.
    difat_chain: Vec<u32>,
    difat_seen: HashSet<u32>,
    /// The directory sectors found so far, in chain order.
    dir_chain: Vec<u32>,
    dir_seen: HashSet<u32>,
}

impl<'a, R: Read + Seek> Reader<'a, R> {
    fn open(file: &'a mut R) -> Option<Self> {
        let len = file.seek(SeekFrom::End(0)).ok()?;
        file.seek(SeekFrom::Start(0)).ok()?;
        let mut header = [0u8; HEADER_LEN];
        file.read_exact(&mut header).ok()?;
        if header[..8] != SIGNATURE || u16_at(&header, 0x1C) != 0xFFFE {
            return None;
        }
        let shift = match (u16_at(&header, 0x1A), u16_at(&header, 0x1E)) {
            (3, 9) => 9,
            (4, 12) => 12,
            _ => return None,
        };
        let sector_len = 1u64 << shift;
        // The header fills the first sector; sector n starts at (n + 1) * len.
        let sectors = u32::try_from(len.div_ceil(sector_len).saturating_sub(1))
            .unwrap_or(MAX_REGULAR_SECTOR)
            .min(MAX_REGULAR_SECTOR);
        let mut header_difat = [0u32; HEADER_DIFAT];
        for (i, slot) in header_difat.iter_mut().enumerate() {
            *slot = u32_at(&header, 0x4C + 4 * i);
        }
        Some(Self {
            file,
            sector_len,
            sectors,
            header_difat,
            first_difat: u32_at(&header, 0x44),
            first_dir: u32_at(&header, 0x30),
            first_minifat: u32_at(&header, 0x3C),
            mini_cutoff: u32_at(&header, 0x38),
            version_3: shift == 9,
            difat_chain: Vec::new(),
            difat_seen: HashSet::new(),
            dir_chain: Vec::new(),
            dir_seen: HashSet::new(),
        })
    }

    fn valid(&self, sector: u32) -> Option<u32> {
        (sector <= MAX_REGULAR_SECTOR && sector < self.sectors).then_some(sector)
    }

    fn seek(&mut self, sector: u32, offset: u64) -> Option<()> {
        let at = (u64::from(sector) + 1) * self.sector_len + offset;
        self.file.seek(SeekFrom::Start(at)).ok().map(|_| ())
    }

    fn read_u32(&mut self, sector: u32, offset: u64) -> Option<u32> {
        self.seek(sector, offset)?;
        let mut word = [0u8; 4];
        self.file.read_exact(&mut word).ok()?;
        Some(u32::from_le_bytes(word))
    }

    /// The `k`th FAT sector, from the header's DIFAT or from the DIFAT
    /// chain, walked only as far as `k` needs.
    fn fat_sector(&mut self, k: u64) -> Option<u32> {
        if let Some(&sector) = usize::try_from(k)
            .ok()
            .and_then(|k| self.header_difat.get(k))
        {
            return self.valid(sector);
        }
        let per = self.sector_len / 4 - 1;
        let index = k - HEADER_DIFAT as u64;
        let wanted = usize::try_from(index / per).ok()?;
        while self.difat_chain.len() <= wanted {
            let next = match self.difat_chain.last() {
                None => self.first_difat,
                Some(&last) => self.read_u32(last, per * 4)?,
            };
            let next = self.valid(next)?;
            if !self.difat_seen.insert(next) {
                return None;
            }
            self.difat_chain.push(next);
        }
        let difat = self.difat_chain[wanted];
        let sector = self.read_u32(difat, (index % per) * 4)?;
        self.valid(sector)
    }

    /// The sector after `sector` in its chain, from its one FAT entry.
    fn next(&mut self, sector: u32) -> Option<u32> {
        let per = self.sector_len / 4;
        let fat = self.fat_sector(u64::from(sector) / per)?;
        self.read_u32(fat, (u64::from(sector) % per) * 4)
    }

    /// Directory entry `index`; `None` past the chain or the caps.
    fn entry(&mut self, index: u32) -> Option<Entry> {
        if index >= MAX_DIRECTORY_INDEX {
            return None;
        }
        let per = self.sector_len / ENTRY_LEN;
        let wanted = usize::try_from(u64::from(index) / per).ok()?;
        while self.dir_chain.len() <= wanted {
            let next = match self.dir_chain.last() {
                None => self.first_dir,
                Some(&last) => self.next(last)?,
            };
            if next == END_OF_CHAIN {
                return None;
            }
            let next = self.valid(next)?;
            if !self.dir_seen.insert(next) {
                return None;
            }
            self.dir_chain.push(next);
        }
        let sector = self.dir_chain[wanted];
        self.seek(sector, (u64::from(index) % per) * ENTRY_LEN)?;
        let mut raw = [0u8; ENTRY_LEN as usize];
        self.file.read_exact(&mut raw).ok()?;
        // The name: UTF-16LE, its length in bytes with the terminator.
        let name_len = usize::from(u16_at(&raw, 0x40)).min(64);
        let units: Vec<u16> = raw[..name_len.saturating_sub(2)]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let size = if self.version_3 {
            u64::from(u32_at(&raw, 0x78))
        } else {
            u64::from(u32_at(&raw, 0x78)) | (u64::from(u32_at(&raw, 0x7C)) << 32)
        };
        Some(Entry {
            name: String::from_utf16_lossy(&units),
            kind: raw[0x42],
            left: u32_at(&raw, 0x44),
            right: u32_at(&raw, 0x48),
            child: u32_at(&raw, 0x4C),
            start: u32_at(&raw, 0x74),
            size,
        })
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
