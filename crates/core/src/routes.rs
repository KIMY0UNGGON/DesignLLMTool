//! Read/view routes: listing, inspect, render snapshots and queries, assets, sources, views,
//! reservations, context, figure render links, events and web preview materialization.
use crate::blobs::is_hash;
use crate::errors::{err, integrity, invalid, not_found, sql, unsupported, ErrExt};
use crate::files::{check_source_path, mime_for, read_source};
use crate::store;
use crate::views::{check_transform, emu_to_screen, screen_to_emu, SceneEntry, SnapshotKey, ViewState, DEFAULT_RESERVATION_MS, MAX_RESERVATION_MS};
use crate::{asset_refs, b64, to_value, EventBatch, Workspace, MAX_CHUNK_BYTES};
use designllm_contracts::*;
use rusqlite::{params, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

// Route params. `crate::parse` round-trips them (Serialize) to refuse keys they do not have.
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct ArtifactIdParams { pub artifact_id: String }
/// `attempt_id` guards job.cancel when supplied; job.get and job.resume continue to ignore it.
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct JobIdParams { pub job_id: String, #[serde(default)] pub attempt_id: Option<String> }
pub(crate) type DesignSystemPublishParams = DesignSystemPublishRequest;
pub(crate) type DesignSystemSetParams = DesignSystemSetRequest;
/// `view_id` + `view_token` (a GUI view, docs/UI_CHANNEL.md): the batch also carries that view's queued runs.
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct PollParams { #[serde(default)] pub after_seq: u64, #[serde(default)] pub limit: Option<usize>, #[serde(default)] pub view_id: Option<String>, #[serde(default)] pub view_token: Option<String> }
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct InspectParams { pub artifact_id: String, #[serde(default)] pub node_ids: Option<Vec<String>>, #[serde(default)] pub revision: Option<Revision> }
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AssetImportParams {
    pub path: String,
    #[serde(default)] pub mime: Option<String>,
    #[serde(default)] pub require_image: bool,
}
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct AssetReadParams { pub sha256: String, #[serde(default)] pub offset: u64, #[serde(default)] pub max_bytes: Option<usize> }
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct HashParams { pub sha256: String }
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct PdfParams { pub sha256: String, #[serde(default)] pub pages: Vec<u32> }
/// `source.import_csv`: `columns` absent (or null) = the inference handler, present = the declared handler.
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct CsvParams { pub sha256: String, #[serde(default)] pub columns: Option<Vec<DeclaredColumn>> }
/// One declared column. `id` is accepted (e.g. echoed from a Dataset) and ignored; an unknown field
/// such as a misspelled `unit` is refused instead of being dropped.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeclaredColumn { pub name: String, pub data_type: String, #[serde(default)] pub unit: Option<String>, #[serde(default, rename = "id")] _id: Option<String> }
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct ReleaseParams { pub reservation_id: String }
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct ReserveParams { #[serde(flatten)] pub request: ReservationRequest, #[serde(default)] pub ttl_ms: Option<u64> }
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct TransformParams { pub view_id: String, pub page_index: u32, pub transform: ViewTransform, #[serde(default)] pub artifact_id: Option<String> }
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct ContextParams { #[serde(default)] pub artifact_id: Option<String>, #[serde(default)] pub view_id: Option<String>, #[serde(default)] pub include_render: bool, #[serde(default)] pub profile: Option<RenderProfile> }
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct FigureRenderParams { pub figure_id: String, pub figure_revision: Revision, pub representation: String, #[serde(default)] pub profile: RenderProfile }
#[derive(Debug, Deserialize, Serialize)] pub(crate) struct MaterializeParams { pub artifact_id: String, pub expected_revision: Revision }

pub(crate) fn check_profile(p: &RenderProfile) -> DltResult<()> { if p.dpi == 0 || p.dpi > 9_600 { Err(invalid("profile.dpi must be 1..=9600")) } else { Ok(()) } }

/// A render region is a non-empty page-EMU rectangle inside the page (nothing is clipped silently).
pub(crate) fn check_region(r: Rect, page: Size, page_index: u32) -> DltResult<()> {
    if r.width <= 0 || r.height <= 0 { return Err(invalid(format!("region must have positive width and height, got {} x {}", r.width, r.height))); }
    let inside = r.x >= 0 && r.y >= 0
        && r.x.checked_add(r.width).is_some_and(|right| right <= page.width)
        && r.y.checked_add(r.height).is_some_and(|bottom| bottom <= page.height);
    if !inside {
        return Err(invalid(format!("region x={} y={} {}x{} is not inside page {page_index} ({} x {} EMU)", r.x, r.y, r.width, r.height, page.width, page.height)));
    }
    Ok(())
}

pub(crate) fn links_for_page(scene: &RenderScene, page: &RenderPage) -> Vec<SceneLink> {
    scene.links.iter().filter(|link| link.page_id == page.id).cloned().collect()
}

pub(crate) fn check_engine_scene_links(scene: &RenderScene, engine: &str) -> DltResult<()> {
    scene.check_links().map_err(|error| {
        let mut mapped = err(ErrorCode::Internal, format!("engine {engine} produced invalid RenderScene.links: {}", error.message));
        mapped.affected_ids = error.affected_ids;
        mapped.diagnostics = error.diagnostics;
        mapped
    })
}

fn validate_scene(scene: &RenderScene, engine: &str) -> DltResult<()> {
    check_engine_scene_links(scene, engine)?;
    if scene.pages.is_empty() {
        // e.g. WebCode: previewed through web.materialize, no page scene. Explicit, with the engine diagnostics.
        return Err(unsupported(format!("engine {engine} has no page layout for this artifact")).diags(scene.diagnostics.clone()));
    }
    let mut seen = BTreeSet::new();
    for p in &scene.pages {
        if !seen.insert(p.page_index) { return Err(err(ErrorCode::Internal, format!("engine {engine} produced duplicate page_index {}", p.page_index))); }
    }
    Ok(())
}

/// An asset result of asset.import / asset.lookup: the AssetRef, plus `image: {width_px, height_px}`
/// for a PNG, JPEG, GIF or WebP. The extra key is accepted (and dropped) where an AssetRef is sent back.
fn with_image(asset: &AssetRef, bytes: &[u8]) -> DltResult<Value> {
    let mut value = to_value(asset)?;
    if asset.mime.starts_with("image/") {
        if let Some((w, h)) = crate::imagesize::image_size(bytes) { value["image"] = json!({ "width_px": w, "height_px": h }); }
    }
    Ok(value)
}

/// How `figure_renders.representation` is stored and how a FigureLink is matched against it.
pub(crate) fn representation_key(representation: &str) -> String { representation.trim().to_ascii_lowercase() }

/// One web file path as a relative path that stays inside the preview folder on Windows. The
/// reference rule for WebCode file names (a project must be writable as a preview tree).
fn safe_relative(path: &str) -> DltResult<PathBuf> {
    let bad = |why: &str| invalid(format!("web file path {path:?} {why}"));
    if path.is_empty() || path.len() > 512 { return Err(bad("must be 1..=512 bytes long")); }
    if path.starts_with('/') { return Err(bad("must be relative")); }
    if path.contains(['\\', ':', '\0']) { return Err(bad("must not contain '\\', ':' or NUL; '/' is the separator")); }
    let mut out = PathBuf::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." { return Err(bad("has an empty, '.' or '..' segment")); }
        if seg.ends_with('.') || seg.ends_with(' ') { return Err(bad("has a segment ending in '.' or ' ', which Windows strips")); }
        if seg.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')) { return Err(bad("has a control character or one of < > \" | ? *")); }
        if seg.encode_utf16().count() > 255 { return Err(bad("has a segment longer than 255 UTF-16 units")); }
        // Device names, also with an extension or trailing spaces before it (CON.txt, "nul .js").
        if local_path::is_windows_device_name(seg) {
            return Err(bad(&format!("uses the Windows device name {seg:?}")));
        }
        out.push(seg);
    }
    Ok(out)
}

/// Name as a case-insensitive Windows file system compares it (per-character simple uppercase).
fn fold_case(name: &str) -> String {
    name.chars().map(|c| { let mut up = c.to_uppercase(); match (up.next(), up.next()) { (Some(u), None) => u, _ => c } }).collect()
}

/// The whole preview tree, checked before anything is written: every path passes `safe_relative`,
/// no two paths name the same file on a case-insensitive file system, and no file is also a
/// folder of another path. Returns the relative paths in input order.
pub(crate) fn check_web_tree<'a>(paths: impl IntoIterator<Item = &'a str>) -> DltResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut involved: BTreeSet<&str> = BTreeSet::new();
    let mut files: BTreeMap<String, &str> = BTreeMap::new();
    let mut folders: BTreeMap<String, &str> = BTreeMap::new();
    for path in paths {
        match safe_relative(path) {
            Ok(rel) => out.push(rel),
            Err(e) => { problems.push(e.message); involved.insert(path); continue; }
        }
        let folded = fold_case(path);
        for (i, _) in folded.match_indices('/') { folders.entry(folded[..i].to_string()).or_insert(path); }
        if let Some(other) = files.insert(folded, path) {
            problems.push(format!("{other:?} and {path:?} are the same file on a case-insensitive file system"));
            involved.extend([other, path]);
        }
    }
    for (folded, path) in &files {
        if let Some(other) = folders.get(folded) {
            problems.push(format!("{path:?} is a file, but {other:?} needs a folder of that name"));
            involved.extend([*path, *other]);
        }
    }
    if problems.is_empty() { return Ok(out); }
    let total = problems.len();
    problems.truncate(crate::strict::MAX_LISTED);
    let more = if total > problems.len() { format!("; and {} more", total - problems.len()) } else { String::new() };
    Err(invalid(format!("the web project cannot be written as a preview folder: {}{more}", problems.join("; "))).ids(involved))
}

/// Writes `plan` into a temporary folder next to `dir`, reads it back, marks the files read-only
/// and renames it to `dir`. On any failure the temporary folder is removed and `dir` is not
/// created. `Ok(false)`: `dir` appeared meanwhile and was left for the caller to compare.
fn publish_tree(parent: &Path, dir: &Path, revision: Revision, plan: &[(PathBuf, &WebFile)]) -> DltResult<bool> {
    fs::create_dir_all(parent).map_err(DltError::from)?;
    let temp = parent.join(format!(".{revision}.{}.partial", new_id()));
    let built = (|| -> DltResult<()> {
        let io = |rel: &Path, e: std::io::Error| DltError::new(ErrorCode::Io, format!("{}: {e}", rel.display()));
        for (rel, file) in plan {
            let target = temp.join(rel);
            if let Some(d) = target.parent() { fs::create_dir_all(d).map_err(|e| io(rel, e))?; }
            fs::write(&target, file.content.as_bytes()).map_err(|e| io(rel, e))?;
        }
        // Read back before publishing: names this file system merges are caught here.
        let differences = tree_differences(&temp, plan)?;
        if !differences.is_empty() {
            return Err(unsupported(format!("the preview folder cannot hold these file names exactly ({}); nothing was written", differences.join(", "))));
        }
        set_tree_read_only(&temp, plan)
    })();
    if let Err(e) = built { let _ = remove_tree(&temp); return Err(e); }
    match fs::rename(&temp, dir) {
        Ok(()) => Ok(true),
        Err(e) => {
            let _ = remove_tree(&temp);
            if dir.exists() { Ok(false) } else { Err(DltError::from(e)) }
        }
    }
}

/// Marks every file of `plan` under `dir` read-only (idempotent; a tree written before trees were
/// read-only is protected the next time it is materialized).
fn set_tree_read_only(dir: &Path, plan: &[(PathBuf, &WebFile)]) -> DltResult<()> {
    for (rel, _) in plan {
        let path = dir.join(rel);
        let mut permissions = fs::metadata(&path).map_err(DltError::from)?.permissions();
        if !permissions.readonly() {
            permissions.set_readonly(true);
            fs::set_permissions(&path, permissions).map_err(|e| DltError::new(ErrorCode::Io, format!("{}: {e}", rel.display())))?;
        }
    }
    Ok(())
}

/// Removes a preview tree whose files are read-only. Clears the attribute on regular files only:
/// a link inside the tree is removed as a link and never changes what it points to.
#[allow(clippy::permissions_set_readonly_false, reason = "Windows read-only attribute of a derived preview file being deleted")]
fn remove_tree(dir: &Path) -> std::io::Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                let mut permissions = entry.metadata()?.permissions();
                if permissions.readonly() {
                    permissions.set_readonly(false);
                    fs::set_permissions(entry.path(), permissions)?;
                }
            }
        }
    }
    fs::remove_dir_all(dir)
}

/// Preview tree rule (A19): an artifact keeps the tree just materialized and newer ones; trees of
/// older revisions are removed unless a preview has them open. A tree in use is detected by
/// renaming it first: the preview host holds its folder without delete sharing, so the rename
/// fails and the tree stays (it is removed by a later materialize once the preview is closed).
/// Left-over `.trash` folders are retried; `.partial` folders may belong to a materialize still in
/// progress and are left alone. Best effort: a failure here never fails the materialize.
fn prune_preview_trees(parent: &Path, kept: Revision) {
    let Ok(entries) = fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') && name.ends_with(".trash") {
            let _ = remove_tree(&entry.path());
            continue;
        }
        let Ok(old) = name.parse::<Revision>() else { continue };
        if old >= kept || name != old.to_string() { continue }
        let trash = parent.join(format!(".{old}.{}.trash", new_id()));
        if fs::rename(entry.path(), &trash).is_ok() {
            let _ = remove_tree(&trash);
        }
    }
}

/// Paths of `plan` that do not read back with their bytes under `dir`, plus a note on files the
/// plan does not have. Empty: the tree is exactly the plan.
fn tree_differences(dir: &Path, plan: &[(PathBuf, &WebFile)]) -> DltResult<Vec<String>> {
    let mut out = Vec::new();
    for (rel, file) in plan {
        match fs::read(dir.join(rel)) {
            Ok(bytes) if sha256(&bytes) == file.sha256 => {}
            Ok(_) => out.push(format!("{} differs", rel.display())),
            Err(_) => out.push(format!("{} is missing", rel.display())),
        }
    }
    let mut on_disk = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).map_err(DltError::from)? {
            let e = e.map_err(DltError::from)?;
            if e.file_type().map_err(DltError::from)?.is_dir() { stack.push(e.path()); } else { on_disk += 1; }
        }
    }
    let expected = plan.len() - out.iter().filter(|d| d.ends_with(" is missing")).count();
    if on_disk != expected { out.push(format!("{} file(s) on disk, {} expected", on_disk, plan.len())); }
    Ok(out)
}

