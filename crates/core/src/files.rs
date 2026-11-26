//! Caller path checks and file publication for artifact.save / artifact.export (CONTRACTS §4,
//! §6, G0_DECISIONS #5). New paths never replace an existing file; overwrite requires the caller's
//! expected hash, re-checked immediately before the replacing rename. Every temp path is
//! registered durably before creation so a crash leaves nothing behind and a completed rename is
//! recognized by its content hash.
use crate::blobs::{fire, is_hash, FaultPoint, SharedHook};
use crate::errors::{err, invalid, io_store, not_found, sql, ErrExt};
use crate::receipts::key_for;
use crate::store::{self, StoredResult};
use crate::{to_value, Workspace, CORE_LOCK_FILE, MAX_IMPORT_BYTES};
use designllm_contracts::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const TEMP_SUFFIX: &str = ".dllt-partial";
static COUNTER: AtomicU64 = AtomicU64::new(0);
mod local;

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn refuses_network_and_device_paths_before_filesystem_access() {
        for raw in [r"\\server\share\file", r"\\?\UNC\server\share\file", r"\\.\pipe\file", r"\\?\GLOBALROOT\Device\file", "//server/share/file"] {
            assert!(absolute_clean(raw, "path", true).is_err(), "{raw}");
        }
        #[cfg(windows)]
        for raw in [r"C:\folder\NUL.txt", r"C:\COM1", r"C:\folder\file:stream"] {
            assert!(absolute_clean(raw, "path", true).is_err(), "{raw}");
        }
    }

    /// 66c r3: a path the user picked in the GUI passes the lexical stage as before 66c (UNC shares and mapped
    /// drives); only the agent rules refuse it. Nothing here touches the filesystem or the network.
    #[cfg(windows)]
    #[test]
    fn gui_paths_keep_unc_and_drive_forms_that_agent_paths_refuse() {
        for raw in [r"\\server\share\folder\file.docx", r"\\?\UNC\server\share\file.docx", "//server/share/file.docx", r"Z:\mapped\file.docx"] {
            assert_eq!(absolute_clean(raw, "path", false).unwrap(), PathBuf::from(raw), "{raw}");
        }
        for raw in [r"\\server\share\folder\file.docx", r"\\?\UNC\server\share\file.docx", "//server/share/file.docx"] {
            assert_eq!(absolute_clean(raw, "path", true).unwrap_err().message, "path must name a local file", "{raw}");
        }
        let mut gui = json!({ "path": r"C:\a.docx" });
        assert!(!take_agent_path(&mut gui));
        let mut marked = json!({ "path": r"C:\a.docx" });
        marked[local_path::AGENT_PATH_PARAM] = json!(true);
        assert!(take_agent_path(&mut marked));
        assert_eq!(marked, json!({ "path": r"C:\a.docx" }), "the mark never reaches the typed request");
    }

    #[test]
    fn source_and_target_refuse_links_at_the_leaf_or_in_a_parent() {
        let temp = std::env::temp_dir();
        #[cfg(unix)]
        let temp = fs::canonicalize(temp).unwrap();
        let root = temp.join(new_id());
        let actual = root.join("actual");
        fs::create_dir_all(&actual).unwrap();
        let file = actual.join("input");
        fs::write(&file, b"original").unwrap();
        let dir_link = root.join("dir-link");
        let file_link = root.join("file-link");
        #[cfg(windows)]
        use std::os::windows::fs::symlink_file;
        #[cfg(unix)]
        use std::os::unix::fs::symlink as symlink_file;
        let created = |result: io::Result<()>, link: &Path| match result {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied || cfg!(windows) && e.raw_os_error() == Some(1314) => {
                eprintln!("SKIP {}: link creation not permitted: {e}", link.display());
                false
            }
            Err(e) => panic!("creating {}: {e}", link.display()),
        };
        #[cfg(windows)]
        let dir_created = {
            // A junction exercises the parent check even on Windows without symlink privilege.
            let output = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command",
                    "try { New-Item -ItemType Junction -Path $env:TEST_LINK -Target $env:TEST_TARGET -ErrorAction Stop | Out-Null } catch [UnauthorizedAccessException] { exit 5 }"])
                .env("TEST_LINK", &dir_link).env("TEST_TARGET", &actual).output().unwrap();
            if output.status.code() == Some(5) {
                created(Err(io::Error::from_raw_os_error(5)), &dir_link)
            } else {
                assert!(output.status.success(), "creating junction: {}", String::from_utf8_lossy(&output.stderr));
                true
            }
        };
        #[cfg(unix)]
        let dir_created = created(symlink_file(&actual, &dir_link), &dir_link);
        let hash = sha256(b"original");
        let assert_link = |e: DltError| {
            assert_eq!(e.code, ErrorCode::InvalidArgument);
            assert_eq!(e.message, "file path contains a link or reparse point");
        };
        let check_existing = |path: &Path, leaf_link: bool| {
            let raw = path.to_str().unwrap();
            assert_eq!(fs::read(path).unwrap(), b"original", "link must point at a readable file");
            assert_link(check_source_path(raw, true).unwrap_err());
            assert_link(check_target(&root, raw, true, Some(&hash), true).err().unwrap());
            // The user's GUI path follows the link, as before 66c (OneDrive and profile junctions).
            drop(check_source_path(raw, false).unwrap());
            if !leaf_link { check_target(&root, raw, true, Some(&hash), false).unwrap(); }
        };
        let raw = file.to_str().unwrap();
        drop(check_source_path(raw, true).unwrap());
        check_target(&root, raw, true, Some(&hash), true).unwrap();
        if created(symlink_file(&file, &file_link), &file_link) {
            check_existing(&file_link, true);
        }
        if dir_created {
            check_existing(&dir_link.join("input"), false);
            assert_link(check_target(&root, dir_link.join("new").to_str().unwrap(), false, None, true).err().unwrap());
            check_target(&root, dir_link.join("new").to_str().unwrap(), false, None, false).unwrap();
            assert!(!actual.join("new").exists());
        }
        assert_eq!(fs::read(&file).unwrap().as_slice(), b"original");
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn checked_paths_allow_write_through_rename_and_an_open_writer() {
        let root = std::env::temp_dir().join(new_id());
        fs::create_dir(&root).unwrap();
        let path = root.join("output");
        let target = check_target(&root, path.to_str().unwrap(), false, None, true).unwrap();
        let temp = root.join("partial");
        fs::write(&temp, b"saved").unwrap();
        crate::fsx::rename_durable(&temp, &target.path, false).unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let canon = fs::canonicalize(&path).unwrap();
        assert!(same_target(&root.join("OUTPUT"), &path, Some(&canon), true));
        drop(writer);
        drop(target);
        fs::remove_dir_all(root).unwrap();
    }
}

