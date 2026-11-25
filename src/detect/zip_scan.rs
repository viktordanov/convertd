//! Our own ZIP central-directory reader: bounded memory on hostile
//! archives, no `Seek` beyond the file, no allocation per entry.
//!
//! It reads the EOCD from the last 64 KiB + 22 bytes (and the ZIP64 EOCD
//! when present), checks the entry count before iterating, streams the
//! records through a 64 KiB buffer keeping only the names it was asked
//! for, adds up the declared sizes and checks each deflated entry's
//! declared ratio. Sizes come from the central directory, so entries with
//! a data descriptor (bit 3, as Go's `archive/zip` writes them) work.
//!
//! The caps are advisory: they read what the archive declares, which a
//! hostile file can falsify. They catch honest bombs cheaply.

use std::io::{self, BufReader, Read, Seek, SeekFrom};

use flate2::read::DeflateDecoder;

use crate::domain::detection::DetectError;

/// At most this many entries.
pub const MAX_ENTRIES: u64 = 10_000;
/// At most this many declared uncompressed bytes in all.
pub const MAX_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// At most this declared ratio per deflated entry…
pub const MAX_RATIO: u64 = 200;
/// …for entries that declare at least this much: small, very repetitive
/// parts legitimately compress better than 200:1.
pub const RATIO_MIN_BYTES: u64 = 1024 * 1024;

const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;
const EOCD_LEN: u64 = 22;
const MAX_COMMENT: u64 = 65_535;

pub const METHOD_STORED: u16 = 0;
pub const METHOD_DEFLATED: u16 = 8;

/// One entry the caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub method: u16,
    pub compressed: u64,
    pub uncompressed: u64,
    pub local_offset: u64,
}

/// Why the archive cannot be read.
#[derive(Debug)]
pub enum ScanError {
    /// Not a readable ZIP (truncated, garbage, multi-disk).
    Malformed,
    /// A declared cap is exceeded; names the cap.
    Limit(&'static str),
    Io(io::Error),
}

impl From<io::Error> for ScanError {
    fn from(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            ScanError::Malformed
        } else {
            ScanError::Io(err)
        }
    }
}