fn workspace_diagnostic(code: &str, message: impl Into<String>, params: Value) -> Diagnostic {
    let params = params.as_object().map(|object| object.iter().map(|(key, value)| (key.clone(), value.clone())).collect()).unwrap_or_default();
    Diagnostic { code: code.to_owned(), message: message.into(), node_id: None, blocking: false, params }
}

fn safe_backup_relative(path: &str) -> bool {
    let mut components = Path::new(path).components();
    matches!(components.next(), Some(Component::Normal(first)) if first == "backups")
        && components.all(|component| matches!(component, Component::Normal(_)))
}

fn notice_field<'a>(text: &'a str, prefix: &str, end: &str) -> Option<&'a str> {
    let tail = text.split_once(prefix)?.1;
    Some(tail.split_once(end).map_or(tail, |(value, _)| value).trim())
}

fn notice_count(text: &str, field: &str) -> Option<usize> {
    let tail = text.split_once(&format!("{field}="))?.1;
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn workspace_notices(conn: &rusqlite::Connection, root: &Path, report: &crate::RecoveryReport) -> DltResult<Vec<Diagnostic>> {
    let mut notices = Vec::new();
    let has_backup_table: bool = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='schema_backups'",
        [], |row| row.get::<_, i64>(0),
    ).map_err(sql)? > 0;
    let mut backup_paths = BTreeSet::new();
    if has_backup_table {
        let mut statement = conn.prepare("SELECT backup_id, source_store_version, restore_min_store_version, path FROM schema_backups WHERE state='ready' ORDER BY created_at_ms, backup_id").map_err(sql)?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?))).map_err(sql)?;
        for row in rows {
            let (backup_id, source_version, restore_min_version, path) = row.map_err(sql)?;
            if !safe_backup_relative(&path) {
                notices.push(workspace_diagnostic(
                    "store_recovery_warning",
                    "작업공간 복구 백업 경로를 안전하게 확인할 수 없습니다.",
                    json!({ "source_store_version": source_version, "restore_min_store_version": restore_min_version }),
                ));
                continue;
            }
            backup_paths.insert(path.clone());
            if root.join(&path).is_file() {
                notices.push(workspace_diagnostic(
                    "store_migrated",
                    format!("작업공간을 새 형식으로 업데이트했습니다. 복구용 데이터베이스 백업: {path}. 이 백업은 현재 버전 이상의 앱에서 검증된 복원 절차로만 사용하세요. 이전 버전으로 열면 일부 내용이 사라질 수 있습니다."),
                    json!({ "from_store_version": source_version, "to_store_version": store::STORE_SCHEMA_VERSION, "backup_path": path, "restore_min_store_version": restore_min_version, "older_core_compatible": false }),
                ));
                if !matches!(store::protected_backup_marker_matches(root, &path, &backup_id, source_version, restore_min_version), Ok(true)) {
                    notices.push(workspace_diagnostic(
                        "store_recovery_warning",
                        format!("이 백업 파일에는 구 버전 Core의 직접 열기를 막는 보호 표식이 없습니다. 현재 버전 이상의 앱에서 검증된 새 폴더 복원 경로만 사용하세요. 이전 버전으로 수동 열면 일부 내용이 사라질 수 있습니다: {path}"),
                        json!({ "backup_path": path, "restore_min_store_version": restore_min_version }),
                    ));
                }
            } else {
                notices.push(workspace_diagnostic(
                    "store_backup_missing",
                    format!("복구용 데이터베이스 백업을 찾을 수 없습니다: {path}. 이 백업을 사용할 수 없으며, 이전 버전 앱으로 복원하면 일부 내용이 사라질 수 있습니다."),
                    json!({ "from_store_version": source_version, "backup_path": path, "restore_min_store_version": restore_min_version }),
                ));
            }
        }
    }

    if report.removed_pending_temps > 0 || report.recovered_saves > 0 {
        notices.push(workspace_diagnostic(
            "pending_saves_recovered",
            format!("시작 중 저장 복구를 실행했습니다. 임시 파일 정리 {}개, 저장 완료 확인 {}건.", report.removed_pending_temps, report.recovered_saves),
            json!({ "removed_pending_temps": report.removed_pending_temps, "recovered_saves": report.recovered_saves }),
        ));
    }

    for warning in &report.migration_warnings {
        if warning.contains("registered schema backup") && warning.contains("is missing at ") {
            if let Some(path) = notice_field(warning, "is missing at ", ";") {
                if backup_paths.contains(path) { continue; }
                if safe_backup_relative(path) {
                    notices.push(workspace_diagnostic(
                        "store_backup_missing",
                        format!("복구용 데이터베이스 백업을 찾을 수 없습니다: {path}. 이전 버전 앱으로 복원하면 일부 내용이 사라질 수 있습니다."),
                        json!({ "backup_path": path }),
                    ));
                    continue;
                }
            }
        }
        if warning.contains("unregistered migration staging") {
            let path = notice_field(warning, "staging at ", ";").filter(|path| safe_backup_relative(path));
            let counts_unknown = warning.contains("exact counts are unknown")
                || warning.contains("numeric fields are untrusted")
                || warning.contains("owner marker is missing")
                || warning.contains("owner marker does not match");
            let counts_not_started = warning.contains("recovery had not started");
            let mut params = serde_json::Map::new();
            if let Some(path) = path { params.insert("staging_path".to_owned(), json!(path)); }
            if counts_unknown {
                params.insert("counts_known".to_owned(), json!(false));
                notices.push(workspace_diagnostic(
                    "store_backup_incomplete_left",
                    format!("이전 마이그레이션의 미완료 파일{}을 보존했습니다. 완료된 백업이 아니며 복구 건수는 확인할 수 없습니다.", path.map(|value| format!(": {value}")).unwrap_or_default()),
                    Value::Object(params),
                ));
            } else {
                let removed = notice_count(warning, "removed_pending_temps");
                let recovered = notice_count(warning, "recovered_saves");
                params.insert("counts_known".to_owned(), json!(true));
                if let Some(count) = removed { params.insert("previous_removed_pending_temps".to_owned(), json!(count)); }
                if let Some(count) = recovered { params.insert("previous_recovered_saves".to_owned(), json!(count)); }
                let detail = if counts_not_started {
                    "저장 복구는 시작되지 않았습니다.".to_owned()
                } else if let (Some(removed), Some(recovered)) = (removed, recovered) {
                    format!("이전 실행의 복구 수치: 임시 파일 정리 {removed}개, 저장 완료 확인 {recovered}건.")
                } else {
                    "이전 실행의 복구 기록을 확인할 수 없습니다.".to_owned()
                };
                notices.push(workspace_diagnostic(
                    "store_backup_incomplete_left",
                    format!("이전 마이그레이션의 미완료 파일{}을 보존했습니다. 완료된 백업이 아닙니다. {detail}", path.map(|value| format!(": {value}")).unwrap_or_default()),
                    Value::Object(params),
                ));
            }
            continue;
        }
        let (message, params) = if warning.contains("recovery-count marker is missing") {
            ("이전 저장 복구의 처리 건수를 확인할 수 없습니다.", serde_json::Map::from_iter([(String::from("counts_known"), json!(false))]))
        } else if warning.contains("could not be removed after its recovery report") {
            let mut params = serde_json::Map::new();
            if let Some(path) = notice_field(warning, "migration staging ", " could not").filter(|path| safe_backup_relative(path)) {
                params.insert("staging_path".to_owned(), json!(path));
            }
            ("복구 안내 파일을 정리하지 못해 다음 시작에도 안내할 수 있습니다.", params)
        } else {
            ("작업공간 복구 중 확인이 필요한 상태를 발견했습니다.", serde_json::Map::new())
        };
        notices.push(workspace_diagnostic("store_recovery_warning", message, Value::Object(params)));
    }
    Ok(notices)
}