/// Who named the file paths of the request being dispatched (66c r3). An agent's path (any connection but the
/// verified gui one, or a gui request marked [`local_path::AGENT_PATH_PARAM`]) gets the strict local rules: no UNC or
/// device path, a local drive, no link or reparse point from the root to the file. A path the user picked in the
/// GUI, and a recorded save path, resolve as the OS does it (OneDrive, mapped and UNC drives, links), as before 66c.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PathCaller { #[default] Agent, Gui }

/// Removes the agent mark from a request's params; true when it was there (any value but `false`).
pub(crate) fn take_agent_path(params: &mut Value) -> bool {
    params.as_object_mut().and_then(|p| p.remove(local_path::AGENT_PATH_PARAM)).is_some_and(|mark| mark != Value::Bool(false))
}

fn absolute_clean(raw: &str, what: &str, strict: bool) -> DltResult<PathBuf> {
    if raw.trim().is_empty() { return Err(invalid(format!("{what} is empty"))); }
    if raw.contains('\0') { return Err(invalid(format!("{what} contains NUL"))); }
    let path = PathBuf::from(raw);
    if !path.is_absolute() { return Err(invalid(format!("{what} must be absolute: {raw:?}"))); }
    if path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(invalid(format!("{what} must not contain . or .. components: {raw:?}")));
    }
    if path.file_name().is_none() { return Err(invalid(format!("{what} has no file name: {raw:?}"))); }
    if strict && !local_path::is_local_file_path(raw) { return Err(invalid(format!("{what} must name a local file"))); }
    Ok(path)
}

/// artifact.open / asset.import input: absolute, regular file, bounded size. `strict`: [`PathCaller::Agent`].
pub(crate) fn check_source_path(raw: &str, strict: bool) -> DltResult<(PathBuf, fs::File)> {
    let path = absolute_clean(raw, "path", strict)?;
    let file = if strict {
        let parent = local::Parent::open(path.parent().unwrap(), true)?;
        local::open_file(&parent.path.join(path.file_name().unwrap()), false, true)?
    } else {
        let missing = || not_found(format!("{raw:?} does not exist"));
        match fs::metadata(&path) {
            Ok(m) if m.is_file() => fs::File::open(&path).map_err(|e| if e.kind() == io::ErrorKind::NotFound { missing() } else { DltError::from(e) })?,
            Ok(_) => return Err(invalid(format!("{raw:?} is not a regular file"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(missing()),
            Err(e) => return Err(DltError::from(e)),
        }
    };
    let len = file.metadata()?.len();
    if len > MAX_IMPORT_BYTES { return Err(invalid(format!("{raw:?} is {len} bytes; the limit is {MAX_IMPORT_BYTES}"))); }
    Ok((path, file))
}
pub(crate) fn read_source(file: impl io::Read) -> DltResult<Vec<u8>> {
    read_limited(file, MAX_IMPORT_BYTES)
}

fn read_limited(file: impl io::Read, limit: u64) -> DltResult<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit { return Err(invalid(format!("file exceeds the {limit} byte import limit"))); }
    Ok(bytes)
}

pub(crate) fn read_artifact_input(path: &Path, file: fs::File, kind: Option<ArtifactKind>) -> DltResult<(ArtifactKind, Vec<u8>)> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if ext == "designllm" {
        let limit = native_envelope::NATIVE_MAX_FILE_BYTES;
        if file.metadata()?.len() > limit { return Err(invalid(format!("native project file exceeds the {limit} byte limit"))); }
        let bytes = read_limited(file, limit)?;
        let kind = match kind { Some(kind) => kind, None => native_project_kind(&bytes)? };
        return Ok((kind, bytes));
    }
    let kind = match (kind, ext.as_str()) {
        (Some(kind), _) => kind,
        (None, "pptx") => ArtifactKind::Slides,
        (None, "docx") => ArtifactKind::Docx,
        (None, "hwpx") => ArtifactKind::Hwpx,
        (None, "hwp") => ArtifactKind::Hwp,
        _ => return Err(invalid(format!("cannot infer the artifact kind of {:?}; pass kind explicitly", path.display()))),
    };
    Ok((kind, read_source(file)?))
}

fn native_project_kind(bytes: &[u8]) -> DltResult<ArtifactKind> {
    #[derive(Deserialize)]
    struct Header<'a> { format: &'a str }
    let header: Header = serde_json::from_slice(bytes).map_err(|_| invalid("invalid DesignLLM project format header"))?;
    match header.format {
        "designllm.canvas" => Ok(ArtifactKind::Canvas),
        "designllm.web" => Ok(ArtifactKind::WebCode),
        "designllm.figure-project" => Ok(ArtifactKind::Figure),
        _ => Err(invalid("unrecognized DesignLLM project format")),
    }
}

pub(crate) fn mime_for(path: &Path, bytes: &[u8]) -> String {
    if bytes.starts_with(&designllm_contracts::media_header::PNG_SIGNATURE) { return "image/png".into(); }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) { return "image/jpeg".into(); }
    if bytes.starts_with(b"%PDF-") { return "application/pdf".into(); }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") { return "image/gif".into(); }
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    // ISO-BMFF `ftyp` also names audio, QuickTime and HEIF files; only an explicit `.mp4` extension is an MP4 declaration.
    match ext.as_str() {
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "hwpx" => "application/hwp+zip",
        "hwp" => "application/x-hwp",
        "bmp" => "image/bmp", "svg" => "image/svg+xml", "webp" => "image/webp", "tif" | "tiff" => "image/tiff",
        "mp4" => "video/mp4", "webm" => "video/webm",
        "csv" => "text/csv", "tsv" | "tab" => "text/tab-separated-values", "txt" => "text/plain",
        "json" => "application/json", "html" | "htm" => "text/html", "css" => "text/css", "js" => "text/javascript",
        _ => "application/octet-stream",
    }.into()
}

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    path: PathBuf,
    hash: Option<String>,
}

