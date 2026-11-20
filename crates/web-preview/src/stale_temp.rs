//! Temporary files and folders that only a forced exit leaves behind (D19.5). Their names carry the process that made them,
//! `<prefix>p<pid>-<uuid><suffix>`, so that a later start removes the ones no running process owns and leaves those of
//! another open window. Only the direct entries of the expected folders are looked at; a link is neither followed nor
//! removed.

use std::path::{Path, PathBuf};

/// The tag this process puts at the start of a temporary name: `p<pid>-`.
pub fn process_tag() -> String {
    format!("p{}-", std::process::id())
}

/// Removes the files of the system temporary folder named `<prefix>p<pid>-<uuid><suffix>` whose process has ended, and
/// `<prefix><uuid><suffix>` (made before names carried the tag). Returns how many were removed.
///
/// A process id that the system has given to a new process keeps its files until that process ends too.
pub fn remove_orphaned_temp_files(prefix: &str, suffix: &str) -> usize {
    let Ok(root) = std::fs::canonicalize(std::env::temp_dir()).map(crate::clean_windows_path) else { return 0 };
    if crate::network_path(&root) {
        return 0;
    }
    remove_orphans(&root, prefix, suffix, Kind::File)
}

/// Removes the per-call poster folders (the video copy, the page, the WebView2 profile) that a forced exit left in the
/// WebView2 user-data folder, checked as poster work checks it (absolute, local, canonical). Returns how many were removed.
pub fn remove_orphaned_video_poster_work() -> usize {
    crate::poster_user_data_root(false).map_or(0, |root| remove_orphaned_poster_work_in(&root))
}

/// The run folders under `root/video-poster/work` and `root/video-poster/profiles`. Nothing is removed when one of those
/// folders is not a real folder at that place (a link, a file).
fn remove_orphaned_poster_work_in(root: &Path) -> usize {
    let base = root.join("video-poster");
    if !real_dir(&base) {
        return 0;
    }
    [base.join("work"), base.join("profiles")]
        .iter()
        .filter(|dir| real_dir(dir))
        .map(|dir| remove_orphans(dir, "", "", Kind::Dir))
        .sum()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

/// Removes the direct entries of `dir` of `kind` named `<prefix>[p<pid>-]<uuid><suffix>` that no running process owns.
/// The entries of this process and of running processes stay, and so does a link or an entry that cannot be removed.
fn remove_orphans(dir: &Path, prefix: &str, suffix: &str, kind: Kind) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let own = std::process::id();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|name| name.strip_prefix(prefix)).and_then(|rest| rest.strip_suffix(suffix)) else {
            continue;
        };
        let owner = match owner_pid(rest) {
            Some(pid) if is_uuid(&rest[rest.find('-').map_or(0, |dash| dash + 1)..]) => Some(pid),
            None if is_uuid(rest) => None,
            _ => continue,
        };
        if owner.is_some_and(|pid| pid == own || process_running(pid)) {
            continue;
        }
        let path: PathBuf = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else { continue };
        if is_link(&metadata) {
            continue;
        }
        let result = match kind {
            Kind::File if metadata.is_file() => std::fs::remove_file(&path),
            Kind::Dir if metadata.is_dir() => std::fs::remove_dir_all(&path),
            _ => continue,
        };
        if result.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// A folder that is itself no link (a symbolic link, a junction or another reparse point).
fn real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir() && !is_link(&metadata))
}