fn append_workspace_notices(result: &mut Value, notices: Vec<Diagnostic>, include_workspace_notices: bool) {
    if include_workspace_notices && !notices.is_empty() { result["workspace_notices"] = json!(notices); }
}

impl Workspace {
    pub(crate) fn list(&self, job: Option<&JobRef>) -> DltResult<Value> {
        let scope = job.map(|j| store::job_row(&self.conn, &j.job_id).map(|r| r.artifact_id)).transpose()?;
        let times = store::artifact_times(&self.conn)?;
        let mut active: BTreeMap<String, Vec<ActiveJob>> = BTreeMap::new();
        for id in store::job_ids_with_status(&self.conn, &["queued", "running", "validating"])? {
            let job = store::job_row(&self.conn, &id)?;
            active.entry(job.artifact_id.clone()).or_default().push(ActiveJob { job_id: job.job_id, runtime: job.request.runtime, client_name: job.client_name, state: job.status });
        }
        let artifacts: Vec<ArtifactSummary> = store::artifact_rows(&self.conn)?.iter().filter(|r| scope.as_ref().is_none_or(|a| a == &r.id))
            .map(|r| ArtifactSummary { modified_at_ms: times.get(&r.id).copied(), active_jobs: active.remove(&r.id).unwrap_or_default(), ..r.summary() }).collect();
        let mut result = json!({ "workspace_id": self.id, "artifacts": artifacts });
        let notices = if job.is_none() { workspace_notices(&self.conn, &self.root, &self.recovery)? } else { Vec::new() };
        append_workspace_notices(&mut result, notices, job.is_none());
        Ok(result)
    }

    pub(crate) fn inspect(&mut self, p: InspectParams) -> DltResult<Value> {
        let (_row, engine, artifact, _) = self.load_revision(&p.artifact_id, p.revision)?;
        self.validate_read_source(&*engine, &artifact)?;
        match p.node_ids.filter(|ids| !ids.is_empty()) {
            None => to_value(&artifact.public_view()),
            Some(ids) => engine.inspect(&artifact, &ids),
        }
    }

    pub(crate) fn validate_artifact(&mut self, p: ArtifactIdParams) -> DltResult<Value> {
        let (row, engine, artifact, _) = self.load_revision(&p.artifact_id, None)?;
        let staging = self.staging();
        let renderer = self.renderer.clone();
        let diagnostics = engine.validate(&artifact, &EngineContext { assets: &staging, renderer: &*renderer });
        let mut result = json!({ "artifact_id": row.id, "revision": row.current_revision, "diagnostics": diagnostics, "capabilities": engine.capabilities(&artifact) });
        let capability_extensions = engine.capability_extensions(&artifact);
        if !capability_extensions.is_empty() { result["capability_extensions"] = json!(capability_extensions); }
        Ok(result)
    }

    /// Layout of one revision, cached with the staging store that holds any derived assets.
    pub(crate) fn scene_for(&mut self, artifact_id: &str, revision: Revision) -> DltResult<Arc<SceneEntry>> { self.scene_with(artifact_id, revision, false) }