impl ProjectFile {
    fn stored(conn: &Connection, artifact_id: &str) -> DltResult<Option<Self>> {
        let value: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1",
            [format!("project_file:{artifact_id}")], |row| row.get(0)).optional().map_err(sql)?;
        value.map(|text| store::from_json(&text, "project file")).transpose()
    }

    fn record(&self, conn: &Connection, artifact_id: &str) -> DltResult<()> {
        conn.execute("INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![format!("project_file:{artifact_id}"), store::to_json(self)?]).map_err(sql)?;
        Ok(())
    }

    fn path_in(&self, project: &Path) -> DltResult<PathBuf> {
        let name = self.path.file_name().ok_or_else(|| invalid("stored project file has no file name"))?;
        Ok(project.join(PROJECT_ARTIFACTS_DIR).join(name))
    }

    fn refresh_pending(&mut self, conn: &Connection, artifact_id: &str, before_revision: Option<Revision>) -> DltResult<()> {
        let path = self.path.to_str().ok_or_else(|| invalid("project path is not Unicode"))?;
        let before_revision = before_revision.map(i64::try_from).transpose().map_err(|_| invalid("revision exceeds the workspace integer range"))?;
        let mut statement = conn.prepare("SELECT content_hash FROM pending_saves WHERE artifact_id=?1 AND purpose='save' AND target_path=?2 AND (?3 IS NULL OR revision < ?3)").map_err(sql)?;
        let hashes = statement.query_map(params![artifact_id, path, before_revision], |row| row.get::<_, String>(0)).map_err(sql)?
            .collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?;
        if hashes.is_empty() { return Ok(()); }
        // A Save As or a later revision can retire an interrupted publication's pending row.
        let Ok(target) = Target::resolve(path, true) else { return Ok(()) };
        if let Ok(Some(hash)) = file_hash(&target.io_path(), true) {
            if hashes.contains(&hash) {
                self.hash = Some(hash);
                self.record(conn, artifact_id)?;
            }
        }
        Ok(())
    }

    fn destination(root: &Path, row: &store::ArtifactRow, stored: Option<Self>) -> DltResult<Self> {
        let root = std::path::absolute(root)?;
        if !root.file_name().is_some_and(|name| name.eq_ignore_ascii_case(PROJECT_STORE_DIR)) {
            return Err(invalid("target_path is required outside a project workspace"));
        }
        let project = root.parent().ok_or_else(|| invalid("workspace has no project folder"))?;
        let parent = local::Parent::open(project, true)?;
        parent.recheck()?;
        match fs::create_dir(parent.path.join(PROJECT_ARTIFACTS_DIR)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(stored) = stored { return Ok(Self { path: stored.path_in(project)?, hash: stored.hash }); }
        let title: String = row.title.trim().chars().take(32)
            .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
        let title = if title.trim().is_empty() { "Artifact" } else { title.trim_end() };
        let id_hash = sha256(row.id.as_bytes());
        let file_name = format!("{title}-{}.{}", &id_hash[..16], artifact_native_extension(row.kind));
        Ok(Self { path: project.join(PROJECT_ARTIFACTS_DIR).join(file_name), hash: None })
    }
}

#[cfg(test)]
mod mime_tests {
    use super::mime_for;
    use std::path::Path;

    #[test]
    fn ambiguous_iso_bmff_ftyp_is_not_sniffed_as_video_without_an_mp4_extension() {
        let mut audio = Vec::new();
        audio.extend_from_slice(&16u32.to_be_bytes());
        audio.extend_from_slice(b"ftypM4A ");
        audio.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(mime_for(Path::new("audio.m4a"), &audio), "application/octet-stream");
        assert_eq!(mime_for(Path::new("movie.mov"), b"\0\0\0\x10ftypqt  \0\0\0\0"), "application/octet-stream");
        assert_eq!(mime_for(Path::new("still.heic"), b"\0\0\0\x10ftypheic\0\0\0\0"), "application/octet-stream");
        assert_eq!(mime_for(Path::new("movie.mp4"), b"\0\0\0\x10ftypisom\0\0\0\0"), "video/mp4");
        assert_eq!(mime_for(Path::new("movie.mkv"), b"\x1A\x45\xDF\xA3"), "application/octet-stream");
        assert_eq!(mime_for(Path::new("movie.webm"), b"\x1A\x45\xDF\xA3"), "video/webm");
    }
}

fn file_hash(path: &Path, strict: bool) -> DltResult<Option<String>> {
    let mut file = match local::open_file(path, false, strict) {
        Ok(file) => file,
        Err(e) if e.code == ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(sha256(&bytes)))
}

pub(crate) struct Target { pub path: PathBuf, parent: local::Parent, file_name: String }

impl Target {
    fn resolve(raw: &str, strict: bool) -> DltResult<Self> {
        let path = absolute_clean(raw, "target_path", strict)?;
        let parent = local::Parent::open(path.parent().unwrap(), strict)?;
        let file_name = path.file_name().unwrap().to_str().ok_or_else(|| invalid("invalid file name"))?.to_owned();
        Ok(Self { path, parent, file_name })
    }

    fn io_path(&self) -> PathBuf { self.parent.path.join(&self.file_name) }
    /// Whether the agent rules apply to this target ([`PathCaller::Agent`]).
    fn strict(&self) -> bool { self.parent.strict }
}

/// Caller target rules: absolute, clean, existing parent directory, not a workspace-internal
/// file, ALREADY_EXISTS without overwrite, INVALID_ARGUMENT for overwrite without hash,
/// SOURCE_CHANGED for a missing or different target.
pub(crate) fn check_target(root: &Path, raw: &str, overwrite: bool, expected: Option<&str>, strict: bool) -> DltResult<Target> {
    let target = target_location(root, raw, strict)?;
    check_target_state(&target, overwrite, expected)?;
    Ok(target)
}

/// Path rules of [`check_target`]: everything except the existing file's state.
fn target_location(root: &Path, raw: &str, strict: bool) -> DltResult<Target> {
    let target = Target::resolve(raw, strict)?;
    if target.file_name.ends_with(TEMP_SUFFIX) { return Err(invalid("target_path uses the reserved temporary suffix")); }
    let parent_canon = fs::canonicalize(&target.parent.path)?;
    if let Ok(root_canon) = fs::canonicalize(root) {
        let internal = [store::DB_FILE.to_string(), format!("{}-wal", store::DB_FILE), format!("{}-shm", store::DB_FILE), CORE_LOCK_FILE.to_string()];
        if parent_canon.starts_with(root_canon.join(store::BLOB_DIR)) || (parent_canon == root_canon && internal.iter().any(|n| n.eq_ignore_ascii_case(&target.file_name))) {
            return Err(invalid("target_path points into workspace-internal storage"));
        }
    }
    Ok(target)
}

/// Existing-file rules of [`check_target`].
fn check_target_state(target: &Target, overwrite: bool, expected: Option<&str>) -> DltResult<()> {
    let path = target.io_path();
    let exists = match local::file_metadata(&path, target.strict())? {
        Some(m) if m.is_file() => true,
        Some(_) => return Err(invalid("target_path exists and is not a regular file")),
        None => false,
    };
    if overwrite {
        let expected = expected.ok_or_else(|| invalid("overwrite=true requires expected_target_hash"))?;
        if !is_hash(expected) { return Err(invalid("expected_target_hash must be a lowercase SHA-256 hex string")); }
        match file_hash(&path, target.strict())? {
            None => return Err(err(ErrorCode::SourceChanged, "target to overwrite no longer exists")),
            Some(h) if h != expected => return Err(err(ErrorCode::SourceChanged, "target differs from expected_target_hash; nothing was written").ids([target.path.display().to_string()])),
            Some(_) => {}
        }
    } else if exists {
        return Err(err(ErrorCode::AlreadyExists, "target exists and overwrite=false").ids([target.path.display().to_string()]));
    }
    Ok(())
}

/// temp write + fsync + verify, then no-replace rename (or hash re-check + replace). Returns the
/// content hash — the caller drops the pending row with its record ([`clear_pending`]).
/// A failure after the rename keeps the pending row: the target already holds these bytes, so
/// a retry of the same publication ([`check_target_or_pending`]) or the next open records it.
#[allow(clippy::too_many_arguments)]
fn publish(conn: &Connection, hook: &SharedHook, target: &Target, overwrite: bool, expected: Option<&str>, bytes: &[u8], artifact_id: &str, revision: Revision, snapshot: &str, purpose: &str) -> DltResult<String> {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let temp_name = format!(".{}.{}-{nanos}-{n}{TEMP_SUFFIX}", target.file_name, std::process::id());
    let temp = target.parent.path.join(&temp_name);
    let destination = target.io_path();
    let temp_str = target.path.with_file_name(temp_name).to_string_lossy().into_owned();
    let hash = sha256(bytes);
    conn.execute(
        "INSERT INTO pending_saves(temp_path, artifact_id, target_path, revision, content_hash, snapshot, purpose) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![temp_str, artifact_id, target.path.to_string_lossy(), revision as i64, hash, snapshot, purpose],
    ).map_err(sql)?;
    let mut renamed = false;
    let mut created_temp = false;
    let outcome = (|| -> DltResult<String> {
        {
            target.parent.recheck()?;
            let mut f = OpenOptions::new().write(true).create_new(true).open(&temp).map_err(DltError::from)?;
            created_temp = true;
            f.write_all(bytes).map_err(DltError::from)?;
            f.sync_all().map_err(DltError::from)?;
        }
        fire(hook, FaultPoint::NativeTempWritten)?;
        target.parent.recheck()?;
        if file_hash(&temp, target.strict())?.as_deref() != Some(hash.as_str()) { return Err(err(ErrorCode::IntegrityError, "temporary file differs from the encoded bytes")); }
        target.parent.recheck()?;
        check_target_state(target, overwrite, expected)?;
        if overwrite {
            // Narrows, does not close, the check-then-replace window (documented limitation).
            crate::fsx::rename_durable(&temp, &destination, true).map_err(DltError::from)?;
        } else {
            match crate::fsx::rename_durable(&temp, &destination, false) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(err(ErrorCode::AlreadyExists, "target appeared during save; it was not replaced")),
                Err(e) => return Err(DltError::from(e)),
            }
        }
        renamed = true;
        target.parent.recheck()?;
        local::open_file(&destination, true, target.strict())?.sync_all()?;
        fire(hook, FaultPoint::NativeAfterRename)?;
        Ok(hash.clone())
    })();
    match outcome {
        Ok(h) => Ok(h),
        Err(e) if renamed => Err(e.diags(vec![Diagnostic {
            code: "target_written_unrecorded".into(),
            message: "the target already holds the new bytes but the save was not recorded; retry the same request to record it".into(),
            node_id: None, blocking: false, params: Default::default(),
        }])),
        Err(e) => {
            if created_temp && (target.parent.recheck().is_err() || fs::remove_file(&temp).is_err()) {
                return Err(e); // Keep the registration for recovery.
            }
            let _ = conn.execute("DELETE FROM pending_saves WHERE temp_path = ?1", params![temp_str]);
            Err(e)
        }
    }
}

