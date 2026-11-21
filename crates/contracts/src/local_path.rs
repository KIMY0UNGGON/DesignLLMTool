//! Lexical checks before a caller-supplied file name reaches the filesystem.
use std::path::Path;

/// Request key the desktop adds (`true`) to artifact.open, asset.import, artifact.save and artifact.export when the
/// path came from an agent's ui.command.run: the core then applies the agent path rules on the gui connection too.
/// The core removes it before parsing; it is not part of any request type.
pub const AGENT_PATH_PARAM: &str = "agent_path";

pub fn is_windows_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    let port = stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT"));
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || port.is_some_and(|n| matches!(n, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}"))
}

pub fn is_local_file_path(raw: &str) -> bool {
    let path = Path::new(raw);
    if !path.is_absolute() || path.file_name().is_none() || raw.contains('\0') {
        return false;
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut parts = path.components();
        let Some(Component::Prefix(prefix)) = parts.next() else { return false };
        if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
            return false;
        }
        // Verbatim paths use only backslash as separator. Keep ordinary canonicalized drive paths.
        if matches!(prefix.kind(), Prefix::VerbatimDisk(_)) && raw.contains('/') {
            return false;
        }
        parts.all(|part| match part {
            Component::RootDir => true,
            Component::Normal(name) => name.to_str().is_some_and(|s| !s.contains(':') && !s.ends_with(['.', ' ']) && !is_windows_device_name(s)),
            _ => false,
        })
    }
    #[cfg(not(windows))]
    {
        !raw.starts_with("//") && !path.components().any(|part| matches!(part, std::path::Component::ParentDir))
    }
}

/// The requests that take a caller-supplied file path (the core's `PathCaller` rules). In an agent's context (a
/// `ui.command.run` that names a file, a UI-test `ui.input` run) the desktop marks them with [`AGENT_PATH_PARAM`]; one list for
/// every marking site. `source.import_csv` and `source.extract_pdf` name an imported asset by hash, not a path.
pub const AGENT_PATH_METHODS: &[&str] = &["artifact.open", "asset.import", "artifact.save", "artifact.export"];

/// Why an agent may not name the existing file `raw`, by the agent path rules (66c), checked before the file is opened:
/// a lexically local absolute path ([`is_local_file_path`], no `\\?\`), on a local drive (not a network or unknown
/// drive), no link or reparse point (junction, symbolic link, cloud placeholder) from the drive root to the file, and a
/// regular file. Only metadata is read. The core applies the same rules again, by handle, to the requests it serves.
pub fn agent_file_refusal(raw: &str) -> Option<String> {
    if let Some(reason) = local_walk_refusal(raw, false, "no such file") {
        return Some(reason);
    }
    match std::fs::symlink_metadata(raw) {
        Ok(meta) if meta.is_file() => None,
        _ => Some(format!("{raw}: not a regular file")),
    }
}

/// Why a save dialog's answer `raw` may not name the file to write (task 68r), by the agent path rules of
/// [`agent_file_refusal`] except for the file itself: it need not exist yet (when it does, it must be a regular file), and
/// the folder that holds it must exist, with no link or reparse point from the drive root down to it. Only metadata is read.
pub fn save_target_refusal(raw: &str) -> Option<String> {
    if let Some(reason) = local_walk_refusal(raw, true, "the folder does not exist") {
        return Some(reason);
    }
    let path = Path::new(raw);
    if path.parent().is_none_or(|folder| !std::fs::metadata(folder).is_ok_and(|meta| meta.is_dir())) {
        return Some(format!("{raw}: the folder does not exist"));
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Ok(meta) if meta.is_file() && !is_link(&meta) => None,
        _ => Some(format!("{raw}: not a regular file")),
    }
}

/// The rules [`agent_file_refusal`] and [`save_target_refusal`] share: a lexically local absolute path (no `\\?\`) on a local
/// drive, with no link or reparse point from the drive root down to the file (`to_folder`: down to the folder that holds it).
/// A part of that walk that does not exist is refused as `missing`.
fn local_walk_refusal(raw: &str, to_folder: bool, missing: &str) -> Option<String> {
    if raw.starts_with(r"\\?\") || !is_local_file_path(raw) {
        return Some(format!("{raw}: not an absolute path on a local drive"));
    }
    let path = Path::new(raw);
    if let Some(reason) = drive_refusal(path) {
        return Some(format!("{raw}: {reason}"));
    }
    let walk = if to_folder {
        let Some(folder) = path.parent() else { return Some(format!("{raw}: no folder")) };
        folder
    } else {
        path
    };
    let mut current = std::path::PathBuf::new();
    for part in walk.components() {
        current.push(part.as_os_str());
        if matches!(part, std::path::Component::Prefix(_) | std::path::Component::RootDir) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => return Some(format!("{raw}: the path holds a link or reparse point")),
            Ok(_) => {}
            Err(_) => return Some(format!("{raw}: {missing}")),
        }
    }
    None
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT also covers junctions and cloud placeholders.
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

