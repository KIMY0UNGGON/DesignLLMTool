//! ZIP container layer.
//!
//! `Package::open` reads every entry once, validates names and sizes, and keeps the original
//! archive bytes. `Package::encode` writes entries in their original order. An entry whose payload
//! was not replaced is copied from the source archive together with its local header, its compressed
//! data, and its data descriptor, and its central directory record is copied with only the local header
//! offset changed. A copy that cannot be shown to be consistent is refused. Replaced and added entries
//! are deflated by the zip crate with a fixed timestamp, which keeps repeated encodes of the same model
//! deterministic.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read, Write};
use std::ops::Range;
use std::sync::Arc;

use designllm_contracts::{sha256, Diagnostic, DltError, DltResult, ErrorCode, PartFingerprint, PreservationManifest};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, System, ZipArchive, ZipWriter};

use crate::opc::ContentTypes;

/// Diagnostic codes emitted by this layer. Engines may forward them unchanged.
pub const DIAG_MALFORMED: &str = "PACKAGE_MALFORMED";
pub const DIAG_DUPLICATE: &str = "PACKAGE_DUPLICATE_ENTRY";
pub const DIAG_UNSAFE_NAME: &str = "PACKAGE_UNSAFE_PART_NAME";
pub const DIAG_LIMIT: &str = "PACKAGE_LIMIT_EXCEEDED";
pub const DIAG_ENCRYPTED: &str = "PACKAGE_ENCRYPTED_ENTRY";
pub const DIAG_METHOD: &str = "PACKAGE_UNSUPPORTED_METHOD";
pub const DIAG_SIZE: &str = "PACKAGE_SIZE_MISMATCH";

/// Fixed timestamp for entries written from new or replaced payloads (2026-11-12 00:00:00).
const WRITE_TIMESTAMP: (u16, u8, u8, u8, u8, u8) = (2026, 11, 12, 0, 0, 0);
const MAX_PART_NAME_BYTES: usize = 1024;
const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const LOCAL_HEADER_LEN: usize = 30;
const CENTRAL_HEADER_LEN: usize = 46;
const EOCD_LEN: usize = 22;
const LOCAL_SIG: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
const DESCRIPTOR_SIG: [u8; 4] = [0x50, 0x4b, 0x07, 0x08];
/// Bit 3: the CRC and sizes follow the data in a data descriptor, and the local header holds zeros.
const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
/// Bit 11: names are UTF-8.
const FLAG_UTF8: u16 = 0x0800;
/// General purpose bits a copy cannot honour: encryption (bit 0), strong encryption (bit 6), and masked header values (bit 13).
const ENCRYPTION_FLAGS: u16 = 0x0001 | 0x0040 | 0x2000;

/// Whether bytes start with the CFB header. This does not identify the document type or protection state.
pub fn is_cfb_container(bytes: &[u8]) -> bool {
    bytes.starts_with(&CFB_MAGIC)
}

/// Resource limits applied while opening a package. The defaults match the product limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted input archive, in bytes.
    pub max_package_bytes: usize,
    /// Largest accepted number of archive entries.
    pub max_entries: usize,
    /// Largest accepted uncompressed size of one entry.
    pub max_entry_bytes: u64,
    /// Largest accepted total uncompressed size of all entries.
    pub max_total_bytes: u64,
    /// Largest accepted ratio of uncompressed to compressed size for entries of at least `ratio_check_min_bytes`.
    pub max_compression_ratio: u64,
    /// Entries smaller than this are exempt from the ratio check.
    pub ratio_check_min_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_package_bytes: 512 * 1024 * 1024,
            max_entries: 10_000,
            max_entry_bytes: 256 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            max_compression_ratio: 1000,
            ratio_check_min_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    data: Vec<u8>,
    /// Position of the entry in the source archive. Its bytes are copied from there while `modified` is false.
    source_index: Option<usize>,
    /// True when the payload was replaced or the entry was added after `open`.
    modified: bool,
}

/// An OPC/ZIP package held in memory.
///
/// Part names are case-sensitive for lookup, but a second name that differs only by ASCII case is
/// rejected, because OPC part names are case-insensitive.
#[derive(Clone, Debug, Default)]
pub struct Package {
    entries: Vec<Entry>,
    index: BTreeMap<String, usize>,
    source: Option<Arc<[u8]>>,
}

impl Package {
    /// Creates an empty package with no source archive. Every entry is written deflated.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens an archive with the default limits.
    pub fn open(bytes: &[u8]) -> DltResult<Self> {
        Self::open_with_limits(bytes, &Limits::default())
    }