/// A `pending_saves` row left on a target file.
struct PendingRow { temp: String, hash: String, revision: Revision, snapshot: String }

/// Whether a recorded path names `target`: the same string, or the same file once
/// `fs::canonicalize` (as `target_location` resolves the parent) resolves both, so `C:\d\a.json`
/// and `c:/d/A.json` match on Windows. `canon` is `target` canonicalized once by the caller.
fn same_target(recorded: &Path, target: &Path, canon: Option<&Path>, strict: bool) -> bool {
    recorded == target || canon.is_some_and(|c| local::canonical_file(recorded, strict).as_deref() == Some(c))
}

/// The pending rows of this artifact and purpose whose target is the same file as `target`.
fn pending_rows(conn: &Connection, target: &Target, artifact_id: &str, purpose: &str) -> DltResult<Vec<PendingRow>> {
    let canon = local::canonical_file(&target.path, target.strict());
    let mut st = conn.prepare("SELECT temp_path, target_path, content_hash, revision, snapshot FROM pending_saves WHERE artifact_id = ?1 AND purpose = ?2").map_err(sql)?;
    let rows = st.query_map(params![artifact_id, purpose], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?))).map_err(sql)?;
    let mut out = Vec::new();
    for row in rows {
        let (temp, path, hash, revision, snapshot) = row.map_err(sql)?;
        if same_target(Path::new(&path), &target.path, canon.as_deref(), target.strict()) { out.push(PendingRow { temp, hash, revision: revision as Revision, snapshot }); }
    }
    Ok(out)
}