    /// `node_frames`: the scene of the engine's `layout_with_node_frames` (GUI selection
    /// outlines), cached apart from the scene of `layout`.
    pub(crate) fn scene_with(&mut self, artifact_id: &str, revision: Revision, node_frames: bool) -> DltResult<Arc<SceneEntry>> {
        if let Some(entry) = self.renders.scene(artifact_id, revision, node_frames) { return Ok(entry); }
        let (_row, engine, artifact, _) = self.load_revision(artifact_id, Some(revision))?;
        let staging = Arc::new(self.staging());
        let renderer = self.renderer.clone();
        let context = EngineContext { assets: &*staging, renderer: &*renderer };
        let scene = if node_frames { engine.layout_with_node_frames(&artifact, &context)? } else { engine.layout(&artifact, &context)? };
        validate_scene(&scene, engine.id())?;
        let entry = Arc::new(SceneEntry { scene: Arc::new(scene), assets: staging });
        self.renders.put_scene(artifact_id, revision, node_frames, entry.clone());
        Ok(entry)
    }

    /// One-page RenderSnapshot + PNG preview. `key == None`: preview-only (not queryable).
    /// `region` (page EMU, inside the page): only that rectangle is encoded; the scene keeps the
    /// whole page and the preview transform's crop is the region, so queries stay in page EMU.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn encode_page(&mut self, scene: &RenderScene, assets: &dyn AssetStore, artifact_id: &str, revision: Revision, page_index: u32, profile: &RenderProfile, region: Option<Rect>, key: Option<SnapshotKey>) -> DltResult<RenderSnapshot> {
        let (snapshot, preview, _) = crate::render_split::page_snapshot(&*self.renderer, scene, assets, artifact_id, revision, page_index, profile, region, false)?;
        self.renders.insert(snapshot.clone(), key, preview);
        Ok(snapshot)
    }

    /// `node_frames` (RenderRequest.include_node_frames): the scene comes from the engine's
    /// `layout_with_node_frames`; the snapshot is cached under its own key.
    pub(crate) fn render_snapshot(&mut self, artifact_id: &str, revision: Revision, page_index: u32, profile: &RenderProfile, region: Option<Rect>, node_frames: bool) -> DltResult<RenderSnapshot> {
        let req = RenderRequest { artifact_id: artifact_id.to_string(), expected_revision: revision, page_index, profile: profile.clone(), region, include_node_frames: node_frames };
        match self.prepare_render(false, String::new(), req)? {
            crate::render_split::Prepared::Cached(snapshot) => Ok(snapshot),
            crate::render_split::Prepared::Job(job) => self.render_now(&job),
        }
    }
    pub(crate) fn render_get(&mut self, req: RenderRequest) -> DltResult<Value> {
        to_value(&self.render_snapshot(&req.artifact_id, req.expected_revision, req.page_index, &req.profile, req.region, req.include_node_frames)?)
    }

    /// Snapshot + transform validity at the protocol boundary, then pure page-EMU queries.
    pub(crate) fn render_query(&mut self, method: &str, q: RenderQuery) -> DltResult<Value> {
        let entry = self.renders.snapshot(&q.snapshot_id).cloned()
            .ok_or_else(|| err(ErrorCode::StaleRender, format!("render snapshot {} is unknown or expired; call render.get", q.snapshot_id)))?;
        let snap = entry.snapshot;
        if entry.key.artifact_id.is_empty() { return Err(err(ErrorCode::StaleRender, "proposal previews are not queryable; render the committed revision")); }
        if snap.artifact_id != q.artifact_id { return Err(invalid("snapshot belongs to another artifact")); }
        let row = store::artifact_row(&self.conn, &q.artifact_id)?;
        if snap.revision != row.current_revision {
            return Err(err(ErrorCode::StaleRender, format!("snapshot is revision {}, artifact is at {}", snap.revision, row.current_revision)).at(row.current_revision));
        }
        let page = snap.scene.pages.first().cloned().ok_or_else(|| integrity("snapshot without page"))?;
        if page.page_index != q.page_index { return Err(invalid(format!("snapshot shows page {}, query names page {}", page.page_index, q.page_index))); }
        let transform = match &q.view_id {
            Some(view_id) => {
                let view = self.views.views.get(view_id).ok_or_else(|| err(ErrorCode::StaleRender, format!("view {view_id} has no transform; call view.transform.set")))?;
                match &view.transform {
                    Some((pi, t)) if view.artifact_id == q.artifact_id && *pi == q.page_index && t.id == q.transform_id => t.clone(),
                    _ => return Err(err(ErrorCode::StaleRender, "view transform changed (id, page or artifact differs); re-read and retry")),
                }
            }
            None => match &snap.preview_transform {
                Some(t) if t.id == q.transform_id => t.clone(),
                _ => return Err(err(ErrorCode::StaleRender, "transform_id is not this snapshot's preview transform")),
            },
        };
        let base = json!({ "snapshot_id": snap.id, "transform_id": transform.id, "page_index": page.page_index, "revision": snap.revision });
        let mut out = base.as_object().cloned().unwrap_or_default();
        match (method, &q.query) {
            ("render.hit_test", RenderQueryKind::HitTest { point }) => {
                let emu = screen_to_emu(&transform, *point)?;
                out.insert("point_emu".into(), to_value(&emu)?);
                out.insert("hit".into(), to_value(&self.renderer.hit_test(&page, emu)?)?);
            }
            ("render.caret_rect", RenderQueryKind::Caret { position }) => {
                let r = self.renderer.caret_rect(&page, position)?;
                out.insert("rect_emu".into(), to_value(&r)?);
                out.insert("rect_screen".into(), to_value(&emu_to_screen(&transform, r)?)?);
            }
            ("render.selection_rects", RenderQueryKind::Selection { range }) => {
                let rects = self.renderer.selection_rects(&page, range)?;
                let screen = rects.iter().map(|r| emu_to_screen(&transform, *r)).collect::<DltResult<Vec<_>>>()?;
                out.insert("rects_emu".into(), to_value(&rects)?);
                out.insert("rects_screen".into(), to_value(&screen)?);
            }
            _ => return Err(invalid(format!("{method} does not accept this query type"))),
        }
        Ok(Value::Object(out))
    }

    // ------------------------------------------------------------------ assets and sources
    pub(crate) fn asset_import(&mut self, p: AssetImportParams) -> DltResult<Value> {
        let (path, file) = check_source_path(&p.path, self.agent_paths())?;
        let bytes = read_source(file)?;
        let is_gif_ext = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gif"));
        let mime = p.mime.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| {
            let detected = mime_for(&path, &bytes);
            if detected == "application/octet-stream" && is_gif_ext { "image/gif".into() } else { detected }
        });
        // Refuse animated or truncated GIFs (and declared/extension GIFs without a valid GIF header) before anything is stored (D19.1).
        // The declared type is compared without its parameters (`image/gif; x` is a GIF).
        let declared_gif = mime.split(';').next().unwrap_or_default().trim().eq_ignore_ascii_case("image/gif");
        if let Some(refusal) = crate::imagesize::import_refusal_for(&bytes, declared_gif) {
            return Err(refusal);
        }
        if p.require_image && (!mime.starts_with("image/") || crate::imagesize::image_size(&bytes).is_none()) {
            return Err(invalid("figure image must have a supported image header and nonzero dimensions"));
        }
        let hash = self.base.blobs.put(&bytes)?;
        if let Some(existing) = self.base.get_ref(&hash) { return with_image(&existing, &bytes); }
        let asset = AssetRef { sha256: hash, mime, bytes: bytes.len() as u64 };
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        store::insert_assets(&tx, std::slice::from_ref(&asset), "import")?;
        store::insert_event(&tx, None, None, "asset.imported", &json!({ "sha256": asset.sha256, "mime": asset.mime, "bytes": asset.bytes }))?;
        tx.commit().map_err(sql)?;
        self.base.add_committed(std::slice::from_ref(&asset));
        with_image(&asset, &bytes)
    }

    /// Assets a job-bound connection may read: those referenced by its artifact, its source_refs,
    /// previews rendered from its artifact, and prepared renders of its pinned Figure revisions.
    /// Pipeline uses the same set against its already loaded base instead of restoring a second copy.
    /// Template previews are in it for the current attempt only (`job_asset_scope_as`).
    pub(crate) fn job_asset_scope_for(&self, row: &store::JobRow, artifact: &Artifact) -> DltResult<BTreeSet<String>> {
        self.job_asset_scope_as(row, artifact, &row.current_attempt)
    }
    /// The scope of attempt `attempt_id`. Its template previews count only while it is the current attempt and active
    /// (the check its writes use), so a superseded or canceled attempt reads none of them.
    fn job_asset_scope_as(&self, row: &store::JobRow, artifact: &Artifact, attempt_id: &str) -> DltResult<BTreeSet<String>> {
        if artifact.id != row.artifact_id { return Err(integrity("job asset scope received another artifact")); }
        let mut set = BTreeSet::new();
        asset_refs(&serde_json::to_value(&artifact).map_err(|e| err(ErrorCode::Internal, e.to_string()))?, &mut set);
        set.extend(row.request.source_refs.iter().map(|a| a.sha256.clone()));
        set.extend(self.renders.previews_of(&row.artifact_id));
        if attempt_id == row.current_attempt {
            let attempt = JobRef { job_id: row.job_id.clone(), attempt_id: attempt_id.to_string() };
            if store::check_job_write(&self.conn, &attempt, &row.artifact_id, &BTreeSet::new()).is_ok() {
                if let Some(previews) = self.job_template_previews.get(&(row.job_id.clone(), attempt_id.to_string())) { set.extend(previews.keys().cloned()); }
            }
        }
        let mut statement = self.conn.prepare("SELECT sha256 FROM figure_renders WHERE figure_id=?1 AND figure_revision=?2").map_err(sql)?;
        for reference in &row.request.figure_refs {
            let hashes = statement.query_map(params![reference.figure_id, reference.figure_revision as i64], |result| result.get::<_, String>(0)).map_err(sql)?;
            for hash in hashes { set.insert(hash.map_err(sql)?); }
        }
        Ok(set)
    }
    fn job_asset_scope(&self, job: &JobRef) -> DltResult<BTreeSet<String>> {
        let row = store::job_row(&self.conn, &job.job_id)?;
        let art = store::artifact_row(&self.conn, &row.artifact_id)?;
        let engine = self.engine_by_id(&art.engine_id)?;
        let artifact = self.load(&*engine, &art.current_snapshot, art.current_revision)?;
        self.job_asset_scope_as(&row, &artifact, &job.attempt_id)
    }
    fn check_asset_scope(&self, sha: &str, job: Option<&JobRef>) -> DltResult<()> {
        if !is_hash(sha) { return Err(invalid(format!("sha256 must be 64 lowercase hex characters: {sha:?}"))); }
        if let Some(j) = job { if !self.job_asset_scope(j)?.contains(sha) { return Err(invalid(format!("asset {sha} is outside job {}'s scope", j.job_id))); } }
        Ok(())
    }

    pub(crate) fn asset_read(&mut self, p: AssetReadParams, job: Option<&JobRef>) -> DltResult<Value> {
        self.check_asset_scope(&p.sha256, job)?;
        let max = p.max_bytes.unwrap_or(MAX_CHUNK_BYTES);
        if max == 0 || max > MAX_CHUNK_BYTES { return Err(invalid(format!("max_bytes must be 1..={MAX_CHUNK_BYTES}"))); }
        let (mime, chunk, total) = if let Some(r) = self.base.get_ref(&p.sha256) {
            let (chunk, total) = self.base.blobs.read_range(&p.sha256, p.offset, max)?;
            if total != r.bytes { return Err(integrity(format!("asset {} length differs from metadata", r.sha256))); }
            (r.mime, chunk, total)
        } else if let Some((mime, data)) = self.held_preview(&p.sha256) {
            let total = data.len() as u64;
            let start = usize::try_from(p.offset.min(total)).unwrap_or(0);
            let end = start.saturating_add(max).min(data.len());
            (mime, data[start..end].to_vec(), total)
        } else {
            return Err(not_found(format!("asset {} not found (previews expire with their render snapshot)", p.sha256)));
        };
        if p.offset > total { return Err(invalid(format!("offset {} is beyond the asset length {total}", p.offset))); }
        let eof = p.offset + chunk.len() as u64 >= total;
        if p.offset == 0 && eof && sha256(&chunk) != p.sha256 { return Err(integrity(format!("asset {} content does not match its hash", p.sha256))); }
        Ok(json!({ "sha256": p.sha256, "mime": mime, "offset": p.offset, "bytes": chunk.len(), "eof": eof, "total_bytes": total, "base64": b64::encode(&chunk) }))
    }

    pub(crate) fn asset_lookup(&mut self, p: HashParams, job: Option<&JobRef>) -> DltResult<Value> {
        self.check_asset_scope(&p.sha256, job)?;
        if let Some(r) = self.base.get_ref(&p.sha256) {
            let (head, _) = self.base.blobs.read_range(&p.sha256, 0, crate::imagesize::HEADER_BYTES)?;
            return with_image(&r, &head);
        }
        if let Some((mime, data)) = self.held_preview(&p.sha256) { return with_image(&AssetRef { sha256: p.sha256, mime, bytes: data.len() as u64 }, &data); }
        Err(not_found(format!("asset {} not found", p.sha256)))
    }

    /// A preview held in memory and not stored in `assets`: a render preview, or else a template preview.
    fn held_preview(&mut self, sha256: &str) -> Option<(String, Arc<Vec<u8>>)> {
        self.renders.preview(sha256).or_else(|| self.template_previews.png(sha256))
    }

    /// Stored asset for a source handler: valid hash, job scope, registered metadata.
    pub(crate) fn source_asset(&self, sha256: &str, job: Option<&JobRef>) -> DltResult<AssetRef> {
        self.check_asset_scope(sha256, job)?;
        self.base.get_ref(sha256).ok_or_else(|| not_found(format!("asset {sha256} not found")))
    }
    pub(crate) fn source_extract_pdf(&mut self, p: PdfParams, job: Option<&JobRef>) -> DltResult<Value> {
        if let Some(reader) = self.pdf_read {
            let asset = self.source_asset(&p.sha256, job)?;
            let staging = self.staging();
            // A host runs the same read without holding the workspace (`Workspace::dispatch_start`, task 34d).
            return crate::pdf_read::reading_value(&asset, &p.pages, reader(&asset, &p.pages, &staging));
        }
        let handler = self.pdf.ok_or_else(|| unsupported("no PDF source handler is registered in this build"))?;
        let asset = self.source_asset(&p.sha256, job)?;
        let staging = self.staging();
        let evidence = handler(&asset, &p.pages, &staging)?;
        // Pages that produced evidence. Coverage of pages without text is the handler's contract.
        let pages_with_evidence: BTreeSet<u32> = evidence.iter().filter_map(|e| e.physical_page_index).collect();
        Ok(json!({ "asset": asset, "requested_pages": p.pages, "pages_with_evidence": pages_with_evidence, "evidence": evidence }))
    }
    pub(crate) fn source_import_csv(&mut self, p: CsvParams, job: Option<&JobRef>) -> DltResult<Value> {
        let dataset = match p.columns {
            None => {
                let handler = self.csv.ok_or_else(|| unsupported("no CSV source handler is registered in this build"))?;
                let asset = self.source_asset(&p.sha256, job)?;
                handler(&asset, &self.staging())?
            }
            Some(declared) => {
                if declared.is_empty() { return Err(invalid("columns must not be empty; omit it to infer the column types")); }
                let handler = self.csv_declared.ok_or_else(|| unsupported("no declared-column CSV handler is registered in this build"))?;
                let asset = self.source_asset(&p.sha256, job)?;
                // Passed exactly as declared; the handler matches names and checks types/units.
                let columns: Vec<DataColumn> = declared.into_iter().map(|c| DataColumn { id: String::new(), name: c.name, data_type: c.data_type, unit: c.unit }).collect();
                handler(&asset, &self.staging(), &columns)?
            }
        };
        to_value(&dataset)
    }

    // ------------------------------------------------------------------ views
    pub(crate) fn selection_set(&mut self, req: SelectionRequest) -> DltResult<Value> {
        let (row, engine, artifact, _) = self.load_revision(&req.artifact_id, None)?;
        if req.expected_revision != row.current_revision {
            return Err(err(ErrorCode::RevisionConflict, format!("selection was made on revision {}, artifact is at {}", req.expected_revision, row.current_revision)).at(row.current_revision));
        }
        if req.view_id.trim().is_empty() { return Err(invalid("view_id is empty")); }
        self.validate_read_source(&*engine, &artifact)?;
        let mut ids = req.selection.node_ids.clone();
        if let Some(r) = &req.selection.text_range {
            if r.start > r.end { return Err(invalid("text_range.start is after text_range.end")); }
            ids.push(r.paragraph_id.clone());
        }
        if !ids.is_empty() { engine.inspect(&artifact, &ids)?; }
        if let Some(entry) = self.renders.scene(&row.id, row.current_revision, false) {
            if !entry.scene.pages.iter().any(|p| p.page_index == req.visible_page) { return Err(invalid(format!("visible_page {} does not exist", req.visible_page))); }
        }
        let transform = self.views.views.get(&req.view_id).filter(|v| v.artifact_id == row.id).and_then(|v| v.transform.clone());
        self.views.views.insert(req.view_id.clone(), ViewState { artifact_id: row.id.clone(), revision: row.current_revision, selection: req.selection.clone(), visible_page: req.visible_page, transform });
        self.views.active = Some(req.view_id.clone());
        // This request already checked the same current artifact before changing the view.
        self.context_read_inner(ContextParams { artifact_id: Some(row.id), view_id: Some(req.view_id), include_render: false, profile: None }, None, true)
    }

    pub(crate) fn targets_reserve(&mut self, p: ReserveParams) -> DltResult<Value> {
        let req = p.request;
        let row = store::artifact_row(&self.conn, &req.artifact_id)?;
        if req.expected_revision != row.current_revision {
            return Err(err(ErrorCode::RevisionConflict, format!("reservation names revision {}, artifact is at {}", req.expected_revision, row.current_revision)).at(row.current_revision));
        }
        if req.owner.trim().is_empty() || req.view_id.trim().is_empty() || req.node_ids.is_empty() { return Err(invalid("owner, view_id and node_ids are required")); }
        let ttl = p.ttl_ms.unwrap_or(DEFAULT_RESERVATION_MS).clamp(1_000, MAX_RESERVATION_MS);
        let held = self.reservation_holds(&req.artifact_id, &req.node_ids)?;
        let r = self.views.reserve(&req, ttl, held)?;
        Ok(json!({ "reservation_id": r.reservation_id, "artifact_id": r.artifact_id, "view_id": r.view_id, "owner": r.owner, "node_ids": r.node_ids, "expires_in_ms": ttl, "revision": row.current_revision }))
    }
    pub(crate) fn targets_release(&mut self, p: ReleaseParams) -> DltResult<Value> {
        self.views.prune();
        let held = self.views.reservations.get(&p.reservation_id).ok_or_else(|| not_found(format!("reservation {} not found or expired", p.reservation_id)))?;
        if let Some(job) = &held.job_id {
            return Err(invalid(format!("reservation {} holds the targets of job {job}; it ends with the job (job.cancel)", p.reservation_id)).ids([job.clone()]));
        }
        if !self.caller_owns(&held.owner) {
            return Err(invalid(format!("reservation {} belongs to another connection; only the connection that made it can release it", p.reservation_id)));
        }
        self.views.reservations.remove(&p.reservation_id);
        Ok(json!({ "released": true, "reservation_id": p.reservation_id }))
    }

    /// Reservations of one artifact: users' (with their remaining ttl) and jobs' (held until the job ends). A connection with a
    /// host-fixed identity sees the `reservation_id` of its own reservations only (`caller`).
    pub(crate) fn targets_list(&mut self, p: ArtifactIdParams) -> DltResult<Value> {
        store::artifact_row(&self.conn, &p.artifact_id)?;
        self.views.prune();
        let now = std::time::Instant::now();
        let mut list: Vec<&crate::views::Reservation> = self.views.reservations.values().filter(|r| r.artifact_id == p.artifact_id).collect();
        list.sort_by(|a, b| (&a.owner, &a.reservation_id).cmp(&(&b.owner, &b.reservation_id)));
        let reservations: Vec<Value> = list.into_iter().map(|r| {
            let mut v = json!({ "view_id": r.view_id, "owner": r.owner, "node_ids": r.node_ids });
            if self.caller_owns(&r.owner) { v["reservation_id"] = json!(r.reservation_id); }
            match &r.job_id {
                Some(job) => { v["job_id"] = json!(job); }
                None => { v["expires_in_ms"] = json!(r.expires.saturating_duration_since(now).as_millis() as u64); }
            }
            v
        }).collect();
        Ok(json!({ "artifact_id": p.artifact_id, "reservations": reservations }))
    }

    pub(crate) fn view_transform_set(&mut self, p: TransformParams) -> DltResult<Value> {
        check_transform(&p.transform)?;
        if p.view_id.trim().is_empty() { return Err(invalid("view_id is empty")); }
        match self.views.views.get_mut(&p.view_id) {
            Some(view) => {
                if p.artifact_id.as_ref().is_some_and(|a| a != &view.artifact_id) { return Err(invalid("view belongs to another artifact")); }
                view.transform = Some((p.page_index, p.transform.clone()));
            }
            None => {
                let artifact_id = p.artifact_id.clone().ok_or_else(|| not_found(format!("view {} is unknown; call selection.set first or pass artifact_id", p.view_id)))?;
                let row = store::artifact_row(&self.conn, &artifact_id)?;
                self.views.views.insert(p.view_id.clone(), ViewState { artifact_id: row.id, revision: row.current_revision, selection: Selection::default(), visible_page: p.page_index, transform: Some((p.page_index, p.transform.clone())) });
            }
        }
        Ok(json!({ "view_id": p.view_id, "page_index": p.page_index, "transform_id": p.transform.id }))
    }

    /// Selection, properties, capabilities and (optionally) the render of one revision. Without
    /// artifact_id/view_id the user's active view is used; nothing is guessed when there is none.
    pub(crate) fn context_read(&mut self, p: ContextParams, _job: Option<&JobRef>) -> DltResult<Value> {
        self.context_read_inner(p, _job, false)
    }

    fn context_read_inner(&mut self, p: ContextParams, _job: Option<&JobRef>, source_validated: bool) -> DltResult<Value> {
        let (artifact_id, view_id) = match (p.artifact_id, p.view_id) {
            (Some(a), Some(v)) => {
                let view = self.views.views.get(&v).ok_or_else(|| not_found(format!("view {v} is unknown")))?;
                if view.artifact_id != a { return Err(invalid(format!("view {v} shows another artifact"))); }
                (a, Some(v))
            }
            (Some(a), None) => {
                let active = self.views.active.clone().filter(|v| self.views.views.get(v).is_some_and(|s| s.artifact_id == a));
                (a, active)
            }
            (None, Some(v)) => {
                let view = self.views.views.get(&v).ok_or_else(|| not_found(format!("view {v} is unknown")))?;
                (view.artifact_id.clone(), Some(v))
            }
            (None, None) => {
                let v = self.views.active.clone().ok_or_else(|| not_found("no active document: no view has a selection; pass artifact_id or view_id"))?;
                let view = self.views.views.get(&v).ok_or_else(|| not_found("active view is gone; pass artifact_id"))?;
                (view.artifact_id.clone(), Some(v))
            }
        };
        let (row, engine, artifact, _) = self.load_revision(&artifact_id, None)?;
        if !source_validated { self.validate_read_source(&*engine, &artifact)?; }
        let rev = row.current_revision;
        let view = view_id.as_ref().and_then(|v| self.views.views.get(v)).cloned();
        let (selection, visible_page, transform, stale) = match &view {
            Some(vs) if vs.revision == rev => (vs.selection.clone(), vs.visible_page, vs.transform.clone(), Value::Null),
            Some(vs) => (Selection::default(), vs.visible_page, vs.transform.clone(), json!({ "revision": vs.revision, "selection": vs.selection })),
            None => (Selection::default(), 0, None, Value::Null),
        };
        let mut ids = selection.node_ids.clone();
        if let Some(r) = &selection.text_range { ids.push(r.paragraph_id.clone()); }
        let nodes = if ids.is_empty() { Value::Null } else { engine.inspect(&artifact, &ids)? };
        // Artifacts without a page scene still return their context; the render gap is explicit.
        let (render_snapshot, render_unavailable) = if p.include_render {
            match self.render_snapshot(&artifact_id, rev, visible_page, &p.profile.unwrap_or_default(), None, false) {
                Ok(s) => (Some(s), Value::Null),
                Err(e) if e.code == ErrorCode::UnsupportedOperation => (None, to_value(&e)?),
                Err(e) => return Err(e),
            }
        } else { (None, Value::Null) };
        to_value(&ContextSnapshot {
            workspace_id: self.id.clone(), artifact_id, revision: rev, view_id: view_id.unwrap_or_default(), selection, visible_page,
            selected_properties: json!({ "nodes": nodes, "stale_selection": stale, "render_unavailable": render_unavailable }), capabilities: engine.capabilities(&artifact),
            capability_extensions: engine.capability_extensions(&artifact),
            render_snapshot, view_transform: transform.map(|(_, t)| t),
        })
    }

    // ------------------------------------------------------------------ figure links
    /// Immutable render of one figure revision for document insertion; UpdateFigureLink must
    /// name a render prepared here.
    pub(crate) fn figure_render_link(&mut self, p: FigureRenderParams, job: Option<&JobRef>) -> DltResult<Value> {
        let representation = representation_key(&p.representation);
        if representation.is_empty() { return Err(invalid("representation is empty")); }
        check_profile(&p.profile)?;
        let job_artifact_id = if let Some(job) = job {
            let row = store::job_row(&self.conn, &job.job_id)?;
            let approved = row.request.figure_refs.iter().any(|reference| {
                reference.figure_id == p.figure_id && reference.figure_revision == p.figure_revision
            });
            if !approved { return Err(invalid("figure revision is outside this job's declared figure_refs")); }
            store::check_job_write(&self.conn, job, &row.artifact_id, &BTreeSet::new())?;
            Some(row.artifact_id)
        } else { None };
        let (row, engine, artifact, _) = self.load_revision(&p.figure_id, Some(p.figure_revision))?;
        if row.kind != ArtifactKind::Figure { return Err(invalid(format!("{} is not a figure artifact", row.id))); }
        let req = ExportRequest { profile: p.profile.clone(), ..ExportRequest::new(row.id.clone(), p.figure_revision, representation.clone(), Editability::None) };
        let staging = self.staging();
        let renderer = self.renderer.clone();
        let out = engine.export(&artifact, &req, &EngineContext { assets: &staging, renderer: &*renderer })?;
        let losses: Vec<Diagnostic> = out.diagnostics.iter().filter(|d| d.blocking).cloned().collect();
        if !losses.is_empty() {
            // An approval covers the figure's content, data, and design (R07.8): a review not approved for the figure as it is now
            // is cured by approving it after the last change.
            let review = losses.iter().any(|d| d.code == diag_codes::VISUAL_REVIEW_NOT_APPROVED || d.code == diag_codes::VISUAL_REVIEW_STALE);
            let message = if review {
                "figure render reports losses: its visual review is not approved for the current content, data, and design; approve it with set_figure_visual_review after the last change, or choose another representation"
            } else {
                "figure render reports losses; choose another representation"
            };
            return Err(err(ErrorCode::ExportLossRequired, message).diags(losses));
        }
        let hash = self.base.blobs.put(&out.bytes)?;
        let asset = self.base.get_ref(&hash).unwrap_or(AssetRef { sha256: hash.clone(), mime: out.mime.clone(), bytes: out.bytes.len() as u64 });
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        if let (Some(job), Some(artifact_id)) = (job, job_artifact_id.as_deref()) {
            store::check_job_write(&tx, job, artifact_id, &BTreeSet::new())?;
        }
        store::insert_assets(&tx, std::slice::from_ref(&asset), "figure_render")?;
        tx.execute(
            "INSERT OR IGNORE INTO figure_renders(figure_id, figure_revision, representation, sha256, profile_json, editability) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![row.id, p.figure_revision as i64, representation, hash, store::to_json(&p.profile)?, store::to_json(&out.editability)?],
        ).map_err(sql)?;
        store::insert_event(&tx, Some(&row.id), Some(p.figure_revision), "figure.render_prepared", &json!({ "representation": representation, "render_hash": hash }))?;
        tx.commit().map_err(sql)?;
        self.base.add_committed(std::slice::from_ref(&asset));
        Ok(json!({ "figure_id": row.id, "figure_revision": p.figure_revision, "representation": representation, "asset": asset, "render_hash": hash, "editability": out.editability, "diagnostics": out.diagnostics }))
    }

    pub(crate) fn poll(&mut self, p: PollParams, job: Option<&JobRef>) -> DltResult<EventBatch> {
        let mut batch = self.events_after(p.after_seq, p.limit.unwrap_or(256))?;
        if let Some(j) = job {
            let artifact = store::job_row(&self.conn, &j.job_id)?.artifact_id;
            batch.events.retain(|e| e.artifact_id.as_deref() == Some(artifact.as_str()));
            // 83: a job polls only its own proposals' events. The proposal row names the job (old applied events do not).
            let conn = &self.conn;
            batch.events.retain(|e| {
                !e.event.starts_with("proposal.")
                    || e.detail.get("proposal_id").and_then(Value::as_str).and_then(|pid| crate::jobs::proposal_owner(conn, pid).ok()).and_then(|(_, owner)| owner).as_deref() == Some(j.job_id.as_str())
            });
            batch.artifacts.retain(|a| a.id == artifact);
        }
        match (&p.view_id, &p.view_token) {
            (Some(view), Some(token)) => batch.ui_runs = self.ui_deliver(view, token)?,
            (None, None) => {}
            _ => return Err(invalid("events.poll takes view_id and view_token together")),
        }
        Ok(batch)
    }

    // ------------------------------------------------------------------ web preview tree
    /// Derived, read-only copy of a WebCode revision under `<root>/preview/<artifact>/<revision>`.
    /// The whole file list is checked first; the tree is written to a temporary folder, read back
    /// and only then renamed into place, so a failed materialize leaves no tree. Never overwrites
    /// an existing different tree and is never read back as source. Afterwards the artifact's
    /// older trees that no preview has open are removed (`prune_preview_trees`).
    pub(crate) fn web_materialize(&mut self, p: MaterializeParams) -> DltResult<Value> {
        let (row, _engine, artifact, _) = self.load_revision(&p.artifact_id, Some(p.expected_revision))?;
        let ArtifactBody::WebCode(doc) = &artifact.body else { return Err(invalid(format!("{} is not a WebCode artifact", row.id))) };
        if !doc.files.contains_key(&doc.entry) { return Err(invalid(format!("entry {:?} is not one of the files", doc.entry))); }
        let rels = check_web_tree(doc.files.keys().map(String::as_str))?;
        let mut plan = Vec::new();
        let mut manifest = serde_json::Map::new();
        for ((path, file), rel) in doc.files.iter().zip(rels) {
            if sha256(file.content.as_bytes()) != file.sha256 { return Err(integrity(format!("web file {path:?} content does not match its recorded hash"))); }
            manifest.insert(path.clone(), json!(file.sha256));
            plan.push((rel, file));
        }
        let source_hash = sha256(canonical_json(&json!({ "entry": doc.entry, "files": manifest }))?.as_bytes());
        // Lower-case ids are used as they are; any other id is hashed, so two ids that differ only
        // in case cannot share a folder.
        let plain_id = !row.id.is_empty() && row.id.len() <= 64 && row.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        let segment = if plain_id { row.id.clone() } else { sha256(row.id.as_bytes())[..32].to_string() };
        let parent = self.root.join("preview").join(&segment);
        let dir = parent.join(p.expected_revision.to_string());
        let published = !dir.exists() && publish_tree(&parent, &dir, p.expected_revision, &plan)?;
        if !published {
            let differences = tree_differences(&dir, &plan)?;
            if !differences.is_empty() {
                return Err(integrity(format!("existing preview tree {} differs from revision {} ({}); it was not overwritten", dir.display(), p.expected_revision, differences.join(", "))));
            }
            set_tree_read_only(&dir, &plan)?;
        }
        prune_preview_trees(&parent, p.expected_revision);
        Ok(json!({
            "artifact_id": row.id, "revision": p.expected_revision, "root_path": dir.to_string_lossy(), "entry_path": dir.join(safe_relative(&doc.entry)?).to_string_lossy(),
            "source_hash": source_hash, "files": plan.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{append_workspace_notices, check_web_tree, links_for_page, publish_tree, safe_relative, validate_scene, workspace_notices};
    use designllm_contracts::{sha256, ErrorCode, Rect, RenderPage, RenderScene, SceneLink, Size, WebFile};
    use rusqlite::Connection;
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    fn scene_link(page_id: &str, node_id: &str, bounds: Rect, url: &str) -> SceneLink {
        SceneLink { page_id: page_id.into(), node_id: node_id.into(), bounds, url: url.into() }
    }

    #[test]
    fn render_snapshot_preserves_selected_page_links_and_full_page_coordinates() {
        let page = RenderPage { id: "p".into(), page_index: 0, size: Size { width: 100, height: 100 }, primitives: vec![], source_map: vec![] };
        let scene = RenderScene {
            pages: vec![page.clone(), RenderPage { id: "q".into(), page_index: 1, ..page.clone() }],
            font_fingerprint: String::new(), diagnostics: vec![], metadata: Default::default(),
            links: vec![
                scene_link("p", "hit", Rect { x: 10, y: 25, width: 25, height: 20 }, "https://example.com/hit"),
                scene_link("p", "outside-region", Rect { x: 45, y: 30, width: 5, height: 5 }, "https://example.com/outside"),
                scene_link("p", "outside-page", Rect { x: -5, y: -5, width: 10, height: 10 }, "https://example.com/page-edge"),
                scene_link("q", "other-page", Rect { x: 20, y: 30, width: 5, height: 5 }, "https://example.com/other"),
                scene_link("missing", "not-in-scene", Rect { x: 20, y: 30, width: 5, height: 5 }, "javascript:ignored"),
                scene_link("p", "zero", Rect { x: 20, y: 30, width: 0, height: 5 }, "https://example.com/zero"),
            ],
        };
        let links = links_for_page(&scene, &page);
        assert_eq!(links, vec![
            scene_link("p", "hit", Rect { x: 10, y: 25, width: 25, height: 20 }, "https://example.com/hit"),
            scene_link("p", "outside-region", Rect { x: 45, y: 30, width: 5, height: 5 }, "https://example.com/outside"),
            scene_link("p", "outside-page", Rect { x: -5, y: -5, width: 10, height: 10 }, "https://example.com/page-edge"),
            scene_link("p", "zero", Rect { x: 20, y: 30, width: 0, height: 5 }, "https://example.com/zero"),
        ]);
    }

    #[test]
    fn core_scene_validation_rejects_in_process_invalid_link_urls() {
        let scene = RenderScene {
            pages: vec![RenderPage { id: "p".into(), page_index: 0, size: Size::a4(), primitives: vec![], source_map: vec![] }],
            font_fingerprint: String::new(), diagnostics: vec![], metadata: Default::default(),
            links: vec![scene_link("p", "n", Rect { x: 0, y: 0, width: 1, height: 1 }, "javascript:alert(1)")],
        };
        let error = validate_scene(&scene, "test-engine").unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("test-engine"), "{}", error.message);
    }

    /// Backstop behind check_web_tree: a plan the disk cannot hold exactly is never published and
    /// leaves no temporary folder. (A repeated path and a file/folder clash stand in for names the
    /// file system merges or refuses.)
    #[test]
    fn a_tree_that_does_not_read_back_or_cannot_be_written_is_never_published() {
        let base = std::env::temp_dir().join(format!("core-publish-{}", designllm_contracts::new_id()));
        let file = |text: &str| WebFile { content: text.into(), sha256: sha256(text.as_bytes()) };
        let (p, h1, page) = (file("p{}"), file("h1{}"), file("<p>b</p>"));
        let twins = vec![(PathBuf::from("a.css"), &p), (PathBuf::from("a.css"), &h1)];
        let clash = vec![(PathBuf::from("a"), &p), (PathBuf::from("a").join("b.html"), &page)];
        for (plan, code) in [(twins, ErrorCode::UnsupportedOperation), (clash, ErrorCode::Io)] {
            let dir = base.join("0");
            assert_eq!(publish_tree(&base, &dir, 0, &plan).unwrap_err().code, code);
            assert!(!dir.exists());
            assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0, "no temporary folder is left");
        }
        let ok = vec![(PathBuf::from("a").join("b.html"), &page), (PathBuf::from("a.css"), &p)];
        assert!(publish_tree(&base, &base.join("1"), 1, &ok).unwrap());
        assert_eq!(std::fs::read_to_string(base.join("1").join("a").join("b.html")).unwrap(), "<p>b</p>");
        std::fs::remove_dir_all(&base).unwrap();
    }
    #[test]
    fn web_paths_cannot_escape_or_hit_devices() {
        for ok in ["index.html", "css/site.css", "a/b/c.js", "img/logo.v2.png", "console.js", "com10.js", "lpt.js", " lead.css", "\u{d55c}\u{ae00}.html"] { assert!(safe_relative(ok).is_ok(), "{ok}"); }
        let long = "x".repeat(256);
        for bad in ["", "/abs.html", "..", "../x", "a/../b", "a//b", "a/", "a\\b", "C:/x", "c:x", "con", "CON.txt", "nul.js", "nul .js", "com1", "com0.js", "lpt0/app.js", "lpt9.html", "com\u{b9}.txt", "conin$", "CONOUT$.log", "a./b", "a /b", "x?.js", "./a", "tab\t.js", long.as_str()] {
            assert!(safe_relative(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_whole_tree_has_no_case_twins_or_file_folder_clashes() {
        for ok in [vec!["index.html", "a/b.html", "a/c.html", "ab", "A b/x.js"], vec!["\u{e9}.css", "e\u{301}.css"]] {
            assert_eq!(check_web_tree(ok.iter().copied()).unwrap().len(), ok.len(), "{ok:?}");
        }
        for (bad, named) in [
            (vec!["index.html", "a.css", "A.css"], vec!["A.css", "a.css"]),
            (vec!["\u{131}.css", "i.css"], vec!["i.css", "\u{131}.css"]),
            (vec!["index.html", "a", "a/b.html"], vec!["a", "a/b.html"]),
            (vec!["A", "a/b/c.html"], vec!["A", "a/b/c.html"]),
            (vec!["x/Y", "x/y/z.js"], vec!["x/Y", "x/y/z.js"]),
            (vec!["ok.js", "com1.js"], vec!["com1.js"]),
        ] {
            let e = check_web_tree(bad.iter().copied()).unwrap_err();
            assert_eq!(e.code, designllm_contracts::ErrorCode::InvalidArgument);
            assert_eq!(e.affected_ids, named, "{bad:?}: {}", e.message);
        }
    }

    #[test]
    fn workspace_list_keeps_legacy_shape_without_notices_and_hides_workspace_notices_from_job_scope() {
        let conn = Connection::open_in_memory().expect("in-memory core db");
        let report = crate::RecoveryReport::default();
        let notices = workspace_notices(&conn, Path::new("unread-workspace"), &report).expect("empty report notices");
        assert!(notices.is_empty());
        let expected = serde_json::json!({ "workspace_id": "w1", "artifacts": [] });
        let mut normal = expected.clone();
        append_workspace_notices(&mut normal, notices.clone(), true);
        assert_eq!(normal, expected, "no-notice workspace.list stays byte-shape compatible");

        let diagnostic = super::workspace_diagnostic("store_migrated", "migration".to_owned(), serde_json::json!({}));
        let mut job_bound = expected.clone();
        append_workspace_notices(&mut job_bound, vec![diagnostic], false);
        assert_eq!(job_bound, expected, "job-bound list never receives workspace-level recovery data");
    }

    #[test]
    fn workspace_list_notices_distinguish_ready_missing_and_uncertain_recovery() {
        let root = std::env::temp_dir().join(format!("workspace-notices-{}", designllm_contracts::new_id()));
        std::fs::create_dir_all(root.join("backups")).expect("temporary backup directory");
        let ready_path = "backups/store1-ready.sqlite3";
        std::fs::write(root.join(ready_path), b"test placeholder").expect("ready backup placeholder");
        let conn = Connection::open_in_memory().expect("in-memory core db");
        conn.execute_batch(
            "CREATE TABLE schema_backups (backup_id TEXT, path TEXT, candidate_path TEXT, staging_path TEXT, state TEXT, source_store_version INTEGER, restore_min_store_version INTEGER, created_at_ms INTEGER);
             INSERT INTO schema_backups VALUES ('ready', 'backups/store1-ready.sqlite3', 'candidate', 'stage', 'ready', 1, 3, 1);
             INSERT INTO schema_backups VALUES ('missing', 'backups/store1-missing.sqlite3', 'candidate', 'stage', 'ready', 1, 3, 2);",
        ).expect("migration table fixture");
        let mut report = crate::RecoveryReport::default();
        report.removed_pending_temps = 2;
        report.recovered_saves = 1;
        report.migration_warnings.push(String::from(
            "preserved unregistered migration staging at backups/.store1-migration-running.staging; it is not a committed backup (manifest state=running; recovery may be partial and its exact counts are unknown (stored numeric fields are untrusted))",
        ));
        let notices = workspace_notices(&conn, &root, &report).expect("workspace notices");
        let codes: Vec<&str> = notices.iter().map(|notice| notice.code.as_str()).collect();
        assert!(codes.contains(&"store_migrated"));
        assert!(codes.contains(&"store_backup_missing"));
        assert!(codes.contains(&"pending_saves_recovered"));
        assert!(codes.contains(&"store_backup_incomplete_left"));
        let migrated = notices.iter().find(|notice| notice.code == "store_migrated").expect("migration notice");
        assert!(migrated.message.contains("현재 버전 이상의 앱에서 검증된 복원 절차로만 사용하세요"));
        assert!(migrated.message.contains("이전 버전으로 열면 일부 내용이 사라질 수 있습니다"));
        assert_eq!(migrated.params.get("restore_min_store_version").and_then(Value::as_i64), Some(3));
        let uncertain = notices.iter().find(|notice| notice.code == "store_backup_incomplete_left").expect("uncertain-stage notice");
        assert!(uncertain.message.contains("확인할 수 없습니다"));
        assert_eq!(uncertain.params.get("counts_known").and_then(Value::as_bool), Some(false));
        assert!(!uncertain.params.contains_key("previous_removed_pending_temps"));
        assert!(!uncertain.params.contains_key("previous_recovered_saves"));
        assert_eq!(notices.iter().find(|notice| notice.code == "pending_saves_recovered").unwrap().params.get("removed_pending_temps").and_then(Value::as_u64), Some(2));
        std::fs::remove_dir_all(&root).expect("remove test-owned notice fixture");
    }
}