    /// Opens an archive with explicit limits. Malformed, traversal, duplicate, encrypted, and oversized
    /// input is rejected with a diagnostic code; a partial package is never returned.
    pub fn open_with_limits(bytes: &[u8], limits: &Limits) -> DltResult<Self> {
        if bytes.len() > limits.max_package_bytes {
            return Err(error(ErrorCode::InvalidArgument, DIAG_LIMIT, "package exceeds the input size limit"));
        }
        let source: Arc<[u8]> = Arc::from(bytes);
        let scanned = scan_central_directory(bytes)?.records.len();
        if scanned > limits.max_entries {
            return Err(error(ErrorCode::InvalidArgument, DIAG_LIMIT, "package has too many entries"));
        }
        let mut entries = Vec::new();
        {
            let mut archive = ZipArchive::new(Cursor::new(&source[..]))
                .map_err(|e| error(ErrorCode::InvalidArgument, DIAG_MALFORMED, format!("ZIP directory is unreadable: {e}")))?;
            // The zip crate keeps one entry per name, so a duplicate central record would vanish silently.
            // The raw scan above counts every record; the two counts must agree.
            if archive.len() != scanned {
                return Err(error(
                    ErrorCode::InvalidArgument,
                    DIAG_DUPLICATE,
                    format!("central directory lists {} records but {} distinct entries", scanned, archive.len()),
                ));
            }
            let mut seen_lowercase = BTreeSet::new();
            let mut total: u64 = 0;
            for index in 0..archive.len() {
                let (name, declared, compressed, encrypted, method) = {
                    let raw = archive
                        .by_index_raw(index)
                        .map_err(|e| error(ErrorCode::InvalidArgument, DIAG_MALFORMED, format!("entry {index} is unreadable: {e}")))?;
                    let name = raw
                        .name()
                        .map_err(|e| error(ErrorCode::InvalidArgument, DIAG_MALFORMED, format!("entry {index} has an invalid name: {e}")))?
                        .into_owned();
                    (name, raw.size(), raw.compressed_size(), raw.encrypted(), raw.compression())
                };
                validate_name(&name, true)?;
                if !seen_lowercase.insert(name.to_ascii_lowercase()) {
                    return Err(error(ErrorCode::InvalidArgument, DIAG_DUPLICATE, format!("duplicate package entry {name:?}")));
                }
                if encrypted {
                    return Err(error(ErrorCode::UnsupportedOperation, DIAG_ENCRYPTED, format!("encrypted entry {name:?} is not supported")));
                }
                if !matches!(method, CompressionMethod::Stored | CompressionMethod::Deflated) {
                    return Err(error(ErrorCode::UnsupportedOperation, DIAG_METHOD, format!("entry {name:?} uses an unsupported compression method")));
                }
                if declared > limits.max_entry_bytes {
                    return Err(error(ErrorCode::InvalidArgument, DIAG_LIMIT, format!("entry {name:?} exceeds the per-entry size limit")));
                }
                total = total.saturating_add(declared);
                if total > limits.max_total_bytes {
                    return Err(error(ErrorCode::InvalidArgument, DIAG_LIMIT, "package exceeds the total uncompressed size limit"));
                }
                if declared >= limits.ratio_check_min_bytes
                    && u128::from(declared) > u128::from(compressed) * u128::from(limits.max_compression_ratio)
                {
                    return Err(error(ErrorCode::InvalidArgument, DIAG_LIMIT, format!("entry {name:?} has a suspicious compression ratio")));
                }
                let mut data = Vec::new();
                {
                    let mut file = archive
                        .by_index(index)
                        .map_err(|e| error(ErrorCode::InvalidArgument, DIAG_MALFORMED, format!("entry {name:?} is corrupt: {e}")))?;
                    // Read at most one byte past the declared size, so a lying header cannot grow the buffer.
                    (&mut file)
                        .take(declared.saturating_add(1))
                        .read_to_end(&mut data)
                        .map_err(|e| error(ErrorCode::InvalidArgument, DIAG_MALFORMED, format!("entry {name:?} is corrupt: {e}")))?;
                }
                if data.len() as u64 != declared {
                    return Err(error(ErrorCode::InvalidArgument, DIAG_SIZE, format!("entry {name:?} size does not match its header")));
                }
                entries.push(Entry { name, data, source_index: Some(index), modified: false });
            }
        }
        let index = build_index(&entries);
        Ok(Self { entries, index, source: Some(source) })
    }