/// Commit-time cleanup: drops the pending rows of this artifact and purpose that the recorded
/// publication supersedes, so a stale row can neither complete a later request nor move
/// saved_path at the next open. Every row on the recorded target goes (the file now holds these
/// bytes); `saved_revision` (a save) also drops the rows of that revision or older on any other
/// path, whose files keep an earlier attempt's bytes but are no longer the record (the files stay).
/// `&mut self` on save and export keeps any other publication of the workspace out of flight.
fn clear_pending(conn: &Connection, target: &Target, artifact_id: &str, purpose: &str, saved_revision: Option<Revision>) -> DltResult<()> {
    if let Some(revision) = saved_revision {
        conn.execute("DELETE FROM pending_saves WHERE artifact_id = ?1 AND purpose = ?2 AND revision <= ?3", params![artifact_id, purpose, revision as i64]).map_err(sql)?;
    }
    for row in pending_rows(conn, target, artifact_id, purpose)? {
        conn.execute("DELETE FROM pending_saves WHERE temp_path = ?1", params![row.temp]).map_err(sql)?;
    }
    Ok(())
}

/// The target still holds `hash`: synced again and true; otherwise false and nothing synced.
fn holds(target: &Target, hash: &str) -> DltResult<bool> {
    let path = target.io_path();
    if file_hash(&path, target.strict())?.as_deref() != Some(hash) { return Ok(false); }
    local::open_file(&path, true, target.strict())?.sync_all()?;
    Ok(true)
}

/// [`check_target`], except that a refusal of the existing file comes back (instead of as the
/// error) with the pending rows that earlier attempts of this revision left on the same file
/// after their rename; the caller decides whether one of them is this publication's result.
/// Such a retry does not compare expected_target_hash again: the file already holds this
/// publication's bytes, which the retry would have written.
#[allow(clippy::too_many_arguments)]
fn check_target_or_pending(conn: &Connection, root: &Path, raw: &str, overwrite: bool, expected: Option<&str>, artifact_id: &str, revision: Revision, snapshot: &str, purpose: &str, strict: bool) -> DltResult<(Target, Option<(DltError, Vec<PendingRow>)>)> {
    let target = target_location(root, raw, strict)?;
    match check_target_state(&target, overwrite, expected) {
        Ok(()) => Ok((target, None)),
        Err(e) if matches!(e.code, ErrorCode::AlreadyExists | ErrorCode::SourceChanged) => {
            let rows = pending_rows(conn, &target, artifact_id, purpose)?.into_iter().filter(|r| r.revision == revision && r.snapshot == snapshot).collect();
            Ok((target, Some((e, rows))))
        }
        Err(e) => Err(e),
    }
}

/// The save this request would make is already on the refused target: an earlier attempt's
/// pending row whose bytes the file holds, synced again before it is reported. Returns the
/// content hash. Limitation: the next open records such an attempt (`recover_pending`) and drops
/// its row, so the original request retried after a reopen is refused like any other existing
/// file (overwrite=false keeps its meaning); the refusal then carries `target_already_saved`.
/// Exports are not recorded at open at all.
fn finished_save(target: &Target, rows: &[PendingRow]) -> DltResult<Option<String>> {
    let Some(current) = file_hash(&target.io_path(), target.strict())? else { return Ok(None) };
    if !rows.iter().any(|r| r.hash == current) || !holds(target, &current)? { return Ok(None); }
    Ok(Some(current))
}

/// A refused save whose target is already the recorded save of this revision and snapshot gets
/// a non-blocking `target_already_saved` diagnostic; the refusal itself is unchanged.
fn note_recorded_save(e: DltError, target: &Target, row: &store::ArtifactRow) -> DltError {
    let recorded = row.saved_revision == Some(row.current_revision) && row.saved_snapshot.as_deref() == Some(row.current_snapshot.as_str())
        && row.saved_path.as_deref().is_some_and(|p| same_target(Path::new(p), &target.path, local::canonical_file(&target.path, target.strict()).as_deref(), target.strict()))
        && matches!(file_hash(&target.io_path(), target.strict()), Ok(Some(h)) if row.saved_hash.as_deref() == Some(h.as_str()));
    if !recorded { return e; }
    e.diags(vec![Diagnostic {
        code: "target_already_saved".into(),
        message: "the target already holds the recorded save of this revision; nothing was written".into(),
        node_id: None, blocking: false, params: Default::default(),
    }])
}