#[cfg(windows)]
fn is_link(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// A lowercase hyphenated UUID, as `new_id` writes it.
fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// The process id of a tagged name's rest (`p<pid>-…`), or `None` for a name without the tag.
fn owner_pid(rest: &str) -> Option<u32> {
    let (pid, _) = rest.strip_prefix('p')?.split_once('-')?;
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// Whether a process with this id runs. Only "no such process" (`ERROR_INVALID_PARAMETER`) or a signalled process handle
/// proves it ended; any other answer counts as running, so that a doubt keeps the files.
#[cfg(windows)]
fn process_running(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    let process = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(process) => process,
        Err(error) => return error.code() != ERROR_INVALID_PARAMETER.to_hresult(),
    };
    let waited = unsafe { WaitForSingleObject(process, 0) };
    unsafe {
        let _ = CloseHandle(process);
    }
    waited != WAIT_OBJECT_0
}

#[cfg(not(windows))]
fn process_running(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    const UUID: &str = "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dlm-stale-temp-{name}-{}", designllm_contracts::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A console helper started with CREATE_NO_WINDOW, so that it opens no window.
    fn quiet(program: &str, args: &[&str]) -> std::process::Child {
        use std::os::windows::process::CommandExt;
        Command::new(program).args(args).creation_flags(0x0800_0000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
    }

    /// The id of a process that has ended: a child started and waited for.
    fn ended_pid() -> u32 {
        let mut child = quiet("cmd", &["/C", "exit 0"]);
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// A directory junction `link` to `target` (no administrator right needed).
    fn junction(link: &Path, target: &Path) {
        let status = quiet("cmd", &["/C", "mklink", "/J", &link.to_string_lossy(), &target.to_string_lossy()]).wait().unwrap();
        assert!(status.success(), "mklink /J {}", link.display());
    }

    /// An outside folder with one file, to prove a link is not followed.
    fn outside_with_file(name: &str) -> (PathBuf, PathBuf) {
        let outside = scratch(name);
        let inner = outside.join(format!("p{}-{UUID}", ended_pid()));
        std::fs::create_dir_all(&inner).unwrap();
        let file = inner.join("keep.txt");
        std::fs::write(&file, b"keep").unwrap();
        (outside, file)
    }

    #[test]
    fn a_start_removes_the_files_ended_processes_left_and_keeps_the_rest() {
        let dir = scratch("owners");
        let mut running = quiet("ping", &["-n", "60", "127.0.0.1"]);
        let ended = ended_pid();
        let name = |tag: String| format!("poster-{tag}{}.png", designllm_contracts::new_id());
        let ended_file = dir.join(name(format!("p{ended}-")));
        let untagged_uuid = dir.join(format!("poster-{UUID}.png"));
        let own_file = dir.join(name(process_tag()));
        let running_file = dir.join(name(format!("p{}-", running.id())));
        let untagged_other = dir.join("poster-mine.png");
        let tagged_not_uuid = dir.join(format!("poster-p{ended}-mine.png"));
        let other_suffix = dir.join(format!("poster-p{ended}-{UUID}.txt"));
        let other_prefix = dir.join(format!("other-p{ended}-{UUID}.png"));
        let folder = dir.join(format!("poster-p{ended}-{UUID}.png"));
        let files = [&ended_file, &untagged_uuid, &own_file, &running_file, &untagged_other, &tagged_not_uuid, &other_suffix, &other_prefix];
        for path in files {
            std::fs::write(path, b"png").unwrap();
        }
        std::fs::create_dir_all(folder.join("inner")).unwrap();

        let removed = remove_orphans(&dir, "poster-", ".png", Kind::File);
        let _ = running.kill();
        let _ = running.wait();

        assert!(!ended_file.exists() && !untagged_uuid.exists(), "the ended process's file and the untagged UUID file go");
        assert_eq!(removed, 2);
        assert!(own_file.exists(), "this process's file stays");
        assert!(running_file.exists(), "a running process's file stays");
        assert!(untagged_other.exists() && tagged_not_uuid.exists(), "a name that is not ours (no UUID) stays");
        assert!(other_suffix.exists() && other_prefix.exists(), "other names are not touched");
        assert!(folder.join("inner").exists(), "a folder is not a poster file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn poster_work_cleanup_removes_only_our_real_folders_and_never_follows_a_junction() {
        let root = scratch("work");
        let work = root.join("video-poster").join("work");
        let profiles = root.join("video-poster").join("profiles");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&profiles).unwrap();
        let ended = ended_pid();
        let ended_run = work.join(format!("p{ended}-{UUID}"));
        let untagged_run = profiles.join(UUID);
        let own_run = work.join(format!("{}{UUID}", process_tag()));
        let not_ours = work.join("notes");
        let file_run = profiles.join(format!("p{ended}-0b1c2d3e-4f50-4a6b-8c7d-000000000000"));
        for folder in [&ended_run, &untagged_run, &own_run, &not_ours] {
            std::fs::create_dir_all(folder.join("inner")).unwrap();
            std::fs::write(folder.join("inner").join("clip.mp4"), b"video").unwrap();
        }
        std::fs::write(&file_run, b"file").unwrap();
        let (outside, outside_file) = outside_with_file("outside-entry");
        let linked_run = profiles.join(format!("p{ended}-0b1c2d3e-4f50-4a6b-8c7d-111111111111"));
        junction(&linked_run, &outside);

        let removed = remove_orphaned_poster_work_in(&root);

        assert!(outside_file.exists(), "a junction's target is not entered");
        assert!(linked_run.exists(), "a junction is not removed");
        assert!(!ended_run.exists() && !untagged_run.exists(), "the ended and the untagged run folders go");
        assert_eq!(removed, 2);
        assert!(own_run.exists(), "this process's run stays");
        assert!(not_ours.exists(), "a folder whose name is not a run stays");
        assert!(file_run.exists(), "a file is not a run folder");
        std::fs::remove_dir(&linked_run).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn poster_work_cleanup_does_nothing_when_the_work_folder_is_a_junction() {
        let root = scratch("linked-work");
        std::fs::create_dir_all(root.join("video-poster")).unwrap();
        let (outside, outside_file) = outside_with_file("outside-work");
        let work = root.join("video-poster").join("work");
        junction(&work, &outside);

        let removed = remove_orphaned_poster_work_in(&root);

        assert!(outside_file.exists(), "nothing is removed through a linked work folder");
        assert_eq!(removed, 0);
        std::fs::remove_dir(&work).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn only_p_digits_dash_uuid_or_a_bare_uuid_is_our_name() {
        assert_eq!(owner_pid("p123-abc"), Some(123));
        assert_eq!(owner_pid("p-abc"), None);
        assert_eq!(owner_pid("p12a-abc"), None);
        assert_eq!(owner_pid("0b1c2d3e-4f50"), None);
        assert_eq!(owner_pid("p99999999999-x"), None);
        assert!(process_running(std::process::id()));
        assert!(!process_running(ended_pid()));
    }
}