    /// Entry names in archive order (new entries follow the original order).
    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// True when an entry with exactly this name exists.
    pub fn has_part(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// Decoded payload of an entry.
    pub fn part(&self, name: &str) -> DltResult<&[u8]> {
        self.index
            .get(name)
            .map(|&i| self.entries[i].data.as_slice())
            .ok_or_else(|| error(ErrorCode::NotFound, "PACKAGE_PART_NOT_FOUND", format!("package part not found: {name}")))
    }

    /// True when the payload was replaced or added after `open`. Entries from a new package are always modified.
    pub fn is_modified(&self, name: &str) -> DltResult<bool> {
        self.index
            .get(name)
            .map(|&i| self.entries[i].modified)
            .ok_or_else(|| error(ErrorCode::NotFound, "PACKAGE_PART_NOT_FOUND", format!("package part not found: {name}")))
    }

    /// Replaces the payload of an existing entry, or adds a new entry at the end.
    ///
    /// Setting identical bytes keeps the entry unmodified. A name that differs from an existing name only
    /// by ASCII case is rejected. Directory names (ending in `/`) are not accepted here.
    pub fn set_part(&mut self, name: &str, bytes: Vec<u8>) -> DltResult<()> {
        validate_name(name, false)?;
        if bytes.len() as u64 > Limits::default().max_entry_bytes {
            return Err(error(ErrorCode::InvalidArgument, DIAG_LIMIT, format!("part {name:?} exceeds the per-entry size limit")));
        }
        if let Some(&position) = self.index.get(name) {
            let entry = &mut self.entries[position];
            if entry.data == bytes {
                return Ok(());
            }
            entry.data = bytes;
            entry.modified = true;
            return Ok(());
        }
        if self.entries.iter().any(|e| e.name.eq_ignore_ascii_case(name)) {
            return Err(error(
                ErrorCode::AlreadyExists,
                DIAG_DUPLICATE,
                format!("part {name:?} differs from an existing part only by case"),
            ));
        }
        if self.entries.len() >= Limits::default().max_entries {
            return Err(error(ErrorCode::InvalidArgument, DIAG_LIMIT, "package has too many entries"));
        }
        self.index.insert(name.to_string(), self.entries.len());
        self.entries.push(Entry { name: name.to_string(), data: bytes, source_index: None, modified: true });
        Ok(())
    }

    /// Removes an entry. Removing a part that another part still references is the caller's error to prevent.
    pub fn remove_part(&mut self, name: &str) -> DltResult<()> {
        let position = self
            .index
            .get(name)
            .copied()
            .ok_or_else(|| error(ErrorCode::NotFound, "PACKAGE_PART_NOT_FOUND", format!("package part not found: {name}")))?;
        self.entries.remove(position);
        self.index = build_index(&self.entries);
        Ok(())
    }

    /// Serializes the package. Unmodified entries are copied from the source archive (see `entry_span`);
    /// replaced and added entries are compressed by the zip crate.
    pub fn encode(&self) -> DltResult<Vec<u8>> {
        self.encode_within(Limits::default().max_package_bytes)
    }

    /// `encode`, refusing an output larger than `max_bytes`.
    fn encode_within(&self, max_bytes: usize) -> DltResult<Vec<u8>> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for entry in self.entries.iter().filter(|entry| !is_copied(entry)) {
            writer
                .start_file(entry.name.as_str(), entry_options()?)
                .map_err(|e| error(ErrorCode::Io, "PACKAGE_WRITE_FAILED", format!("cannot start {}: {e}", entry.name)))?;
            writer
                .write_all(&entry.data)
                .map_err(|e| error(ErrorCode::Io, "PACKAGE_WRITE_FAILED", format!("cannot write {}: {e}", entry.name)))?;
        }
        let fresh = writer
            .finish()
            .map_err(|e| error(ErrorCode::Io, "PACKAGE_WRITE_FAILED", format!("cannot finish archive: {e}")))?
            .into_inner();
        let fresh_directory = scan_central_directory(&fresh)?;

        let source = self.source.as_deref();
        let source_directory = match source {
            Some(bytes) => Some(scan_central_directory(bytes)?),
            None => None,
        };
        // Every entry's bytes and central record are chosen before any output is allocated.
        let mut pieces = Vec::with_capacity(self.entries.len());
        let mut fresh_records = fresh_directory.records.iter();
        for entry in &self.entries {
            let piece = if is_copied(entry) {
                let (Some(bytes), Some(directory), Some(index)) = (source, source_directory.as_ref(), entry.source_index) else {
                    return Err(error(ErrorCode::Internal, DIAG_MALFORMED, format!("entry {:?} has no source record", entry.name)));
                };
                let record = directory
                    .records
                    .get(index)
                    .ok_or_else(|| error(ErrorCode::Internal, DIAG_MALFORMED, format!("entry {:?} has no source record", entry.name)))?;
                Piece { archive: bytes, span: entry_span(bytes, directory, record, &entry.name)?, record: *record, copied: true }
            } else {
                let record = fresh_records
                    .next()
                    .ok_or_else(|| error(ErrorCode::Internal, DIAG_MALFORMED, format!("entry {:?} has no written record", entry.name)))?;
                let archive = fresh.as_slice();
                Piece { archive, span: entry_span(archive, &fresh_directory, record, &entry.name)?, record: *record, copied: false }
            };
            pieces.push(piece);
        }

        // Copied spans must not overlap: an overlap copies the same bytes twice, and a crafted compressed size could
        // make the output many times larger than the input.
        let mut source_spans: Vec<Range<usize>> = pieces.iter().filter(|piece| piece.copied).map(|piece| piece.span.clone()).collect();
        source_spans.sort_by_key(|span| span.start);
        if source_spans.windows(2).any(|pair| pair[0].end > pair[1].start) {
            return Err(error(ErrorCode::InvalidArgument, DIAG_MALFORMED, "entries overlap in the source archive"));
        }

        // The size of the output is known now, so an oversized save is refused before the output is allocated.
        let planned = pieces
            .iter()
            .map(|piece| (piece.span.len() + piece.record.end - piece.record.start) as u64)
            .sum::<u64>()
            + EOCD_LEN as u64;
        if planned > max_bytes as u64 {
            return Err(error(ErrorCode::InvalidArgument, DIAG_LIMIT, "the saved package would exceed the size limit"));
        }
        let mut out = Vec::with_capacity(planned as usize);
        let mut central = Vec::new();
        for piece in &pieces {
            let local_offset = out.len();
            out.extend_from_slice(&piece.archive[piece.span.clone()]);
            push_central_record(&mut central, piece.archive, &piece.record, local_offset)?;
        }
        let central_offset = out.len();
        out.extend_from_slice(&central);
        push_end_of_central_directory(&mut out, self.entries.len(), central.len(), central_offset)?;
        Ok(out)
    }