/// Open-time handling of in-flight publications: remove exactly the registered temp files; a
/// target that already holds the bytes being saved is recorded as saved. One transaction, rows in
/// revision order and then registration order (rowid): the newest revision ends as the record,
/// the first registered of one revision stays it, and a failure records nothing.
pub(crate) fn recover_pending(conn: &mut Connection, recovered: &mut usize) -> DltResult<usize> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
    let pending: Vec<(String, String, String, i64, String, String, String)> = {
        let mut st = tx.prepare("SELECT temp_path, artifact_id, target_path, revision, content_hash, snapshot, purpose FROM pending_saves ORDER BY revision, rowid").map_err(sql)?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).map_err(sql)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?
    };
    let (mut removed, mut saved) = (0, 0);
    for (temp, artifact_id, target, revision, content_hash, snapshot, purpose) in &pending {
        let mut project_file = if purpose == "save" {
            ProjectFile::stored(&tx, artifact_id)?.filter(|file| file.path == Path::new(target))
        } else { None };
        let strict = project_file.is_some();
        let path = Path::new(temp);
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(TEMP_SUFFIX)) {
            let remove_temp = |target: Target| -> DltResult<bool> {
                let path = target.io_path();
                match local::file_metadata(&path, strict)? {
                    None => return Ok(false),
                    Some(meta) if !meta.is_file() => return Err(err(ErrorCode::StorageFailure, "pending temporary path is not a regular file")),
                    Some(_) => {}
                }
                match fs::remove_file(path) {
                    Ok(()) => Ok(true),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                    Err(e) => Err(io_store(e)),
                }
            };
            match Target::resolve(temp, strict).and_then(remove_temp) {
                Ok(deleted) => removed += usize::from(deleted),
                Err(e) if matches!(e.code, ErrorCode::InvalidArgument | ErrorCode::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        if purpose == "save" && Target::resolve(target, strict).and_then(|target| file_hash(&target.io_path(), strict)).ok().flatten().as_ref() == Some(content_hash) {
            if let Some(file) = project_file.as_mut() {
                file.hash = Some(content_hash.clone());
                file.record(&tx, artifact_id)?;
            }
            saved += tx.execute(
                // Strictly newer only: a save of the same revision recorded elsewhere stays the record.
                "UPDATE artifacts SET saved_revision = ?2, saved_path = ?3, saved_hash = ?4, saved_snapshot = ?5 WHERE artifact_id = ?1 AND (saved_revision IS NULL OR saved_revision < ?2)",
                params![artifact_id, revision, target, content_hash, snapshot],
            ).map_err(sql)?;
        }
    }
    if !pending.is_empty() { tx.execute("DELETE FROM pending_saves", []).map_err(sql)?; }
    tx.commit().map_err(sql)?;
    *recovered += saved;
    Ok(removed)
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportResult {
    pub request_id: Id, pub artifact_id: Id, pub revision: Revision, pub format: String, pub target_path: String,
    pub sha256: String, pub bytes: u64, pub mime: String, pub editability: Editability, pub diagnostics: Vec<Diagnostic>,
}

pub(crate) fn editability_rank(e: Editability) -> u8 { match e { Editability::None => 0, Editability::Vector => 1, Editability::Objects => 2, Editability::NativeData => 3 } }

/// Largest `ExportRequest.node_ids`.
const MAX_EXPORT_NODES: usize = 10_000;

/// `ExportRequest.node_ids`: non-empty, unique, existing nodes, and an engine that exports only
/// those nodes in this format (else UNSUPPORTED_OPERATION, instead of exporting everything).
fn check_node_export(engine: &dyn EngineAdapter, artifact: &Artifact, context: &EngineContext<'_>, format: &str, node_ids: Option<&[Id]>) -> DltResult<()> {
    let Some(ids) = node_ids else { return Ok(()) };
    if ids.is_empty() || ids.len() > MAX_EXPORT_NODES { return Err(invalid(format!("node_ids must name 1..={MAX_EXPORT_NODES} nodes; leave it out to export whole pages"))); }
    let mut seen = std::collections::BTreeSet::new();
    if let Some(twice) = ids.iter().find(|id| !seen.insert(id.as_str())) { return Err(invalid(format!("node_ids lists {twice} twice")).ids([twice.clone()])); }
    if !engine.exports_nodes(format) {
        return Err(err(ErrorCode::UnsupportedOperation, format!("engine {} cannot export only some nodes as {format}; export whole pages or another format", engine.id())));
    }
    engine.validate_read_source(artifact, context)?;
    engine.inspect(artifact, ids)?;
    Ok(())
}

impl Workspace {
    /// [`Workspace::dispatch_start`] for the host-verified gui connection (`client_role: "gui"`, not job-bound): the
    /// paths of artifact.open, asset.import, artifact.save and artifact.export are the user's ([`PathCaller::Gui`]),
    /// unless the window marks the request with [`local_path::AGENT_PATH_PARAM`] (an agent's ui.command.run).
    pub fn dispatch_start_gui(&mut self, request: RpcRequest) -> crate::Dispatch {
        self.path_caller = PathCaller::Gui;
        let dispatched = self.dispatch_start(request, None);
        self.path_caller = PathCaller::Agent;
        dispatched
    }

    /// Whether the request being dispatched gets the agent path rules.
    pub(crate) fn agent_paths(&self) -> bool { self.path_caller == PathCaller::Agent }

    fn save_state_of(&self, row: &store::ArtifactRow, check_external: bool) -> DltResult<SaveState> {
        let mut target_path = row.saved_path.clone();
        let mut project_target = false;
        if self.root.file_name().is_some_and(|name| name.eq_ignore_ascii_case(PROJECT_STORE_DIR)) {
            if let Some(file) = ProjectFile::stored(&self.conn, &row.id)?.filter(|file| file.path.to_str() == row.saved_path.as_deref()) {
                let root = std::path::absolute(&self.root)?;
                target_path = Some(file.path_in(root.parent().ok_or_else(|| invalid("workspace has no project folder"))?)?.to_string_lossy().into_owned());
                project_target = true;
            }
        }
        let externally_modified = match (check_external, &target_path, &row.saved_hash) {
            (true, Some(path), Some(hash)) if project_target => match Target::resolve(path, true) {
                Ok(target) => file_hash(&target.io_path(), true)?.as_deref() != Some(hash),
                Err(error) if error.code == ErrorCode::NotFound => true,
                Err(error) => return Err(error),
            },
            (true, Some(p), Some(h)) => file_hash(Path::new(p), false)?.as_deref() != Some(h.as_str()),
            _ => false,
        };
        Ok(SaveState {
            artifact_id: row.id.clone(), workspace_revision: row.current_revision, saved_revision: row.saved_revision,
            target_path, target_hash: row.saved_hash.clone(), dirty: row.dirty(), externally_modified,
        })
    }
    pub(crate) fn save_state(&mut self, p: crate::routes::ArtifactIdParams) -> DltResult<Value> {
        let row = store::artifact_row(&self.conn, &p.artifact_id)?;
        to_value(&self.save_state_of(&row, true)?)
    }

    /// artifact.save: native same-format file. encode_native (or the untouched original bytes)
    /// -> verify_saved (no blocking diagnostics) -> publish. Workspace commits are untouched.
    pub(crate) fn save(&mut self, req: SaveRequest) -> DltResult<Value> {
        let key = key_for("artifact.save", &req.request_id, &req)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        let row = store::artifact_row(&self.conn, &req.artifact_id)?;
        if req.expected_revision != row.current_revision {
            return Err(err(ErrorCode::RevisionConflict, format!("expected_revision {} is not the current revision {}", req.expected_revision, row.current_revision)).at(row.current_revision));
        }
        let use_project = req.target_path.is_empty();
        if use_project && (req.overwrite || req.expected_target_hash.is_some()) {
            return Err(invalid("overwrite and expected_target_hash require an explicit target_path"));
        }
        let mut stored = if self.root.file_name().is_some_and(|name| name.eq_ignore_ascii_case(PROJECT_STORE_DIR)) {
            ProjectFile::stored(&self.conn, &row.id)?
        } else { None };
        if let Some(file) = stored.as_mut() { file.refresh_pending(&self.conn, &row.id, use_project.then_some(row.current_revision))?; }
        let mut project_file = if use_project {
            Some(ProjectFile::destination(&self.root, &row, stored)?)
        } else {
            stored.filter(|file| file.path == Path::new(&req.target_path))
        };
        let (path, overwrite, expected) = if use_project {
            let file = project_file.as_ref().expect("a project destination was resolved");
            (file.path.to_str().ok_or_else(|| invalid("project path is not Unicode"))?, file.hash.is_some(), file.hash.as_deref())
        } else {
            (req.target_path.as_str(), req.overwrite, req.expected_target_hash.as_deref())
        };
        let ext = artifact_native_extension(row.kind);
        if ext != "designllm" {
            let got = Path::new(path).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            if got != ext { return Err(invalid(format!("a {:?} artifact is saved as .{ext}, not {:?}", row.kind, req.target_path))); }
        }
        let (target, refused) = check_target_or_pending(&self.conn, &self.root, path, overwrite, expected, &row.id, row.current_revision, &row.current_snapshot, "save", use_project || self.agent_paths())?;
        let finished = match refused {
            None => None,
            Some((e, rows)) => match finished_save(&target, &rows)? {
                Some(hash) => Some(hash),
                None => return Err(note_recorded_save(e, &target, &row)),
            },
        };
        let (hash, saved_detail) = match finished {
            // An earlier save of this revision to this target encoded, verified and renamed these
            // bytes into place. Its verification is not repeated, so this event detail carries
            // completed_after_failure and no checked_parts / checked_regions.
            Some(hash) => (hash, json!({ "completed_after_failure": true, "diagnostics": [] })),
            None => {
                let engine = self.engine_by_id(&row.engine_id)?;
                let artifact = self.load(&*engine, &row.current_snapshot, row.current_revision)?;
                let staging = self.staging();
                let renderer = self.renderer.clone();
                let ctx = EngineContext { assets: &staging, renderer: &*renderer };
                // Unchanged since open: the original file bytes are the exact native representation.
                let untouched = row.current_snapshot == row.base_snapshot && row.source_hash.is_some()
                    && artifact.native.as_ref().map(|n| Some(&n.asset.sha256) == row.source_hash.as_ref()).unwrap_or(false);
                let (bytes, mut diagnostics) = if untouched {
                    engine.validate_read_source(&artifact, &ctx)?;
                    (staging.read(&artifact.native.as_ref().map(|n| n.asset.clone()).unwrap_or(AssetRef { sha256: String::new(), mime: String::new(), bytes: 0 }))?, vec![Diagnostic { code: "original_bytes".into(), message: "unchanged since open; original file bytes written".into(), node_id: None, blocking: false, params: Default::default() }])
                } else {
                    let out = engine.encode_native(&artifact, &ctx)?;
                    let refused: Vec<Diagnostic> = out.diagnostics.iter().filter(|d| d.blocking).cloned().collect();
                    if !refused.is_empty() { return Err(err(ErrorCode::UnsafeRoundtrip, "native encoder reported blocking diagnostics; nothing was written").diags(refused)); }
                    (out.bytes, out.diagnostics)
                };
                let verification = engine.verify_saved(&artifact, &bytes, &ctx)?;
                let refused: Vec<Diagnostic> = verification.diagnostics.iter().filter(|d| d.blocking).cloned().collect();
                if !refused.is_empty() {
                    return Err(err(ErrorCode::UnsafeRoundtrip, format!("verify_saved found {} blocking difference(s); nothing was written", refused.len())).diags(refused));
                }
                diagnostics.extend(verification.diagnostics);
                // Recovery needs the destination before rename. Ownership of the new bytes is recorded only after publication.
                if let Some(file) = &project_file { file.record(&self.conn, &row.id)?; }
                let hash = publish(&self.conn, &self.hook, &target, overwrite, expected, &bytes, &row.id, row.current_revision, &row.current_snapshot, "save")?;
                (hash, json!({ "checked_parts": verification.checked_parts, "checked_regions": verification.checked_regions, "diagnostics": diagnostics }))
            }
        };
        let state = SaveState {
            artifact_id: row.id.clone(), workspace_revision: row.current_revision, saved_revision: Some(row.current_revision),
            target_path: Some(target.path.to_string_lossy().into_owned()), target_hash: Some(hash.clone()), dirty: false, externally_modified: false,
        };
        let result = to_value(&state)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        clear_pending(&tx, &target, &row.id, "save", Some(row.current_revision))?;
        if let Some(file) = project_file.as_mut() {
            file.hash = Some(hash.clone());
            file.record(&tx, &row.id)?;
        }
        tx.execute(
            "UPDATE artifacts SET saved_revision = ?2, saved_path = ?3, saved_hash = ?4, saved_snapshot = ?5 WHERE artifact_id = ?1",
            params![row.id, row.current_revision as i64, target.path.to_string_lossy(), hash, row.current_snapshot],
        ).map_err(sql)?;
        store::insert_receipt(&tx, &key, Some(&row.id), &StoredResult::Committed { result: result.clone() })?;
        let mut detail = json!({ "request_id": key.request_id, "target_path": target.path.to_string_lossy(), "target_hash": hash });
        if let (Some(detail), Value::Object(extra)) = (detail.as_object_mut(), saved_detail) { detail.extend(extra); }
        store::insert_event(&tx, Some(&row.id), Some(row.current_revision), "artifact.saved", &detail)?;
        fire(&self.hook, FaultPoint::TxnBeforeCommit)?;
        tx.commit().map_err(sql)?;
        Ok(result)
    }

    /// artifact.export: other-format output. Refuses (EXPORT_LOSS_REQUIRED) when the output's
    /// editability is below the request or a blocking loss is not in allowed_losses.
    pub(crate) fn export(&mut self, req: ExportFileRequest) -> DltResult<Value> {
        if req.dry_run == Some(true) { return self.export_dry_run(req); }
        let key = key_for("artifact.export", &req.request_id, &req)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        let o = &req.options;
        let format = o.format.trim().to_ascii_lowercase();
        if format.is_empty() { return Err(invalid("format is empty")); }
        if o.profile.dpi == 0 || o.profile.dpi > 9_600 { return Err(invalid("profile.dpi must be 1..=9600")); }
        let (row, engine, artifact, _hash) = self.load_revision(&o.artifact_id, Some(o.expected_revision))?;
        let staging = self.staging();
        let renderer = self.renderer.clone();
        let ctx = EngineContext { assets: &staging, renderer: &*renderer };
        check_node_export(&*engine, &artifact, &ctx, &format, o.node_ids.as_deref())?;
        if o.node_ids.is_none() && matches!(format.as_str(), "png" | "svg") {
            let pages = match &o.page_indices {
                Some(p) => p.len(),
                None => self.scene_for(&o.artifact_id, o.expected_revision)?.scene.pages.len(),
            };
            if pages != 1 { return Err(invalid(format!("{format} export writes exactly one page; pass page_indices with one entry ({pages} selected)"))); }
        }
        let (target, refused) = check_target_or_pending(&self.conn, &self.root, &req.target_path, req.overwrite, req.expected_target_hash.as_deref(), &row.id, o.expected_revision, "", "export", self.agent_paths())?;
        let earlier = match refused {
            None => None,
            Some((e, rows)) if rows.is_empty() => return Err(e),
            Some((_, rows)) => Some(rows),
        };
        let out = engine.export(&artifact, o, &ctx)?;
        let not_allowed: Vec<Diagnostic> = out.diagnostics.iter().filter(|d| d.blocking && !o.allowed_losses.iter().any(|a| a == &d.code)).cloned().collect();
        if editability_rank(out.editability) < editability_rank(o.required_editability) || !not_allowed.is_empty() {
            let mut losses = not_allowed;
            if editability_rank(out.editability) < editability_rank(o.required_editability) {
                losses.push(Diagnostic { code: "editability_below_required".into(), message: format!("output is {:?}, required {:?}", out.editability, o.required_editability), node_id: None, blocking: true, params: Default::default() });
            }
            return Err(err(ErrorCode::ExportLossRequired, "export would lose required editability or content; nothing was written").diags(losses));
        }
        let hash = match earlier {
            None => publish(&self.conn, &self.hook, &target, req.overwrite, req.expected_target_hash.as_deref(), &out.bytes, &row.id, o.expected_revision, "", "export")?,
            Some(rows) => {
                // An earlier export of this revision renamed exactly these bytes onto the target:
                // the file is checked (and synced) only now, right before the record.
                let out_hash = sha256(&out.bytes);
                target.parent.recheck()?;
                if rows.iter().any(|r| r.hash == out_hash) && holds(&target, &out_hash)? { out_hash } else {
                    // Other bytes this time, or the file changed since: checked as for any new
                    // publication (normally refused, the file left as is).
                    check_target_state(&target, req.overwrite, req.expected_target_hash.as_deref())?;
                    publish(&self.conn, &self.hook, &target, req.overwrite, req.expected_target_hash.as_deref(), &out.bytes, &row.id, o.expected_revision, "", "export")?
                }
            }
        };
        let result = ExportResult {
            request_id: key.request_id.clone(), artifact_id: row.id.clone(), revision: o.expected_revision, format, target_path: target.path.to_string_lossy().into_owned(),
            sha256: hash, bytes: out.bytes.len() as u64, mime: out.mime.clone(), editability: out.editability, diagnostics: out.diagnostics.clone(),
        };
        let value = to_value(&result)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        clear_pending(&tx, &target, &row.id, "export", None)?;
        store::insert_receipt(&tx, &key, Some(&row.id), &StoredResult::Committed { result: value.clone() })?;
        store::insert_event(&tx, Some(&row.id), Some(o.expected_revision), "artifact.exported", &json!({ "request_id": key.request_id, "target_path": result.target_path, "sha256": result.sha256, "format": result.format, "editability": result.editability }))?;
        fire(&self.hook, FaultPoint::TxnBeforeCommit)?;
        tx.commit().map_err(sql)?;
        Ok(value)
    }

    /// artifact.export with dry_run: the same checks and the same encode, then the losses a real
    /// export would refuse for. No file, receipt, temp registration or event; request_id is not used.
    fn export_dry_run(&mut self, req: ExportFileRequest) -> DltResult<Value> {
        let o = &req.options;
        let format = o.format.trim().to_ascii_lowercase();
        if format.is_empty() { return Err(invalid("format is empty")); }
        if o.profile.dpi == 0 || o.profile.dpi > 9_600 { return Err(invalid("profile.dpi must be 1..=9600")); }
        let (row, engine, artifact, _hash) = self.load_revision(&o.artifact_id, Some(o.expected_revision))?;
        let staging = self.staging();
        let renderer = self.renderer.clone();
        let ctx = EngineContext { assets: &staging, renderer: &*renderer };
        check_node_export(&*engine, &artifact, &ctx, &format, o.node_ids.as_deref())?;
        if o.node_ids.is_none() && matches!(format.as_str(), "png" | "svg") {
            let pages = match &o.page_indices {
                Some(p) => p.len(),
                None => self.scene_for(&o.artifact_id, o.expected_revision)?.scene.pages.len(),
            };
            if pages != 1 { return Err(invalid(format!("{format} export writes exactly one page; pass page_indices with one entry ({pages} selected)"))); }
        }
        let target_path = if req.target_path.trim().is_empty() { None } else {
            Some(check_target(&self.root, &req.target_path, req.overwrite, req.expected_target_hash.as_deref(), self.agent_paths())?.path.to_string_lossy().into_owned())
        };
        let out = engine.export(&artifact, o, &ctx)?;
        let mut losses: Vec<Diagnostic> = out.diagnostics.iter().filter(|d| d.blocking && !o.allowed_losses.iter().any(|a| a == &d.code)).cloned().collect();
        if editability_rank(out.editability) < editability_rank(o.required_editability) {
            losses.push(Diagnostic { code: "editability_below_required".into(), message: format!("output is {:?}, required {:?}", out.editability, o.required_editability), node_id: None, blocking: true, params: Default::default() });
        }
        to_value(&ExportDryRun {
            dry_run: true, artifact_id: row.id, revision: o.expected_revision, format, mime: out.mime, bytes: out.bytes.len() as u64,
            editability: out.editability, losses, diagnostics: out.diagnostics, target_path,
        })
    }
}