#[cfg(windows)]
fn drive_refusal(path: &Path) -> Option<&'static str> {
    use std::path::{Component, Prefix};
    let Some(Component::Prefix(prefix)) = path.components().next() else { return Some("no drive") };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else { return Some("not a local drive") };
    let root = [u16::from(letter), 58, 92, 0];
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
    }
    // SAFETY: root is a NUL-terminated UTF-16 drive root. Removable, fixed, CD and RAM drives are local (the core's rule).
    match unsafe { GetDriveTypeW(root.as_ptr()) } {
        2 | 3 | 5 | 6 => None,
        _ => Some("not a local drive"),
    }
}

#[cfg(not(windows))]
fn drive_refusal(_path: &Path) -> Option<&'static str> {
    None
}

#[cfg(test)]
mod agent_file_tests {
    use super::*;

    /// Task 68r: a save target may be a new file, but its folder must exist and a name must not be a folder or a device.
    #[test]
    fn a_save_target_is_a_new_file_in_an_existing_local_folder() {
        let dir = std::env::temp_dir().join(format!("designllm-save-target-{}", crate::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(save_target_refusal(&dir.join("new.png").to_string_lossy()), None, "a new file in a folder");
        let existing = dir.join("old.png");
        std::fs::write(&existing, b"x").unwrap();
        assert_eq!(save_target_refusal(&existing.to_string_lossy()), None, "an existing regular file");
        let folder = dir.join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        let mut bad = vec![
            dir.join("no-such-folder").join("a.png").to_string_lossy().into_owned(),
            folder.to_string_lossy().into_owned(),
            "a.png".to_owned(),
            r"\\server\share\a.png".to_owned(),
            format!(r"\\?\{}", dir.join("a.png").display()),
        ];
        if cfg!(windows) {
            bad.push(dir.join("CON.png").to_string_lossy().into_owned());
        }
        for raw in bad {
            assert!(save_target_refusal(&raw).is_some(), "{raw}");
        }
        #[cfg(windows)]
        {
            // Junctions (mklink /J needs no privilege): one as the folder of the target, one in the file's own place.
            let real = dir.join("real");
            std::fs::create_dir_all(&real).unwrap();
            // A junction that cannot be made (mklink fails) is not checked, as in the agent file test below; the reason is printed.
            let junction = |name: &str| {
                let link = dir.join(name);
                match std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(&real).output() {
                    Ok(made) if made.status.success() => Some(link),
                    made => {
                        eprintln!("a_save_target_is_a_new_file_in_an_existing_local_folder: mklink /J {} failed, not checked: {made:?}", link.display());
                        None
                    }
                }
            };
            assert_eq!(save_target_refusal(&real.join("new.png").to_string_lossy()), None, "the folder itself is plain");
            if let Some(folder_link) = junction("link") {
                let refusal = save_target_refusal(&folder_link.join("new.png").to_string_lossy());
                assert!(refusal.as_deref().is_some_and(|reason| reason.ends_with("the path holds a link or reparse point")), "{refusal:?}");
                std::fs::remove_dir(&folder_link).unwrap();
            }
            if let Some(file_link) = junction("j.png") {
                assert!(save_target_refusal(&file_link.to_string_lossy()).is_some(), "a reparse point in the file's place");
                std::fs::remove_dir(&file_link).unwrap();
            }
            // A symbolic link to a file needs the privilege to make one (developer mode); checked where it can be made.
            let symlink = dir.join("s.png");
            if std::os::windows::fs::symlink_file(&existing, &symlink).is_ok() {
                assert!(save_target_refusal(&symlink.to_string_lossy()).is_some(), "a symbolic link file");
                std::fs::remove_file(&symlink).unwrap();
            } else {
                eprintln!("a_save_target_is_a_new_file_in_an_existing_local_folder: no privilege for a file symlink, not checked");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_agent_file_is_a_local_regular_file_without_links() {
        let dir = std::env::temp_dir().join(format!("designllm-agent-file-{}", crate::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.png");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(agent_file_refusal(&file.to_string_lossy()), None);
        for bad in [
            r"\\server\share\a.png".to_owned(),
            format!(r"\\?\{}", file.display()),
            "a.png".to_owned(),
            dir.join("missing.png").to_string_lossy().into_owned(),
            dir.to_string_lossy().into_owned(),
        ] {
            assert!(agent_file_refusal(&bad).is_some(), "{bad}");
        }
        #[cfg(windows)]
        {
            // A junction in the path (mklink /J needs no privilege) is refused before the file is opened.
            let target = dir.join("real");
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("b.png"), b"x").unwrap();
            let junction = dir.join("link");
            let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&junction).arg(&target).output();
            if made.is_ok_and(|out| out.status.success()) {
                assert!(agent_file_refusal(&junction.join("b.png").to_string_lossy()).is_some(), "a junction is refused");
                std::fs::remove_dir(&junction).unwrap();
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