    /// Hashes every decoded payload. `immutable` is true for parts that still match the opened source and
    /// false for parts that were replaced or added. Engines use this to decide which parts they may change.
    /// Directory entries are omitted.
    pub fn preservation_manifest(&self) -> PreservationManifest {
        let content_types = self
            .part(crate::opc::CONTENT_TYPES_PART)
            .ok()
            .and_then(|bytes| ContentTypes::parse(bytes).ok());
        let mut parts = BTreeMap::new();
        for entry in &self.entries {
            if entry.name.ends_with('/') {
                continue;
            }
            let content_type = content_types
                .as_ref()
                .and_then(|ct| ct.content_type_for(&entry.name))
                .map(str::to_string);
            parts.insert(
                entry.name.clone(),
                PartFingerprint { sha256: sha256(&entry.data), content_type, immutable: !entry.modified },
            );
        }
        PreservationManifest { parts, opaque_regions: vec![] }
    }
}

fn build_index(entries: &[Entry]) -> BTreeMap<String, usize> {
    entries.iter().enumerate().map(|(i, e)| (e.name.clone(), i)).collect()
}

/// One central directory record, with the fields that a copy has to check.
#[derive(Clone, Copy, Debug)]
struct CentralRecord {
    /// Byte range of the whole record: fixed header, name, extra field, and comment.
    start: usize,
    end: usize,
    name_len: usize,
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    disk_start: u16,
    local_offset: u32,
}

/// Where the central directory starts, and its records in directory order.
#[derive(Debug)]
struct CentralDirectory {
    start: usize,
    records: Vec<CentralRecord>,
}

/// Reads every central-directory record directly from the bytes, without the zip crate.
/// The number of records must equal the end-of-directory count. Duplicates are reported by the caller.
fn scan_central_directory(bytes: &[u8]) -> DltResult<CentralDirectory> {
    const EOCD: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
    const ZIP64_LOCATOR: [u8; 4] = [0x50, 0x4b, 0x06, 0x07];
    const ZIP64_EOCD: [u8; 4] = [0x50, 0x4b, 0x06, 0x06];
    const CENTRAL: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
    let malformed = |m: &str| error(ErrorCode::InvalidArgument, DIAG_MALFORMED, m.to_string());
    // `at + len` is checked, so an offset taken from the file cannot wrap around.
    let field = |at: usize, len: usize| at.checked_add(len).and_then(|end| bytes.get(at..end));
    let u16_at = |at: usize| field(at, 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let u32_at = |at: usize| field(at, 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    let u64_at = |at: usize| {
        field(at, 8).map(|s| {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(s);
            u64::from_le_bytes(buf)
        })
    };

    if bytes.len() < 22 {
        return Err(malformed("end of central directory not found"));
    }
    let search_from = bytes.len().saturating_sub(22 + 65_535);
    let eocd = (search_from..=bytes.len() - 22)
        .rev()
        .find(|&i| bytes.get(i..i + 4) == Some(&EOCD[..]))
        .ok_or_else(|| malformed("end of central directory not found"))?;
    let mut total = u64::from(u16_at(eocd + 10).ok_or_else(|| malformed("truncated end of central directory"))?);
    let mut cd_size = u64::from(u32_at(eocd + 12).ok_or_else(|| malformed("truncated end of central directory"))?);
    let mut cd_offset = u64::from(u32_at(eocd + 16).ok_or_else(|| malformed("truncated end of central directory"))?);
    if total == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
        let locator = eocd.checked_sub(20).ok_or_else(|| malformed("zip64 locator is missing"))?;
        if field(locator, 4) != Some(&ZIP64_LOCATOR[..]) {
            return Err(malformed("zip64 locator is missing"));
        }
        // The record offset comes from the file: it must fit a usize, and every later offset is checked.
        let record = usize::try_from(u64_at(locator + 8).ok_or_else(|| malformed("zip64 locator is truncated"))?)
            .map_err(|_| malformed("zip64 end of central directory lies outside the archive"))?;
        if field(record, 4) != Some(&ZIP64_EOCD[..]) {
            return Err(malformed("zip64 end of central directory is missing"));
        }
        let zip64_u64 = |offset: usize| {
            record.checked_add(offset).and_then(|at| u64_at(at)).ok_or_else(|| malformed("zip64 record is truncated"))
        };
        total = zip64_u64(32)?;
        cd_size = zip64_u64(40)?;
        cd_offset = zip64_u64(48)?;
    }
    let start = usize::try_from(cd_offset).map_err(|_| malformed("central directory offset is out of range"))?;
    let end = usize::try_from(cd_offset.saturating_add(cd_size)).map_err(|_| malformed("central directory size is out of range"))?;
    if end > bytes.len() || start > end {
        return Err(malformed("central directory lies outside the archive"));
    }
    let mut records = Vec::new();
    let mut position = start;
    while position < end {
        if bytes.get(position..position + 4) != Some(&CENTRAL[..]) {
            return Err(malformed("central directory record expected"));
        }
        let name_len = usize::from(u16_at(position + 28).ok_or_else(|| malformed("central record is truncated"))?);
        let extra_len = usize::from(u16_at(position + 30).ok_or_else(|| malformed("central record is truncated"))?);
        let comment_len = usize::from(u16_at(position + 32).ok_or_else(|| malformed("central record is truncated"))?);
        let record_end = position + CENTRAL_HEADER_LEN + name_len + extra_len + comment_len;
        if record_end > end {
            return Err(malformed("central record lies outside the central directory"));
        }
        records.push(CentralRecord {
            start: position,
            end: record_end,
            name_len,
            flags: u16_at(position + 8).ok_or_else(|| malformed("central record is truncated"))?,
            method: u16_at(position + 10).ok_or_else(|| malformed("central record is truncated"))?,
            crc32: u32_at(position + 16).ok_or_else(|| malformed("central record is truncated"))?,
            compressed_size: u32_at(position + 20).ok_or_else(|| malformed("central record is truncated"))?,
            uncompressed_size: u32_at(position + 24).ok_or_else(|| malformed("central record is truncated"))?,
            disk_start: u16_at(position + 34).ok_or_else(|| malformed("central record is truncated"))?,
            local_offset: u32_at(position + 42).ok_or_else(|| malformed("central record is truncated"))?,
        });
        position = record_end;
    }
    if records.len() as u64 != total {
        return Err(malformed("central directory record count does not match the end record"));
    }
    Ok(CentralDirectory { start, records })
}

/// One entry of the output: the bytes to copy from `archive`, and the central record that describes them.
struct Piece<'a> {
    archive: &'a [u8],
    span: Range<usize>,
    record: CentralRecord,
    /// True when the bytes come from the source archive, not from the replaced or added entries.
    copied: bool,
}

