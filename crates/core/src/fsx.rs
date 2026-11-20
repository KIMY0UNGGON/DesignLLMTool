//! Durable file primitives used by the blob store and native save/export publication.
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Flush data and metadata of a file (FlushFileBuffers on Windows; needs a writable handle).
pub fn sync_file(path: &Path) -> io::Result<()> { OpenOptions::new().write(true).open(path)?.sync_all() }

/// Same-volume rename. With `replace == false` an existing target makes the call fail with
/// `ErrorKind::AlreadyExists`; there is no check-then-act window for the no-replace case.
#[cfg(windows)]
pub fn rename_durable(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    fn wide(p: &Path) -> io::Result<Vec<u16>> {
        let absolute = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir()?.join(p) };
        let units: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        fn has_dot_segments(path: &[u16]) -> bool {
            let mut start = 0;
            for end in 0..=path.len() {
                if end == path.len() || path[end] == b'\\' as u16 || path[end] == b'/' as u16 {
                    let component = &path[start..end];
                    if component == [b'.' as u16] || component == [b'.' as u16, b'.' as u16] { return true; }
                    start = end + 1;
                }
            }
            false
        }
        if units.starts_with(&"\\\\?\\".encode_utf16().collect::<Vec<_>>()) {
            let mut extended = units;
            for unit in &mut extended { if *unit == b'/' as u16 { *unit = b'\\' as u16; } }
            if has_dot_segments(&extended) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "verbatim Windows path contains ambiguous '.' or '..' components"));
            }
            return Ok(extended.into_iter().chain(std::iter::once(0)).collect());
        }
        if units.len() <= 259 {
            return Ok(p.as_os_str().encode_wide().chain(std::iter::once(0)).collect());
        }

        let mut normalized = units;
        for unit in &mut normalized { if *unit == b'/' as u16 { *unit = b'\\' as u16; } }
        if has_dot_segments(&normalized) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "long Windows path with '.' or '..' components is refused; choose a canonical path"));
        }
        let mut extended: Vec<u16>;
        if normalized.starts_with(&[b'\\' as u16, b'\\' as u16]) {
            extended = "\\\\?\\UNC\\".encode_utf16().collect();
            extended.extend_from_slice(&normalized[2..]);
        } else if normalized.len() >= 3
            && normalized[1] == b':' as u16
            && normalized[2] == b'\\' as u16
        {
            extended = "\\\\?\\".encode_utf16().collect();
            extended.extend_from_slice(&normalized);
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "long Windows path is not an absolute drive or UNC path"));
        }
        if extended.len() >= 32_767 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "long Windows path exceeds the extended path limit"));
        }
        Ok(extended.into_iter().chain(std::iter::once(0)).collect())
    }
    let flags = MOVEFILE_WRITE_THROUGH | if replace { MOVEFILE_REPLACE_EXISTING } else { 0 };
    let (src, dst) = (wide(from)?, wide(to)?);
    // SAFETY: both buffers are NUL-terminated UTF-16 strings that outlive the call.
    let ok = unsafe { MoveFileExW(src.as_ptr(), dst.as_ptr(), flags) };
    if ok == 0 {
        let err = io::Error::last_os_error();
        // ERROR_ALREADY_EXISTS (183) / ERROR_FILE_EXISTS (80)
        if matches!(err.raw_os_error(), Some(183) | Some(80)) { return Err(io::Error::new(io::ErrorKind::AlreadyExists, err)); }
        Err(err)
    } else { Ok(()) }
}

/// Non-Windows fallback (not exercised by the Windows test suite).
#[cfg(not(windows))]
pub fn rename_durable(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    if replace { std::fs::rename(from, to)?; } else { std::fs::hard_link(from, to)?; std::fs::remove_file(from)?; }
    if let Some(dir) = to.parent() { File::open(dir)?.sync_all()?; }
    Ok(())
}

/// Read at most `max` bytes at `offset`; returns (bytes, total file length).
pub fn read_range(path: &Path, offset: u64, max: usize) -> io::Result<(Vec<u8>, u64)> {
    let mut file = File::open(path)?;
    let total = file.metadata()?.len();
    if offset >= total { return Ok((Vec::new(), total)); }
    file.seek(SeekFrom::Start(offset))?;
    let want = usize::try_from((total - offset).min(max as u64)).unwrap_or(max);
    let mut buf = vec![0u8; want];
    file.read_exact(&mut buf)?;
    Ok((buf, total))
}
