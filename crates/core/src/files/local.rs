use super::*;
use std::fs::{File, Metadata};

fn reject_link(meta: &Metadata) -> DltResult<()> {
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT also covers junctions.
    };
    #[cfg(not(windows))]
    let linked = meta.file_type().is_symlink();
    if linked { Err(invalid("file path contains a link or reparse point")) } else { Ok(()) }
}

#[cfg(windows)]
fn check_drive(path: &Path) -> DltResult<()> {
    use std::path::Prefix;
    let Some(Component::Prefix(prefix)) = path.components().next() else { return Err(invalid("expected a local drive")) };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else { return Err(invalid("expected a local drive")) };
    let root = [u16::from(letter), 58, 92, 0];
    #[link(name = "kernel32")]
    unsafe extern "system" { fn GetDriveTypeW(root: *const u16) -> u32; }
    // SAFETY: root is a terminated UTF-16 drive root. Reject remote and indeterminate drives.
    match unsafe { GetDriveTypeW(root.as_ptr()) } {
        2 | 3 | 5 | 6 => Ok(()),
        _ => Err(invalid("file path must be on a local drive")),
    }
}

/// The checked parent directory of a caller path. `strict` = the agent rules (local drive, no link or reparse point
/// from the root down); otherwise the directory is used as the OS resolves it, as before 66c.
pub(super) struct Parent {
    #[cfg(target_os = "linux")]
    _directory: Option<File>,
    pub(super) path: PathBuf,
    pub(super) strict: bool,
}

impl Parent {
    pub(super) fn open(dir: &Path, strict: bool) -> DltResult<Self> {
        if !strict {
            return match fs::metadata(dir) {
                Ok(meta) if meta.is_dir() => Ok(Self {
                    path: dir.to_path_buf(),
                    strict,
                    #[cfg(target_os = "linux")]
                    _directory: None,
                }),
                Ok(_) => Err(not_found(format!("{} is not a directory", dir.display()))),
                Err(_) => Err(not_found(format!("target directory {} does not exist", dir.display()))),
            };
        }
        #[cfg(windows)]
        check_drive(dir)?;
        let mut path = PathBuf::new();
        #[cfg(target_os = "linux")]
        let mut directory = None;
        for ancestor in dir.ancestors().collect::<Vec<_>>().into_iter().rev() {
            path = match ancestor.file_name() {
                Some(name) => path.join(name),
                None => ancestor.to_path_buf(),
            };
            #[cfg(windows)]
            let meta = {
                use std::os::windows::fs::OpenOptionsExt;
                // Inspect the directory entry itself; write-through renames need write sharing.
                let handle = OpenOptions::new().access_mode(0x80).share_mode(0x1 | 0x2 | 0x4)
                    .custom_flags(0x0200_0000 | 0x0020_0000).open(&path).map_err(path_error)?;
                handle.metadata()?
            };
            #[cfg(not(windows))]
            let meta = fs::symlink_metadata(&path).map_err(path_error)?;
            reject_link(&meta)?;
            if !meta.is_dir() { return Err(not_found("parent is not a directory")); }
            #[cfg(target_os = "linux")]
            {
                use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
                // O_DIRECTORY | O_NOFOLLOW. Resolve the next component relative to this inode.
                let handle = OpenOptions::new().read(true).custom_flags(0x10000 | 0x20000).open(&path).map_err(path_error)?;
                path = PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd()));
                directory = Some(handle);
            }
        }
        Ok(Self {
            path,
            strict,
            #[cfg(target_os = "linux")]
            _directory: Some(directory.ok_or_else(|| invalid("file path has no directory"))?),
        })
    }

    /// The strict checks again, right before a step that writes or removes in this directory.
    pub(super) fn recheck(&self) -> DltResult<()> {
        #[cfg(not(target_os = "linux"))]
        {
            if self.strict {
                Self::open(&self.path, true)?;
            }
        }
        Ok(())
    }
}

fn path_error(error: io::Error) -> DltError {
    if error.kind() == io::ErrorKind::NotFound { not_found("file or directory does not exist") } else { error.into() }
}

/// The entry itself (a link is not followed). `strict` also refuses a link or reparse point.
pub(super) fn file_metadata(path: &Path, strict: bool) -> DltResult<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => { if strict { reject_link(&meta)?; } Ok(Some(meta)) }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A regular file for reading or (`write`) syncing. `strict` opens the entry itself and refuses a link or reparse
/// point; otherwise the OS follows links, as before 66c.
pub(super) fn open_file(path: &Path, write: bool, strict: bool) -> DltResult<File> {
    if !strict {
        let file = OpenOptions::new().read(!write).write(write).open(path).map_err(path_error)?;
        if !file.metadata()?.is_file() { return Err(invalid("path is not a regular file")); }
        return Ok(file);
    }
    let meta = file_metadata(path, true)?.ok_or_else(|| not_found("file does not exist"))?;
    if !meta.is_file() { return Err(invalid("path is not a regular file")); }
    let mut options = OpenOptions::new();
    options.read(!write).write(write);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0x1 | 0x2 | 0x4).custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000 | 0x800); // O_NOFOLLOW | O_NONBLOCK (also refuse a swapped FIFO).
    }
    let file = options.open(path).map_err(path_error)?;
    let meta = file.metadata()?;
    reject_link(&meta)?;
    if !meta.is_file() { return Err(invalid("path is not a regular file")); }
    Ok(file)
}

/// The stored spelling of an existing file, including its case, for matching a pending save.
pub(super) fn canonical_file(path: &Path, strict: bool) -> Option<PathBuf> {
    if !strict { return fs::canonicalize(path).ok(); }
    let target = Target::resolve(path.to_str()?, true).ok()?;
    let leaf = target.io_path();
    #[cfg(windows)]
    {
        use std::ffi::{c_void, OsString};
        use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
        #[link(name = "kernel32")]
        unsafe extern "system" { fn GetFinalPathNameByHandleW(file: *mut c_void, path: *mut u16, size: u32, flags: u32) -> u32; }
        let file = open_file(&leaf, false, true).ok()?;
        // Query the checked handle so a later name lookup cannot follow a replacement link.
        // SAFETY: the file stays open; a null buffer and zero size request the required capacity.
        let size = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), std::ptr::null_mut(), 0, 0) };
        if size == 0 { return None; }
        let mut name = vec![0; size as usize];
        // SAFETY: name has size writable UTF-16 units.
        let len = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), name.as_mut_ptr(), size, 0) };
        if len == 0 || len >= size { return None; }
        Some(PathBuf::from(OsString::from_wide(&name[..len as usize])))
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let guard = open_file(&leaf, false, true).ok()?;
        fs::read_link(format!("/proc/self/fd/{}", guard.as_raw_fd())).ok()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        file_metadata(&leaf, true).ok()??;
        fs::canonicalize(leaf).ok()
    }
}