/// True when the entry's bytes come from the source archive, because its payload was not replaced.
fn is_copied(entry: &Entry) -> bool {
    !entry.modified && entry.source_index.is_some()
}

/// The byte range of one entry in `archive`: its local header, its compressed data, and its data descriptor.
///
/// The range is copied unchanged. Every field that a reader uses to find or check the entry must already agree
/// with the central record, so a combination that cannot be shown to agree is refused rather than rewritten.
fn entry_span(archive: &[u8], directory: &CentralDirectory, record: &CentralRecord, name: &str) -> DltResult<Range<usize>> {
    let malformed = |why: &str| error(ErrorCode::InvalidArgument, DIAG_MALFORMED, format!("cannot copy {name:?} unchanged: {why}"));
    let unsupported = |diagnostic: &str, why: &str| {
        error(ErrorCode::UnsupportedOperation, diagnostic, format!("cannot copy {name:?} unchanged: {why}"))
    };
    if record.disk_start != 0 {
        return Err(unsupported(DIAG_MALFORMED, "the entry starts on another disk"));
    }
    if record.local_offset == u32::MAX || record.compressed_size == u32::MAX || record.uncompressed_size == u32::MAX {
        return Err(unsupported(DIAG_LIMIT, "the entry uses ZIP64 fields"));
    }
    if record.flags & ENCRYPTION_FLAGS != 0 {
        return Err(error(ErrorCode::UnsupportedOperation, DIAG_ENCRYPTED, format!("encrypted entry {name:?} is not supported")));
    }
    if record.method == 0 && record.compressed_size != record.uncompressed_size {
        return Err(malformed("a stored entry has different compressed and uncompressed sizes"));
    }
    let central_name = &archive[record.start + CENTRAL_HEADER_LEN..record.start + CENTRAL_HEADER_LEN + record.name_len];
    let local = record.local_offset as usize;
    let header = archive
        .get(local..local + LOCAL_HEADER_LEN)
        .ok_or_else(|| malformed("the local header is outside the archive"))?;
    if header[..4] != LOCAL_SIG {
        return Err(malformed("the local header signature is missing"));
    }
    let local_flags = le16(header, 6);
    let local_method = le16(header, 8);
    let local_sizes = (le32(header, 14), le32(header, 18), le32(header, 22));
    let name_len = usize::from(le16(header, 26));
    let extra_len = usize::from(le16(header, 28));
    let name_at = local + LOCAL_HEADER_LEN;
    let data_at = name_at + name_len + extra_len;
    if data_at > directory.start {
        return Err(malformed("the local header runs into the central directory"));
    }
    if archive.get(name_at..name_at + name_len) != Some(central_name) {
        return Err(malformed("the local name differs from the central record"));
    }
    if local_method != record.method {
        return Err(malformed("the local compression method differs from the central record"));
    }
    if local_flags & ENCRYPTION_FLAGS != 0 {
        return Err(error(ErrorCode::UnsupportedOperation, DIAG_ENCRYPTED, format!("encrypted entry {name:?} is not supported")));
    }
    if (local_flags ^ record.flags) & (FLAG_UTF8 | FLAG_DATA_DESCRIPTOR) != 0 {
        return Err(malformed("the UTF-8 or data descriptor flag differs from the central record"));
    }
    if local_sizes.1 == u32::MAX || local_sizes.2 == u32::MAX {
        return Err(unsupported(DIAG_LIMIT, "the local header uses ZIP64 sizes"));
    }
    let central_sizes = (record.crc32, record.compressed_size, record.uncompressed_size);
    let local_zeroed = local_sizes == (0, 0, 0) && local_flags & FLAG_DATA_DESCRIPTOR != 0;
    if !local_zeroed && local_sizes != central_sizes {
        return Err(malformed("the local CRC or sizes differ from the central record"));
    }
    let data_end = data_at + record.compressed_size as usize;
    if data_end > directory.start {
        return Err(malformed("the entry data runs into the central directory"));
    }
    let descriptor = if local_flags & FLAG_DATA_DESCRIPTOR == 0 {
        0
    } else {
        // The descriptor width comes from the local ZIP64 field; a malformed extra block leaves it unknown.
        let zip64 = extra_has_zip64(&archive[name_at + name_len..data_at])
            .ok_or_else(|| malformed("the local extra field is malformed, so the descriptor width is unknown"))?;
        descriptor_len(archive, data_end, directory.start, central_sizes, zip64)
            .ok_or_else(|| malformed("the data descriptor does not match the central record"))?
    };
    Ok(local..data_end + descriptor)
}

