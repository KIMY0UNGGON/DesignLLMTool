//! Minimal ZIP reader and writer for WebCode source folders.
//! Reader: stored (0) and deflate (8) entries, CRC-32 checked, size limits, no encryption, no Zip64, no symlinks.
//! Writer: deterministic output (fixed DOS timestamp, sorted names, UTF-8 name flag), stored or deflate per entry.

use crate::common::{fail, invalid};
use designllm_contracts::{DltResult, ErrorCode};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use std::io::{Read, Write};

pub(crate) const MAX_ENTRIES: usize = 2_000;
pub(crate) const MAX_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
/// DOS date 1980-01-01 and time 00:00, so archives are byte-identical for identical input.
const DOS_DATE: u16 = (1 << 5) | 1;
const DOS_TIME: u16 = 0;

fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    table
}

pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let table = crc_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

fn le16(bytes: &[u8], at: usize) -> DltResult<u16> {
    let slice = bytes.get(at..at + 2).ok_or_else(|| invalid("ZIP_TRUNCATED", "archive is truncated"))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn le32(bytes: &[u8], at: usize) -> DltResult<u32> {
    let slice = bytes.get(at..at + 4).ok_or_else(|| invalid("ZIP_TRUNCATED", "archive is truncated"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Writes entries (already validated names, sorted by name) into a ZIP archive.
pub(crate) fn write_zip(entries: &[(String, Vec<u8>)]) -> DltResult<Vec<u8>> {
    let mut sorted: Vec<&(String, Vec<u8>)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in sorted {
        let crc = crc32(data);
        let deflated = {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(data).map_err(|e| fail(ErrorCode::Io, "ZIP_WRITE", e.to_string()))?;
            encoder.finish().map_err(|e| fail(ErrorCode::Io, "ZIP_WRITE", e.to_string()))?
        };
        let (method, payload): (u16, &[u8]) =
            if deflated.len() < data.len() { (8, &deflated) } else { (0, data) };
        let offset = out.len() as u32;
        let name_bytes = name.as_bytes();
        out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&DOS_TIME.to_le_bytes());
        out.extend_from_slice(&DOS_DATE.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(payload);

        central.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // made by
        central.extend_from_slice(&20u16.to_le_bytes()); // needed
        central.extend_from_slice(&0x0800u16.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&DOS_TIME.to_le_bytes());
        central.extend_from_slice(&DOS_DATE.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name_bytes);
    }
    if entries.len() > u16::MAX as usize || out.len() > u32::MAX as usize - central.len() {
        return Err(fail(ErrorCode::InvalidArgument, "ZIP_TOO_LARGE", "archive exceeds ZIP32 limits"));
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&SIG_EOCD.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

/// Reads all file entries. Directory entries are skipped. Errors on encryption, Zip64, symlinks, duplicates,
/// bad CRC, unsupported methods, and size limits. Names are returned exactly as stored.
pub(crate) fn read_zip(bytes: &[u8]) -> DltResult<Vec<(String, Vec<u8>)>> {
    // Find the end-of-central-directory record in the last 64 KiB plus 22 bytes.
    let search_start = bytes.len().saturating_sub(65_557);
    let eocd = (search_start..bytes.len().saturating_sub(21))
        .rev()
        .find(|&i| le32(bytes, i).map(|v| v == SIG_EOCD).unwrap_or(false))
        .ok_or_else(|| invalid("ZIP_NOT_ARCHIVE", "file is not a ZIP archive"))?;
    let count = le16(bytes, eocd + 10)? as usize;
    let cd_size = le32(bytes, eocd + 12)? as usize;
    let cd_offset = le32(bytes, eocd + 16)? as usize;
    if count == 0xFFFF || cd_offset == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
        return Err(invalid("ZIP_ZIP64", "Zip64 archives are not supported"));
    }
    if count > MAX_ENTRIES {
        return Err(invalid("ZIP_TOO_MANY_ENTRIES", format!("archive has more than {MAX_ENTRIES} entries")));
    }

    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut total: u64 = 0;
    let mut pos = cd_offset;
    for _ in 0..count {
        if le32(bytes, pos)? != SIG_CENTRAL {
            return Err(invalid("ZIP_CORRUPT", "central directory entry is malformed"));
        }
        let flags = le16(bytes, pos + 8)?;
        let method = le16(bytes, pos + 10)?;
        let crc = le32(bytes, pos + 16)?;
        let comp_size = le32(bytes, pos + 20)?;
        let uncomp_size = le32(bytes, pos + 24)?;
        let name_len = le16(bytes, pos + 28)? as usize;
        let extra_len = le16(bytes, pos + 30)? as usize;
        let comment_len = le16(bytes, pos + 32)? as usize;
        let external = le32(bytes, pos + 38)?;
        let local_offset = le32(bytes, pos + 42)? as usize;
        let name_bytes = bytes
            .get(pos + 46..pos + 46 + name_len)
            .ok_or_else(|| invalid("ZIP_TRUNCATED", "entry name is truncated"))?;
        let name = String::from_utf8(name_bytes.to_vec())
            .map_err(|_| invalid("ZIP_NAME", "entry name is not valid UTF-8"))?;
        pos += 46 + name_len + extra_len + comment_len;

        if flags & 0x0001 != 0 {
            return Err(invalid("ZIP_ENCRYPTED", format!("entry {name} is encrypted")));
        }
        if comp_size == u32::MAX || uncomp_size == u32::MAX || local_offset == u32::MAX as usize {
            return Err(invalid("ZIP_ZIP64", "Zip64 entries are not supported"));
        }
        if (external >> 16) & 0o170000 == 0o120000 {
            return Err(invalid("ZIP_SYMLINK", format!("entry {name} is a symbolic link")));
        }
        if name.ends_with('/') {
            continue;
        }
        if !seen.insert(name.clone()) {
            return Err(invalid("ZIP_DUPLICATE", format!("entry {name} appears more than once")));
        }
        if u64::from(uncomp_size) > MAX_ENTRY_BYTES {
            return Err(fail(ErrorCode::InvalidArgument, "ZIP_ENTRY_TOO_LARGE", format!("entry {name} is too large")));
        }
        total += u64::from(uncomp_size);
        if total > MAX_TOTAL_BYTES {
            return Err(fail(ErrorCode::InvalidArgument, "ZIP_TOTAL_TOO_LARGE", "archive expands beyond the limit"));
        }

        if le32(bytes, local_offset)? != SIG_LOCAL {
            return Err(invalid("ZIP_CORRUPT", format!("local header for {name} is malformed")));
        }
        let local_name_len = le16(bytes, local_offset + 26)? as usize;
        let local_extra_len = le16(bytes, local_offset + 28)? as usize;
        let data_start = local_offset + 30 + local_name_len + local_extra_len;
        let raw = bytes
            .get(data_start..data_start + comp_size as usize)
            .ok_or_else(|| invalid("ZIP_TRUNCATED", format!("entry {name} data is truncated")))?;
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut decoder = flate2::read::DeflateDecoder::new(raw);
                let mut buf = Vec::with_capacity(uncomp_size as usize);
                decoder
                    .by_ref()
                    .take(MAX_ENTRY_BYTES + 1)
                    .read_to_end(&mut buf)
                    .map_err(|e| invalid("ZIP_DEFLATE", format!("entry {name} cannot be inflated: {e}")))?;
                buf
            }
            other => {
                return Err(invalid("ZIP_METHOD", format!("entry {name} uses unsupported method {other}")));
            }
        };
        if data.len() != uncomp_size as usize {
            return Err(invalid("ZIP_SIZE", format!("entry {name} size does not match its header")));
        }
        if crc32(&data) != crc {
            return Err(invalid("ZIP_CRC", format!("entry {name} fails its CRC-32 check")));
        }
        out.push((name, data));
    }
    if out.len() > MAX_ENTRIES {
        return Err(invalid("ZIP_TOO_MANY_ENTRIES", format!("archive has more than {MAX_ENTRIES} entries")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn archive_round_trips_stored_and_deflated_entries() {
        let entries = vec![
            ("b/app.js".to_string(), b"console.log(1);".repeat(200)),
            ("index.html".to_string(), b"<!doctype html>".to_vec()),
        ];
        let bytes = write_zip(&entries).unwrap();
        let read = read_zip(&bytes).unwrap();
        assert_eq!(read.len(), 2);
        let mut expected = entries.clone();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(read, expected);
        // Deterministic: the same input produces the same bytes.
        assert_eq!(write_zip(&entries).unwrap(), bytes);
    }

    #[test]
    fn corrupt_crc_is_rejected() {
        let mut bytes = write_zip(&[("a.txt".to_string(), b"hello world".to_vec())]).unwrap();
        // Flip one byte of the stored payload (after the 30-byte local header and the 5-byte name).
        let target = 30 + 5;
        bytes[target] ^= 0x20;
        let error = read_zip(&bytes).unwrap_err();
        assert!(error.diagnostics[0].code == "ZIP_CRC" || error.diagnostics[0].code == "ZIP_DEFLATE");
    }

    #[test]
    fn non_zip_bytes_are_rejected() {
        assert_eq!(read_zip(b"<html></html>").unwrap_err().diagnostics[0].code, "ZIP_NOT_ARCHIVE");
    }
}