impl From<ScanError> for DetectError {
    fn from(err: ScanError) -> Self {
        match err {
            ScanError::Malformed => DetectError::Unsupported { label: "unknown" },
            ScanError::Limit(limit) => DetectError::LimitExceeded { limit },
            ScanError::Io(io) => DetectError::Io(io),
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Where the central directory is.
struct Directory {
    entries: u64,
    offset: u64,
    size: u64,
}

fn read_directory<R: Read + Seek>(file: &mut R, file_size: u64) -> Result<Directory, ScanError> {
    if file_size < EOCD_LEN {
        return Err(ScanError::Malformed);
    }
    let tail_len = file_size.min(EOCD_LEN + MAX_COMMENT);
    let tail_start = file_size - tail_len;
    file.seek(SeekFrom::Start(tail_start))?;
    let mut tail = vec![0; tail_len as usize];
    file.read_exact(&mut tail)?;
    let at = (0..=tail.len() - EOCD_LEN as usize)
        .rev()
        .find(|&i| u32_at(&tail, i) == EOCD_SIG)
        .ok_or(ScanError::Malformed)?;
    let eocd = &tail[at..];
    let (disk, cd_disk) = (u16_at(eocd, 4), u16_at(eocd, 6));
    let on_disk = u64::from(u16_at(eocd, 8));
    let mut entries = u64::from(u16_at(eocd, 10));
    let mut size = u64::from(u32_at(eocd, 12));
    let mut offset = u64::from(u32_at(eocd, 16));
    if disk != 0 || cd_disk != 0 || on_disk != entries {
        return Err(ScanError::Malformed);
    }
    let zip64 = entries == 0xFFFF || size == 0xFFFF_FFFF || offset == 0xFFFF_FFFF;
    if zip64 {
        let eocd_pos = tail_start + at as u64;
        let locator_pos = eocd_pos.checked_sub(20).ok_or(ScanError::Malformed)?;
        file.seek(SeekFrom::Start(locator_pos))?;
        let mut locator = [0; 20];
        file.read_exact(&mut locator)?;
        if u32_at(&locator, 0) != ZIP64_LOCATOR_SIG
            || u32_at(&locator, 4) != 0
            || u32_at(&locator, 16) > 1
        {
            return Err(ScanError::Malformed);
        }
        file.seek(SeekFrom::Start(u64_at(&locator, 8)))?;
        let mut record = [0; 56];
        file.read_exact(&mut record)?;
        if u32_at(&record, 0) != ZIP64_EOCD_SIG
            || u32_at(&record, 16) != 0
            || u32_at(&record, 20) != 0
        {
            return Err(ScanError::Malformed);
        }
        entries = u64_at(&record, 32);
        size = u64_at(&record, 40);
        offset = u64_at(&record, 48);
    }
    if entries > MAX_ENTRIES {
        return Err(ScanError::Limit("entry count"));
    }
    if offset.checked_add(size).is_none_or(|end| end > file_size) {
        return Err(ScanError::Malformed);
    }
    Ok(Directory {
        entries,
        offset,
        size,
    })
}

/// Scans the central directory. `found[i]` is set for each `names[i]`
/// present in the archive (the first one wins). Memory is the 64 KiB
/// buffer plus one name at a time, whatever the entry count.
pub fn scan<R: Read + Seek>(
    file: &mut R,
    file_size: u64,
    names: &[&[u8]],
    found: &mut [Option<Entry>],
) -> Result<(), ScanError> {
    let directory = read_directory(file, file_size)?;
    file.seek(SeekFrom::Start(directory.offset))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file.take(directory.size));
    let mut expanded: u64 = 0;
    let mut name = Vec::with_capacity(256);
    let mut extra = Vec::with_capacity(256);
    for _ in 0..directory.entries {
        let mut header = [0; 46];
        reader.read_exact(&mut header)?;
        if u32_at(&header, 0) != CENTRAL_SIG {
            return Err(ScanError::Malformed);
        }
        let method = u16_at(&header, 10);
        let mut compressed = u64::from(u32_at(&header, 20));
        let mut uncompressed = u64::from(u32_at(&header, 24));
        let name_len = usize::from(u16_at(&header, 28));
        let extra_len = usize::from(u16_at(&header, 30));
        let comment_len = u64::from(u16_at(&header, 32));
        let mut local_offset = u64::from(u32_at(&header, 42));

        name.resize(name_len, 0);
        reader.read_exact(&mut name)?;
        extra.resize(extra_len, 0);
        reader.read_exact(&mut extra)?;
        io::copy(&mut (&mut reader).take(comment_len), &mut io::sink())?;

        // ZIP64 extended information (0x0001): only the fields that are
        // 0xFFFFFFFF in the header are present, in this order.
        let mut at = 0;
        while at + 4 <= extra.len() {
            let (id, len) = (u16_at(&extra, at), usize::from(u16_at(&extra, at + 2)));
            let body = extra
                .get(at + 4..at + 4 + len)
                .ok_or(ScanError::Malformed)?;
            if id == 0x0001 {
                let mut fields = body
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| u64::from_le_bytes(*c));
                if uncompressed == 0xFFFF_FFFF {
                    uncompressed = fields.next().ok_or(ScanError::Malformed)?;
                }
                if compressed == 0xFFFF_FFFF {
                    compressed = fields.next().ok_or(ScanError::Malformed)?;
                }
                if local_offset == 0xFFFF_FFFF {
                    local_offset = fields.next().ok_or(ScanError::Malformed)?;
                }
            }
            at += 4 + len;
        }

        expanded = expanded.saturating_add(uncompressed);
        if expanded > MAX_EXPANDED_BYTES {
            return Err(ScanError::Limit("expanded size"));
        }
        if method == METHOD_DEFLATED
            && uncompressed >= RATIO_MIN_BYTES
            && uncompressed / compressed.max(1) > MAX_RATIO
        {
            return Err(ScanError::Limit("compression ratio"));
        }
        if let Some(i) = names.iter().position(|wanted| *wanted == name.as_slice())
            && found[i].is_none()
        {
            found[i] = Some(Entry {
                method,
                compressed,
                uncompressed,
                local_offset,
            });
        }
    }
    Ok(())
}

/// Reads at most `cap` bytes of one entry's content, inflating a deflated
/// entry. `None` when the entry is larger than `cap` or uses another
/// method.
pub fn read_entry<R: Read + Seek>(
    file: &mut R,
    entry: &Entry,
    cap: usize,
) -> Result<Option<Vec<u8>>, ScanError> {
    file.seek(SeekFrom::Start(entry.local_offset))?;
    let mut local = [0; 30];
    file.read_exact(&mut local)?;
    if u32_at(&local, 0) != LOCAL_SIG {
        return Err(ScanError::Malformed);
    }
    let skip = i64::from(u16_at(&local, 26)) + i64::from(u16_at(&local, 28));
    file.seek(SeekFrom::Current(skip))?;
    let data = file.take(entry.compressed);
    let limit = cap as u64 + 1;
    let mut out = Vec::with_capacity(cap.min(64 * 1024));
    match entry.method {
        METHOD_STORED => data.take(limit).read_to_end(&mut out)?,
        METHOD_DEFLATED => DeflateDecoder::new(data)
            .take(limit)
            .read_to_end(&mut out)
            .map_err(|err| {
                if err.kind() == io::ErrorKind::InvalidInput
                    || err.kind() == io::ErrorKind::InvalidData
                {
                    ScanError::Malformed
                } else {
                    ScanError::from(err)
                }
            })?,
        _ => return Ok(None),
    };
    Ok((out.len() <= cap).then_some(out))
}