/// Length of the data descriptor at `at`, or `None` when it does not match the central record. Its sizes are
/// 8 bytes when the local header has a ZIP64 extra field and 4 bytes otherwise. The signature is optional and is
/// recognised by its first four bytes; no other layout is tried.
fn descriptor_len(archive: &[u8], at: usize, limit: usize, expected: (u32, u32, u32), zip64: bool) -> Option<usize> {
    let signature = if archive.get(at..at + 4) == Some(&DESCRIPTOR_SIG[..]) { 4 } else { 0 };
    let width = if zip64 { 8 } else { 4 };
    let len = signature + 4 + 2 * width;
    if at.checked_add(len)? > limit {
        return None;
    }
    let body = archive.get(at + signature..at + len)?;
    let (compressed, uncompressed) = if zip64 {
        (le64(body, 4), le64(body, 12))
    } else {
        (u64::from(le32(body, 4)), u64::from(le32(body, 8)))
    };
    let (crc, expected_compressed, expected_uncompressed) = expected;
    let matches = le32(body, 0) == crc
        && compressed == u64::from(expected_compressed)
        && uncompressed == u64::from(expected_uncompressed);
    matches.then_some(len)
}

/// Whether an extra field block holds a ZIP64 extended information field (0x0001). `None` when the block is malformed.
fn extra_has_zip64(extra: &[u8]) -> Option<bool> {
    let mut at = 0;
    let mut found = false;
    while at < extra.len() {
        let header = extra.get(at..at + 4)?;
        let end = (at + 4).checked_add(usize::from(le16(header, 2)))?;
        if end > extra.len() {
            return None;
        }
        found |= le16(header, 0) == 0x0001;
        at = end;
    }
    Some(found)
}

/// Appends a central record copied from `archive`, with its local header offset set to `local_offset`.
fn push_central_record(out: &mut Vec<u8>, archive: &[u8], record: &CentralRecord, local_offset: usize) -> DltResult<()> {
    let offset = u32::try_from(local_offset)
        .map_err(|_| error(ErrorCode::InvalidArgument, DIAG_LIMIT, "package is too large for 32-bit ZIP offsets"))?;
    let at = out.len();
    out.extend_from_slice(&archive[record.start..record.end]);
    out[at + 42..at + 46].copy_from_slice(&offset.to_le_bytes());
    Ok(())
}

/// Appends an end-of-central-directory record without a comment.
fn push_end_of_central_directory(out: &mut Vec<u8>, entries: usize, size: usize, offset: usize) -> DltResult<()> {
    let too_large = || error(ErrorCode::InvalidArgument, DIAG_LIMIT, "package is too large for a 32-bit ZIP directory");
    let count = u16::try_from(entries).map_err(|_| too_large())?;
    let size = u32::try_from(size).map_err(|_| too_large())?;
    let offset = u32::try_from(offset).map_err(|_| too_large())?;
    out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    Ok(())
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(raw)
}

/// Rejects names that are empty, absolute, contain backslashes, colons, control characters, or `.`/`..`
/// segments, and in `allow_directories = false` mode also names ending in `/`.
pub fn validate_name(name: &str, allow_directories: bool) -> DltResult<()> {
    let reject = |why: &str| error(ErrorCode::InvalidArgument, DIAG_UNSAFE_NAME, format!("unsafe part name {name:?}: {why}"));
    if name.is_empty() {
        return Err(reject("empty"));
    }
    if name.len() > MAX_PART_NAME_BYTES {
        return Err(reject("too long"));
    }
    if name.starts_with('/') {
        return Err(reject("absolute path"));
    }
    if name.contains('\\') || name.contains(':') {
        return Err(reject("backslash or colon"));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(reject("control character"));
    }
    let body = match name.strip_suffix('/') {
        Some(stripped) if allow_directories => stripped,
        Some(_) => return Err(reject("directory name")),
        None => name,
    };
    if body.is_empty() {
        return Err(reject("empty"));
    }
    for segment in body.split('/') {
        match segment {
            "" => return Err(reject("empty segment")),
            "." | ".." => {
                return Err(error(ErrorCode::InvalidArgument, DIAG_UNSAFE_NAME, format!("path traversal in {name:?}")))
            }
            _ => {}
        }
    }
    Ok(())
}

fn entry_options() -> DltResult<SimpleFileOptions> {
    let (year, month, day, hour, minute, second) = WRITE_TIMESTAMP;
    let stamp = DateTime::from_date_and_time(year, month, day, hour, minute, second)
        .map_err(|e| error(ErrorCode::Internal, "PACKAGE_WRITE_FAILED", format!("fixed timestamp is invalid: {e:?}")))?;
    Ok(SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(stamp)
        .unix_permissions(0o644)
        // The zip crate writes the host OS into "version made by" unless told: fixed, so every OS writes the same bytes.
        .system(System::Dos))
}

/// Builds a `DltError` that carries one blocking diagnostic with the given code.
pub fn error(code: ErrorCode, diagnostic: &str, message: impl Into<String>) -> DltError {
    let message = message.into();
    let mut err = DltError::new(code, message.clone());
    err.diagnostics.push(Diagnostic { code: diagnostic.to_string(), message, node_id: None, blocking: true, params: Default::default() });
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_reject_traversal_absolute_backslash_and_colon() {
        for bad in ["../evil.xml", "a/../../b", "/abs.xml", "a\\b.xml", "C:x.xml", "a//b", "./a", "a/./b", ""] {
            let err = validate_name(bad, true).unwrap_err();
            assert!(!err.diagnostics.is_empty(), "{bad:?} must carry a diagnostic");
        }
        validate_name("ppt/slides/slide1.xml", false).unwrap();
        validate_name("ppt/media/", true).unwrap();
        assert!(validate_name("ppt/media/", false).is_err());
    }

    #[test]
    fn set_part_rejects_case_only_collision_and_keeps_identical_bytes_unmodified() {
        let mut pkg = Package::new();
        pkg.set_part("ppt/Slide1.xml", b"<a/>".to_vec()).unwrap();
        let collision = pkg.set_part("ppt/slide1.xml", b"<b/>".to_vec()).unwrap_err();
        assert_eq!(collision.code, ErrorCode::AlreadyExists);
        pkg.set_part("ppt/Slide1.xml", b"<a/>".to_vec()).unwrap();
        assert_eq!(pkg.part("ppt/Slide1.xml").unwrap(), b"<a/>");
        assert_eq!(pkg.part("ppt/missing.xml").unwrap_err().code, ErrorCode::NotFound);
    }

    #[test]
    fn new_package_roundtrips_and_reports_immutability() {
        let mut pkg = Package::new();
        pkg.set_part("[Content_Types].xml", b"<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"xml\" ContentType=\"application/xml\"/></Types>".to_vec()).unwrap();
        pkg.set_part("docs/a.xml", b"<a>text</a>".to_vec()).unwrap();
        let bytes = pkg.encode().unwrap();
        let reopened = Package::open(&bytes).unwrap();
        assert_eq!(reopened.names(), pkg.names());
        assert_eq!(reopened.part("docs/a.xml").unwrap(), b"<a>text</a>");
        let manifest = reopened.preservation_manifest();
        assert_eq!(manifest.parts["docs/a.xml"].sha256, sha256(b"<a>text</a>"));
        assert_eq!(manifest.parts["docs/a.xml"].content_type.as_deref(), Some("application/xml"));
        assert!(manifest.parts.values().all(|p| p.immutable), "a freshly opened package is fully immutable");
    }

    #[test]
    fn unmodified_entries_keep_their_compressed_bytes_and_modified_ones_are_flagged() {
        let mut pkg = Package::new();
        pkg.set_part("a.txt", b"alpha alpha alpha alpha".repeat(40)).unwrap();
        pkg.set_part("b.txt", b"beta beta beta".repeat(40)).unwrap();
        let first = pkg.encode().unwrap();
        let mut reopened = Package::open(&first).unwrap();
        reopened.set_part("b.txt", b"gamma".to_vec()).unwrap();
        assert!(!reopened.is_modified("a.txt").unwrap());
        assert!(reopened.is_modified("b.txt").unwrap());
        let second = reopened.encode().unwrap();
        assert_eq!(raw_compressed(&first, "a.txt"), raw_compressed(&second, "a.txt"));
        let manifest = Package::open(&second).unwrap().preservation_manifest();
        assert!(manifest.parts["a.txt"].immutable);
        assert_eq!(Package::open(&second).unwrap().part("b.txt").unwrap(), b"gamma");
    }

    #[test]
    fn remove_part_and_missing_remove_are_reported() {
        let mut pkg = Package::new();
        pkg.set_part("x.xml", b"<x/>".to_vec()).unwrap();
        pkg.set_part("y.xml", b"<y/>".to_vec()).unwrap();
        pkg.remove_part("x.xml").unwrap();
        assert_eq!(pkg.names(), vec!["y.xml".to_string()]);
        assert_eq!(pkg.remove_part("x.xml").unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(Package::open(&pkg.encode().unwrap()).unwrap().names(), vec!["y.xml".to_string()]);
    }

    #[test]
    fn truncated_and_non_zip_inputs_are_rejected_without_panic() {
        assert_eq!(Package::open(b"not a zip").unwrap_err().code, ErrorCode::InvalidArgument);
        let mut pkg = Package::new();
        pkg.set_part("a.xml", b"<a/>".repeat(100)).unwrap();
        let bytes = pkg.encode().unwrap();
        assert!(Package::open(&bytes[..bytes.len() / 2]).is_err());
        assert!(Package::open(&[]).is_err());
    }

    #[test]
    fn compression_bomb_and_size_limits_are_enforced() {
        // 2 MiB of zeros deflates to a few kilobytes: far above the 1000:1 limit.
        let bomb = zip_bytes(&[("bomb.xml", vec![0u8; 2 * 1024 * 1024])]);
        let err = Package::open(&bomb).unwrap_err();
        assert!(err.diagnostics.iter().any(|d| d.code == DIAG_LIMIT), "{err:?}");

        // Tight limits exercise the per-entry and total checks without large fixtures.
        let small = zip_bytes(&[("a.xml", b"0123456789".to_vec()), ("b.xml", b"abcdefghij".to_vec())]);
        let per_entry = Limits { max_entry_bytes: 5, ..Limits::default() };
        assert!(Package::open_with_limits(&small, &per_entry).is_err());
        let total = Limits { max_total_bytes: 15, ..Limits::default() };
        assert!(Package::open_with_limits(&small, &total).is_err());
        let count = Limits { max_entries: 1, ..Limits::default() };
        assert!(Package::open_with_limits(&small, &count).is_err());
        assert!(Package::open_with_limits(&small, &Limits::default()).is_ok());
    }

    #[test]
    fn duplicate_names_including_case_only_duplicates_are_rejected() {
        // The zip writer refuses exact duplicates, so create two entries and rename the second one in its
        // local and central headers. The names have equal length, so offsets do not change.
        let mut dup = zip_bytes(&[("aaaa.xml", b"1".to_vec()), ("bbbb.xml", b"2".to_vec())]);
        replace_all(&mut dup, b"bbbb.xml", b"aaaa.xml");
        let err = Package::open(&dup).unwrap_err();
        assert!(err.diagnostics.iter().any(|d| d.code == DIAG_DUPLICATE), "{err:?}");

        let case = zip_bytes(&[("Doc.xml", b"1".to_vec()), ("doc.xml", b"2".to_vec())]);
        let err = Package::open(&case).unwrap_err();
        assert!(err.diagnostics.iter().any(|d| d.code == DIAG_DUPLICATE), "{err:?}");
    }

    fn replace_all(haystack: &mut [u8], from: &[u8], to: &[u8]) {
        assert_eq!(from.len(), to.len());
        let mut count = 0;
        for start in 0..=haystack.len().saturating_sub(from.len()) {
            if &haystack[start..start + from.len()] == from {
                haystack[start..start + to.len()].copy_from_slice(to);
                count += 1;
            }
        }
        assert!(count >= 2, "expected the name in both local and central headers");
    }

    #[test]
    fn traversal_entry_names_inside_an_archive_are_rejected() {
        let evil = zip_bytes(&[("../escape.xml", b"<x/>".to_vec())]);
        let err = Package::open(&evil).unwrap_err();
        assert!(err.diagnostics.iter().any(|d| d.code == DIAG_UNSAFE_NAME), "{err:?}");
    }

    #[test]
    fn encrypted_flag_is_rejected_as_unsupported() {
        let mut bytes = zip_bytes(&[("secret.xml", b"<s/>".to_vec())]);
        // Set the general-purpose encryption bit (bit 0) in the central directory header.
        let central = bytes.windows(4).rposition(|w| w == [0x50, 0x4b, 0x01, 0x02]).expect("central header");
        bytes[central + 8] |= 0x01;
        let err = Package::open(&bytes).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedOperation);
        assert!(err.diagnostics.iter().any(|d| d.code == DIAG_ENCRYPTED));
    }

    #[test]
    fn cfb_probe_checks_the_container_signature_without_assigning_a_format() {
        let mut cfb = CFB_MAGIC.to_vec();
        cfb.extend_from_slice(b"payload");
        assert!(is_cfb_container(&cfb));
        assert!(!is_cfb_container(b"PK\x03\x04not CFB"));
        assert!(!is_cfb_container(&CFB_MAGIC[..7]));
        let error = Package::open(&cfb).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.diagnostics.iter().any(|d| d.code == DIAG_MALFORMED));
        assert!(!error.diagnostics.iter().any(|d| d.code == DIAG_ENCRYPTED), "the ZIP package layer does not label every CFB container encrypted");
    }

    /// Builds an archive with the zip crate, bypassing this crate's validation.
    fn zip_bytes(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            writer.start_file(*name, SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn raw_compressed(zip: &[u8], name: &str) -> Vec<u8> {
        let mut archive = ZipArchive::new(Cursor::new(zip)).unwrap();
        for index in 0..archive.len() {
            let file = archive.by_index_raw(index).unwrap();
            if file.name().unwrap() == name {
                let start = file.data_start().unwrap() as usize;
                let end = start + file.compressed_size() as usize;
                return zip[start..end].to_vec();
            }
        }
        panic!("entry {name} not found");
    }

    /// Builds an archive of stored entries with the zip crate, bypassing this crate's validation.
    fn stored_zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            writer.start_file(*name, SimpleFileOptions::default().compression_method(CompressionMethod::Stored)).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn encode_refuses_an_output_over_the_limit_before_allocating_it() {
        let mut pkg = Package::new();
        pkg.set_part("a.xml", b"<a/>".repeat(100)).unwrap();
        let err = pkg.encode_within(64).unwrap_err();
        assert!(err.diagnostics.iter().any(|d| d.code == DIAG_LIMIT), "{err:?}");
        assert!(pkg.encode_within(64 * 1024).is_ok());
    }

    #[test]
    fn a_stored_entry_whose_sizes_differ_is_refused_when_copied() {
        let mut bytes = stored_zip(&[("s.xml", b"0123456789".to_vec())]);
        let central = bytes.windows(4).rposition(|w| w == [0x50, 0x4b, 0x01, 0x02]).expect("central header");
        bytes[central + 20..central + 24].copy_from_slice(&12u32.to_le_bytes());
        let directory = scan_central_directory(&bytes).expect("the directory is readable");
        let err = entry_span(&bytes, &directory, &directory.records[0], "s.xml").unwrap_err();
        assert!(err.diagnostics.iter().any(|d| d.code == DIAG_MALFORMED), "{err:?}");
    }
}
