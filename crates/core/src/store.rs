//! SQLite schema v3 and row helpers. The database owns current revision pointers, history,
//! receipts, jobs, proposals and events; model snapshots and assets are immutable blobs that are
//! durable before any row references them. WAL + synchronous=FULL, verified at open.
use crate::errors::{err, integrity, invalid, not_found, sql};
use designllm_contracts::*;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const STORE_SCHEMA_VERSION: i64 = 3;
/// Prevents a build that cannot retain exact AI Figure pins from reopening this workspace.
pub const JOB_FIGURE_REFS_FEATURE: &str = "core.job_figure_refs.v1";
/// Prevents a build that would drop a job's `deliver_as: proposal` (and so let its `changes.apply` through) from reopening this workspace.
pub const JOB_DELIVER_AS_FEATURE: &str = "core.job_deliver_as.v1";
/// Legacy `PRAGMA application_id` used by store-v1/v2 workspaces ("DLLT").
pub const APPLICATION_ID: i64 = 0x444C_4C54;
/// `PRAGMA application_id` for live store-v3 workspaces ("DLT3"). Older cores refuse this id before changing journal mode.
pub const STORE3_APPLICATION_ID: i64 = 0x444C_5433;
/// A migration checkpoint is a valid SQLite file, but not an openable workspace ("DLBK").
const BACKUP_APPLICATION_ID: i64 = 0x444C_424B;
const BACKUP_MARKER_VERSION: i64 = 1;
const BACKUP_RESTORE_MIN_STORE_VERSION: i64 = 3;
const BACKUP_MARKER_KEYS: &[&str] = &[
    "dlt_backup_kind", "dlt_backup_marker_version", "dlt_backup_id",
    "dlt_backup_source_store_version", "dlt_backup_restore_min_store_version",
    "dlt_backup_original_application_id", "dlt_backup_original_user_version",
];
pub const DB_FILE: &str = "workspace.sqlite3";
pub const BLOB_DIR: &str = "blobs";
/// Events kept for `events.poll` replay; older ones are pruned at open (resync_required).
pub const EVENT_RETENTION: i64 = 10_000;
/// Bundled SQLite's `win32-longpath` VFS accepts at most 32,767 UTF-16 units. Keep seven units
/// for the extended-path prefix and terminator; check each generated DB and sidecar path.
const MAX_SAFE_WINDOWS_PATH_UNITS: usize = 32_760;

const SCHEMA_V1: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
CREATE TABLE assets (sha256 TEXT PRIMARY KEY, mime TEXT NOT NULL, bytes INTEGER NOT NULL CHECK (bytes >= 0), origin TEXT NOT NULL) STRICT;
CREATE TABLE artifacts (
    artifact_id TEXT PRIMARY KEY, kind TEXT NOT NULL, engine_id TEXT NOT NULL, title TEXT NOT NULL, origin TEXT NOT NULL,
    current_revision INTEGER NOT NULL CHECK (current_revision >= 0), current_snapshot TEXT NOT NULL, base_snapshot TEXT NOT NULL,
    source_path TEXT, source_hash TEXT,
    saved_revision INTEGER, saved_path TEXT, saved_hash TEXT, saved_snapshot TEXT,
    created_seq INTEGER NOT NULL
) STRICT;
CREATE TABLE revisions (
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id), revision INTEGER NOT NULL, snapshot TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('create', 'open', 'change', 'undo', 'redo')),
    request_id TEXT, actor TEXT NOT NULL, job_id TEXT, attempt_id TEXT, intents TEXT NOT NULL, changed_ids TEXT NOT NULL,
    PRIMARY KEY (artifact_id, revision)
) STRICT;
CREATE TABLE undo_log (
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id), seq INTEGER NOT NULL, change_revision INTEGER NOT NULL,
    before_snapshot TEXT NOT NULL, after_snapshot TEXT NOT NULL, intents TEXT NOT NULL, changed_ids TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('done', 'undone', 'discarded')),
    PRIMARY KEY (artifact_id, seq)
) STRICT;
CREATE TABLE receipts (
    request_id TEXT PRIMARY KEY, method TEXT NOT NULL, content_hash TEXT NOT NULL, artifact_id TEXT,
    outcome TEXT NOT NULL CHECK (outcome IN ('committed', 'rejected')), result_json TEXT NOT NULL
) STRICT;
CREATE TABLE jobs (
    job_id TEXT PRIMARY KEY, artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id), base_revision INTEGER NOT NULL,
    allowed_intents TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'validating', 'completed', 'canceled', 'failed', 'conflict')),
    current_attempt TEXT NOT NULL, attempt_count INTEGER NOT NULL, request_json TEXT NOT NULL, error_json TEXT
) STRICT;
CREATE TABLE attempts (
    job_id TEXT NOT NULL REFERENCES jobs(job_id), attempt_id TEXT NOT NULL, seq INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('active', 'canceled', 'ended', 'superseded')),
    PRIMARY KEY (job_id, attempt_id)
) STRICT;
CREATE TABLE proposals (
    proposal_id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, content_hash TEXT NOT NULL,
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id), base_revision INTEGER NOT NULL, change_set_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('draft', 'validated', 'applied', 'rejected', 'conflict')),
    job_id TEXT, attempt_id TEXT, error_json TEXT, diagnostics_json TEXT NOT NULL
) STRICT;
CREATE TABLE figure_renders (
    figure_id TEXT NOT NULL, figure_revision INTEGER NOT NULL, representation TEXT NOT NULL, sha256 TEXT NOT NULL,
    profile_json TEXT NOT NULL, editability TEXT NOT NULL,
    PRIMARY KEY (figure_id, figure_revision, representation, sha256)
) STRICT;
CREATE TABLE events (
    event_seq INTEGER PRIMARY KEY AUTOINCREMENT, artifact_id TEXT, revision INTEGER, event TEXT NOT NULL, detail TEXT NOT NULL
);
CREATE TABLE pending_saves (
    temp_path TEXT PRIMARY KEY, artifact_id TEXT NOT NULL, target_path TEXT NOT NULL, revision INTEGER NOT NULL,
    content_hash TEXT NOT NULL, snapshot TEXT NOT NULL, purpose TEXT NOT NULL CHECK (purpose IN ('save', 'export'))
) STRICT;
"#;

/// Tables added after schema v1 without a version change: created on every open when missing.
/// Builds that do not know them leave them alone, and a missing row means "not reported".
const SCHEMA_V1_ADDITIONS: &str = r#"
CREATE TABLE IF NOT EXISTS job_clients (
    job_id TEXT NOT NULL, attempt_id TEXT NOT NULL, client_name TEXT NOT NULL, client_version TEXT,
    PRIMARY KEY (job_id, attempt_id)
) STRICT;
CREATE TABLE IF NOT EXISTS artifact_times (
    artifact_id TEXT PRIMARY KEY, modified_at_ms INTEGER NOT NULL CHECK (modified_at_ms >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS job_created_ids (
    job_id TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY (job_id, id)
) STRICT;
"#;

/// Store-v2 backup metadata. Blob rows keep every hash needed by a retained schema backup alive.
#[cfg(test)]
const SCHEMA_V2_ADDITIONS: &str = r#"
CREATE TABLE schema_backups (
    backup_id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
    candidate_path TEXT NOT NULL UNIQUE, staging_path TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK (state IN ('publishing', 'ready')),
    source_store_version INTEGER NOT NULL CHECK (source_store_version = 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
) STRICT;
CREATE TABLE schema_backup_blobs (
    backup_id TEXT NOT NULL REFERENCES schema_backups(backup_id), sha256 TEXT NOT NULL,
    PRIMARY KEY (backup_id, sha256)
) STRICT;
CREATE INDEX schema_backup_blobs_by_sha256 ON schema_backup_blobs(sha256);
"#;

/// Store-v3 metadata. `undo_log.group_id` is deliberately not a SQL foreign key: store-v2
/// already has that nullable column, and rebuilding the historical table adds risk without
/// replacing the application checks on complete group membership. Group commits must create
/// `history_groups` and every member row in one transaction; readers validate count and uniqueness.
const SCHEMA_V3_BACKUP_ADDITIONS: &str = r#"
CREATE TABLE schema_backups (
    backup_id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
    candidate_path TEXT NOT NULL UNIQUE, staging_path TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK (state IN ('publishing', 'ready')),
    source_store_version INTEGER NOT NULL CHECK (source_store_version IN (1, 2)),
    restore_min_store_version INTEGER NOT NULL CHECK (restore_min_store_version >= 3),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
) STRICT;
CREATE TABLE schema_backup_blobs (
    backup_id TEXT NOT NULL REFERENCES schema_backups(backup_id), sha256 TEXT NOT NULL,
    PRIMARY KEY (backup_id, sha256)
) STRICT;
CREATE INDEX schema_backup_blobs_by_sha256 ON schema_backup_blobs(sha256);
"#;

const SCHEMA_V3_HISTORY_ADDITIONS: &str = r#"
CREATE TABLE history_groups (
    group_id TEXT PRIMARY KEY, request_id TEXT NOT NULL,
    member_count INTEGER NOT NULL CHECK (member_count >= 2)
) STRICT;
CREATE INDEX undo_log_by_group_id ON undo_log(group_id) WHERE group_id IS NOT NULL;
CREATE UNIQUE INDEX undo_log_group_member ON undo_log(group_id, artifact_id) WHERE group_id IS NOT NULL;
"#;

/// Ids the job's own commits created (they are in the scope of a job with target_ids).
pub fn job_created_ids(conn: &Connection, job_id: &str) -> DltResult<BTreeSet<String>> {
    let mut st = conn.prepare("SELECT id FROM job_created_ids WHERE job_id = ?1").map_err(sql)?;
    let rows = st.query_map(params![job_id], |r| r.get::<_, String>(0)).map_err(sql)?;
    rows.collect::<rusqlite::Result<BTreeSet<_>>>().map_err(sql)
}
pub fn add_job_created_ids(conn: &Connection, job_id: &str, ids: &BTreeSet<String>) -> DltResult<()> {
    for id in ids {
        conn.execute("INSERT OR IGNORE INTO job_created_ids(job_id, id) VALUES (?1, ?2)", params![job_id, id]).map_err(sql)?;
    }
    Ok(())
}

fn absolute_path(path: &Path) -> DltResult<PathBuf> {
    if path.is_absolute() { return Ok(path.to_path_buf()); }
    std::env::current_dir().map(|cwd| cwd.join(path)).map_err(crate::errors::io_store)
}

fn sqlite_path(path: &Path) -> DltResult<PathBuf> {
    let path = absolute_path(path)?;
    #[cfg(windows)]
    {
        let text = path.to_string_lossy().replace('/', "\\");
        if text.starts_with(r"\\?\") {
            if has_windows_dot_segments(&text) { return Err(ambiguous_windows_path_error()); }
            return Ok(PathBuf::from(text));
        }
        let sidecar_needs_prefix = ["-wal", "-shm", "-journal"]
            .iter()
            .any(|suffix| path_units(&backup_sidecar_path(&path, suffix)) > 259);
        if path_units(&path) > 259 || sidecar_needs_prefix {
            if has_windows_dot_segments(&text) { return Err(ambiguous_windows_path_error()); }
            if text.starts_with(r"\\") {
                return Ok(PathBuf::from(format!(r"\\?\UNC\{}", text.trim_start_matches('\\'))));
            }
            let bytes = text.as_bytes();
            if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
                return Ok(PathBuf::from(format!(r"\\?\{}", text)));
            }
            return Err(err(ErrorCode::StorageFailure, "SQLite long path must be an absolute drive or UNC path"));
        }
    }
    Ok(path)
}

#[cfg(windows)]
fn has_windows_dot_segments(path: &str) -> bool {
    path.split('\\').any(|component| component == "." || component == "..")
}

#[cfg(windows)]
fn ambiguous_windows_path_error() -> DltError {
    let message = "긴 Windows 경로에 '.' 또는 '..' 구성요소가 있어 안전하게 열 수 없습니다. 해당 경로를 정규화해 다시 선택하세요. 이 시도에서는 데이터베이스와 기존 백업이 변경되지 않았습니다.";
    let mut error = err(
        ErrorCode::StorageFailure,
        message,
    );
    error.diagnostics.push(Diagnostic {
        code: "core.workspace_path_ambiguous".into(),
        message: message.into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    });
    error
}

fn open_connection(path: impl AsRef<Path>) -> DltResult<Connection> {
    open_connection_with_flags(path, OpenFlags::default())
}

fn open_connection_with_flags(path: impl AsRef<Path>, flags: OpenFlags) -> DltResult<Connection> {
    let path = sqlite_path(path.as_ref())?;
    #[cfg(windows)]
    { Connection::open_with_flags_and_vfs(path, flags, "win32-longpath").map_err(sql) }
    #[cfg(not(windows))]
    { Connection::open_with_flags(path, flags).map_err(sql) }
}

fn path_units(path: &Path) -> usize {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().count()
    }
    #[cfg(not(windows))]
    {
        path.as_os_str().to_string_lossy().chars().count()
    }
}

fn ensure_path_limit(path: &Path) -> DltResult<()> {
    if cfg!(windows) && path_units(path) > MAX_SAFE_WINDOWS_PATH_UNITS {
        let message = "작업공간 경로가 너무 길어 안전하게 열거나 백업할 수 없습니다. 작업공간을 더 짧은 폴더 경로로 옮긴 뒤 다시 여세요. 데이터베이스 버전과 기존 백업은 변경되지 않았습니다.";
        let mut error = err(ErrorCode::StorageFailure, message);
        error.diagnostics.push(Diagnostic {
            code: "core.workspace_path_too_long".into(),
            message: message.into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        });
        return Err(error);
    }
    Ok(())
}

fn ensure_database_paths_supported(path: &Path) -> DltResult<()> {
    let path = absolute_path(path)?;
    let _sqlite_path = sqlite_path(&path)?;
    ensure_path_limit(&path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        ensure_path_limit(&backup_sidecar_path(&path, suffix))?;
    }
    Ok(())
}

pub(crate) fn validate_workspace_path(root: &Path) -> DltResult<()> {
    ensure_database_paths_supported(&root.join(DB_FILE))
}

fn ensure_migration_paths_supported(root: &Path, source_store_version: i64) -> DltResult<()> {
    let root = absolute_path(root)?;
    ensure_path_limit(&root.join("backups"))?;
    let prefix = stage_prefix(source_store_version)?;
    let stage = root.join("backups").join(format!("{prefix}{}{}", "x".repeat(36), STORE1_STAGE_SUFFIX));
    ensure_path_limit(&stage)?;
    ensure_path_limit(&stage.join(STORE1_OWNER_FILE))?;
    let partial = stage.join("candidate.sqlite3.partial");
    ensure_path_limit(&partial)?;
    for suffix in ["-wal", "-shm", "-journal"] { ensure_path_limit(&backup_sidecar_path(&partial, suffix))?; }
    ensure_path_limit(&stage.join("candidate.sqlite3"))?;
    ensure_path_limit(&root.join("backups").join(format!("store{source_store_version}-{}-{}.sqlite3", "9".repeat(19), "x".repeat(36))))
}

fn preflight_snapshot_features(
    root: &Path,
    conn: &Connection,
    engines: &[std::sync::Arc<dyn EngineAdapter>],
    supported: &BTreeSet<String>,
) -> DltResult<BTreeSet<String>> {
    let mut discovered = BTreeSet::new();
    let mut statement = conn.prepare(
        "SELECT snapshot_hash FROM (
             SELECT current_snapshot AS snapshot_hash FROM artifacts
             UNION SELECT base_snapshot FROM artifacts
             UNION SELECT saved_snapshot FROM artifacts WHERE saved_snapshot IS NOT NULL
             UNION SELECT snapshot FROM revisions
             UNION SELECT before_snapshot FROM undo_log
             UNION SELECT after_snapshot FROM undo_log
             UNION SELECT snapshot FROM pending_saves
         ) WHERE snapshot_hash IS NOT NULL ORDER BY snapshot_hash",
    ).map_err(sql)?;
    let mut rows = statement.query([]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        let hash: String = row.get(0).map_err(sql)?;
        if !crate::blobs::is_hash(&hash) { return Err(integrity("snapshot table contains a malformed blob hash")); }
        let bytes = fs::read(root.join(BLOB_DIR).join(&hash)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound { integrity(format!("persisted snapshot blob {hash} is missing during migration preflight")) } else { crate::errors::io_store(error) }
        })?;
        if sha256(&bytes) != hash { return Err(integrity(format!("persisted snapshot blob {hash} failed hash verification during migration preflight"))); }
        crate::snapshot_guard::check(&bytes).map_err(|error| err(ErrorCode::SchemaUnsupported, format!("store migration stopped before backup because snapshot {hash} cannot be read without losing fields: {}", error.message)))?;
        let raw: Value = serde_json::from_slice(&bytes).map_err(|error| err(ErrorCode::SchemaUnsupported, format!("store migration cannot classify snapshot {hash}: {error}")))?;
        let engine_id = raw.get("engine_id").and_then(Value::as_str)
            .ok_or_else(|| err(ErrorCode::SchemaUnsupported, format!("snapshot {hash} has no engine_id for migration preflight")))?;
        let engine = engines.iter().find(|engine| engine.id() == engine_id)
            .ok_or_else(|| err(ErrorCode::SchemaUnsupported, format!("engine {engine_id:?} is unavailable while preflighting snapshot {hash}")))?;
        let artifact = engine.restore(&bytes).map_err(|error| err(ErrorCode::SchemaUnsupported, format!("store migration stopped before backup because engine {engine_id} cannot restore snapshot {hash}: {}", error.message)))?;
        if artifact.engine_id != engine_id || artifact.schema_version != SCHEMA_VERSION {
            return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot {hash} has unsupported engine identity or contract schema during migration preflight")));
        }
        let used = engine.persisted_features(&artifact).map_err(|error| err(ErrorCode::SchemaUnsupported, format!("store migration stopped before backup because engine {engine_id} cannot classify snapshot {hash}: {}", error.message)))?;
        let engine_supported = engine.supported_persisted_features();
        if let Some(key) = used.difference(&engine_supported).next() {
            return Err(err(ErrorCode::SchemaUnsupported, format!("engine {engine_id} detected persisted feature {key:?} it does not declare as supported in snapshot {hash}")));
        }
        if let Some(key) = used.difference(supported).next() {
            return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot {hash} requires persisted feature {key:?} not supported by this build")));
        }
        discovered.extend(used);
    }
    Ok(discovered)
}

fn valid_persisted_feature_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 128 && key.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn supported_persisted_features(engines: &[std::sync::Arc<dyn EngineAdapter>]) -> DltResult<BTreeSet<String>> {
    let mut supported = BTreeSet::from([JOB_FIGURE_REFS_FEATURE.to_owned(), JOB_DELIVER_AS_FEATURE.to_owned()]);
    for engine in engines {
        for key in engine.supported_persisted_features() {
            if !valid_persisted_feature_key(&key) { return Err(integrity(format!("engine {} declared an invalid persisted feature key {key:?}", engine.id()))); }
            supported.insert(key);
        }
    }
    Ok(supported)
}

fn stored_feature_set(conn: &Connection) -> DltResult<BTreeSet<String>> {
    let raw: String = conn.query_row("SELECT value FROM meta WHERE key='store_features'", [], |row| row.get(0)).optional().map_err(sql)?
        .ok_or_else(|| integrity("store_features is missing from store v3"))?;
    let features: BTreeSet<String> = from_json(&raw, "store_features")?;
    if features.iter().any(|key| !valid_persisted_feature_key(key)) { return Err(integrity("store_features contains an invalid feature key")); }
    Ok(features)
}

pub fn ensure_store_features_supported(conn: &Connection, supported: &BTreeSet<String>) -> DltResult<()> {
    let stored = stored_feature_set(conn)?;
    let unknown: Vec<String> = stored.difference(supported).cloned().collect();
    if !unknown.is_empty() {
        return Err(err(ErrorCode::SchemaUnsupported, format!("workspace requires persisted feature(s) not supported by this build: {unknown:?}")));
    }
    Ok(())
}

pub fn merge_store_features(conn: &Connection, additions: &BTreeSet<String>) -> DltResult<()> {
    if additions.iter().any(|key| !valid_persisted_feature_key(key)) { return Err(integrity("engine returned an invalid persisted feature key")); }
    let mut features = stored_feature_set(conn)?;
    let old_len = features.len();
    features.extend(additions.iter().cloned());
    if features.len() != old_len {
        conn.execute("UPDATE meta SET value=?1 WHERE key='store_features'", params![to_json(&features)?]).map_err(sql)?;
    }
    Ok(())
}

fn store_features_backfilled(conn: &Connection) -> DltResult<bool> {
    let value: String = conn.query_row("SELECT value FROM meta WHERE key='store_features_backfilled'", [], |row| row.get(0)).optional().map_err(sql)?
        .ok_or_else(|| integrity("store_features_backfilled is missing from store v3"))?;
    match value.as_str() {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(integrity("store_features_backfilled is not 0 or 1")),
    }
}

/// Backfill feature keys from old snapshots one at a time. The query deduplicates hashes in SQLite;
/// only one snapshot byte buffer and Artifact are held at once. The feature set and completion
/// marker commit together, so an interrupted scan is safe to retry.
pub fn backfill_store_features(
    conn: &mut Connection,
    blobs: &crate::blobs::BlobStore,
    engines: &[std::sync::Arc<dyn EngineAdapter>],
    supported: &BTreeSet<String>,
) -> DltResult<()> {
    ensure_store_features_supported(conn, supported)?;
    if store_features_backfilled(conn)? { return Ok(()); }
    let mut discovered = stored_feature_set(conn)?;
    {
        let mut statement = conn.prepare(
            "SELECT engine_id, snapshot_hash FROM (
                 SELECT engine_id, current_snapshot AS snapshot_hash FROM artifacts
                 UNION SELECT engine_id, base_snapshot FROM artifacts
                 UNION SELECT engine_id, saved_snapshot FROM artifacts WHERE saved_snapshot IS NOT NULL
                 UNION SELECT a.engine_id, r.snapshot FROM revisions r JOIN artifacts a ON a.artifact_id=r.artifact_id
                 UNION SELECT a.engine_id, u.before_snapshot FROM undo_log u JOIN artifacts a ON a.artifact_id=u.artifact_id
                 UNION SELECT a.engine_id, u.after_snapshot FROM undo_log u JOIN artifacts a ON a.artifact_id=u.artifact_id
             ) ORDER BY engine_id, snapshot_hash",
        ).map_err(sql)?;
        let mut rows = statement.query([]).map_err(sql)?;
        while let Some(row) = rows.next().map_err(sql)? {
            let engine_id: String = row.get(0).map_err(sql)?;
            let hash: String = row.get(1).map_err(sql)?;
            let engine = engines.iter().find(|engine| engine.id() == engine_id)
                .ok_or_else(|| err(ErrorCode::SchemaUnsupported, format!("engine {engine_id:?} is unavailable while backfilling persisted feature keys")))?;
            let bytes = blobs.get(&hash).map_err(|error| err(ErrorCode::SchemaUnsupported, format!("cannot inspect snapshot {hash} for persisted features: {}", error.message)))?;
            crate::snapshot_guard::check(&bytes).map_err(|error| err(ErrorCode::SchemaUnsupported, format!("snapshot {hash} cannot be safely inspected for persisted features: {}", error.message)))?;
            let artifact = engine.restore(&bytes).map_err(|error| err(ErrorCode::SchemaUnsupported, format!("engine {} cannot classify persisted features in snapshot {hash}: {}", engine.id(), error.message)))?;
            if artifact.engine_id != engine.id() || artifact.schema_version != SCHEMA_VERSION {
                return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot {hash} has unsupported artifact identity or schema while backfilling features")));
            }
            let used = engine.persisted_features(&artifact).map_err(|error| err(ErrorCode::SchemaUnsupported, format!("engine {} cannot classify persisted features in snapshot {hash}: {}", engine.id(), error.message)))?;
            let engine_supported: BTreeSet<String> = engine.supported_persisted_features();
            if let Some(unknown) = used.difference(&engine_supported).next() {
                return Err(err(ErrorCode::SchemaUnsupported, format!("engine {} detected persisted feature {unknown:?} that it does not declare as supported", engine.id())));
            }
            if let Some(unknown) = used.difference(supported).next() {
                return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot {hash} requires persisted feature {unknown:?} not supported by this build")));
            }
            discovered.extend(used);
        }
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
    let version = store_version(&tx)?;
    if version != STORE_SCHEMA_VERSION { return Err(err(ErrorCode::SchemaUnsupported, format!("workspace store schema changed to {version} during feature backfill"))); }
    if store_features_backfilled(&tx)? { return Ok(()); }
    let mut current = stored_feature_set(&tx)?;
    current.extend(discovered);
    tx.execute("UPDATE meta SET value=?1 WHERE key='store_features'", params![to_json(&current)?]).map_err(sql)?;
    let changed = tx.execute("UPDATE meta SET value='1' WHERE key='store_features_backfilled' AND value='0'", []).map_err(sql)?;
    if changed != 1 { return Err(integrity("store feature backfill marker changed unexpectedly")); }
    tx.commit().map_err(sql)
}

pub fn validate_history_groups(conn: &Connection) -> DltResult<()> {
    if !db_has_table(conn, "history_groups")? { return Err(integrity("history_groups is missing from store v3")); }
    let bad_group: Option<(String, i64, i64)> = conn.query_row(
        "SELECT h.group_id,h.member_count,count(u.group_id) FROM history_groups h LEFT JOIN undo_log u ON u.group_id=h.group_id GROUP BY h.group_id HAVING count(u.group_id) != h.member_count LIMIT 1",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(sql)?;
    if let Some((id, expected, found)) = bad_group {
        return Err(integrity(format!("history group {id} expects {expected} undo rows but has {found}")));
    }
    let orphan: Option<String> = conn.query_row(
        "SELECT u.group_id FROM undo_log u LEFT JOIN history_groups h ON h.group_id=u.group_id WHERE u.group_id IS NOT NULL AND h.group_id IS NULL LIMIT 1",
        [], |row| row.get(0),
    ).optional().map_err(sql)?;
    if let Some(id) = orphan { return Err(integrity(format!("undo row references missing history group {id}"))); }
    let duplicate: Option<String> = conn.query_row(
        "SELECT group_id FROM undo_log WHERE group_id IS NOT NULL GROUP BY group_id,artifact_id HAVING count(*) > 1 LIMIT 1",
        [], |row| row.get(0),
    ).optional().map_err(sql)?;
    if let Some(id) = duplicate { return Err(integrity(format!("history group {id} contains multiple undo rows for one artifact"))); }
    Ok(())
}
/// The job whose commit made undo-log entry `seq` (None: a user's commit).
pub fn undo_entry_job(conn: &Connection, artifact_id: &str, seq: i64) -> DltResult<Option<String>> {
    conn.query_row(
        "SELECT r.job_id FROM undo_log u JOIN revisions r ON r.artifact_id = u.artifact_id AND r.revision = u.change_revision WHERE u.artifact_id = ?1 AND u.seq = ?2",
        params![artifact_id, seq], |r| r.get::<_, Option<String>>(0),
    ).optional().map_err(sql).map(Option::flatten)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)).unwrap_or(0)
}
/// Records that the artifact's current revision was committed now (inside the commit transaction).
pub fn touch_artifact(conn: &Connection, artifact_id: &str) -> DltResult<()> {
    conn.execute(
        "INSERT INTO artifact_times(artifact_id, modified_at_ms) VALUES (?1, ?2) ON CONFLICT(artifact_id) DO UPDATE SET modified_at_ms = excluded.modified_at_ms",
        params![artifact_id, now_ms()],
    ).map_err(sql)?;
    Ok(())
}
/// Commit times recorded by this build (artifacts last committed before it have none).
pub fn artifact_times(conn: &Connection) -> DltResult<std::collections::HashMap<String, u64>> {
    let mut st = conn.prepare("SELECT artifact_id, modified_at_ms FROM artifact_times").map_err(sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map_err(sql)?;
    rows.map(|r| r.map(|(id, ms)| (id, ms.max(0) as u64))).collect::<rusqlite::Result<_>>().map_err(sql)
}

#[cfg(test)]
pub fn open_db(root: &Path, open_recovery: &mut crate::RecoveryReport) -> DltResult<(Connection, String)> {
    open_db_inner(root, open_recovery, None, false)
}

pub fn open_db_with_engines(
    root: &Path,
    open_recovery: &mut crate::RecoveryReport,
    engines: &[std::sync::Arc<dyn EngineAdapter>],
    supported: &BTreeSet<String>,
) -> DltResult<(Connection, String)> {
    open_db_inner(root, open_recovery, Some((engines, supported)), false)
}

pub(crate) fn open_db_with_engines_for_backup_restore(
    root: &Path,
    open_recovery: &mut crate::RecoveryReport,
    engines: &[std::sync::Arc<dyn EngineAdapter>],
    supported: &BTreeSet<String>,
) -> DltResult<(Connection, String)> {
    open_db_inner(root, open_recovery, Some((engines, supported)), true)
}

fn open_db_inner(
    root: &Path,
    open_recovery: &mut crate::RecoveryReport,
    engine_context: Option<(&[std::sync::Arc<dyn EngineAdapter>], &BTreeSet<String>)>,
    allow_backup_restore: bool,
) -> DltResult<(Connection, String)> {
    let database_path = root.join(DB_FILE);
    ensure_database_paths_supported(&database_path)?;
    let mut conn = open_connection(&database_path)?;
    conn.busy_timeout(Duration::from_secs(5)).map_err(sql)?;
    // Identify the file before changing anything in it (journal mode is persistent).
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table'", [], |r| r.get(0)).map_err(sql)?;
    let app_id: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0)).map_err(sql)?;
    if tables == 0 && app_id != 0 {
        return Err(err(ErrorCode::SchemaUnsupported, format!("empty SQLite database has unexpected application_id {app_id:#x}")));
    }
    if tables > 0 && !matches!(app_id, APPLICATION_ID | STORE3_APPLICATION_ID | BACKUP_APPLICATION_ID) {
        return Err(err(ErrorCode::SchemaUnsupported, format!("not a workspace database (application_id {app_id:#x})")));
    }
    let has_meta = tables > 0 && db_has_table(&conn, "meta")?;
    if tables > 0 && !has_meta {
        return Err(err(ErrorCode::SchemaUnsupported, "workspace or backup application_id is missing its metadata table"));
    }
    let restore_marker = if has_meta { read_backup_marker(&conn)? } else { None };
    if restore_marker.is_some() && !allow_backup_restore {
        return Err(err(ErrorCode::SchemaUnsupported, "this is a protected migration backup; restore it with the current app into a new folder"));
    }
    if allow_backup_restore && tables == 0 {
        return Err(err(ErrorCode::SchemaUnsupported, "backup restore requires a non-empty registered workspace backup"));
    }
    let mut preflight_features = None;
    let mut legacy_store3_preflighted = false;
    if has_meta {
        let stored_version: Option<String> = conn.query_row(
            "SELECT value FROM meta WHERE key='store_schema_version'", [], |row| row.get(0),
        ).optional().map_err(sql)?;
        let parsed_version = stored_version.and_then(|value| value.parse::<i64>().ok());
        if app_id == STORE3_APPLICATION_ID && parsed_version != Some(STORE_SCHEMA_VERSION) {
            return Err(err(ErrorCode::SchemaUnsupported, "store-v3 application_id does not match the workspace schema version"));
        }
        if app_id == APPLICATION_ID && parsed_version.is_none() {
            return Err(err(ErrorCode::SchemaUnsupported, "legacy workspace application_id has a missing or invalid store schema version"));
        }
        if app_id == APPLICATION_ID && parsed_version.is_some_and(|version| version > STORE_SCHEMA_VERSION) {
            return Err(err(ErrorCode::SchemaUnsupported, "legacy workspace application_id does not match a newer store schema"));
        }
        if app_id == APPLICATION_ID && parsed_version == Some(STORE_SCHEMA_VERSION) {
            let Some((engines, supported)) = engine_context else {
                return Err(err(ErrorCode::SchemaUnsupported, "legacy store-v3 workspace requires engine preflight before its application_id can be upgraded"));
            };
            validate_workspace_contract_metadata(&conn)?;
            store_features_backfilled(&conn)?;
            ensure_store_features_supported(&conn, supported)?;
            preflight_snapshot_features(root, &conn, engines, supported)?;
            validate_history_groups(&conn)?;
            for table in ["schema_backups", "schema_backup_blobs", "pending_saves"] {
                if !db_has_table(&conn, table)? {
                    return Err(integrity(format!("legacy store-v3 workspace is missing {table}")));
                }
            }
            preflight_legacy_store3_backup_registry(root, &conn)?;
            promote_legacy_store3_application_id(&mut conn)?;
            legacy_store3_preflighted = true;
        }
        if let Some(version) = parsed_version {
            if let Some(marker) = &restore_marker {
                if marker.source_store_version != version || marker.restore_min_store_version > STORE_SCHEMA_VERSION {
                    return Err(err(ErrorCode::SchemaUnsupported, "protected backup version metadata does not match this restore reader"));
                }
            }
            if version > STORE_SCHEMA_VERSION {
                return Err(err(ErrorCode::SchemaUnsupported, format!("workspace store schema {version} is newer than supported {STORE_SCHEMA_VERSION}")));
            }
            let should_preflight_migration = matches!(version, 1 | 2);
            let needs_feature_backfill = version == STORE_SCHEMA_VERSION && !store_features_backfilled(&conn)?;
            if should_preflight_migration { ensure_migration_paths_supported(root, version)?; }
            if let Some((engines, supported)) = engine_context {
                if version == STORE_SCHEMA_VERSION { ensure_store_features_supported(&conn, supported)?; }
                if (should_preflight_migration || needs_feature_backfill) && !legacy_store3_preflighted {
                    preflight_features = Some(preflight_snapshot_features(root, &conn, engines, supported)?);
                }
            }
        }
    }
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).map_err(sql)?;
    if !mode.eq_ignore_ascii_case("wal") { return Err(err(ErrorCode::StorageFailure, format!("journal_mode is {mode}, expected wal"))); }
    conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;").map_err(sql)?;
    let sync: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0)).map_err(sql)?;
    if sync != 2 { return Err(err(ErrorCode::StorageFailure, format!("synchronous is {sync}, expected 2 (FULL)"))); }
    let workspace_id = init_schema(&mut conn, root, open_recovery, preflight_features.as_ref(), restore_marker.as_ref())?;
    Ok((conn, workspace_id))
}

fn validate_workspace_contract_metadata(conn: &Connection) -> DltResult<String> {
    let meta = |key: &str| -> DltResult<Option<String>> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| row.get(0))
            .optional()
            .map_err(sql)
    };
    let contract: i64 = meta("contract_schema_version")?.and_then(|value| value.parse().ok()).unwrap_or(-1);
    if contract != i64::from(SCHEMA_VERSION) {
        return Err(err(ErrorCode::SchemaUnsupported, format!("workspace contract schema {contract} differs from {}", SCHEMA_VERSION)));
    }
    let alg = meta("request_hash_alg")?.unwrap_or_default();
    if alg != crate::receipts::REQUEST_HASH_ALG {
        return Err(err(ErrorCode::SchemaUnsupported, format!("receipt hash algorithm {alg:?} differs from this build")));
    }
    meta("workspace_id")?.filter(|value| !value.is_empty()).ok_or_else(|| integrity("workspace_id missing"))
}

fn promote_legacy_store3_application_id(conn: &mut Connection) -> DltResult<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
    if store_version(&tx)? != STORE_SCHEMA_VERSION {
        return Err(err(ErrorCode::SchemaUnsupported, "legacy application_id is only promoted for a validated store-v3 workspace"));
    }
    let current: i64 = tx.query_row("PRAGMA application_id", [], |row| row.get(0)).map_err(sql)?;
    if current != APPLICATION_ID {
        return Err(err(ErrorCode::SchemaUnsupported, "legacy store-v3 application_id changed before promotion"));
    }
    tx.execute_batch(&format!("PRAGMA application_id={STORE3_APPLICATION_ID};")).map_err(sql)?;
    tx.commit().map_err(sql)
}

fn init_schema(
    conn: &mut Connection,
    root: &Path,
    open_recovery: &mut crate::RecoveryReport,
    preflight_features: Option<&BTreeSet<String>>,
    restore_marker: Option<&BackupMarker>,
) -> DltResult<String> {
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table'", [], |r| r.get(0)).map_err(sql)?;
    if tables == 0 {
        let workspace_id = new_id();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
        tx.execute_batch(SCHEMA_V1).map_err(sql)?;
        tx.execute_batch(SCHEMA_V1_ADDITIONS).map_err(sql)?;
        tx.execute_batch("ALTER TABLE undo_log ADD COLUMN group_id TEXT NULL;").map_err(sql)?;
        tx.execute_batch(SCHEMA_V3_BACKUP_ADDITIONS).map_err(sql)?;
        tx.execute_batch(SCHEMA_V3_HISTORY_ADDITIONS).map_err(sql)?;
        tx.execute_batch(&format!("PRAGMA application_id = {STORE3_APPLICATION_ID};")).map_err(sql)?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('store_schema_version', ?1), ('contract_schema_version', ?2), ('workspace_id', ?3), ('request_hash_alg', ?4), ('store_features', '[]'), ('store_features_backfilled', '1')",
            params![STORE_SCHEMA_VERSION.to_string(), SCHEMA_VERSION.to_string(), workspace_id, crate::receipts::REQUEST_HASH_ALG],
        ).map_err(sql)?;
        tx.commit().map_err(sql)?;
        return Ok(workspace_id);
    }
    let meta = |key: &str| -> DltResult<Option<String>> { conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional().map_err(sql) };
    let version: i64 = meta("store_schema_version")?.and_then(|v| v.parse().ok()).ok_or_else(|| err(ErrorCode::SchemaUnsupported, "store_schema_version missing"))?;
    if version > STORE_SCHEMA_VERSION {
        return Err(err(ErrorCode::SchemaUnsupported, format!("workspace store schema {version} is newer than supported {STORE_SCHEMA_VERSION}")));
    }
    if version < 1 { return Err(err(ErrorCode::SchemaUnsupported, format!("no migration from store schema {version}"))); }
    let workspace_id = validate_workspace_contract_metadata(conn)?;
    if version == 1 || version == 2 {
        open_recovery.migration_warnings.extend(unregistered_migration_warnings(root, conn)?);
        migrate_existing_store_to3(root, conn, open_recovery, version, preflight_features, restore_marker)?;
    } else if version == STORE_SCHEMA_VERSION {
        open_recovery.migration_warnings.extend(unregistered_migration_warnings(root, conn)?);
        let removed_before = open_recovery.removed_pending_temps;
        let recovered_before = open_recovery.recovered_saves;
        if let Err(error) = finalize_registered_backups(root, conn, open_recovery, true) {
            return Err(error_with_pending_recovery(
                error,
                open_recovery.removed_pending_temps.saturating_sub(removed_before),
                open_recovery.recovered_saves.saturating_sub(recovered_before),
                false,
            ));
        }
    } else if version != STORE_SCHEMA_VERSION {
        return Err(err(ErrorCode::SchemaUnsupported, format!("no migration from store schema {version}")));
    }
    conn.execute_batch(SCHEMA_V1_ADDITIONS).map_err(sql)?;
    let feature_meta_rows: i64 = conn.query_row(
        "SELECT count(*) FROM meta WHERE key IN ('store_features','store_features_backfilled')",
        [], |row| row.get(0),
    ).map_err(sql)?;
    if feature_meta_rows != 2 {
        return Err(integrity("store-v3 feature metadata is missing"));
    }
    if version == STORE_SCHEMA_VERSION && preflight_features.is_some() && !store_features_backfilled(conn)? {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
        merge_store_features(&tx, preflight_features.unwrap())?;
        let changed = tx.execute("UPDATE meta SET value='1' WHERE key='store_features_backfilled' AND value='0'", []).map_err(sql)?;
        if changed != 1 { return Err(integrity("preflighted store feature manifest was not committed")); }
        tx.commit().map_err(sql)?;
    }
    Ok(workspace_id)
}

fn migrate_existing_store_to3(
    root: &Path,
    conn: &mut Connection,
    open_recovery: &mut crate::RecoveryReport,
    source_version: i64,
    preflight_features: Option<&BTreeSet<String>>,
    restore_marker: Option<&BackupMarker>,
) -> DltResult<()> {
    let mut stage = reserve_store_backup_stage(root, new_id(), source_version)?;
    if let Err(error) = write_stage_manifest(&stage, "running", 0, 0) {
        return Err(error_with_stage_cleanup(error, root, &stage));
    }
    let mut recovered_saves = 0;
    let removed_pending_temps = match crate::files::recover_pending(conn, &mut recovered_saves) {
        Ok(removed) => removed,
        Err(error) => {
            open_recovery.recovered_saves += recovered_saves;
            let _ = write_stage_manifest(&stage, "interrupted", 0, recovered_saves);
            let error = error_with_stage_cleanup(error, root, &stage);
            return Err(error_with_pending_recovery(error, 0, recovered_saves, true));
        }
    };
    open_recovery.removed_pending_temps += removed_pending_temps;
    open_recovery.recovered_saves += recovered_saves;
    if let Err(error) = write_stage_manifest(&stage, "complete", removed_pending_temps, recovered_saves) {
        return Err(error_with_stage_cleanup(
            error_with_pending_recovery(error, removed_pending_temps, recovered_saves, false), root, &stage,
        ));
    }
    let backup = match backup_store(root, conn, &mut stage) {
        Ok(backup) => backup,
        Err(error) => {
            return Err(error_with_pending_recovery(error_with_stage_cleanup(error, root, &stage), removed_pending_temps, recovered_saves, false));
        }
    };
    if let Err(error) = migrate_store_to3(conn, &backup, preflight_features, restore_marker) {
        let version_after = store_version(conn);
        if matches!(&version_after, Ok(version) if *version == source_version) {
            let error = error_with_stage_cleanup(error, root, &stage);
            return Err(error_with_pending_recovery(error, removed_pending_temps, recovered_saves, false));
        }
        let message = match version_after {
            Ok(version) => format!("store migration failed after version became {version}; registered candidate was preserved for retry"),
            Err(check_error) => format!("store migration outcome is uncertain ({check_error:?}); candidate was preserved for retry"),
        };
        let mut error = error;
        error.message = format!("{}; {message}", error.message);
        return Err(error_with_pending_recovery(error, removed_pending_temps, recovered_saves, false));
    }
    if let Err(error) = finalize_registered_backups(root, conn, open_recovery, false) {
        return Err(error_with_pending_recovery(error, removed_pending_temps, recovered_saves, false));
    }
    Ok(())
}

const STORE1_STAGE_PREFIX: &str = ".store1-migration-";
const STORE2_STAGE_PREFIX: &str = ".store2-migration-";
const STORE1_STAGE_SUFFIX: &str = ".staging";
const STORE1_OWNER_FILE: &str = "owner.txt";
const STORE1_OWNER_MAGIC: &str = "designllm-store1-stage-v1";
const STORE2_OWNER_MAGIC: &str = "designllm-store2-stage-v1";
const RESTORE_STAGE_SUFFIX: &str = ".restore-staging";
const RESTORE_STAGE_OWNER_FILE: &str = "restore-owner.txt";
const RESTORE_STAGE_OWNER_MAGIC: &str = "designllm-restore-stage-v1";

#[derive(Debug, Clone)]
struct Store1BackupStage {
    backup_id: String,
    source_store_version: i64,
    created_at_ms: i64,
    staging_relative_path: String,
    candidate_relative_path: String,
    final_relative_path: String,
    staging_path: PathBuf,
    owner_path: PathBuf,
    candidate_path: PathBuf,
    partial_path: PathBuf,
    owns_staging: bool,
    owns_owner: bool,
    owns_partial: bool,
    owns_candidate: bool,
    owns_sidecars: bool,
}

#[derive(Debug, Clone)]
struct Store1Backup {
    stage: Store1BackupStage,
    blob_hashes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BackupMarker {
    backup_id: String,
    source_store_version: i64,
    restore_min_store_version: i64,
    original_application_id: i64,
    original_user_version: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct BackupRestoreStage {
    pub staging_root: PathBuf,
    pub destination_root: PathBuf,
    pub diagnostics: Vec<Diagnostic>,
    owner_path: PathBuf,
    restore_id: String,
    backup_id: String,
    source_store_version: i64,
}

#[derive(Debug, Clone)]
struct Store1StageManifest {
    source_store_version: i64,
    backup_id: String,
    created_at_ms: i64,
    recovery_state: String,
    removed_pending_temps: usize,
    recovered_saves: usize,
}

/// Reserve an isolated staging directory before recovery or backup work. `create_dir` is the
/// ownership boundary: a colliding directory is never inspected, overwritten, or removed.
#[cfg(test)]
fn reserve_store1_backup_stage(root: &Path, backup_id: String) -> DltResult<Store1BackupStage> {
    reserve_store_backup_stage(root, backup_id, 1)
}

fn stage_prefix(source_store_version: i64) -> DltResult<&'static str> {
    match source_store_version {
        1 => Ok(STORE1_STAGE_PREFIX),
        2 => Ok(STORE2_STAGE_PREFIX),
        other => Err(err(ErrorCode::SchemaUnsupported, format!("no schema-backup path for source store version {other}"))),
    }
}

fn reserve_store_backup_stage(root: &Path, backup_id: String, source_store_version: i64) -> DltResult<Store1BackupStage> {
    if !valid_backup_id(&backup_id) { return Err(integrity("generated schema-backup id is not path-safe")); }
    let prefix = stage_prefix(source_store_version)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| err(ErrorCode::StorageFailure, format!("cannot timestamp schema backup: {e}")))?
        .as_millis();
    let created_at_ms = i64::try_from(timestamp).map_err(|_| err(ErrorCode::StorageFailure, "backup timestamp overflow"))?;
    let backup_dir = root.join("backups");
    fs::create_dir_all(&backup_dir).map_err(crate::errors::io_store)?;
    let staging_relative_path = format!("backups/{prefix}{backup_id}{STORE1_STAGE_SUFFIX}");
    let candidate_relative_path = format!("{staging_relative_path}/candidate.sqlite3");
    let final_relative_path = format!("backups/store{source_store_version}-{created_at_ms}-{backup_id}.sqlite3");
    let staging_path = root.join(&staging_relative_path);
    fs::create_dir(&staging_path).map_err(crate::errors::io_store)?;
    let mut stage = Store1BackupStage {
        backup_id,
        source_store_version,
        created_at_ms,
        staging_relative_path,
        candidate_relative_path,
        final_relative_path,
        owner_path: staging_path.join(STORE1_OWNER_FILE),
        candidate_path: staging_path.join("candidate.sqlite3"),
        partial_path: staging_path.join("candidate.sqlite3.partial"),
        staging_path,
        owns_staging: true,
        owns_owner: false,
        owns_partial: false,
        owns_candidate: false,
        owns_sidecars: false,
    };
    let mut owner = match OpenOptions::new().write(true).create_new(true).open(&stage.owner_path) {
        Ok(owner) => owner,
        Err(error) => return Err(error_with_stage_cleanup(crate::errors::io_store(error), root, &stage)),
    };
    stage.owns_owner = true;
    if let Err(error) = owner.write_all(&stage_manifest_text(&stage, "prepared", 0, 0)).and_then(|_| owner.sync_all()) {
        return Err(error_with_stage_cleanup(crate::errors::io_store(error), root, &stage));
    }
    Ok(stage)
}

fn valid_backup_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn stage_manifest_text(stage: &Store1BackupStage, recovery_state: &str, removed_pending_temps: usize, recovered_saves: usize) -> Vec<u8> {
    if stage.source_store_version == 1 {
        format!(
            "{STORE1_OWNER_MAGIC}\nbackup_id={}\ncreated_at_ms={}\nrecovery_state={recovery_state}\nremoved_pending_temps={removed_pending_temps}\nrecovered_saves={recovered_saves}\n",
            stage.backup_id, stage.created_at_ms,
        ).into_bytes()
    } else {
        format!(
            "{STORE2_OWNER_MAGIC}\nsource_store_version={}\nbackup_id={}\ncreated_at_ms={}\nrecovery_state={recovery_state}\nremoved_pending_temps={removed_pending_temps}\nrecovered_saves={recovered_saves}\n",
            stage.source_store_version, stage.backup_id, stage.created_at_ms,
        ).into_bytes()
    }
}

fn parse_stage_manifest(path: &Path) -> Option<Store1StageManifest> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let magic = lines.next()?;
    let source_store_version = match magic {
        STORE1_OWNER_MAGIC => 1,
        STORE2_OWNER_MAGIC => lines.next()?.strip_prefix("source_store_version=")?.parse().ok()?,
        _ => return None,
    };
    if !matches!(source_store_version, 1 | 2) { return None; }
    let backup_id = lines.next()?.strip_prefix("backup_id=")?.to_owned();
    let created_at_ms = lines.next()?.strip_prefix("created_at_ms=")?.parse().ok()?;
    let recovery_state = lines.next()?.strip_prefix("recovery_state=")?.to_owned();
    let removed_pending_temps = lines.next()?.strip_prefix("removed_pending_temps=")?.parse().ok()?;
    let recovered_saves = lines.next()?.strip_prefix("recovered_saves=")?.parse().ok()?;
    if lines.next().is_some() || !valid_backup_id(&backup_id) { return None; }
    if !matches!(recovery_state.as_str(), "prepared" | "running" | "complete" | "interrupted") { return None; }
    Some(Store1StageManifest { source_store_version, backup_id, created_at_ms, recovery_state, removed_pending_temps, recovered_saves })
}

fn write_stage_manifest(stage: &Store1BackupStage, recovery_state: &str, removed_pending_temps: usize, recovered_saves: usize) -> DltResult<()> {
    let existing = parse_stage_manifest(&stage.owner_path).ok_or_else(|| integrity("schema-backup staging owner marker is malformed"))?;
    if existing.source_store_version != stage.source_store_version
        || existing.backup_id != stage.backup_id
        || existing.created_at_ms != stage.created_at_ms
    {
        return Err(integrity("schema-backup staging owner marker does not match its reserved directory"));
    }
    let mut owner = OpenOptions::new().write(true).truncate(true).open(&stage.owner_path).map_err(crate::errors::io_store)?;
    owner.write_all(&stage_manifest_text(stage, recovery_state, removed_pending_temps, recovered_saves)).map_err(crate::errors::io_store)?;
    owner.sync_all().map_err(crate::errors::io_store)
}

fn error_with_pending_recovery(mut error: DltError, removed_pending_temps: usize, recovered_saves: usize, outcome_unknown: bool) -> DltError {
    if removed_pending_temps == 0 && recovered_saves == 0 && !outcome_unknown { return error; }
    let (message, params) = if outcome_unknown {
        let mut params = std::collections::BTreeMap::new();
        params.insert("counts_known".to_owned(), Value::Bool(false));
        ("pending-save recovery may have partially run before this failure; exact counts are unknown and safe to retry".to_owned(), params)
    } else {
        let mut params = std::collections::BTreeMap::new();
        params.insert("counts_known".to_owned(), Value::Bool(true));
        params.insert("removed_pending_temps".to_owned(), Value::from(removed_pending_temps as u64));
        params.insert("recovered_saves".to_owned(), Value::from(recovered_saves as u64));
        (format!("pending-save recovery already ran before this failure; removed_pending_temps={removed_pending_temps}, recovered_saves={recovered_saves}; these file/database changes are persisted"), params)
    };
    error.message = format!("{}; {message}", error.message);
    error.diagnostics.push(Diagnostic {
        code: "core.pending_recovery_before_migration_failure".to_owned(),
        message,
        node_id: None,
        blocking: false,
        params,
    });
    error
}

fn error_with_stage_cleanup(mut error: DltError, root: &Path, stage: &Store1BackupStage) -> DltError {
    if let Err(cleanup) = cleanup_current_stage(stage) {
        error.message = format!("{}; unregistered staging output was preserved at {} because cleanup failed: {cleanup}", error.message, root.join(&stage.staging_relative_path).display());
    }
    error
}

/// Remove only paths this invocation created inside its atomically-created private directory.
fn cleanup_current_stage(stage: &Store1BackupStage) -> std::io::Result<()> {
    if stage.owns_partial { remove_if_present(&stage.partial_path)?; }
    if stage.owns_candidate { remove_if_present(&stage.candidate_path)?; }
    if stage.owns_sidecars {
        for suffix in ["-wal", "-shm", "-journal"] { remove_if_present(&backup_sidecar_path(&stage.partial_path, suffix))?; }
    }
    if stage.owns_staging {
        let mut names = HashSet::new();
        if stage.staging_path.exists() {
            for entry in fs::read_dir(&stage.staging_path)? { names.insert(entry?.file_name().to_string_lossy().into_owned()); }
        }
        let owned_names: HashSet<String> = [
            if stage.owns_owner { Some(STORE1_OWNER_FILE.to_owned()) } else { None },
            if stage.owns_partial { Some("candidate.sqlite3.partial".to_owned()) } else { None },
            if stage.owns_candidate { Some("candidate.sqlite3".to_owned()) } else { None },
            if stage.owns_sidecars { Some("candidate.sqlite3.partial-wal".to_owned()) } else { None },
            if stage.owns_sidecars { Some("candidate.sqlite3.partial-shm".to_owned()) } else { None },
            if stage.owns_sidecars { Some("candidate.sqlite3.partial-journal".to_owned()) } else { None },
        ].into_iter().flatten().collect();
        if !names.is_subset(&owned_names) {
            return Err(std::io::Error::new(std::io::ErrorKind::DirectoryNotEmpty, "staging directory contains unowned paths; owner marker retained"));
        }
        if stage.owns_owner { remove_if_present(&stage.owner_path)?; }
        remove_dir_if_present(&stage.staging_path)?;
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_dir_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Build and verify a candidate inside the private staging directory. The DB row and pins are
/// committed before this candidate is renamed to its public backup path.
#[cfg(test)]
fn backup_store1(root: &Path, conn: &Connection, stage: &mut Store1BackupStage) -> DltResult<Store1Backup> {
    backup_store(root, conn, stage)
}

fn backup_store(root: &Path, conn: &Connection, stage: &mut Store1BackupStage) -> DltResult<Store1Backup> {
    let owner = parse_stage_manifest(&stage.owner_path).ok_or_else(|| integrity("schema-backup staging owner marker is malformed"))?;
    if owner.source_store_version != stage.source_store_version
        || owner.backup_id != stage.backup_id
        || owner.created_at_ms != stage.created_at_ms
        || owner.recovery_state != "complete"
    {
        return Err(integrity("schema-backup staging owner marker is not ready for candidate creation"));
    }
    // Preserve both live references and blobs pinned by older retained schema backups. A v2
    // database backup contains those pin rows, so its own pin set must keep them alive too.
    let mut blob_hashes: Vec<String> = referenced_blobs(conn)?.into_iter().collect();
    blob_hashes.sort();
    let inherited_marker = read_backup_marker(conn)?;
    if inherited_marker.as_ref().is_some_and(|marker| marker.source_store_version != stage.source_store_version) {
        return Err(integrity("restore-stage backup marker source version differs from the new checkpoint"));
    }
    let current_application_id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0)).map_err(sql)?;
    let current_user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(sql)?;
    let original_application_id = inherited_marker.as_ref().map_or(current_application_id, |marker| marker.original_application_id);
    let original_user_version = inherited_marker.as_ref().map_or(current_user_version, |marker| marker.original_user_version);
    for hash in &blob_hashes {
        if !crate::blobs::is_hash(hash) { return Err(integrity("workspace references a malformed blob hash")); }
        let bytes = fs::read(root.join(BLOB_DIR).join(hash)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound { integrity(format!("referenced blob {hash} is missing before migration")) } else { crate::errors::io_store(e) }
        })?;
        if sha256(&bytes) != hash.as_str() { return Err(integrity(format!("referenced blob {hash} failed hash verification before migration"))); }
    }

    let reservation = OpenOptions::new().write(true).create_new(true).open(&stage.partial_path).map_err(crate::errors::io_store)?;
    stage.owns_partial = true;
    reservation.sync_all().map_err(crate::errors::io_store)?;
    drop(reservation);
    if backup_sidecars_exist(&stage.partial_path) {
        return Err(err(ErrorCode::StorageFailure, "pre-existing SQLite sidecar collided with the owned candidate path; sidecar preserved"));
    }
    stage.owns_sidecars = true;
    let mut destination = open_connection(&stage.partial_path)?;
    {
        let backup = rusqlite::backup::Backup::new(conn, &mut destination).map_err(sql)?;
        backup
            .run_to_completion(128, Duration::from_millis(10), None::<fn(rusqlite::backup::Progress)>)
            .map_err(sql)?;
    }
    write_backup_marker(&mut destination, stage, original_application_id, original_user_version)?;
    let journal_mode: String = destination.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0)).map_err(sql)?;
    if !journal_mode.eq_ignore_ascii_case("delete") {
        return Err(err(ErrorCode::StorageFailure, format!("backup journal_mode is {journal_mode}, expected delete")));
    }
    drop(destination);
    if backup_sidecars_exist(&stage.partial_path) {
        return Err(err(ErrorCode::StorageFailure, "SQLite backup sidecar remained after checkpoint"));
    }
    crate::fsx::sync_file(&stage.partial_path).map_err(crate::errors::io_store)?;
    let backup = open_connection_with_flags(&stage.partial_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let marker = read_backup_marker(&backup)?.ok_or_else(|| integrity("new schema backup is missing its old-core refusal marker"))?;
    if marker.backup_id != stage.backup_id || marker.source_store_version != stage.source_store_version {
        return Err(integrity("schema backup marker does not match the current migration"));
    }
    let integrity_result: String = backup.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(sql)?;
    if integrity_result != "ok" { return Err(integrity("schema-1 backup failed SQLite integrity_check")); }
    let version: String = backup.query_row(
        "SELECT value FROM meta WHERE key = 'store_schema_version'", [], |row| row.get(0),
    ).map_err(sql)?;
    if version.parse::<i64>().ok() != Some(stage.source_store_version) {
        return Err(integrity(format!("schema backup does not contain source store version {}", stage.source_store_version)));
    }
    drop(backup);
    if backup_sidecars_exist(&stage.partial_path) {
        return Err(err(ErrorCode::StorageFailure, "backup verification left a SQLite sidecar"));
    }
    crate::fsx::rename_durable(&stage.partial_path, &stage.candidate_path, false).map_err(crate::errors::io_store)?;
    stage.owns_partial = false;
    stage.owns_candidate = true;
    stage.owns_sidecars = false;
    Ok(Store1Backup { stage: stage.clone(), blob_hashes })
}

fn backup_sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn backup_sidecars_exist(database: &Path) -> bool {
    ["-wal", "-shm", "-journal"].iter().any(|suffix| match fs::symlink_metadata(backup_sidecar_path(database, suffix)) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    })
}

fn marker_meta_value(conn: &Connection, key: &str) -> DltResult<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", params![key], |row| row.get(0))
        .optional().map_err(sql)
}

fn read_backup_marker(conn: &Connection) -> DltResult<Option<BackupMarker>> {
    let application_id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0)).map_err(sql)?;
    let mut marker_values = Vec::new();
    for key in BACKUP_MARKER_KEYS { marker_values.push((*key, marker_meta_value(conn, key)?)); }
    let has_marker_metadata = marker_values.iter().any(|(_, value)| value.is_some());

    if matches!(application_id, APPLICATION_ID | STORE3_APPLICATION_ID) {
        if has_marker_metadata { return Err(integrity("backup marker metadata is present without its backup application_id")); }
        return Ok(None);
    }
    if application_id != BACKUP_APPLICATION_ID {
        return Err(err(ErrorCode::SchemaUnsupported, format!("database application_id {application_id:#x} is neither a workspace nor a protected backup")));
    }

    let value = |key: &str| -> DltResult<String> {
        marker_meta_value(conn, key)?.ok_or_else(|| integrity(format!("protected backup marker is missing {key}")))
    };
    let kind = value("dlt_backup_kind")?;
    if kind != "schema_migration" { return Err(integrity(format!("unknown protected backup kind {kind:?}"))); }
    let marker_version: i64 = value("dlt_backup_marker_version")?.parse().map_err(|_| integrity("backup marker version is malformed"))?;
    if marker_version != BACKUP_MARKER_VERSION { return Err(err(ErrorCode::SchemaUnsupported, format!("backup marker version {marker_version} is unsupported"))); }
    let backup_id = value("dlt_backup_id")?;
    if !valid_backup_id(&backup_id) { return Err(integrity("backup marker id is not path-safe")); }
    let source_store_version: i64 = value("dlt_backup_source_store_version")?.parse().map_err(|_| integrity("backup source store version is malformed"))?;
    if !matches!(source_store_version, 1 | 2) { return Err(integrity("backup marker has an invalid source store version")); }
    let restore_min_store_version: i64 = value("dlt_backup_restore_min_store_version")?.parse().map_err(|_| integrity("backup restore minimum is malformed"))?;
    if restore_min_store_version < BACKUP_RESTORE_MIN_STORE_VERSION { return Err(integrity("protected backup restore minimum is too old")); }
    let original_application_id: i64 = value("dlt_backup_original_application_id")?.parse().map_err(|_| integrity("backup original application_id is malformed"))?;
    if original_application_id != APPLICATION_ID { return Err(integrity("backup marker has an unexpected original application_id")); }
    let original_user_version: i64 = value("dlt_backup_original_user_version")?.parse().map_err(|_| integrity("backup original user_version is malformed"))?;
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(sql)?;
    if user_version != original_user_version { return Err(integrity("protected backup user_version differs from its marker")); }
    let stored_version = store_version(conn)?;
    if stored_version != source_store_version { return Err(integrity("protected backup marker source version differs from the SQLite store version")); }
    Ok(Some(BackupMarker { backup_id, source_store_version, restore_min_store_version, original_application_id, original_user_version }))
}

pub(crate) fn protected_backup_marker_matches(
    root: &Path,
    relative_path: &str,
    backup_id: &str,
    source_store_version: i64,
    restore_min_store_version: i64,
) -> DltResult<bool> {
    let path = resolve_registered_backup_file(root, relative_path)?;
    let backup = ReadOnlySqlite::open(&path, &std::env::temp_dir(), std::ffi::OsStr::new("backup-marker"))?;
    let Some(marker) = read_backup_marker(&backup.connection)? else { return Ok(false) };
    if marker.backup_id != backup_id
        || marker.source_store_version != source_store_version
        || marker.restore_min_store_version != restore_min_store_version
    {
        return Err(integrity("protected backup marker does not match its registered workspace metadata"));
    }
    Ok(true)
}

struct SqliteReadSnapshot {
    root: PathBuf,
    database_path: PathBuf,
    owned_files: Vec<PathBuf>,
    cleaned: bool,
}

impl SqliteReadSnapshot {
    fn create(source_database: &Path, destination_parent: &Path, destination_name: &std::ffi::OsStr) -> DltResult<Self> {
        let root = destination_parent.join(format!(".{}.restore-read-{}{}", destination_name.to_string_lossy(), new_id(), RESTORE_STAGE_SUFFIX));
        match fs::create_dir(&root) {
            Ok(()) => {}
            Err(error) => return Err(crate::errors::io_store(error)),
        }
        let database_path = root.join(DB_FILE);
        let owner_path = root.join("restore-read-owner.txt");
        let owned_files = vec![
            database_path.clone(),
            backup_sidecar_path(&database_path, "-wal"),
            backup_sidecar_path(&database_path, "-shm"),
            backup_sidecar_path(&database_path, "-journal"),
            owner_path.clone(),
        ];
        let snapshot = Self {
            root,
            owned_files,
            database_path,
            cleaned: false,
        };
        ensure_database_paths_supported(&snapshot.database_path)?;

        let mut owner = OpenOptions::new().write(true).create_new(true).open(&owner_path).map_err(crate::errors::io_store)?;
        owner.write_all(b"designllm-restore-read-snapshot-v1\nstate=copying\n").map_err(crate::errors::io_store)?;
        owner.sync_all().map_err(crate::errors::io_store)?;

        let sidecars = [
            (source_database.to_path_buf(), snapshot.database_path.clone()),
            (backup_sidecar_path(source_database, "-wal"), backup_sidecar_path(&snapshot.database_path, "-wal")),
            (backup_sidecar_path(source_database, "-journal"), backup_sidecar_path(&snapshot.database_path, "-journal")),
        ];
        let mut source_state = Vec::with_capacity(sidecars.len());
        for (source, staged) in &sidecars {
            let before = match fs::symlink_metadata(source) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        return Err(integrity("source SQLite database or journal sidecar is not a regular file"));
                    }
                    let hash = file_sha256(source)?;
                    let mut input = fs::File::open(source).map_err(crate::errors::io_store)?;
                    let mut output = match OpenOptions::new().write(true).create_new(true).open(staged) {
                        Ok(file) => file,
                        Err(error) => return Err(crate::errors::io_store(error)),
                    };
                    std::io::copy(&mut input, &mut output).map_err(crate::errors::io_store)?;
                    output.sync_all().map_err(crate::errors::io_store)?;
                    if file_sha256(staged)? != hash { return Err(integrity("source database snapshot copy differs from its input bytes")); }
                    Some(hash)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(crate::errors::io_store(error)),
            };
            source_state.push((source.clone(), before));
        }
        for (source, expected) in source_state {
            let after = match fs::symlink_metadata(&source) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        return Err(integrity("source SQLite database changed while being snapshotted"));
                    }
                    Some(file_sha256(&source)?)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(crate::errors::io_store(error)),
            };
            if after != expected { return Err(err(ErrorCode::SourceChanged, "source workspace changed while its read-only restore snapshot was being copied")); }
        }
        Ok(snapshot)
    }

    fn cleanup(&mut self) -> DltResult<()> {
        if self.cleaned { return Ok(()); }
        for path in &self.owned_files {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err(integrity(format!("restore read snapshot contains an unexpected non-file at {}", path.display())));
                }
                Ok(_) => fs::remove_file(path).map_err(crate::errors::io_store)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(crate::errors::io_store(error)),
            }
        }
        fs::remove_dir(&self.root).map_err(crate::errors::io_store)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for SqliteReadSnapshot {
    fn drop(&mut self) { let _ = self.cleanup(); }
}

struct ReadOnlySqlite {
    connection: Connection,
    direct_path: Option<PathBuf>,
    direct_sha256: Option<String>,
    _snapshot: Option<SqliteReadSnapshot>,
}

impl ReadOnlySqlite {
    fn open(path: &Path, scratch_parent: &Path, snapshot_name: &std::ffi::OsStr) -> DltResult<Self> {
        if !backup_sidecars_exist(path) && sqlite_header_journal_mode(path)? == (1, 1) {
            let before = file_sha256(path)?;
            let connection = open_connection_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            if backup_sidecars_exist(path) || file_sha256(path)? != before {
                return Err(err(ErrorCode::SourceChanged, "registered SQLite backup changed while it was opened"));
            }
            return Ok(Self {
                connection,
                direct_path: Some(path.to_path_buf()),
                direct_sha256: Some(before),
                _snapshot: None,
            });
        }
        let snapshot = SqliteReadSnapshot::create(path, scratch_parent, snapshot_name)?;
        let connection = open_connection(&snapshot.database_path)?;
        let integrity_result: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(sql)?;
        if integrity_result != "ok" { return Err(integrity("private SQLite read snapshot failed integrity_check")); }
        Ok(Self { connection, direct_path: None, direct_sha256: None, _snapshot: Some(snapshot) })
    }

    fn copy_to(&self, destination_path: &Path) -> DltResult<()> {
        ensure_database_paths_supported(destination_path)?;
        if let Some(source_path) = &self.direct_path {
            let before = file_sha256(source_path)?;
            if self.direct_sha256.as_deref() != Some(before.as_str()) {
                return Err(err(ErrorCode::SourceChanged, "registered SQLite backup changed after validation"));
            }
            let mut input = fs::File::open(source_path).map_err(crate::errors::io_store)?;
            let mut output = OpenOptions::new().write(true).create_new(true).open(destination_path).map_err(crate::errors::io_store)?;
            std::io::copy(&mut input, &mut output).map_err(crate::errors::io_store)?;
            output.sync_all().map_err(crate::errors::io_store)?;
            if file_sha256(source_path)? != before || file_sha256(destination_path)? != before {
                return Err(err(ErrorCode::SourceChanged, "registered backup changed while being copied"));
            }
            return Ok(());
        }

        let mut destination = open_connection(destination_path)?;
        {
            let backup = rusqlite::backup::Backup::new(&self.connection, &mut destination).map_err(sql)?;
            backup.run_to_completion(128, Duration::from_millis(10), None::<fn(rusqlite::backup::Progress)>).map_err(sql)?;
        }
        let journal_mode: String = destination.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0)).map_err(sql)?;
        if !journal_mode.eq_ignore_ascii_case("delete") {
            return Err(err(ErrorCode::StorageFailure, format!("restored checkpoint journal_mode is {journal_mode}, expected delete")));
        }
        drop(destination);
        if backup_sidecars_exist(destination_path) { return Err(integrity("normalized checkpoint retained a SQLite sidecar")); }
        crate::fsx::sync_file(destination_path).map_err(crate::errors::io_store)
    }
}

fn sqlite_header_journal_mode(path: &Path) -> DltResult<(u8, u8)> {
    let mut file = fs::File::open(path).map_err(crate::errors::io_store)?;
    let mut header = [0u8; 20];
    file.read_exact(&mut header).map_err(crate::errors::io_store)?;
    if &header[..16] != b"SQLite format 3\0" { return Err(integrity("registered backup is not a SQLite database")); }
    Ok((header[18], header[19]))
}

pub(crate) fn prepare_backup_restore_stage(
    source_root: &Path,
    backup_id: &str,
    destination_root: &Path,
    engines: &[std::sync::Arc<dyn EngineAdapter>],
    supported: &BTreeSet<String>,
) -> DltResult<BackupRestoreStage> {
    if !valid_backup_id(backup_id) { return Err(invalid("backup_id is not path-safe")); }
    let source_root = absolute_path(source_root)?;
    ensure_database_paths_supported(&source_root.join(DB_FILE))?;
    let source_root = fs::canonicalize(&source_root).map_err(crate::errors::io_store)?;
    let destination_root = absolute_path(destination_root)?;
    validate_workspace_path(&destination_root)?;
    match fs::symlink_metadata(&destination_root) {
        Ok(_) => return Err(err(ErrorCode::AlreadyExists, "restore destination already exists; no files were changed")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(crate::errors::io_store(error)),
    }
    let destination_name = destination_root.file_name().ok_or_else(|| invalid("restore destination must name a new folder"))?.to_os_string();
    let destination_parent = destination_root.parent().ok_or_else(|| invalid("restore destination must have an existing parent folder"))?;
    let destination_parent = fs::canonicalize(destination_parent).map_err(crate::errors::io_store)?;
    let destination_root = destination_parent.join(&destination_name);
    if destination_root.starts_with(&source_root) {
        return Err(invalid("restore destination must be outside the source workspace"));
    }
    match fs::symlink_metadata(&destination_root) {
        Ok(_) => return Err(err(ErrorCode::AlreadyExists, "restore destination already exists; no files were changed")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(crate::errors::io_store(error)),
    }
    let mut restore_diagnostics = previous_restore_staging_diagnostics(&destination_parent, &destination_name)?;

    let _source_lock = crate::acquire_restore_source_lock(&source_root)?;
    let source_snapshot = SqliteReadSnapshot::create(&source_root.join(DB_FILE), &destination_parent, &destination_name)?;
    let source_db = open_connection(&source_snapshot.database_path)?;
    let source_integrity: String = source_db.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(sql)?;
    if source_integrity != "ok" { return Err(integrity("source workspace database snapshot failed SQLite integrity_check")); }
    let source_app_id: i64 = source_db.query_row("PRAGMA application_id", [], |row| row.get(0)).map_err(sql)?;
    if !matches!(source_app_id, APPLICATION_ID | STORE3_APPLICATION_ID) {
        return Err(err(ErrorCode::SchemaUnsupported, "backup source is not a current DesignLLMTool workspace"));
    }
    if store_version(&source_db)? != STORE_SCHEMA_VERSION { return Err(err(ErrorCode::SchemaUnsupported, "registered checkpoint restore requires a current store-v3 source workspace")); }

    let (backup_relative, source_store_version, restore_min_store_version): (String, i64, i64) = source_db.query_row(
        "SELECT path,source_store_version,restore_min_store_version FROM schema_backups WHERE backup_id=?1 AND state='ready'",
        params![backup_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(sql)?.ok_or_else(|| err(ErrorCode::NotFound, "registered ready schema backup was not found"))?;
    if !safe_registered_backup_path(&backup_relative) { return Err(integrity("registered backup path is not a safe workspace-relative path")); }
    if !matches!(source_store_version, 1 | 2) || restore_min_store_version > STORE_SCHEMA_VERSION {
        return Err(err(ErrorCode::SchemaUnsupported, format!("backup requires store version {restore_min_store_version}, this build supports {STORE_SCHEMA_VERSION}")));
    }
    let backup_path = resolve_registered_backup_file(&source_root, &backup_relative)?;
    if !backup_path.is_file() { return Err(err(ErrorCode::NotFound, "registered backup file is missing")); }
    let backup = ReadOnlySqlite::open(&backup_path, &destination_parent, &destination_name)?;
    let backup_conn = &backup.connection;
    let integrity_result: String = backup_conn.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(sql)?;
    if integrity_result != "ok" { return Err(integrity("registered backup failed SQLite integrity_check")); }
    if store_version(backup_conn)? != source_store_version { return Err(integrity("registered backup source version differs from workspace metadata")); }
    let marker = read_backup_marker(backup_conn)?;
    if let Some(marker) = &marker {
        if marker.backup_id != backup_id
            || marker.source_store_version != source_store_version
            || marker.restore_min_store_version != restore_min_store_version
        {
            return Err(integrity("registered backup marker differs from workspace metadata"));
        }
    }
    if db_has_table(backup_conn, "pending_saves")? {
        let pending: i64 = backup_conn.query_row("SELECT count(*) FROM pending_saves", [], |row| row.get(0)).map_err(sql)?;
        if pending > 0 {
            let mut error = err(ErrorCode::StorageFailure, format!("registered backup contains {pending} unfinished saves with external paths; restore was refused without touching those files"));
            error.diagnostics.push(Diagnostic {
                code: "core.restore_pending_saves_unsupported".into(),
                message: "이 백업에는 원래 폴더 밖의 파일을 가리킬 수 있는 미완료 저장이 있어 새 폴더 복원을 거부했습니다. 원본과 외부 파일은 변경되지 않았습니다.".into(),
                node_id: None,
                blocking: true,
                params: BTreeMap::from([(String::from("pending_count"), Value::from(pending))]),
            });
            return Err(error);
        }
    }

    let referenced: BTreeSet<String> = referenced_blobs(backup_conn)?.into_iter().collect();
    let mut pin_statement = source_db.prepare("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1").map_err(sql)?;
    let pins: BTreeSet<String> = pin_statement.query_map(params![backup_id], |row| row.get::<_, String>(0)).map_err(sql)?
        .collect::<rusqlite::Result<_>>().map_err(sql)?;
    if referenced != pins { return Err(integrity("registered backup snapshot references differ from their committed blob pins")); }
    for hash in &pins {
        if !crate::blobs::is_hash(hash) { return Err(integrity("registered backup contains a malformed blob hash")); }
        let blob = source_root.join(BLOB_DIR).join(hash);
        if file_sha256(&blob)? != *hash { return Err(integrity(format!("registered backup blob {hash} is missing or failed SHA-256 verification"))); }
    }
    preflight_snapshot_features(&source_root, backup_conn, engines, supported)?;
    let backup_chain = registered_backup_files_in_checkpoint(backup_conn, &source_root, &destination_parent, &destination_name, source_store_version, &pins)?;
    for nested in &backup_chain {
        if !nested.marked_for_old_core_refusal {
            let message = format!("복원된 작업공간의 기존 백업 {}에는 구 버전 Core의 직접 열기를 막는 표식이 없습니다. 이 백업도 현재 또는 이후 버전의 앱에서 검증된 새 폴더 복원 경로로만 사용하세요.", nested.relative_path);
            restore_diagnostics.push(Diagnostic {
                code: "core.restore_legacy_backup_unmarked".into(),
                message,
                node_id: None,
                blocking: false,
                params: BTreeMap::from([
                    (String::from("backup_path"), Value::String(nested.relative_path.clone())),
                    (String::from("restore_min_store_version"), Value::from(BACKUP_RESTORE_MIN_STORE_VERSION)),
                    (String::from("backup_chain"), Value::Bool(true)),
                ]),
            });
        }
    }
    drop(pin_statement);
    drop(source_db);
    let mut source_snapshot = source_snapshot;
    source_snapshot.cleanup()?;

    let restore_id = new_id();
    let staging_root = destination_parent.join(format!(".{}.restore-{}{}", destination_name.to_string_lossy(), restore_id, RESTORE_STAGE_SUFFIX));
    validate_workspace_path(&staging_root)?;
    ensure_migration_paths_supported(&staging_root, source_store_version)?;
    match fs::symlink_metadata(&staging_root) {
        Ok(_) => return Err(err(ErrorCode::AlreadyExists, "restore staging path already exists; it was left untouched")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(crate::errors::io_store(error)),
    }
    fs::create_dir(&staging_root).map_err(crate::errors::io_store)?;
    let owner_path = staging_root.join(RESTORE_STAGE_OWNER_FILE);
    let stage = BackupRestoreStage {
        staging_root,
        destination_root,
        diagnostics: {
            if marker.is_none() {
                restore_diagnostics.push(Diagnostic {
                    code: "core.restore_legacy_backup_unmarked".into(),
                    message: format!("This registered legacy backup has no marker that blocks older cores; it was validated and is being restored by the current core into a new folder. Do not open the raw backup with an older app: {backup_relative}"),
                    node_id: None,
                    blocking: false,
                    params: BTreeMap::from([
                        (String::from("backup_path"), Value::String(backup_relative.clone())),
                        (String::from("restore_min_store_version"), Value::from(restore_min_store_version)),
                    ]),
                });
            }
            restore_diagnostics
        },
        owner_path,
        restore_id,
        backup_id: backup_id.to_owned(),
        source_store_version,
    };

    let prepared = (|| {
        let mut owner = OpenOptions::new().write(true).create_new(true).open(&stage.owner_path).map_err(crate::errors::io_store)?;
        owner.write_all(&restore_stage_owner_text(&stage.restore_id, backup_id, source_store_version, "copying")).map_err(crate::errors::io_store)?;
        owner.sync_all().map_err(crate::errors::io_store)?;

        let staged_db = stage.staging_root.join(DB_FILE);
        backup.copy_to(&staged_db)?;

        let staged_blobs = stage.staging_root.join(BLOB_DIR);
        fs::create_dir(&staged_blobs).map_err(crate::errors::io_store)?;
        for hash in &pins {
            let mut input = fs::File::open(source_root.join(BLOB_DIR).join(hash)).map_err(crate::errors::io_store)?;
            let staged_blob = staged_blobs.join(hash);
            let mut output = OpenOptions::new().write(true).create_new(true).open(&staged_blob).map_err(crate::errors::io_store)?;
            std::io::copy(&mut input, &mut output).map_err(crate::errors::io_store)?;
            output.sync_all().map_err(crate::errors::io_store)?;
            if file_sha256(&staged_blob)? != *hash { return Err(integrity(format!("staged restore blob {hash} failed SHA-256 verification"))); }
        }

        for nested in &backup_chain {
            let staged_backup = stage.staging_root.join(&nested.relative_path);
            ensure_database_paths_supported(&staged_backup)?;
            let parent = staged_backup.parent().ok_or_else(|| integrity("registered checkpoint path has no parent"))?;
            fs::create_dir_all(parent).map_err(crate::errors::io_store)?;
            nested.source.copy_to(&staged_backup)?;
            let staged_nested = open_connection_with_flags(&staged_backup, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let staged_integrity: String = staged_nested.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(sql)?;
            if staged_integrity != "ok" || store_version(&staged_nested)? != 1 {
                return Err(integrity(format!("staged registered checkpoint {} failed validation", nested.relative_path)));
            }
            if let Some(marker) = read_backup_marker(&staged_nested)? {
                if marker.backup_id != nested.backup_id || marker.source_store_version != 1 || marker.restore_min_store_version != BACKUP_RESTORE_MIN_STORE_VERSION {
                    return Err(integrity("staged nested checkpoint marker differs from its backup metadata"));
                }
            }
            let staged_pins: BTreeSet<String> = referenced_blobs(&staged_nested)?.into_iter().collect();
            if !staged_pins.is_subset(&pins) {
                return Err(integrity("staged nested checkpoint references blobs not copied into the restored workspace"));
            }
        }

        let staged_conn = open_connection_with_flags(&staged_db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let staged_marker = read_backup_marker(&staged_conn)?;
        if staged_marker != marker || store_version(&staged_conn)? != source_store_version {
            return Err(integrity("staged restore database metadata differs from the registered backup"));
        }
        let mut owner = OpenOptions::new().write(true).truncate(true).open(&stage.owner_path).map_err(crate::errors::io_store)?;
        owner.write_all(&restore_stage_owner_text(&stage.restore_id, backup_id, source_store_version, "ready")).map_err(crate::errors::io_store)?;
        owner.sync_all().map_err(crate::errors::io_store)?;
        Ok(())
    })();
    drop(backup);
    drop(backup_chain);
    if let Err(error) = prepared { return Err(restore_stage_error(error, &stage)); }
    Ok(stage)
}

struct RestorableBackupFile {
    backup_id: String,
    relative_path: String,
    marked_for_old_core_refusal: bool,
    source: ReadOnlySqlite,
}

fn resolve_registered_backup_file(root: &Path, relative: &str) -> DltResult<PathBuf> {
    if !safe_registered_backup_path(relative) {
        return Err(integrity("registered backup path is not a safe workspace-relative path"));
    }
    let canonical_root = fs::canonicalize(root).map_err(crate::errors::io_store)?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(crate::errors::io_store)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(integrity("registered backup path is not a regular file"));
    }
    let canonical_path = fs::canonicalize(&path).map_err(crate::errors::io_store)?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(integrity("registered backup path resolves outside its workspace"));
    }
    Ok(path)
}

fn registered_backup_files_in_checkpoint(
    checkpoint: &Connection,
    source_root: &Path,
    scratch_parent: &Path,
    snapshot_name: &std::ffi::OsStr,
    checkpoint_version: i64,
    checkpoint_pins: &BTreeSet<String>,
) -> DltResult<Vec<RestorableBackupFile>> {
    if checkpoint_version == 1 {
        if db_has_table(checkpoint, "schema_backups")? {
            return Err(integrity("store-v1 checkpoint unexpectedly contains registered backup metadata"));
        }
        return Ok(Vec::new());
    }
    if checkpoint_version != 2 { return Err(err(ErrorCode::SchemaUnsupported, "checkpoint backup ancestry is unsupported for this store version")); }
    if !db_has_table(checkpoint, "schema_backups")? || !db_has_table(checkpoint, "schema_backup_blobs")? {
        return Err(integrity("store-v2 checkpoint is missing its registered backup metadata"));
    }

    let columns = {
        let mut statement = checkpoint.prepare("PRAGMA table_info(schema_backups)").map_err(sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1)).map_err(sql)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?
    };
    if columns != ["backup_id", "path", "candidate_path", "staging_path", "state", "source_store_version", "created_at_ms"] {
        return Err(err(ErrorCode::SchemaUnsupported, "store-v2 checkpoint backup metadata has an unsupported shape"));
    }
    let records = {
        let mut statement = checkpoint.prepare(
            "SELECT backup_id,path,candidate_path,staging_path,state,source_store_version,created_at_ms FROM schema_backups ORDER BY created_at_ms,backup_id",
        ).map_err(sql)?;
        let rows = statement.query_map([], |row| Ok(RegisteredStore1Backup {
            backup_id: row.get(0)?,
            final_relative_path: row.get(1)?,
            candidate_relative_path: row.get(2)?,
            staging_relative_path: row.get(3)?,
            state: row.get(4)?,
            source_store_version: row.get(5)?,
            restore_min_store_version: BACKUP_RESTORE_MIN_STORE_VERSION,
            created_at_ms: row.get(6)?,
        })).map_err(sql)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?
    };
    let mut output = Vec::with_capacity(records.len());
    for record in records {
        if record.source_store_version != 1 || record.state != "ready" {
            return Err(err(ErrorCode::StorageFailure, format!("registered store-v2 checkpoint {} is not a ready store-v1 backup", record.backup_id)));
        }
        validate_registered_backup_paths(&record)?;
        let path = resolve_registered_backup_file(source_root, &record.final_relative_path)
            .map_err(|error| if error.code == ErrorCode::NotFound { err(ErrorCode::NotFound, format!("registered checkpoint {} is missing", record.backup_id)) } else { error })?;
        let nested = ReadOnlySqlite::open(&path, scratch_parent, snapshot_name)?;
        let nested_conn = &nested.connection;
        let integrity_result: String = nested_conn.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(sql)?;
        if integrity_result != "ok" { return Err(integrity(format!("registered checkpoint {} failed SQLite integrity_check", record.backup_id))); }
        if store_version(nested_conn)? != 1 { return Err(integrity(format!("registered checkpoint {} does not contain store version 1", record.backup_id))); }
        let nested_marker = read_backup_marker(nested_conn)?;
        match &nested_marker {
            Some(marker) if marker.backup_id == record.backup_id
                && marker.source_store_version == 1
                && marker.restore_min_store_version == BACKUP_RESTORE_MIN_STORE_VERSION => {}
            Some(_) => return Err(integrity("registered nested checkpoint marker differs from its backup metadata")),
            None => {} // Existing pre-marker checkpoint bytes are copied without rewriting them.
        }
        let nested_pins: BTreeSet<String> = referenced_blobs(nested_conn)?.into_iter().collect();
        if !nested_pins.is_subset(checkpoint_pins) {
            return Err(integrity(format!("registered checkpoint {} references blobs not pinned by the source checkpoint", record.backup_id)));
        }
        output.push(RestorableBackupFile {
            backup_id: record.backup_id,
            relative_path: record.final_relative_path,
            marked_for_old_core_refusal: nested_marker.is_some(),
            source: nested,
        });
    }
    Ok(output)
}

pub(crate) fn publish_backup_restore_stage(stage: &BackupRestoreStage) -> DltResult<()> {
    match fs::symlink_metadata(&stage.destination_root) {
        Ok(_) => return Err(err(ErrorCode::AlreadyExists, "restore destination appeared during staging; it was preserved")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(crate::errors::io_store(error)),
    }
    crate::fsx::rename_durable(&stage.staging_root, &stage.destination_root, false).map_err(crate::errors::io_store)
}

pub(crate) fn remove_published_restore_owner(stage: &BackupRestoreStage) -> DltResult<()> {
    let path = stage.destination_root.join(RESTORE_STAGE_OWNER_FILE);
    let text = fs::read_to_string(&path).map_err(crate::errors::io_store)?;
    if text != String::from_utf8_lossy(&restore_stage_owner_text(&stage.restore_id, &stage.backup_id, stage.source_store_version, "ready_to_publish")) {
        return Err(integrity("published restore owner marker does not match this restore operation"));
    }
    fs::remove_file(path).map_err(crate::errors::io_store)
}

pub(crate) fn mark_backup_restore_stage_ready(stage: &BackupRestoreStage) -> DltResult<()> {
    write_restore_stage_owner(stage, "ready_to_publish")
}

fn file_sha256(path: &Path) -> DltResult<String> {
    let mut file = fs::File::open(path).map_err(crate::errors::io_store)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(crate::errors::io_store)?;
        if read == 0 { break; }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn safe_registered_backup_path(path: &str) -> bool {
    let mut components = Path::new(path).components();
    matches!(components.next(), Some(std::path::Component::Normal(first)) if first == "backups")
        && components.all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn previous_restore_staging_diagnostics(parent: &Path, destination_name: &std::ffi::OsStr) -> DltResult<Vec<Diagnostic>> {
    let prefixes = [
        format!(".{}.restore-", destination_name.to_string_lossy()),
        format!(".{}.restore-read-", destination_name.to_string_lossy()),
    ];
    let entries = fs::read_dir(parent).map_err(crate::errors::io_store)?;
    let mut diagnostics = Vec::new();
    for entry in entries {
        let entry = entry.map_err(crate::errors::io_store)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(RESTORE_STAGE_SUFFIX) || !prefixes.iter().any(|prefix| name.starts_with(prefix)) {
            continue;
        }
        if !entry.file_type().map_err(crate::errors::io_store)?.is_dir() { continue; }
        let path = entry.path();
        let restore_owner = path.join(RESTORE_STAGE_OWNER_FILE);
        let read_owner = path.join("restore-read-owner.txt");
        let owner_text = if restore_owner.is_file() {
            fs::read_to_string(restore_owner).ok()
        } else if read_owner.is_file() {
            fs::read_to_string(read_owner).ok()
        } else {
            None
        };
        let Some(owner_text) = owner_text else { continue };
        if !owner_text.starts_with(RESTORE_STAGE_OWNER_MAGIC) && !owner_text.starts_with("designllm-restore-read-snapshot-v1") {
            continue;
        }
        let display = path.display().to_string();
        diagnostics.push(Diagnostic {
            code: "core.restore_backup_incomplete_left".into(),
            message: format!("A previous restore staging folder remains at {display}; it was preserved while this restore used a new staging folder."),
            node_id: None,
            blocking: false,
            params: BTreeMap::from([(String::from("staging_path"), Value::String(display))]),
        });
    }
    Ok(diagnostics)
}

fn restore_stage_owner_text(restore_id: &str, backup_id: &str, source_store_version: i64, state: &str) -> Vec<u8> {
    format!(
        "{RESTORE_STAGE_OWNER_MAGIC}\nrestore_id={restore_id}\nbackup_id={backup_id}\nsource_store_version={source_store_version}\nstate={state}\n"
    ).into_bytes()
}

fn write_restore_stage_owner(stage: &BackupRestoreStage, state: &str) -> DltResult<()> {
    let mut file = OpenOptions::new().write(true).truncate(true).open(&stage.owner_path).map_err(crate::errors::io_store)?;
    file.write_all(&restore_stage_owner_text(&stage.restore_id, &stage.backup_id, stage.source_store_version, state)).map_err(crate::errors::io_store)?;
    file.sync_all().map_err(crate::errors::io_store)
}

pub(crate) fn restore_stage_error(mut error: DltError, stage: &BackupRestoreStage) -> DltError {
    let message = format!("an incomplete restore staging folder was preserved at {}", stage.staging_root.display());
    error.message = format!("{}; {message}", error.message);
    error.diagnostics.push(Diagnostic {
        code: "core.restore_backup_incomplete_left".into(),
        message,
        node_id: None,
        blocking: false,
        params: BTreeMap::from([(String::from("staging_path"), Value::String(stage.staging_root.display().to_string()))]),
    });
    error.diagnostics.extend(stage.diagnostics.clone());
    error
}

fn write_backup_marker(
    destination: &mut Connection,
    stage: &Store1BackupStage,
    original_application_id: i64,
    original_user_version: i64,
) -> DltResult<()> {
    if original_application_id != APPLICATION_ID { return Err(integrity("source workspace application_id changed before checkpoint creation")); }
    let inherited_marker = read_backup_marker(destination)?;
    if let Some(marker) = &inherited_marker {
        if marker.original_application_id != original_application_id || marker.original_user_version != original_user_version {
            return Err(integrity("nested checkpoint marker changed the original SQLite header values"));
        }
    } else {
        let application_id: i64 = destination.query_row("PRAGMA application_id", [], |row| row.get(0)).map_err(sql)?;
        if application_id != original_application_id { return Err(integrity("unmarked checkpoint source has an unexpected application_id")); }
        for key in BACKUP_MARKER_KEYS {
            if marker_meta_value(destination, key)?.is_some() {
                return Err(integrity(format!("source snapshot already uses reserved backup marker key {key}")));
            }
        }
    }
    // The application_id is the early refusal gate for older cores. If a crash leaves the
    // checkpoint partially marked inside staging, it still cannot be opened as a workspace.
    destination.execute_batch(&format!(
        "PRAGMA application_id={BACKUP_APPLICATION_ID}; PRAGMA user_version={original_user_version};"
    )).map_err(sql)?;
    let tx = destination.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
    if inherited_marker.is_some() {
        for key in BACKUP_MARKER_KEYS {
            let removed = tx.execute("DELETE FROM meta WHERE key=?1", params![key]).map_err(sql)?;
            if removed != 1 { return Err(integrity(format!("inherited backup marker is missing {key}"))); }
        }
    }
    for (key, value) in [
        ("dlt_backup_kind", "schema_migration".to_owned()),
        ("dlt_backup_marker_version", BACKUP_MARKER_VERSION.to_string()),
        ("dlt_backup_id", stage.backup_id.clone()),
        ("dlt_backup_source_store_version", stage.source_store_version.to_string()),
        ("dlt_backup_restore_min_store_version", BACKUP_RESTORE_MIN_STORE_VERSION.to_string()),
        ("dlt_backup_original_application_id", original_application_id.to_string()),
        ("dlt_backup_original_user_version", original_user_version.to_string()),
    ] {
        tx.execute("INSERT INTO meta(key,value) VALUES (?1,?2)", params![key, value]).map_err(sql)?;
    }
    tx.commit().map_err(sql)?;
    let marker = read_backup_marker(destination)?.ok_or_else(|| integrity("new checkpoint did not retain its protected backup marker"))?;
    if marker.backup_id != stage.backup_id || marker.source_store_version != stage.source_store_version {
        return Err(integrity("new checkpoint marker differs from its reserved stage"));
    }
    Ok(())
}

/// Register the candidate, pins and schema version in one transaction before publishing the file.
#[cfg(test)]
fn migrate_store1_to2(conn: &mut Connection, backup: &Store1Backup) -> DltResult<()> {
    let stage = &backup.stage;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
    tx.execute_batch("ALTER TABLE undo_log ADD COLUMN group_id TEXT NULL;").map_err(sql)?;
    tx.execute_batch(SCHEMA_V2_ADDITIONS).map_err(sql)?;
    tx.execute(
        "INSERT INTO schema_backups(backup_id, path, candidate_path, staging_path, state, source_store_version, created_at_ms) VALUES (?1, ?2, ?3, ?4, 'publishing', 1, ?5)",
        params![stage.backup_id, stage.final_relative_path, stage.candidate_relative_path, stage.staging_relative_path, stage.created_at_ms],
    ).map_err(sql)?;
    for hash in &backup.blob_hashes {
        tx.execute("INSERT INTO schema_backup_blobs(backup_id, sha256) VALUES (?1, ?2)", params![stage.backup_id, hash]).map_err(sql)?;
    }
    let updated = tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'store_schema_version' AND value = '1'",
        params!["2"],
    ).map_err(sql)?;
    if updated != 1 { return Err(integrity("schema-1 store version changed during migration")); }
    tx.commit().map_err(sql)
}

fn rebuild_schema_backup_tables_for_v3(tx: &rusqlite::Transaction<'_>) -> DltResult<()> {
    tx.execute_batch(
        "CREATE TABLE schema_backups_v3 (
             backup_id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
             candidate_path TEXT NOT NULL UNIQUE, staging_path TEXT NOT NULL UNIQUE,
             state TEXT NOT NULL CHECK (state IN ('publishing', 'ready')),
             source_store_version INTEGER NOT NULL CHECK (source_store_version IN (1, 2)),
             restore_min_store_version INTEGER NOT NULL CHECK (restore_min_store_version >= 3),
             created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
         ) STRICT;
         CREATE TABLE schema_backup_blobs_v3 (
             backup_id TEXT NOT NULL REFERENCES schema_backups_v3(backup_id), sha256 TEXT NOT NULL,
             PRIMARY KEY (backup_id, sha256)
         ) STRICT;
         INSERT INTO schema_backups_v3(backup_id,path,candidate_path,staging_path,state,source_store_version,restore_min_store_version,created_at_ms)
             SELECT backup_id,path,candidate_path,staging_path,state,source_store_version,3,created_at_ms FROM schema_backups;
         INSERT INTO schema_backup_blobs_v3(backup_id,sha256)
             SELECT backup_id,sha256 FROM schema_backup_blobs;
         DROP TABLE schema_backup_blobs;
         DROP TABLE schema_backups;
         ALTER TABLE schema_backups_v3 RENAME TO schema_backups;
         ALTER TABLE schema_backup_blobs_v3 RENAME TO schema_backup_blobs;
         CREATE INDEX schema_backup_blobs_by_sha256 ON schema_backup_blobs(sha256);",
    ).map_err(sql)?;
    let mut statement = tx.prepare("PRAGMA foreign_key_check").map_err(sql)?;
    let mut rows = statement.query([]).map_err(sql)?;
    if rows.next().map_err(sql)?.is_some() {
        return Err(integrity("store-v3 schema-backup table rebuild introduced a foreign-key violation"));
    }
    Ok(())
}

fn migrate_store_to3(
    conn: &mut Connection,
    backup: &Store1Backup,
    preflight_features: Option<&BTreeSet<String>>,
    restore_marker: Option<&BackupMarker>,
) -> DltResult<()> {
    let source_version = backup.stage.source_store_version;
    if source_version == 2 {
        // SQLite cannot alter the source-version CHECK in place. Rebuild the backup metadata
        // tables while foreign keys are temporarily disabled, then verify every relation before
        // committing. Existing backup records and pins are copied, never regenerated.
        conn.execute_batch("PRAGMA foreign_keys=OFF;").map_err(sql)?;
    }
    let result = (|| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
        match source_version {
            1 => {
                tx.execute_batch("ALTER TABLE undo_log ADD COLUMN group_id TEXT NULL;").map_err(sql)?;
                tx.execute_batch(SCHEMA_V3_BACKUP_ADDITIONS).map_err(sql)?;
            }
            2 => rebuild_schema_backup_tables_for_v3(&tx)?,
            other => return Err(err(ErrorCode::SchemaUnsupported, format!("no migration from store schema {other}"))),
        }
        tx.execute_batch(SCHEMA_V3_HISTORY_ADDITIONS).map_err(sql)?;
        let features = preflight_features.cloned().unwrap_or_default();
        let backfill_state = if preflight_features.is_some() { "1" } else { "0" };
        tx.execute(
            "INSERT INTO meta(key,value) VALUES ('store_features',?1), ('store_features_backfilled',?2)",
            params![to_json(&features)?, backfill_state],
        ).map_err(sql)?;
        let stage = &backup.stage;
        tx.execute(
            "INSERT INTO schema_backups(backup_id,path,candidate_path,staging_path,state,source_store_version,restore_min_store_version,created_at_ms) VALUES (?1,?2,?3,?4,'publishing',?5,3,?6)",
            params![stage.backup_id, stage.final_relative_path, stage.candidate_relative_path, stage.staging_relative_path, source_version, stage.created_at_ms],
        ).map_err(sql)?;
        for hash in &backup.blob_hashes {
            tx.execute("INSERT INTO schema_backup_blobs(backup_id,sha256) VALUES (?1,?2)", params![stage.backup_id, hash]).map_err(sql)?;
        }
        // Store schema and application identity move together. A rollback leaves the prior reader id intact.
        tx.execute_batch(&format!("PRAGMA application_id={STORE3_APPLICATION_ID};")).map_err(sql)?;
        let updated = tx.execute(
            "UPDATE meta SET value=?1 WHERE key='store_schema_version' AND value=?2",
            params![STORE_SCHEMA_VERSION.to_string(), source_version.to_string()],
        ).map_err(sql)?;
        if updated != 1 { return Err(integrity(format!("store schema version changed during {source_version}→3 migration"))); }
        if let Some(marker) = restore_marker {
            if marker.source_store_version != source_version || marker.original_application_id != APPLICATION_ID {
                return Err(integrity("backup restore marker does not match the schema migration source"));
            }
            for key in BACKUP_MARKER_KEYS {
                let changed = tx.execute("DELETE FROM meta WHERE key=?1", params![key]).map_err(sql)?;
                if changed != 1 { return Err(integrity(format!("backup restore marker is missing {key}"))); }
            }
            tx.execute_batch(&format!("PRAGMA user_version={};", marker.original_user_version)).map_err(sql)?;
        }
        {
            let mut statement = tx.prepare("PRAGMA foreign_key_check").map_err(sql)?;
            let mut rows = statement.query([]).map_err(sql)?;
            if rows.next().map_err(sql)?.is_some() {
                return Err(integrity("store-v3 migration produced a foreign-key violation"));
            }
        }
        tx.commit().map_err(sql)
    })();
    if source_version == 2 {
        let restore_foreign_keys = conn.execute_batch("PRAGMA foreign_keys=ON;").map_err(sql);
        if result.is_ok() { restore_foreign_keys?; }
    }
    result
}

#[derive(Debug)]
struct RegisteredStore1Backup {
    backup_id: String,
    source_store_version: i64,
    restore_min_store_version: i64,
    final_relative_path: String,
    candidate_relative_path: String,
    staging_relative_path: String,
    state: String,
    created_at_ms: i64,
}

fn store_version(conn: &Connection) -> DltResult<i64> {
    let value: String = conn.query_row("SELECT value FROM meta WHERE key='store_schema_version'", [], |row| row.get(0)).map_err(sql)?;
    value.parse().map_err(|_| integrity("store_schema_version is not an integer"))
}

fn db_has_table(conn: &Connection, name: &str) -> DltResult<bool> {
    conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1", params![name], |row| row.get::<_, i64>(0))
        .map(|count| count > 0).map_err(sql)
}

fn unregistered_migration_warnings(root: &Path, conn: &Connection) -> DltResult<Vec<String>> {
    let backup_dir = root.join("backups");
    let entries = match fs::read_dir(&backup_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(crate::errors::io_store(error)),
    };
    let registered: HashSet<String> = if db_has_table(conn, "schema_backups")? {
        let mut statement = conn.prepare("SELECT staging_path FROM schema_backups").map_err(sql)?;
        let paths = statement.query_map([], |row| row.get::<_, String>(0)).map_err(sql)?;
        paths.collect::<rusqlite::Result<HashSet<_>>>().map_err(sql)?
    } else {
        HashSet::new()
    };
    let mut warnings = Vec::new();
    for entry in entries {
        let entry = entry.map_err(crate::errors::io_store)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_stage = [STORE1_STAGE_PREFIX, STORE2_STAGE_PREFIX].iter().any(|prefix| name.starts_with(prefix));
        if !is_stage || !name.ends_with(STORE1_STAGE_SUFFIX) { continue; }
        let relative = format!("backups/{name}");
        if registered.contains(&relative) { continue; }
        let owner_path = entry.path().join(STORE1_OWNER_FILE);
        let detail = match parse_stage_manifest(&owner_path) {
            Some(manifest) if stage_prefix(manifest.source_store_version).is_ok_and(|prefix| name == format!("{prefix}{}{STORE1_STAGE_SUFFIX}", manifest.backup_id)) => {
                match manifest.recovery_state.as_str() {
                    "complete" => format!("completed recovery counts: removed_pending_temps={}, recovered_saves={}", manifest.removed_pending_temps, manifest.recovered_saves),
                    "prepared" => "pending-save recovery had not started; zero counts are reliable".to_owned(),
                    state => format!("recovery state={state}; recovery may be partial and its exact counts are unknown (stored numeric fields are untrusted)"),
                }
            }
            Some(_) => "owner marker does not match the staging directory; recovery counts are untrusted".to_owned(),
            None => "owner marker is missing or malformed; recovery outcome is unknown".to_owned(),
        };
        warnings.push(format!("preserved unregistered migration staging at {relative}; it is not a committed backup ({detail})"));
    }
    Ok(warnings)
}

fn registered_store1_backups(conn: &Connection) -> DltResult<Vec<RegisteredStore1Backup>> {
    let mut statement = conn.prepare(
        "SELECT backup_id, path, candidate_path, staging_path, state, source_store_version, restore_min_store_version, created_at_ms FROM schema_backups ORDER BY created_at_ms, backup_id",
    ).map_err(sql)?;
    let rows = statement.query_map([], |row| Ok(RegisteredStore1Backup {
        backup_id: row.get(0)?,
        final_relative_path: row.get(1)?,
        candidate_relative_path: row.get(2)?,
        staging_relative_path: row.get(3)?,
        state: row.get(4)?,
        source_store_version: row.get(5)?,
        restore_min_store_version: row.get(6)?,
        created_at_ms: row.get(7)?,
    })).map_err(sql)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)
}

/// Reads only committed, registered checkpoints under the same source lock and SQLite snapshot
/// discipline as restore. A missing checkpoint file is listed as unavailable; restore revalidates
/// the complete database and blob set before publishing anything.
pub(crate) fn list_migration_checkpoints(root: &Path) -> DltResult<Vec<crate::MigrationCheckpointSummary>> {
    let root = absolute_path(root)?;
    ensure_database_paths_supported(&root.join(DB_FILE))?;
    let root = fs::canonicalize(&root).map_err(crate::errors::io_store)?;
    let _source_lock = crate::acquire_restore_source_lock(&root)?;

    let scratch_parent = std::env::temp_dir();
    let scratch_name = std::ffi::OsString::from(format!("checkpoint-list-{}", new_id()));
    let snapshot = SqliteReadSnapshot::create(&root.join(DB_FILE), &scratch_parent, &scratch_name)?;
    let conn = open_connection_with_flags(&snapshot.database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity_result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(sql)?;
    if integrity_result != "ok" { return Err(integrity("source workspace failed SQLite integrity_check during checkpoint listing")); }
    let application_id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0)).map_err(sql)?;
    if !matches!(application_id, APPLICATION_ID | STORE3_APPLICATION_ID) {
        return Err(err(ErrorCode::SchemaUnsupported, "checkpoint listing requires a DesignLLMTool workspace"));
    }
    if store_version(&conn)? != STORE_SCHEMA_VERSION {
        return Err(err(ErrorCode::SchemaUnsupported, "checkpoint listing requires a migrated Store3 workspace; this command does not migrate it"));
    }
    if !db_has_table(&conn, "schema_backups")? { return Err(integrity("Store3 workspace is missing its schema_backups registry")); }

    let mut checkpoints = Vec::new();
    for record in registered_store1_backups(&conn)? {
        match record.state.as_str() {
            "ready" => {}
            "publishing" => continue,
            _ => return Err(integrity("registered migration checkpoint has an unknown state")),
        }
        validate_registered_backup_paths(&record)?;
        let path = root.join(&record.final_relative_path);
        let file_available = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(integrity("registered migration checkpoint is not a regular file"));
                }
                let _ = resolve_registered_backup_file(&root, &record.final_relative_path)?;
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(crate::errors::io_store(error)),
        };
        checkpoints.push(crate::MigrationCheckpointSummary {
            backup_id: record.backup_id,
            source_store_version: record.source_store_version,
            restore_min_store_version: record.restore_min_store_version,
            created_at_ms: record.created_at_ms,
            file_available,
        });
    }
    Ok(checkpoints)
}

fn validate_registered_backup_paths(record: &RegisteredStore1Backup) -> DltResult<()> {
    if !valid_backup_id(&record.backup_id) || record.created_at_ms < 0 || record.restore_min_store_version < 3 {
        return Err(integrity("registered schema-backup identity is invalid"));
    }
    if record.restore_min_store_version > STORE_SCHEMA_VERSION {
        return Err(err(ErrorCode::SchemaUnsupported, format!("schema backup {} requires store version {} or later", record.backup_id, record.restore_min_store_version)));
    }
    let prefix = stage_prefix(record.source_store_version)?;
    let expected_stage = format!("backups/{prefix}{}{STORE1_STAGE_SUFFIX}", record.backup_id);
    let expected_candidate = format!("{expected_stage}/candidate.sqlite3");
    let expected_final = format!("backups/store{}-{}-{}.sqlite3", record.source_store_version, record.created_at_ms, record.backup_id);
    if record.staging_relative_path != expected_stage
        || record.candidate_relative_path != expected_candidate
        || record.final_relative_path != expected_final
    {
        return Err(integrity("registered schema-backup paths do not match their generated identity"));
    }
    Ok(())
}

/// Validate only the registered checkpoint state needed before upgrading a legacy store-v3
/// application id. This is read-only; normal open still owns publication and warning delivery.
fn preflight_legacy_store3_backup_registry(root: &Path, conn: &Connection) -> DltResult<()> {
    let records = registered_store1_backups(conn)?;
    let mut foreign_keys = conn.prepare("PRAGMA foreign_key_check(schema_backup_blobs)").map_err(sql)?;
    let mut violations = foreign_keys.query([]).map_err(sql)?;
    if violations.next().map_err(sql)?.is_some() {
        return Err(integrity("schema-backup blob registry contains a foreign-key violation"));
    }
    let orphan_pin: Option<(String, String)> = conn.query_row(
        "SELECT pins.backup_id,pins.sha256 FROM schema_backup_blobs pins \
         LEFT JOIN schema_backups backups ON backups.backup_id=pins.backup_id \
         WHERE backups.backup_id IS NULL LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(sql)?;
    if orphan_pin.is_some() {
        return Err(integrity("schema-backup blob registry contains a pin without a backup record"));
    }

    for record in records {
        validate_registered_backup_paths(&record)?;
        let stage_path = root.join(&record.staging_relative_path);
        let candidate_path = root.join(&record.candidate_relative_path);
        let final_path = root.join(&record.final_relative_path);
        let manifest = validate_stage_manifest(&stage_path, &record)?;
        match record.state.as_str() {
            "ready" => {
                if final_path.is_file() {
                    verify_registered_backup_file(root, conn, &record, &final_path)?;
                }
            }
            "publishing" => {
                let candidate_exists = candidate_path.is_file();
                let final_exists = final_path.is_file();
                if manifest.is_some() { validate_stage_entries(&stage_path, candidate_exists)?; }
                let backup_path = match (candidate_exists, final_exists) {
                    (true, true) => return Err(err(ErrorCode::StorageFailure, format!("registered backup {} has both candidate and final paths; preserving both", record.backup_id))),
                    (false, false) => return Err(err(ErrorCode::StorageFailure, format!("registered backup {} is missing both candidate and final files; pins remain active", record.backup_id))),
                    (true, false) => &candidate_path,
                    (false, true) => &final_path,
                };
                verify_registered_backup_file(root, conn, &record, backup_path)?;
            }
            other => return Err(integrity(format!("registered schema backup has unknown state {other:?}"))),
        }
    }
    Ok(())
}

fn validate_stage_manifest(stage_path: &Path, record: &RegisteredStore1Backup) -> DltResult<Option<Store1StageManifest>> {
    let owner_path = stage_path.join(STORE1_OWNER_FILE);
    if !stage_path.exists() { return Ok(None); }
    if !owner_path.exists() {
        let mut entries = fs::read_dir(stage_path).map_err(crate::errors::io_store)?;
        if entries.next().is_none() { return Ok(None); }
        return Err(integrity("registered schema-backup staging has unexpected entries but no owner marker"));
    }
    let manifest = parse_stage_manifest(&owner_path).ok_or_else(|| integrity("registered schema-backup staging owner marker is missing or malformed"))?;
    if manifest.source_store_version != record.source_store_version
        || manifest.backup_id != record.backup_id
        || manifest.created_at_ms != record.created_at_ms
        || manifest.recovery_state != "complete"
    {
        return Err(integrity("registered schema-backup staging owner marker does not match the committed migration"));
    }
    Ok(Some(manifest))
}

fn validate_stage_entries(stage_path: &Path, candidate_expected: bool) -> DltResult<()> {
    let mut names = HashSet::new();
    for entry in fs::read_dir(stage_path).map_err(crate::errors::io_store)? {
        names.insert(entry.map_err(crate::errors::io_store)?.file_name().to_string_lossy().into_owned());
    }
    let mut expected = HashSet::from([STORE1_OWNER_FILE.to_owned()]);
    if candidate_expected { expected.insert("candidate.sqlite3".to_owned()); }
    if names != expected {
        return Err(err(ErrorCode::StorageFailure, format!("registered staging directory contains unexpected entries: {names:?}")));
    }
    Ok(())
}

fn verify_registered_backup_file(root: &Path, conn: &Connection, record: &RegisteredStore1Backup, path: &Path) -> DltResult<()> {
    let relative = path.strip_prefix(root).map_err(|_| integrity("registered backup path escapes its workspace"))?;
    let relative = relative.to_str().ok_or_else(|| integrity("registered backup path is not valid UTF-8"))?;
    let path = resolve_registered_backup_file(root, relative)?;
    let backup = ReadOnlySqlite::open(&path, &std::env::temp_dir(), std::ffi::OsStr::new("registered-backup"))?;
    let backup_conn = &backup.connection;
    match read_backup_marker(backup_conn)? {
        Some(marker) if marker.backup_id == record.backup_id
            && marker.source_store_version == record.source_store_version
            && marker.restore_min_store_version == record.restore_min_store_version => {}
        Some(_) => return Err(integrity("registered schema backup marker differs from workspace metadata")),
        None if record.state == "publishing" => return Err(integrity("new schema backup is not protected by the old-core refusal marker")),
        None => {} // A registered pre-marker backup remains byte-identical and is reported as unprotected.
    }
    let integrity_result: String = backup_conn.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(sql)?;
    if integrity_result != "ok" { return Err(integrity("registered schema backup failed SQLite integrity_check")); }
    let version: String = backup_conn.query_row("SELECT value FROM meta WHERE key='store_schema_version'", [], |row| row.get(0)).map_err(sql)?;
    if version.parse::<i64>().ok() != Some(record.source_store_version) {
        return Err(integrity(format!("registered schema backup does not contain source store version {}", record.source_store_version)));
    }
    let backup_hashes = referenced_blobs(backup_conn)?;
    let mut statement = conn.prepare("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1").map_err(sql)?;
    let rows = statement.query_map(params![record.backup_id], |row| row.get::<_, String>(0)).map_err(sql)?;
    let pinned = rows.collect::<rusqlite::Result<HashSet<_>>>().map_err(sql)?;
    if backup_hashes != pinned { return Err(integrity("registered schema backup blob set differs from its committed pins")); }
    for hash in &pinned {
        let bytes = fs::read(root.join(BLOB_DIR).join(hash)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound { integrity(format!("pinned schema-backup blob {hash} is missing")) } else { crate::errors::io_store(error) }
        })?;
        if sha256(&bytes) != hash.as_str() { return Err(integrity(format!("pinned schema-backup blob {hash} failed hash verification"))); }
    }
    Ok(())
}

fn mark_registered_backup_ready(conn: &mut Connection, backup_id: &str) -> DltResult<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
    let changed = tx.execute("UPDATE schema_backups SET state='ready' WHERE backup_id=?1 AND state='publishing'", params![backup_id]).map_err(sql)?;
    if changed != 1 { return Err(integrity("registered schema-backup publishing state changed unexpectedly")); }
    tx.commit().map_err(sql)
}

fn add_manifest_recovery_to_report(report: &mut crate::RecoveryReport, manifest: &Store1StageManifest) {
    report.removed_pending_temps += manifest.removed_pending_temps;
    report.recovered_saves += manifest.recovered_saves;
}

/// Resume only candidates whose database row and blob pins committed. File publication is
/// idempotent across the rename/ready-update crash window.
fn finalize_registered_backups(root: &Path, conn: &mut Connection, report: &mut crate::RecoveryReport, include_recovery_report: bool) -> DltResult<()> {
    if !db_has_table(conn, "schema_backups")? { return Ok(()); }
    let records = registered_store1_backups(conn)?;
    for record in records {
        validate_registered_backup_paths(&record)?;
        let stage_path = root.join(&record.staging_relative_path);
        let candidate_path = root.join(&record.candidate_relative_path);
        let final_path = root.join(&record.final_relative_path);
        let manifest = validate_stage_manifest(&stage_path, &record)?;
        if include_recovery_report {
            if let Some(manifest) = &manifest { add_manifest_recovery_to_report(report, manifest); }
            else {
                report.migration_warnings.push(format!("registered migration {} has no recovery-count marker; previous recovery counts may already have been acknowledged", record.backup_id));
            }
        }
        match record.state.as_str() {
            "ready" => {
                if !final_path.is_file() {
                    report.migration_warnings.push(format!("registered schema backup {} is missing at {}; blob pins remain active", record.backup_id, record.final_relative_path));
                }
            }
            "publishing" => {
                if let Some(_) = manifest { validate_stage_entries(&stage_path, candidate_path.exists())?; }
                let candidate_exists = candidate_path.is_file();
                let final_exists = final_path.is_file();
                match (candidate_exists, final_exists) {
                    (true, true) => return Err(err(ErrorCode::StorageFailure, format!("registered backup {} has both candidate and final paths; preserving both", record.backup_id))),
                    (false, false) => return Err(err(ErrorCode::StorageFailure, format!("registered backup {} is missing both candidate and final files; pins remain active", record.backup_id))),
                    (true, false) => {
                        verify_registered_backup_file(root, conn, &record, &candidate_path)?;
                        crate::fsx::rename_durable(&candidate_path, &final_path, false).map_err(crate::errors::io_store)?;
                    }
                    (false, true) => verify_registered_backup_file(root, conn, &record, &final_path)?,
                }
                if backup_sidecars_exist(&final_path) { return Err(integrity("published schema backup has a SQLite sidecar")); }
                // Keep the owner marker until Workspace::open has returned the recovery report.
                mark_registered_backup_ready(conn, &record.backup_id)?;
            }
            other => return Err(integrity(format!("registered schema backup has unknown state {other:?}"))),
        }
    }
    Ok(())
}

/// Remove only the registered migration's owner marker and an otherwise empty staging directory.
/// Any unexpected file is preserved and surfaced in the returned warning.
pub(crate) fn acknowledge_migration_recovery(root: &Path, conn: &Connection) -> DltResult<Vec<String>> {
    let mut warnings = Vec::new();
    if !db_has_table(conn, "schema_backups")? { return Ok(warnings); }
    for record in registered_store1_backups(conn)?.into_iter().filter(|record| record.state == "ready") {
        validate_registered_backup_paths(&record)?;
        let stage_path = root.join(&record.staging_relative_path);
        if !stage_path.exists() { continue; }
        if let Some(manifest) = validate_stage_manifest(&stage_path, &record)? {
            if manifest.recovery_state != "complete" { return Err(integrity("ready schema backup has incomplete recovery marker")); }
        }
        let entries = fs::read_dir(&stage_path).map_err(crate::errors::io_store)?;
        let mut names = HashSet::new();
        for entry in entries { names.insert(entry.map_err(crate::errors::io_store)?.file_name().to_string_lossy().into_owned()); }
        if names == HashSet::from([STORE1_OWNER_FILE.to_owned()]) {
            let owner = stage_path.join(STORE1_OWNER_FILE);
            remove_if_present(&owner).map_err(crate::errors::io_store)?;
        } else if !names.is_empty() {
            warnings.push(format!("migration staging {} has unexpected entries and was preserved: {names:?}", record.staging_relative_path));
            continue;
        }
        if let Err(error) = remove_dir_if_present(&stage_path) {
            warnings.push(format!("migration staging {} could not be removed after its recovery report: {error}", record.staging_relative_path));
        }
    }
    Ok(warnings)
}

pub fn to_json<T: Serialize>(value: &T) -> DltResult<String> { serde_json::to_string(value).map_err(|e| invalid(e.to_string())) }
pub fn from_json<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> DltResult<T> {
    serde_json::from_str(text).map_err(|e| integrity(format!("stored {what} is unreadable: {e}")))
}

pub fn kind_str(kind: ArtifactKind) -> String { serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default() }
pub fn parse_kind(s: &str) -> DltResult<ArtifactKind> { serde_json::from_value(Value::String(s.into())).map_err(|_| integrity(format!("unknown artifact kind {s:?}"))) }
pub fn status_str(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued", JobStatus::Running => "running", JobStatus::Validating => "validating",
        JobStatus::Completed => "completed", JobStatus::Canceled => "canceled", JobStatus::Failed => "failed", JobStatus::Conflict => "conflict",
    }
}
pub fn parse_status(s: &str) -> DltResult<JobStatus> { serde_json::from_value(Value::String(s.into())).map_err(|_| integrity(format!("unknown job status {s:?}"))) }

#[derive(Debug, Clone)]
pub struct ArtifactRow {
    pub id: String, pub kind: ArtifactKind, pub engine_id: String, pub title: String, #[allow(dead_code)] pub origin: String,
    pub current_revision: Revision, pub current_snapshot: String, pub base_snapshot: String,
    #[allow(dead_code)] pub source_path: Option<String>, pub source_hash: Option<String>,
    pub saved_revision: Option<Revision>, pub saved_path: Option<String>, pub saved_hash: Option<String>, pub saved_snapshot: Option<String>,
}
impl ArtifactRow {
    pub fn dirty(&self) -> bool { self.saved_snapshot.as_deref() != Some(self.current_snapshot.as_str()) }
    pub fn summary(&self) -> ArtifactSummary {
        ArtifactSummary { id: self.id.clone(), kind: self.kind, title: self.title.clone(), revision: self.current_revision, engine_id: self.engine_id.clone(), dirty: self.dirty(), modified_at_ms: None, active_jobs: vec![] }
    }
}
const ARTIFACT_COLS: &str = "artifact_id, kind, engine_id, title, origin, current_revision, current_snapshot, base_snapshot, source_path, source_hash, saved_revision, saved_path, saved_hash, saved_snapshot";
fn read_artifact(r: &rusqlite::Row<'_>) -> rusqlite::Result<(ArtifactRow, String)> {
    let kind: String = r.get(1)?;
    Ok((ArtifactRow {
        id: r.get(0)?, kind: ArtifactKind::Canvas, engine_id: r.get(2)?, title: r.get(3)?, origin: r.get(4)?,
        current_revision: r.get::<_, i64>(5)? as u64, current_snapshot: r.get(6)?, base_snapshot: r.get(7)?,
        source_path: r.get(8)?, source_hash: r.get(9)?, saved_revision: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        saved_path: r.get(11)?, saved_hash: r.get(12)?, saved_snapshot: r.get(13)?,
    }, kind))
}
pub fn artifact_row(conn: &Connection, id: &str) -> DltResult<ArtifactRow> {
    let found = conn.query_row(&format!("SELECT {ARTIFACT_COLS} FROM artifacts WHERE artifact_id = ?1"), params![id], read_artifact).optional().map_err(sql)?;
    let (mut row, kind) = found.ok_or_else(|| not_found(format!("artifact {id} not found")))?;
    row.kind = parse_kind(&kind)?;
    Ok(row)
}
pub fn artifact_rows(conn: &Connection) -> DltResult<Vec<ArtifactRow>> {
    let mut st = conn.prepare(&format!("SELECT {ARTIFACT_COLS} FROM artifacts ORDER BY created_seq, artifact_id")).map_err(sql)?;
    let rows = st.query_map([], read_artifact).map_err(sql)?;
    let mut out = Vec::new();
    for row in rows {
        let (mut a, kind) = row.map_err(sql)?;
        a.kind = parse_kind(&kind)?;
        out.push(a);
    }
    Ok(out)
}
pub fn artifact_exists(conn: &Connection, id: &str) -> DltResult<bool> {
    Ok(conn.query_row("SELECT 1 FROM artifacts WHERE artifact_id = ?1", params![id], |_| Ok(())).optional().map_err(sql)?.is_some())
}
pub fn snapshot_at(conn: &Connection, artifact_id: &str, revision: Revision) -> DltResult<String> {
    conn.query_row("SELECT snapshot FROM revisions WHERE artifact_id = ?1 AND revision = ?2", params![artifact_id, revision as i64], |r| r.get(0))
        .optional().map_err(sql)?.ok_or_else(|| not_found(format!("revision {revision} of artifact {artifact_id} not found")))
}

pub fn insert_event(conn: &Connection, artifact_id: Option<&str>, revision: Option<Revision>, event: &str, detail: &Value) -> DltResult<u64> {
    conn.execute("INSERT INTO events(artifact_id, revision, event, detail) VALUES (?1, ?2, ?3, ?4)", params![artifact_id, revision.map(|r| r as i64), event, to_json(detail)?]).map_err(sql)?;
    Ok(conn.last_insert_rowid() as u64)
}
pub fn last_event_seq(conn: &Connection) -> DltResult<u64> {
    let seq: Option<i64> = conn.query_row("SELECT seq FROM sqlite_sequence WHERE name = 'events'", [], |r| r.get(0)).optional().map_err(sql)?;
    Ok(seq.unwrap_or(0).max(0) as u64)
}
pub fn min_event_seq(conn: &Connection) -> DltResult<Option<u64>> {
    let v: Option<i64> = conn.query_row("SELECT MIN(event_seq) FROM events", [], |r| r.get(0)).map_err(sql)?;
    Ok(v.map(|v| v as u64))
}
pub fn events_after(conn: &Connection, after: u64, limit: usize) -> DltResult<Vec<CoreEvent>> {
    let mut st = conn.prepare("SELECT event_seq, artifact_id, revision, event, detail FROM events WHERE event_seq > ?1 ORDER BY event_seq LIMIT ?2").map_err(sql)?;
    let rows = st.query_map(params![after as i64, limit as i64], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
    }).map_err(sql)?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, artifact_id, revision, event, detail) = row.map_err(sql)?;
        out.push(CoreEvent { protocol_version: PROTOCOL_VERSION, event_seq: seq as u64, artifact_id, revision: revision.map(|r| r as u64), event, detail: from_json(&detail, "event detail")? });
    }
    Ok(out)
}

/// Stored receipt payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StoredResult { Committed { result: Value }, Rejected { error: DltError } }

/// `Ok(None)`: unknown request id. `Ok(Some(Ok(v)))`/`Ok(Some(Err(e)))`: the stored outcome.
/// Same id with different content or method: REQUEST_ID_MISMATCH (never recorded).
pub fn find_receipt(conn: &Connection, key: &crate::receipts::RequestKey) -> DltResult<Option<Result<Value, DltError>>> {
    let row: Option<(String, String, String)> = conn
        .query_row("SELECT method, content_hash, result_json FROM receipts WHERE request_id = ?1", params![key.request_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional().map_err(sql)?;
    let Some((method, hash, json)) = row else { return Ok(None) };
    if hash != key.content_hash || method != key.method {
        return Ok(Some(Err(err(ErrorCode::RequestIdMismatch, format!("request_id {} was already used with different content ({method})", key.request_id)).ids_one(&key.request_id))));
    }
    Ok(Some(match from_json::<StoredResult>(&json, "receipt")? { StoredResult::Committed { result } => Ok(result), StoredResult::Rejected { error } => Err(error) }))
}
trait IdsOne { fn ids_one(self, id: &str) -> Self; }
impl IdsOne for DltError { fn ids_one(mut self, id: &str) -> Self { self.affected_ids.push(id.to_string()); self } }

pub fn insert_receipt(conn: &Connection, key: &crate::receipts::RequestKey, artifact_id: Option<&str>, stored: &StoredResult) -> DltResult<()> {
    let outcome = match stored { StoredResult::Committed { .. } => "committed", StoredResult::Rejected { .. } => "rejected" };
    conn.execute(
        "INSERT INTO receipts(request_id, method, content_hash, artifact_id, outcome, result_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key.request_id, key.method, key.content_hash, artifact_id, outcome, to_json(stored)?],
    ).map_err(sql)?;
    Ok(())
}

pub fn asset_rows(conn: &Connection) -> DltResult<Vec<AssetRef>> {
    let mut st = conn.prepare("SELECT sha256, mime, bytes FROM assets").map_err(sql)?;
    let rows = st.query_map([], |r| Ok(AssetRef { sha256: r.get(0)?, mime: r.get(1)?, bytes: r.get::<_, i64>(2)? as u64 })).map_err(sql)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)
}
pub fn insert_assets(conn: &Connection, refs: &[AssetRef], origin: &str) -> DltResult<()> {
    for r in refs {
        conn.execute("INSERT OR IGNORE INTO assets(sha256, mime, bytes, origin) VALUES (?1, ?2, ?3, ?4)", params![r.sha256, r.mime, r.bytes as i64, origin]).map_err(sql)?;
    }
    Ok(())
}

pub fn referenced_blobs(conn: &Connection) -> DltResult<HashSet<String>> {
    let mut referenced = base_referenced_blobs(conn)?;
    let pin_table: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_backup_blobs'",
        [], |row| row.get(0),
    ).map_err(sql)?;
    if pin_table > 0 {
        let mut st = conn.prepare("SELECT sha256 FROM schema_backup_blobs").map_err(sql)?;
        let rows = st.query_map([], |row| row.get::<_, String>(0)).map_err(sql)?;
        referenced.extend(rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?);
    }
    Ok(referenced)
}

fn base_referenced_blobs(conn: &Connection) -> DltResult<HashSet<String>> {
    let mut st = conn.prepare(
        "SELECT current_snapshot FROM artifacts UNION SELECT base_snapshot FROM artifacts UNION SELECT saved_snapshot FROM artifacts WHERE saved_snapshot IS NOT NULL \
         UNION SELECT snapshot FROM revisions UNION SELECT before_snapshot FROM undo_log UNION SELECT after_snapshot FROM undo_log \
         UNION SELECT sha256 FROM assets UNION SELECT sha256 FROM figure_renders",
    ).map_err(sql)?;
    let rows = st.query_map([], |r| r.get::<_, String>(0)).map_err(sql)?;
    rows.collect::<rusqlite::Result<HashSet<_>>>().map_err(sql)
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub job_id: String, pub artifact_id: String, pub base_revision: Revision, pub allowed: BTreeSet<Intent>, pub status: JobStatus,
    pub current_attempt: String, pub attempt_count: i64, pub request: JobStartRequest, pub error: Option<DltError>,
    /// MCP client of the current attempt (job.set_client), if it reported one.
    pub client_name: Option<String>,
}
impl JobRow {
    pub fn job(&self) -> Job {
        Job {
            id: self.job_id.clone(), attempt_id: self.current_attempt.clone(), artifact_id: self.artifact_id.clone(), base_revision: self.base_revision,
            status: self.status, allowed_intents: self.allowed.clone(), model: self.request.model.clone(), effort: self.request.effort.clone(), error: self.error.clone(),
            runtime: self.request.runtime.clone(), target_ids: self.request.target_ids.clone(), client_name: self.client_name.clone(),
            deliver_as: self.request.deliver_as, attempt_seq: u64::try_from(self.attempt_count).ok(),
        }
    }
}
pub fn set_job_client(conn: &Connection, job: &JobRef, name: &str, version: Option<&str>) -> DltResult<()> {
    conn.execute(
        "INSERT INTO job_clients(job_id, attempt_id, client_name, client_version) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(job_id, attempt_id) DO UPDATE SET client_name = excluded.client_name, client_version = excluded.client_version",
        params![job.job_id, job.attempt_id, name, version],
    ).map_err(sql)?;
    Ok(())
}
pub fn job_row(conn: &Connection, job_id: &str) -> DltResult<JobRow> {
    let row: Option<(String, i64, String, String, String, i64, String, Option<String>)> = conn
        .query_row(
            "SELECT artifact_id, base_revision, allowed_intents, status, current_attempt, attempt_count, request_json, error_json FROM jobs WHERE job_id = ?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
        ).optional().map_err(sql)?;
    let (artifact_id, base, allowed, status, current_attempt, attempt_count, request, error) = row.ok_or_else(|| not_found(format!("job {job_id} not found")))?;
    let client_name: Option<String> = conn
        .query_row("SELECT client_name FROM job_clients WHERE job_id = ?1 AND attempt_id = ?2", params![job_id, current_attempt], |r| r.get(0))
        .optional().map_err(sql)?;
    Ok(JobRow {
        job_id: job_id.to_string(), artifact_id, base_revision: base as u64, allowed: from_json(&allowed, "job intents")?, status: parse_status(&status)?,
        current_attempt, attempt_count, request: from_json(&request, "job request")?, error: error.map(|e| from_json(&e, "job error")).transpose()?, client_name,
    })
}
pub fn job_ids_with_status(conn: &Connection, statuses: &[&str]) -> DltResult<Vec<String>> {
    let mut st = conn.prepare("SELECT job_id, status FROM jobs ORDER BY rowid").map_err(sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(sql)?;
    let mut out = Vec::new();
    for row in rows { let (id, s) = row.map_err(sql)?; if statuses.contains(&s.as_str()) { out.push(id); } }
    Ok(out)
}
pub fn attempt_state(conn: &Connection, job_id: &str, attempt_id: &str) -> DltResult<Option<String>> {
    conn.query_row("SELECT state FROM attempts WHERE job_id = ?1 AND attempt_id = ?2", params![job_id, attempt_id], |r| r.get(0)).optional().map_err(sql)
}

/// Job/attempt check for writes. Called during evaluation and again inside the commit
/// transaction (the same serialization point as cancel).
pub fn check_job_write(conn: &Connection, job: &JobRef, artifact_id: &str, allowed: &BTreeSet<Intent>) -> DltResult<JobRow> {
    let row = job_row(conn, &job.job_id)?;
    let attempt = attempt_state(conn, &job.job_id, &job.attempt_id)?
        .ok_or_else(|| not_found(format!("attempt {} of job {} not found", job.attempt_id, job.job_id)))?;
    if row.status == JobStatus::Canceled || attempt == "canceled" {
        return Err(err(ErrorCode::Canceled, format!("job {} attempt {} was canceled; late results are rejected", job.job_id, job.attempt_id)));
    }
    if attempt != "active" || row.current_attempt != job.attempt_id || !matches!(row.status, JobStatus::Queued | JobStatus::Running | JobStatus::Validating) {
        return Err(err(ErrorCode::JobNotActive, format!("job {} is {} and attempt {} is {attempt}", job.job_id, status_str(row.status), job.attempt_id)));
    }
    if row.artifact_id != artifact_id { return Err(invalid(format!("job {} is bound to artifact {}, not {artifact_id}", job.job_id, row.artifact_id))); }
    let exceeding: Vec<&Intent> = allowed.iter().filter(|i| !row.allowed.contains(i)).collect();
    if !exceeding.is_empty() {
        return Err(err(ErrorCode::ContentLocked, format!("allowed_intents {exceeding:?} exceed the scope of job {}", job.job_id)));
    }
    Ok(row)
}

/// The check of a job's document commit (every revision a job writes goes through `commit_inner` or `commit_group_inner`):
/// [`check_job_write`], and a job started with `deliver_as: proposal` never commits itself. Its work reaches the document
/// only through the user's `proposal.apply`, which commits without a job.
pub fn check_job_commit(conn: &Connection, job: &JobRef, artifact_id: &str, allowed: &BTreeSet<Intent>) -> DltResult<JobRow> {
    let row = check_job_write(conn, job, artifact_id, allowed)?;
    if row.request.deliver_as == Some(DeliverAs::Proposal) {
        return Err(err(ErrorCode::ContentLocked, format!("job {} delivers its work as a proposal; it cannot change the artifact itself, send the change set with proposal.create", job.job_id)));
    }
    Ok(row)
}

#[cfg(test)]
mod store2_migration_draft_tests {
    use super::*;
    use crate::blobs::{BlobStore, SharedHook};
    use rusqlite::Connection;
    use std::collections::{BTreeMap, HashMap};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("store2-{label}-{}", new_id()));
            fs::create_dir_all(&path).expect("create temp workspace");
            Self(path)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn hook() -> SharedHook { Arc::new(Mutex::new(None)) }

    fn has_group_id(conn: &Connection) -> bool {
        let mut stmt = conn.prepare("PRAGMA table_info(undo_log)").expect("table info");
        let names = stmt.query_map([], |row| row.get::<_, String>(1)).expect("columns")
            .collect::<rusqlite::Result<Vec<_>>>().expect("column names");
        names.iter().any(|name| name == "group_id")
    }

    fn has_table(conn: &Connection, name: &str) -> bool {
        conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1", params![name], |row| row.get::<_, i64>(0))
            .expect("table lookup") > 0
    }

    fn scalar_string(conn: &Connection, sql: &str) -> String {
        conn.query_row(sql, [], |row| row.get(0)).expect("scalar string")
    }

    /// A store marked with the built-in job `feature` is refused by a build without it and opened by this one.
    fn job_feature_refuses_an_older_reader(tag: &str, feature: &str) {
        let root = TempRoot::new(tag);
        let mut recovery = crate::RecoveryReport::default();
        let (mut conn, _) = open_db(&root.0, &mut recovery).expect("empty store3");
        let marked = BTreeSet::from([feature.to_owned()]);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).expect("start feature tx");
        merge_store_features(&tx, &marked).expect("register the feature atomically");
        tx.commit().expect("commit feature marker");

        let old_build = BTreeSet::new();
        assert_eq!(ensure_store_features_supported(&conn, &old_build).unwrap_err().code, ErrorCode::SchemaUnsupported);
        let current_build = supported_persisted_features(&[]).expect("Core feature is built in");
        assert!(current_build.contains(feature));
        ensure_store_features_supported(&conn, &current_build).expect("current Core preserves the feature");
    }

    #[test]
    fn job_figure_refs_feature_refuses_a_reader_that_cannot_preserve_the_pins() {
        job_feature_refuses_an_older_reader("job-figure-refs-feature", JOB_FIGURE_REFS_FEATURE);
    }

    #[test]
    fn job_deliver_as_feature_refuses_a_reader_that_would_drop_proposal_delivery() {
        job_feature_refuses_an_older_reader("job-deliver-as-feature", JOB_DELIVER_AS_FEATURE);
    }

    fn seed_registered_candidate(root: &Path, pending_save: bool) -> (Connection, Store1Backup, usize, usize) {
        seed_store1(root, pending_save);
        let mut conn = Connection::open(root.join(DB_FILE)).expect("open store1 db");
        conn.busy_timeout(Duration::from_secs(5)).expect("busy timeout");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;").expect("configure v1 db");
        let mut stage = reserve_store1_backup_stage(root, new_id()).expect("reserve isolated stage");
        write_stage_manifest(&stage, "running", 0, 0).expect("mark pending recovery");
        let mut recovered_saves = 0;
        let removed_pending_temps = crate::files::recover_pending(&mut conn, &mut recovered_saves).expect("recover pending fixture");
        write_stage_manifest(&stage, "complete", removed_pending_temps, recovered_saves).expect("record recovery counts");
        let backup = backup_store1(root, &conn, &mut stage).expect("create registered candidate");
        migrate_store1_to2(&mut conn, &backup).expect("atomically register backup and pins");
        (conn, backup, removed_pending_temps, recovered_saves)
    }

    fn seed_registered_store2_to3_candidate(root: &Path, pending_save: bool) -> (Connection, Store1Backup, usize, usize) {
        let (mut conn, _old_store1_backup, removed_before, recovered_before) = seed_registered_candidate(root, pending_save);
        let mut stage = reserve_store_backup_stage(root, new_id(), 2).expect("reserve store2-to3 stage");
        write_stage_manifest(&stage, "running", 0, 0).expect("mark store2 recovery");
        let mut recovered_saves = 0;
        let removed_pending_temps = crate::files::recover_pending(&mut conn, &mut recovered_saves).expect("recover store2 pending saves");
        write_stage_manifest(&stage, "complete", removed_pending_temps, recovered_saves).expect("record store2 recovery");
        let backup = backup_store(root, &conn, &mut stage).expect("create store2 candidate");
        migrate_store_to3(&mut conn, &backup, None, None).expect("atomically register store3 backup and pins");
        (conn, backup, removed_before + removed_pending_temps, recovered_before + recovered_saves)
    }

    fn file_hashes(root: &Path) -> BTreeMap<String, String> {
        fn walk(root: &Path, dir: &Path, output: &mut BTreeMap<String, String>) {
            for entry in fs::read_dir(dir).expect("read fixture directory") {
                let entry = entry.expect("fixture entry");
                let path = entry.path();
                if entry.file_type().expect("fixture file type").is_dir() {
                    walk(root, &path, output);
                } else {
                    let relative = path.strip_prefix(root).expect("relative fixture path").to_string_lossy().replace('\\', "/");
                    output.insert(relative, sha256(&fs::read(&path).expect("read fixture file")));
                }
            }
        }
        let mut output = BTreeMap::new();
        walk(root, root, &mut output);
        output
    }

    #[cfg(windows)]
    fn long_workspace_root(base: &Path) -> PathBuf {
        let mut root = base.to_path_buf();
        // Stop just beyond the backup limit so existing v2 files still fit at any TEMP depth.
        for width in [200, 32, 1] {
            let segment = "a".repeat(width);
            loop {
                let next = root.join(&segment);
                if ensure_migration_paths_supported(&next, 1).is_err() { break; }
                root = next;
            }
        }
        let root = root.join("a");
        fs::create_dir_all(&root).expect("create test-owned long workspace path");
        let workspace_db = root.join(DB_FILE);
        let blob_path = root.join(BLOB_DIR).join("f".repeat(64));
        assert!(path_units(&workspace_db) <= MAX_SAFE_WINDOWS_PATH_UNITS);
        assert!(path_units(&blob_path) <= MAX_SAFE_WINDOWS_PATH_UNITS, "test root path is too long for its intended boundary: {} ({})", root.display(), path_units(&blob_path));
        assert!(ensure_migration_paths_supported(&root, 1).is_err(), "backup or its sidecar should exceed the supported boundary");
        root
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).expect("create fixture copy directory");
        for entry in fs::read_dir(source).expect("read fixture source") {
            let entry = entry.expect("fixture source entry");
            let from = entry.path();
            let to = destination.join(entry.file_name());
            if entry.file_type().expect("fixture source type").is_dir() {
                copy_tree(&from, &to);
            } else {
                fs::copy(&from, &to).expect("copy fixture file");
            }
        }
    }

    fn remap_pending_paths_to_copy(root: &Path) -> usize {
        let conn = Connection::open(root.join(DB_FILE)).expect("open copied fixture db");
        let rows: Vec<String> = {
            let mut statement = conn.prepare("SELECT temp_path FROM pending_saves ORDER BY temp_path").expect("pending query");
            statement.query_map([], |row| row.get(0)).expect("pending rows")
                .collect::<rusqlite::Result<Vec<_>>>().expect("pending paths")
        };
        for (index, original_temp) in rows.iter().enumerate() {
            let temp = root.join(format!("fixture-pending-{index}.dllt-partial"));
            let target = root.join(format!("fixture-target-{index}.native"));
            fs::write(&temp, b"isolated fixture temp").expect("create copied pending temp");
            let changed = conn.execute(
                "UPDATE pending_saves SET temp_path=?1, target_path=?2 WHERE temp_path=?3",
                params![temp.to_string_lossy(), target.to_string_lossy(), original_temp],
            ).expect("remap only copied pending paths");
            assert_eq!(changed, 1);
        }
        rows.len()
    }

    #[test]
    fn history_group_integrity_checks_members_without_rebuilding_undo_log() {
        let conn = Connection::open_in_memory().expect("in-memory group fixture");
        conn.execute_batch(
            "CREATE TABLE history_groups(group_id TEXT PRIMARY KEY, request_id TEXT NOT NULL, member_count INTEGER NOT NULL CHECK(member_count>=2));
             CREATE TABLE undo_log(artifact_id TEXT NOT NULL, seq INTEGER NOT NULL, group_id TEXT, PRIMARY KEY(artifact_id,seq));",
        ).expect("group tables");
        let mut statement = conn.prepare("PRAGMA foreign_key_list(undo_log)").unwrap();
        let foreign_keys = statement.query_map([], |_| Ok(())).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap().len();
        assert_eq!(foreign_keys, 0, "store3 keeps the existing undo table unrebuilt; application membership checks supply integrity");
        validate_history_groups(&conn).expect("empty group history");

        conn.execute("INSERT INTO history_groups VALUES ('g','request',2)", []).unwrap();
        conn.execute("INSERT INTO undo_log VALUES ('a',1,'g')", []).unwrap();
        let missing = validate_history_groups(&conn).expect_err("missing member is rejected");
        assert_eq!(missing.code, ErrorCode::IntegrityError);
        conn.execute("INSERT INTO undo_log VALUES ('b',1,'g')", []).unwrap();
        validate_history_groups(&conn).expect("complete group passes");

        conn.execute("DELETE FROM undo_log", []).unwrap();
        conn.execute("DELETE FROM history_groups", []).unwrap();
        conn.execute("INSERT INTO history_groups VALUES ('duplicate','request2',2)", []).unwrap();
        conn.execute("INSERT INTO undo_log VALUES ('a',1,'duplicate'), ('a',2,'duplicate')", []).unwrap();
        let duplicate = validate_history_groups(&conn).expect_err("duplicate artifact membership is rejected");
        assert_eq!(duplicate.code, ErrorCode::IntegrityError);

        conn.execute("DELETE FROM history_groups", []).unwrap();
        let orphan = validate_history_groups(&conn).expect_err("orphan undo group id is rejected");
        assert_eq!(orphan.code, ErrorCode::IntegrityError);
    }

    #[test]
    fn sqlite_application_id_change_rolls_back_and_commits_with_its_transaction() {
        let mut conn = Connection::open_in_memory().expect("application id transaction fixture");
        conn.execute_batch(&format!("PRAGMA application_id={APPLICATION_ID};")).expect("set legacy id");
        {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
            tx.execute_batch(&format!("PRAGMA application_id={STORE3_APPLICATION_ID};")).unwrap();
            assert_eq!(tx.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).unwrap(), STORE3_APPLICATION_ID);
            // Drop without commit to model a failed version/schema transaction.
        }
        assert_eq!(conn.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).unwrap(), APPLICATION_ID);

        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        tx.execute_batch(&format!("PRAGMA application_id={STORE3_APPLICATION_ID};")).unwrap();
        tx.commit().unwrap();
        assert_eq!(conn.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).unwrap(), STORE3_APPLICATION_ID);
    }

    #[test]
    fn legacy_store3_invalid_contract_metadata_is_rejected_without_mutation() {
        let cases = [
            ("contract_schema_version", Some("999")),
            ("contract_schema_version", Some("not-a-version")),
            ("request_hash_alg", Some("unsupported-hash-alg")),
            ("workspace_id", None),
            ("store_features_backfilled", Some("2")),
            ("store_features_backfilled", None),
        ];
        for (key, value) in cases {
            let root = TempRoot::new("legacy-store3-invalid-metadata");
            let mut report = crate::RecoveryReport::default();
            let (conn, _) = open_db(&root.0, &mut report).expect("create valid store-v3 fixture");
            drop(conn);

            let mut conn = open_connection(root.0.join(DB_FILE)).expect("reopen store-v3 fixture");
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).expect("begin metadata corruption fixture");
            match value {
                Some(value) => assert_eq!(
                    tx.execute("UPDATE meta SET value=?1 WHERE key=?2", params![value, key]).expect("corrupt existing metadata"),
                    1,
                    "fixture metadata key exists",
                ),
                None => assert_eq!(
                    tx.execute("DELETE FROM meta WHERE key=?1", params![key]).expect("remove required metadata"),
                    1,
                    "fixture metadata key exists",
                ),
            }
            tx.execute_batch(&format!("PRAGMA application_id={APPLICATION_ID};")).expect("restore legacy application id");
            tx.commit().expect("commit corrupt legacy metadata fixture");
            drop(conn);

            let before = file_hashes(&root.0);
            let mut report = crate::RecoveryReport::default();
            let error = match open_db_with_engines(&root.0, &mut report, &[], &BTreeSet::new()) {
                Err(error) => error,
                Ok((conn, _)) => {
                    drop(conn);
                    panic!("invalid {key} metadata was accepted")
                }
            };
            let expected = if matches!(key, "workspace_id" | "store_features_backfilled") { ErrorCode::IntegrityError } else { ErrorCode::SchemaUnsupported };
            assert_eq!(error.code, expected, "wrong rejection for {key}={value:?}: {error:?}");
            assert_eq!(file_hashes(&root.0), before, "failed legacy store-v3 open changed files for {key}={value:?}");
        }
    }

    #[test]
    fn legacy_store3_backup_registry_preflight_preserves_invalid_state_and_accepts_valid_state() {
        for corruption in ["valid", "bad_path", "orphan_pin", "missing_pin"] {
            let root = TempRoot::new("legacy-store3-invalid-backup-registry");
            let (conn, backup, _, _) = seed_registered_store2_to3_candidate(&root.0, false);
            conn.execute_batch(
                "PRAGMA foreign_keys=OFF;
                 DELETE FROM undo_log;
                 DELETE FROM revisions;
                 DELETE FROM artifacts;",
            ).expect("remove invalid fixture snapshot before engine preflight");
            match corruption {
                "valid" => {}
                "bad_path" => {
                    assert_eq!(conn.execute(
                        "UPDATE schema_backups SET path='backups/not-generated.sqlite3' WHERE backup_id=?1",
                        params![backup.stage.backup_id],
                    ).expect("corrupt registered path"), 1);
                }
                "orphan_pin" => {
                    conn.execute(
                        "INSERT INTO schema_backup_blobs(backup_id,sha256) VALUES ('orphan',?1)",
                        params!["0".repeat(64)],
                    ).expect("add orphan pin with FK enforcement disabled");
                }
                "missing_pin" => {
                    let hash = backup.blob_hashes.first().expect("fixture backup has a pinned blob");
                    assert_eq!(conn.execute(
                        "DELETE FROM schema_backup_blobs WHERE backup_id=?1 AND sha256=?2",
                        params![backup.stage.backup_id, hash],
                    ).expect("remove one registered backup pin"), 1);
                }
                _ => unreachable!(),
            }
            conn.execute_batch(&format!("PRAGMA application_id={APPLICATION_ID};")).expect("mark fixture with legacy application id");
            drop(conn);

            let before = file_hashes(&root.0);
            let mut report = crate::RecoveryReport::default();
            let opened = open_db_with_engines(&root.0, &mut report, &[], &BTreeSet::new());
            if corruption == "valid" {
                let (conn, _) = opened.expect("valid registered backup rows pass preflight and finish publication");
                assert_eq!(conn.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).unwrap(), STORE3_APPLICATION_ID);
                assert_eq!(store_version(&conn).expect("store v3 after promotion"), 3);
                assert_eq!(conn.query_row("SELECT count(*) FROM schema_backups WHERE state='ready'", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
                drop(conn);
                continue;
            }
            let error = match opened {
                Err(error) => error,
                Ok((conn, _)) => {
                    drop(conn);
                    panic!("corrupt backup registry {corruption} was accepted")
                }
            };
            assert_eq!(error.code, ErrorCode::IntegrityError, "wrong rejection for {corruption}: {error:?}");
            assert_eq!(file_hashes(&root.0), before, "failed legacy open changed files for {corruption}");
        }
    }

    fn seed_store1(root: &Path, pending_save: bool) -> (String, Option<(PathBuf, PathBuf, Vec<u8>, Vec<u8>)>) {
        fs::create_dir_all(root).expect("workspace root");
        let blob_bytes = b"schema migration fixture blob";
        let blob_store = BlobStore::open(root.join(BLOB_DIR), hook()).expect("blob store");
        let blob_hash = blob_store.put(blob_bytes).expect("fixture blob");
        drop(blob_store);

        let artifact_id = new_id();
        let workspace_id = new_id();
        let mut conn = open_connection(root.join(DB_FILE)).expect("open store1 db");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")
            .expect("configure fixture db");
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).expect("begin fixture");
        tx.execute_batch(SCHEMA_V1).expect("schema v1");
        tx.execute_batch(SCHEMA_V1_ADDITIONS).expect("v1 additions");
        tx.execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID};")).expect("application id");
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('store_schema_version', '1'), ('contract_schema_version', ?1), ('workspace_id', ?2), ('request_hash_alg', ?3)",
            params![SCHEMA_VERSION.to_string(), workspace_id, crate::receipts::REQUEST_HASH_ALG],
        ).expect("meta");
        tx.execute("INSERT INTO assets(sha256, mime, bytes, origin) VALUES (?1, 'application/octet-stream', ?2, 'fixture')", params![blob_hash, blob_bytes.len() as i64]).expect("asset ref");
        tx.execute(
            "INSERT INTO artifacts(artifact_id,kind,engine_id,title,origin,current_revision,current_snapshot,base_snapshot,source_path,source_hash,saved_revision,saved_path,saved_hash,saved_snapshot,created_seq) VALUES (?1,'canvas','fixture-engine','fixture','created',0,?2,?2,NULL,NULL,NULL,NULL,NULL,NULL,1)",
            params![artifact_id, blob_hash],
        ).expect("artifact row");
        tx.execute(
            "INSERT INTO revisions(artifact_id,revision,snapshot,kind,request_id,actor,job_id,attempt_id,intents,changed_ids) VALUES (?1,0,?2,'create',NULL,'fixture',NULL,NULL,'[]','[]')",
            params![artifact_id, blob_hash],
        ).expect("revision row");
        tx.execute(
            "INSERT INTO undo_log(artifact_id,seq,change_revision,before_snapshot,after_snapshot,intents,changed_ids,state) VALUES (?1,1,0,?2,?2,'[]','[]','done')",
            params![artifact_id, blob_hash],
        ).expect("undo row");

        let pending_paths = if pending_save {
            let temp = root.join("pending-output.dllt-partial");
            let target = root.join("saved-output.hwpx");
            let temp_bytes = b"pending native temp".to_vec();
            let target_bytes = b"already published output".to_vec();
            fs::write(&temp, &temp_bytes).expect("pending temp");
            fs::write(&target, &target_bytes).expect("published target");
            let temp_path = temp.to_string_lossy().into_owned();
            let target_path = target.to_string_lossy().into_owned();
            tx.execute(
                "INSERT INTO pending_saves(temp_path,artifact_id,target_path,revision,content_hash,snapshot,purpose) VALUES (?1,?2,?3,0,?4,?5,'save')",
                params![temp_path, artifact_id, target_path, sha256(&target_bytes), blob_hash],
            ).expect("pending save row");
            Some((temp, target, temp_bytes, target_bytes))
        } else {
            None
        };
        tx.commit().expect("commit fixture db");
        (blob_hash, pending_paths)
    }

    fn seed_empty_store1(root: &Path) {
        fs::create_dir_all(root).expect("empty store1 fixture root");
        drop(BlobStore::open(root.join(BLOB_DIR), hook()).expect("empty fixture blob store"));
        let mut conn = open_connection(root.join(DB_FILE)).expect("empty store1 db");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")
            .expect("configure empty store1");
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).expect("empty store1 transaction");
        tx.execute_batch(SCHEMA_V1).expect("empty v1 schema");
        tx.execute_batch(SCHEMA_V1_ADDITIONS).expect("empty v1 additions");
        tx.execute_batch(&format!("PRAGMA application_id={APPLICATION_ID};")).expect("workspace app id");
        tx.execute(
            "INSERT INTO meta(key,value) VALUES ('store_schema_version','1'), ('contract_schema_version',?1), ('workspace_id',?2), ('request_hash_alg',?3)",
            params![SCHEMA_VERSION.to_string(), new_id(), crate::receipts::REQUEST_HASH_ALG],
        ).expect("empty v1 metadata");
        tx.commit().expect("commit empty v1 schema");
    }

    #[test]
    fn store1_migration_records_backup_pins_and_survives_blob_gc() {
        let root = TempRoot::new("pins");
        let (blob_hash, _) = seed_store1(&root.0, false);
        let mut open_recovery = crate::RecoveryReport::default();
        let (mut conn, _) = open_db(&root.0, &mut open_recovery).expect("migrate store1");
        assert_eq!(open_recovery.removed_pending_temps, 0);
        assert_eq!(open_recovery.recovered_saves, 0);
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "3");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='contract_schema_version'"), "1");
        assert!(has_group_id(&conn), "store3 retains the nullable group field");
        assert_eq!(conn.query_row("SELECT group_id FROM undo_log WHERE seq=1", [], |row| row.get::<_, Option<String>>(0)).expect("old undo row"), None);
        assert!(has_table(&conn, "history_groups"), "store3 creates empty group metadata without rewriting existing undo rows");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_features_backfilled'"), "0");

        let (backup_id, backup_rel, candidate_rel, staging_rel, state): (String, String, String, String, String) = conn.query_row(
            "SELECT backup_id, path, candidate_path, staging_path, state FROM schema_backups", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        ).expect("backup record");
        assert_eq!(state, "ready");
        let backup_path = root.0.join(backup_rel);
        assert!(backup_path.is_file(), "backup path is retained");
        assert!(!backup_sidecars_exist(&backup_path), "published backup has no SQLite sidecars");
        assert!(!root.0.join(candidate_rel).exists(), "registered candidate was moved to the final path");
        assert!(root.0.join(&staging_rel).join(STORE1_OWNER_FILE).is_file(), "recovery marker remains until Workspace::open acknowledges the report");
        let backup = Connection::open_with_flags(&backup_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("backup db");
        assert_eq!(backup.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).expect("backup app id"), BACKUP_APPLICATION_ID);
        let marker = read_backup_marker(&backup).expect("backup marker").expect("new store1 checkpoint has a refusal marker");
        assert_eq!(marker.backup_id, backup_id);
        assert_eq!(marker.source_store_version, 1);
        assert_eq!(marker.restore_min_store_version, 3);
        assert_eq!(scalar_string(&backup, "SELECT value FROM meta WHERE key='store_schema_version'"), "1");
        assert!(!has_group_id(&backup), "the rollback database remains schema1");
        assert_eq!(scalar_string(&backup, "PRAGMA integrity_check"), "ok");
        assert_eq!(backup.query_row("SELECT sha256 FROM assets", [], |row| row.get::<_, String>(0)).expect("backup asset"), blob_hash);
        assert_eq!(conn.query_row("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1", params![backup_id], |row| row.get::<_, String>(0)).expect("blob pin"), blob_hash);
        drop(backup);
        assert!(acknowledge_migration_recovery(&root.0, &conn).expect("ack migration recovery").is_empty());
        assert!(!root.0.join(staging_rel).exists(), "acknowledgement removes only the registered empty staging directory");

        let blob_store = BlobStore::open(root.0.join(BLOB_DIR), hook()).expect("blob store");
        conn.execute("DELETE FROM undo_log", []).expect("simulate history pruning");
        conn.execute("DELETE FROM revisions", []).expect("simulate revision pruning");
        conn.execute("DELETE FROM artifacts", []).expect("simulate artifact removal");
        conn.execute("DELETE FROM assets", []).expect("simulate asset ref pruning");
        let engines: Vec<Arc<dyn designllm_contracts::EngineAdapter>> = Vec::new();
        let by_id = HashMap::new();
        let report = crate::recover(&mut conn, &blob_store, &engines, &by_id).expect("pinned blob survives recovery");
        assert_eq!(report.removed_unreferenced_blobs, 0);
        assert!(blob_store.contains(&blob_hash), "the schema backup pin retains the blob");

        // Reproduce the earlier DB-only failure by removing the pin as if no retention reference existed.
        conn.execute("DELETE FROM schema_backup_blobs WHERE backup_id=?1", params![backup_id]).expect("drop pin");
        conn.execute("DELETE FROM schema_backups WHERE backup_id=?1", params![backup_id]).expect("drop backup record");
        let report = crate::recover(&mut conn, &blob_store, &engines, &by_id).expect("unreferenced blob GC");
        assert_eq!(report.removed_unreferenced_blobs, 1);
        assert!(!blob_store.contains(&blob_hash));

        let restored = TempRoot::new("db-only-restore");
        fs::create_dir_all(restored.0.join(BLOB_DIR)).expect("empty restored blob dir");
        let mut restored_db = Connection::open(restored.0.join(DB_FILE)).expect("restore target db");
        restored_db.restore("main", &backup_path, None::<fn(rusqlite::backup::Progress)>).expect("restore schema1 backup");
        let restored_blobs = BlobStore::open(restored.0.join(BLOB_DIR), hook()).expect("restored blob store");
        let err = crate::recover(&mut restored_db, &restored_blobs, &engines, &by_id).expect_err("DB-only backup cannot restore its missing blob");
        assert_eq!(err.code, ErrorCode::IntegrityError);
    }

    #[test]
    fn empty_migration_warnings_are_omitted_from_recovery_json() {
        let empty = serde_json::to_value(crate::RecoveryReport::default()).expect("serialize legacy-shaped report");
        assert!(!empty.as_object().expect("report object").contains_key("migration_warnings"));
        let mut with_warning = crate::RecoveryReport::default();
        with_warning.migration_warnings.push("preserved staging directory".to_owned());
        let value = serde_json::to_value(with_warning).expect("serialize warning report");
        assert_eq!(value["migration_warnings"][0], "preserved staging directory");
    }

    #[test]
    fn store2_migration_to3_preserves_old_backup_pins_and_undo_rows() {
        let root = TempRoot::new("store2-to3-preservation");
        let (conn, old_candidate, _, _) = seed_registered_candidate(&root.0, false);
        // Simulate a checkpoint created before the marker format. It must remain byte-identical
        // when the surrounding store metadata is upgraded.
        let legacy_file = Connection::open(&old_candidate.stage.candidate_path).expect("open test-owned candidate to make a legacy checkpoint fixture");
        for key in BACKUP_MARKER_KEYS {
            legacy_file.execute("DELETE FROM meta WHERE key=?1", params![key]).expect("remove only test fixture marker row");
        }
        legacy_file.execute_batch(&format!("PRAGMA application_id={APPLICATION_ID};")).expect("restore legacy workspace application_id");
        drop(legacy_file);
        let old_backup_bytes_sha = file_sha256(&old_candidate.stage.candidate_path).expect("hash test-owned legacy backup");
        crate::fsx::rename_durable(&old_candidate.stage.candidate_path, &root.0.join(&old_candidate.stage.final_relative_path), false)
            .expect("publish the pre-existing store1 backup without v3 metadata");
        conn.execute("UPDATE schema_backups SET state='ready' WHERE backup_id=?1", params![old_candidate.stage.backup_id])
            .expect("mark the pre-existing store1 backup ready");

        let (old_backup_id, old_backup_path, old_state): (String, String, String) = conn.query_row(
            "SELECT backup_id,path,state FROM schema_backups WHERE source_store_version=1",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).expect("existing v1 backup metadata");
        assert_eq!(old_state, "ready");
        let old_pins: BTreeSet<String> = {
            let mut statement = conn.prepare("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1").expect("old pins query");
            statement.query_map(params![old_backup_id], |row| row.get(0)).expect("old pin rows")
                .collect::<rusqlite::Result<_>>().expect("old pin hashes")
        };
        assert!(!old_pins.is_empty());
        let undo_before: (String, i64, String, String, String, Option<String>) = conn.query_row(
            "SELECT artifact_id,seq,before_snapshot,after_snapshot,state,group_id FROM undo_log LIMIT 1",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).expect("existing store2 undo row");
        drop(conn);

        let mut report = crate::RecoveryReport::default();
        let (conn, _) = open_db(&root.0, &mut report).expect("migrate store2 directly to store3");
        assert_eq!(store_version(&conn).expect("store version"), 3);
        assert!(has_table(&conn, "history_groups"));
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_features'"), "[]");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_features_backfilled'"), "0");
        assert_eq!(
            conn.query_row("SELECT artifact_id,seq,before_snapshot,after_snapshot,state,group_id FROM undo_log LIMIT 1", [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, Option<String>>(5)?)))
                .expect("undo row after migration"),
            undo_before,
            "store3 adds indexes/metadata without rewriting historical undo rows",
        );
        let old_after: (String, String, String, i64) = conn.query_row(
            "SELECT backup_id,path,state,restore_min_store_version FROM schema_backups WHERE backup_id=?1",
            params![old_backup_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).expect("preserved v1 backup metadata");
        assert_eq!(old_after, (old_backup_id.clone(), old_backup_path.clone(), "ready".to_owned(), 3));
        let old_file = root.0.join(&old_backup_path);
        assert_eq!(file_sha256(&old_file).expect("hash legacy backup after migration"), old_backup_bytes_sha, "legacy backup bytes are never rewritten or marked in place");
        let old_backup = open_connection_with_flags(&old_file, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("read preserved legacy backup");
        assert_eq!(read_backup_marker(&old_backup).expect("read legacy marker state"), None);
        assert_eq!(old_backup.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).expect("legacy app id"), APPLICATION_ID);
        drop(old_backup);
        let old_pins_after: BTreeSet<String> = {
            let mut statement = conn.prepare("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1").expect("old pins after query");
            statement.query_map(params![old_backup_id], |row| row.get(0)).expect("old pins after rows")
                .collect::<rusqlite::Result<_>>().expect("old pins after hashes")
        };
        assert_eq!(old_pins_after, old_pins, "table recreation preserves every existing blob pin");

        let (new_backup_id, new_backup_path, new_state, restore_min): (String, String, String, i64) = conn.query_row(
            "SELECT backup_id,path,state,restore_min_store_version FROM schema_backups WHERE source_store_version=2",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).expect("new store2 backup metadata");
        assert!(new_backup_path.starts_with("backups/store2-"));
        assert_eq!(new_state, "ready");
        assert_eq!(restore_min, 3, "v2 backup is not advertised as safe for pre-store3 apps");
        let new_pins: BTreeSet<String> = {
            let mut statement = conn.prepare("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1").expect("new pins query");
            statement.query_map(params![new_backup_id], |row| row.get(0)).expect("new pin rows")
                .collect::<rusqlite::Result<_>>().expect("new pin hashes")
        };
        assert!(old_pins.is_subset(&new_pins), "store2 recovery checkpoint pins every blob referenced by retained backups");

        let published = root.0.join(&new_backup_path);
        assert!(published.is_file());
        assert!(!backup_sidecars_exist(&published));
        let backup = Connection::open_with_flags(&published, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("store2 checkpoint DB");
        assert_eq!(backup.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).expect("store2 backup application id"), BACKUP_APPLICATION_ID);
        let marker = read_backup_marker(&backup).expect("store2 backup marker").expect("new store2 checkpoint is protected");
        assert_eq!(marker.backup_id, new_backup_id);
        assert_eq!(marker.source_store_version, 2);
        assert_eq!(marker.restore_min_store_version, 3);
        assert_eq!(scalar_string(&backup, "SELECT value FROM meta WHERE key='store_schema_version'"), "2");
        assert_eq!(backup.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).expect("protected backup application id"), BACKUP_APPLICATION_ID);
        let marker = read_backup_marker(&backup).expect("protected backup marker").expect("new migration backup is marked");
        assert_eq!(marker.backup_id, new_backup_id);
        assert_eq!(marker.source_store_version, 2);
        assert_eq!(marker.restore_min_store_version, 3);
        assert_eq!(backup.query_row("SELECT count(*) FROM schema_backups", [], |row| row.get::<_, i64>(0)).expect("old backup rows in checkpoint"), 1);
        assert!(!has_table(&backup, "history_groups"), "checkpoint remains the actual pre-migration store2 schema");
        assert!(backup.query_row("SELECT count(*) FROM schema_backup_blobs WHERE backup_id=?1", params![old_backup_id], |row| row.get::<_, i64>(0)).expect("old checkpoint pins") > 0);
        assert_eq!(scalar_string(&backup, "PRAGMA integrity_check"), "ok");
        drop(backup);

        let protected_copy = TempRoot::new("marked-backup-is-not-a-workspace");
        fs::copy(&published, protected_copy.0.join(DB_FILE)).expect("copy only the marked D checkpoint into a test folder");
        let before_open = file_hashes(&protected_copy.0);
        let error = open_db(&protected_copy.0, &mut crate::RecoveryReport::default())
            .expect_err("normal Workspace open must not treat a protected checkpoint as a workspace");
        assert_eq!(error.code, ErrorCode::SchemaUnsupported);
        assert!(error.message.contains("protected migration backup"), "{error:?}");
        assert_eq!(file_hashes(&protected_copy.0), before_open, "refusal leaves the copied checkpoint byte-identical");

        let mut foreign_keys = conn.prepare("PRAGMA foreign_key_list(schema_backup_blobs)").expect("rebuilt pin foreign key");
        let targets: Vec<String> = foreign_keys.query_map([], |row| row.get(2)).expect("foreign key rows")
            .collect::<rusqlite::Result<_>>().expect("foreign key targets");
        assert_eq!(targets, ["schema_backups"]);
        let mut check = conn.prepare("PRAGMA foreign_key_check").expect("foreign key check");
        assert!(check.query([]).expect("foreign key check rows").next().expect("foreign key check result").is_none());
    }

    #[test]
    fn store2_to3_ddl_failure_rolls_back_rebuilt_backup_tables_and_preserves_pins() {
        let root = TempRoot::new("store2-to3-rollback");
        let (conn, old_candidate, _, _) = seed_registered_candidate(&root.0, false);
        crate::fsx::rename_durable(&old_candidate.stage.candidate_path, &root.0.join(&old_candidate.stage.final_relative_path), false)
            .expect("publish the pre-existing store1 backup");
        conn.execute("UPDATE schema_backups SET state='ready' WHERE backup_id=?1", params![old_candidate.stage.backup_id])
            .expect("mark the old backup ready");
        let old_backup_id = old_candidate.stage.backup_id.clone();
        let old_path: String = conn.query_row("SELECT path FROM schema_backups WHERE backup_id=?1", params![old_backup_id], |row| row.get(0)).expect("old backup path");
        let old_pins: BTreeSet<String> = {
            let mut statement = conn.prepare("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1").expect("old pins query");
            statement.query_map(params![old_backup_id], |row| row.get(0)).expect("old pins")
                .collect::<rusqlite::Result<_>>().expect("old pin hashes")
        };
        conn.execute_batch(
            "CREATE TRIGGER fail_store3_version BEFORE UPDATE OF value ON meta
             WHEN OLD.key='store_schema_version' AND NEW.value='3'
             BEGIN SELECT RAISE(ABORT, 'test store3 transaction rollback'); END;",
        ).expect("install rollback trigger");
        drop(conn);
        let before = file_hashes(&root.0);

        let mut report = crate::RecoveryReport::default();
        let error = open_db(&root.0, &mut report).expect_err("injected transaction failure refuses migration");
        assert!(error.message.contains("test store3 transaction rollback"), "{error:?}");
        let rolled_back = Connection::open(root.0.join(DB_FILE)).expect("old application id after failed store3 transaction");
        assert_eq!(rolled_back.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).unwrap(), APPLICATION_ID);
        assert_eq!(store_version(&rolled_back).unwrap(), 2);
        drop(rolled_back);
        assert_eq!(file_hashes(&root.0), before, "the owned current staging area is cleaned; old backup files are unchanged");

        let conn = Connection::open(root.0.join(DB_FILE)).expect("store2 remains readable");
        assert_eq!(store_version(&conn).expect("version after rollback"), 2);
        assert!(!has_table(&conn, "history_groups"));
        assert!(!has_table(&conn, "store_features"));
        assert!(!has_table(&conn, "store_features_backfilled"));
        assert!(has_group_id(&conn), "store2 group_id column is preserved");
        assert_eq!(conn.query_row("SELECT path FROM schema_backups WHERE backup_id=?1", params![old_backup_id], |row| row.get::<_, String>(0)).expect("old backup still registered"), old_path);
        let pins_after: BTreeSet<String> = {
            let mut statement = conn.prepare("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1").expect("pins after rollback");
            statement.query_map(params![old_backup_id], |row| row.get(0)).expect("pin rows after rollback")
                .collect::<rusqlite::Result<_>>().expect("pin hashes after rollback")
        };
        assert_eq!(pins_after, old_pins);
        let mut columns = conn.prepare("PRAGMA table_info(schema_backups)").expect("old backup table schema");
        let names: Vec<String> = columns.query_map([], |row| row.get(1)).expect("old backup columns")
            .collect::<rusqlite::Result<_>>().expect("column names");
        assert!(!names.iter().any(|name| name == "restore_min_store_version"), "failed table recreation is rolled back");
        assert_eq!(conn.query_row("SELECT count(*) FROM schema_backups", [], |row| row.get::<_, i64>(0)).expect("only prior backup remains"), 1);
    }

    #[test]
    fn pending_saves_are_recovered_before_migration_backup() {
        let root = TempRoot::new("pending");
        let (_blob_hash, paths) = seed_store1(&root.0, true);
        let (temp, target, _temp_bytes, target_bytes) = paths.expect("pending paths");
        let mut open_recovery = crate::RecoveryReport::default();
        let (conn, _) = open_db(&root.0, &mut open_recovery).expect("recover pending files, then migrate");
        assert_eq!(open_recovery.removed_pending_temps, 1);
        assert_eq!(open_recovery.recovered_saves, 1);
        assert!(!temp.exists(), "normal open recovery removes its registered temp file");
        assert_eq!(fs::read(&target).expect("target remains"), target_bytes);
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "3");
        assert_eq!(conn.query_row("SELECT count(*) FROM pending_saves", [], |row| row.get::<_, i64>(0)).expect("no pending rows"), 0);
        assert_eq!(conn.query_row("SELECT saved_revision FROM artifacts", [], |row| row.get::<_, Option<i64>>(0)).expect("saved revision"), Some(0));
        let backup_path: String = conn.query_row("SELECT path FROM schema_backups", [], |row| row.get(0)).expect("backup path");
        let backup = Connection::open_with_flags(root.0.join(backup_path), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("backup db");
        assert_eq!(backup.query_row("SELECT count(*) FROM pending_saves", [], |row| row.get::<_, i64>(0)).expect("backup pending rows"), 0);
        assert_eq!(fs::read(&target).expect("unchanged target"), target_bytes);
        assert!(!temp.exists(), "the backup follows the existing pending-save recovery semantics");
    }

    #[test]
    fn backup_directory_failure_preserves_store1_schema_and_data() {
        let root = TempRoot::new("backup-failure");
        let (blob_hash, _) = seed_store1(&root.0, false);
        let backup_dir = root.0.join("backups");
        fs::write(&backup_dir, b"do not overwrite").expect("backup path sentinel");

        let mut open_recovery = crate::RecoveryReport::default();
        assert!(open_db(&root.0, &mut open_recovery).is_err(), "unusable backup directory refuses migration");
        assert_eq!(fs::read(&backup_dir).expect("sentinel remains"), b"do not overwrite");
        let conn = Connection::open(root.0.join(DB_FILE)).expect("store1 db remains");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "1");
        assert!(!has_group_id(&conn));
        assert!(!has_table(&conn, "schema_backups"));
        assert_eq!(conn.query_row("SELECT sha256 FROM assets", [], |row| row.get::<_, String>(0)).expect("asset unchanged"), blob_hash);
    }

    #[test]
    fn staging_directory_collision_preserves_existing_sidecars() {
        let root = TempRoot::new("stage-collision");
        seed_store1(&root.0, false);
        let stage_path = root.0.join("backups").join(format!("{STORE1_STAGE_PREFIX}collision-fixture{STORE1_STAGE_SUFFIX}"));
        fs::create_dir_all(&stage_path).expect("create pre-existing staging directory");
        let names = [
            STORE1_OWNER_FILE,
            "candidate.sqlite3.partial",
            "candidate.sqlite3.partial-wal",
            "candidate.sqlite3.partial-shm",
            "candidate.sqlite3.partial-journal",
        ];
        let preserved: Vec<_> = names.iter().enumerate().map(|(index, name)| {
            (stage_path.join(name), format!("unowned staging sentinel {index}").into_bytes())
        }).collect();
        for (path, bytes) in &preserved { fs::write(path, bytes).expect("write existing sentinel"); }

        assert!(reserve_store1_backup_stage(&root.0, "collision-fixture".to_owned()).is_err(), "atomic create_dir must reject an existing stage");
        for (path, expected) in &preserved {
            assert_eq!(fs::read(path).expect("unowned stage file remains"), *expected, "unowned path changed: {}", path.display());
        }
        let conn = Connection::open(root.0.join(DB_FILE)).expect("open schema1 db");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "1");
        assert!(!has_group_id(&conn));
        assert!(!has_table(&conn, "schema_backups"));
    }

    #[test]
    fn sidecar_only_collision_preserves_sidecar_after_partial_reservation() {
        let root = TempRoot::new("sidecar-only-collision");
        seed_store1(&root.0, false);
        let mut stage = reserve_store1_backup_stage(&root.0, new_id()).expect("reserve private stage");
        write_stage_manifest(&stage, "running", 0, 0).expect("mark recovery");
        write_stage_manifest(&stage, "complete", 0, 0).expect("complete recovery");
        let sidecar = backup_sidecar_path(&stage.partial_path, "-wal");
        let sentinel = b"pre-existing sidecar inside newly reserved directory";
        fs::write(&sidecar, sentinel).expect("create colliding sidecar");
        let conn = Connection::open(root.0.join(DB_FILE)).expect("open schema1 db");

        let error = backup_store1(&root.0, &conn, &mut stage).expect_err("sidecar collision must refuse SQLite open");
        let error = error_with_stage_cleanup(error, &root.0, &stage);
        assert!(error.message.contains("sidecar preserved"));
        assert_eq!(fs::read(&sidecar).expect("unowned sidecar remains"), sentinel);
        assert!(!stage.partial_path.exists(), "only this call's reserved base partial is removed");
        assert!(stage.owner_path.exists(), "owner marker remains beside the unowned sidecar");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "1");
        assert!(!has_group_id(&conn));
    }

    #[test]
    fn unregistered_candidate_is_reported_preserved_and_does_not_block_retry() {
        let root = TempRoot::new("unregistered-candidate");
        let (_blob, _) = seed_store1(&root.0, true);
        let mut conn = Connection::open(root.0.join(DB_FILE)).expect("open schema1 db");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;").expect("configure db");
        let mut stage = reserve_store1_backup_stage(&root.0, new_id()).expect("reserve first stage");
        write_stage_manifest(&stage, "running", 0, 0).expect("mark recovery");
        let mut recovered_saves = 0;
        let removed_pending_temps = crate::files::recover_pending(&mut conn, &mut recovered_saves).expect("recover pending");
        assert_eq!((removed_pending_temps, recovered_saves), (1, 1));
        write_stage_manifest(&stage, "complete", removed_pending_temps, recovered_saves).expect("record recovery");
        let candidate = backup_store1(&root.0, &conn, &mut stage).expect("write unregistered candidate");
        let old_candidate = candidate.stage.candidate_path.clone();
        let old_stage = candidate.stage.staging_path.clone();
        drop(conn); // Simulate process exit after candidate fsync but before the registration transaction.

        let mut report = crate::RecoveryReport::default();
        let (conn, _) = open_db(&root.0, &mut report).expect("retry with a new unique staging directory");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "3");
        assert!(report.migration_warnings.iter().any(|warning| warning.contains("unregistered migration staging") && warning.contains("removed_pending_temps=1") && warning.contains("recovered_saves=1")));
        assert!(old_candidate.is_file(), "the unregistered candidate is preserved and never treated as committed backup");
        assert!(old_stage.is_dir());
        assert_eq!(conn.query_row("SELECT count(*) FROM schema_backups", [], |row| row.get::<_, i64>(0)).expect("only retry backup registered"), 1);
    }

    #[test]
    fn running_manifest_after_recovery_reports_counts_unknown_not_zero() {
        let root = TempRoot::new("running-recovery-counts-unknown");
        let (_blob, _) = seed_store1(&root.0, true);
        let mut conn = Connection::open(root.0.join(DB_FILE)).expect("open schema1 db");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;").expect("configure db");
        let stage = reserve_store1_backup_stage(&root.0, new_id()).expect("reserve first stage");
        write_stage_manifest(&stage, "running", 0, 0).expect("record recovery running before filesystem operations");
        let mut recovered_saves = 0;
        let removed_pending_temps = crate::files::recover_pending(&mut conn, &mut recovered_saves).expect("recover pending files");
        assert_eq!((removed_pending_temps, recovered_saves), (1, 1));
        let stage_path = stage.staging_path.clone();
        drop(conn); // Simulate a crash after recovery completed, before the completed-count marker was written.

        let mut report = crate::RecoveryReport::default();
        let (conn, _) = open_db(&root.0, &mut report).expect("continue with a new isolated stage");
        let warning = report.migration_warnings.iter().find(|warning| warning.contains(&stage.staging_relative_path)).expect("orphan stage warning");
        assert!(warning.contains("exact counts are unknown"), "{warning}");
        assert!(warning.contains("numeric fields are untrusted"), "{warning}");
        assert!(!warning.contains("removed_pending_temps=0"), "must not present the pre-recovery marker as fact: {warning}");
        assert!(!warning.contains("recovered_saves=0"), "must not present the pre-recovery marker as fact: {warning}");
        assert!(stage_path.join(STORE1_OWNER_FILE).is_file(), "unregistered crash evidence is preserved");
        assert_eq!((report.removed_pending_temps, report.recovered_saves), (0, 0), "these are counts from this open, not the prior uncertain attempt");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "3");
    }

    #[test]
    fn registered_candidate_is_published_on_restart_after_registration_crash() {
        let root = TempRoot::new("registered-before-rename");
        let (conn, backup, removed, recovered) = seed_registered_store2_to3_candidate(&root.0, true);
        assert_eq!((removed, recovered), (1, 1));
        assert_eq!(conn.query_row("SELECT state FROM schema_backups WHERE backup_id=?1", params![backup.stage.backup_id], |row| row.get::<_, String>(0)).expect("new publishing row"), "publishing");
        assert!(backup.stage.candidate_path.is_file());
        assert!(!root.0.join(&backup.stage.final_relative_path).exists());
        drop(conn); // Simulate process exit after schema3, row and pins commit but before file publication.

        let mut report = crate::RecoveryReport::default();
        let (conn, _) = open_db(&root.0, &mut report).expect("resume registered candidate");
        assert_eq!(report.removed_pending_temps, 1);
        assert_eq!(report.recovered_saves, 1);
        assert_eq!(conn.query_row("SELECT state FROM schema_backups WHERE backup_id=?1", params![backup.stage.backup_id], |row| row.get::<_, String>(0)).expect("new ready row"), "ready");
        assert_eq!(conn.query_row("SELECT count(*) FROM schema_backups WHERE source_store_version=1 AND state='ready'", [], |row| row.get::<_, i64>(0)).expect("old backup published too"), 1);
        assert!(!backup.stage.candidate_path.exists());
        assert!(root.0.join(&backup.stage.final_relative_path).is_file());
        assert!(backup.stage.staging_path.join(STORE1_OWNER_FILE).is_file(), "report marker stays until open completes");
        assert!(acknowledge_migration_recovery(&root.0, &conn).expect("acknowledge report").is_empty());
        assert!(!backup.stage.staging_path.exists());
    }

    #[test]
    fn registered_final_file_is_reconciled_after_rename_crash() {
        let root = TempRoot::new("renamed-before-ready");
        let (conn, backup, _, _) = seed_registered_store2_to3_candidate(&root.0, false);
        crate::fsx::rename_durable(&backup.stage.candidate_path, &root.0.join(&backup.stage.final_relative_path), false).expect("simulate publish rename");
        drop(conn); // Simulate process exit after rename but before the row's ready transition.

        let mut report = crate::RecoveryReport::default();
        let (conn, _) = open_db(&root.0, &mut report).expect("reconcile already-renamed backup");
        assert_eq!(conn.query_row("SELECT state FROM schema_backups WHERE backup_id=?1", params![backup.stage.backup_id], |row| row.get::<_, String>(0)).expect("new backup ready"), "ready");
        assert_eq!(conn.query_row("SELECT count(*) FROM schema_backups WHERE source_store_version=1 AND state='ready'", [], |row| row.get::<_, i64>(0)).expect("old backup also ready"), 1);
        assert!(root.0.join(&backup.stage.final_relative_path).is_file());
        assert!(!backup.stage.candidate_path.exists());
        assert!(acknowledge_migration_recovery(&root.0, &conn).expect("acknowledge").is_empty());
    }

    #[test]
    fn backup_failure_reports_pending_recovery_that_already_happened() {
        let root = TempRoot::new("backup-failure-after-recovery");
        let (blob_hash, paths) = seed_store1(&root.0, true);
        let (temp, target, _temp_bytes, target_bytes) = paths.expect("pending paths");
        fs::remove_file(root.0.join(BLOB_DIR).join(blob_hash)).expect("force pre-backup integrity failure");
        let mut report = crate::RecoveryReport::default();
        let error = open_db(&root.0, &mut report).expect_err("missing referenced blob aborts backup");
        assert!(error.message.contains("removed_pending_temps=1"));
        assert!(error.message.contains("recovered_saves=1"));
        let recovery = error.diagnostics.iter().find(|diagnostic| diagnostic.code == "core.pending_recovery_before_migration_failure").expect("structured recovery counts");
        assert_eq!(recovery.params.get("counts_known").and_then(Value::as_bool), Some(true));
        assert_eq!(recovery.params.get("removed_pending_temps").and_then(Value::as_u64), Some(1));
        assert_eq!(recovery.params.get("recovered_saves").and_then(Value::as_u64), Some(1));
        assert!(!temp.exists());
        assert_eq!(fs::read(&target).expect("native output remains unchanged"), target_bytes);
        let conn = Connection::open(root.0.join(DB_FILE)).expect("schema1 remains openable");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "1");
        assert_eq!(conn.query_row("SELECT count(*) FROM pending_saves", [], |row| row.get::<_, i64>(0)).expect("recovered pending row"), 0);
        assert_eq!(conn.query_row("SELECT saved_revision FROM artifacts", [], |row| row.get::<_, Option<i64>>(0)).expect("saved state persisted"), Some(0));
        assert!(!has_group_id(&conn));
        let stage_dirs = fs::read_dir(root.0.join("backups")).expect("backup directory").filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(STORE1_STAGE_PREFIX)).count();
        assert_eq!(stage_dirs, 0, "the failed current attempt cleans only its own empty stage");
    }

    #[test]
    #[cfg(windows)]
    fn too_long_migration_backup_paths_fail_before_changing_v1_or_v2_data() {
        let base_v3 = TempRoot::new("long-v3-no-migration");
        let v3_root = long_workspace_root(&base_v3.0);
        let mut report = crate::RecoveryReport::default();
        let (conn, _) = open_db(&v3_root, &mut report).expect("long but usable store3 path needs no backup stage");
        assert_eq!(store_version(&conn).expect("fresh store3 version"), 3);
        drop(conn);
        let (conn, _) = open_db(&v3_root, &mut report).expect("do not reject store3 just because a future backup path would be long");
        assert_eq!(store_version(&conn).expect("reopened store3 version"), 3);
        drop(conn);

        let base_v1 = TempRoot::new("long-v1");
        let v1_root = long_workspace_root(&base_v1.0);
        let (v1_blob, _) = seed_store1(&v1_root, false);
        let before_v1 = file_hashes(&v1_root);
        let mut report = crate::RecoveryReport::default();
        let error = open_db(&v1_root, &mut report).expect_err("unsafe long backup path is refused");
        assert_eq!(error.code, ErrorCode::StorageFailure);
        assert!(error.message.contains("경로가 너무 길어"), "{error:?}");
        assert!(error.diagnostics.iter().any(|d| d.code == "core.workspace_path_too_long"));
        assert_eq!(file_hashes(&v1_root), before_v1, "store1 DB, blob, and backup files are unchanged");
        let v1 = open_connection(v1_root.join(DB_FILE)).expect("v1 remains readable");
        assert_eq!(store_version(&v1).expect("v1 version"), 1);
        assert_eq!(v1.query_row("SELECT sha256 FROM assets", [], |row| row.get::<_, String>(0)).expect("v1 asset"), v1_blob);
        assert!(!v1_root.join("backups").exists(), "path refusal happens before a staging directory is created");

        let short_v2 = TempRoot::new("v2-long-source");
        let (v2_conn, old_candidate, _, _) = seed_registered_candidate(&short_v2.0, false);
        crate::fsx::rename_durable(&old_candidate.stage.candidate_path, &short_v2.0.join(&old_candidate.stage.final_relative_path), false)
            .expect("publish the existing store1 backup in the v2 fixture");
        v2_conn.execute("UPDATE schema_backups SET state='ready' WHERE backup_id=?1", params![old_candidate.stage.backup_id])
            .expect("mark the existing v1 backup ready in the v2 fixture");
        drop(v2_conn);
        let base_v2 = TempRoot::new("long-v2");
        let v2_root = long_workspace_root(&base_v2.0);
        copy_tree(&short_v2.0, &v2_root);
        let v2_conn = open_connection(v2_root.join(DB_FILE)).expect("v2 copy DB");
        let old_backup_id: String = v2_conn.query_row("SELECT backup_id FROM schema_backups", [], |row| row.get(0)).expect("old backup row");
        let old_pins: BTreeSet<String> = {
            let mut statement = v2_conn.prepare("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1").expect("old pins query");
            statement.query_map(params![old_backup_id], |row| row.get(0)).expect("old pin rows")
                .collect::<rusqlite::Result<_>>().expect("old pin hashes")
        };
        drop(v2_conn);
        let before_v2 = file_hashes(&v2_root);
        let mut report = crate::RecoveryReport::default();
        let error = open_db(&v2_root, &mut report).expect_err("unsafe long backup path is refused");
        assert_eq!(error.code, ErrorCode::StorageFailure);
        assert!(error.message.contains("경로가 너무 길어"), "{error:?}");
        assert!(error.diagnostics.iter().any(|d| d.code == "core.workspace_path_too_long"));
        assert_eq!(file_hashes(&v2_root), before_v2, "store2 DB, existing backup, and pins are byte-identical");
        let v2 = open_connection(v2_root.join(DB_FILE)).expect("v2 remains readable");
        assert_eq!(store_version(&v2).expect("v2 version"), 2);
        assert_eq!(v2.query_row("SELECT count(*) FROM schema_backups", [], |row| row.get::<_, i64>(0)).expect("old backup row remains"), 1);
        let pins_after: BTreeSet<String> = {
            let mut statement = v2.prepare("SELECT sha256 FROM schema_backup_blobs WHERE backup_id=?1").expect("old pins after query");
            statement.query_map(params![old_backup_id], |row| row.get(0)).expect("old pins after rows")
                .collect::<rusqlite::Result<_>>().expect("old pins after hashes")
        };
        assert_eq!(pins_after, old_pins);
    }

    #[test]
    fn windows_sqlite_path_prefixes_long_unc_and_normalizes_separators() {
        if !cfg!(windows) { return; }
        let path = PathBuf::from(r"\\server\share")
            .join("한글-".to_owned() + &"x".repeat(270))
            .join("backups/candidate.sqlite3");
        let encoded = sqlite_path(&path).expect("long UNC path conversion").to_string_lossy().into_owned();
        assert!(encoded.starts_with(r"\\?\UNC\server\share\"), "{encoded}");
        assert!(!encoded.contains('/'), "extended UNC paths use backslash separators: {encoded}");
    }

    #[test]
    fn windows_long_paths_with_dot_segments_are_refused_before_sqlite_open() {
        if !cfg!(windows) { return; }
        let base = TempRoot::new("long-dot-path-refusal");
        let mut root = base.0.clone();
        while path_units(&root.join(DB_FILE)) <= 280 {
            root = root.join(format!("segment-{}", "x".repeat(40)));
        }
        let relative = root.strip_prefix(&base.0).expect("long root relative path").to_string_lossy().replace('/', "\\");
        let with_current = PathBuf::from(format!("{}\\.\\{}", base.0.display(), relative));
        let with_parent = PathBuf::from(format!("{}\\link\\..\\{}", base.0.display(), relative));
        for path in [&with_current, &with_parent] {
            assert!(path_units(&path.join(DB_FILE)) > 259);
            let error = ensure_database_paths_supported(&path.join(DB_FILE)).expect_err("ambiguous long path is refused before open");
            assert!(error.message.contains("'.' 또는 '..'"), "{error:?}");
        }

        let verbatim = PathBuf::from(format!(r"\\?\{}", with_parent.display()));
        let error = sqlite_path(&verbatim).expect_err("verbatim dot segments are not silently normalized");
        assert!(error.message.contains("'.' 또는 '..'"), "{error:?}");
    }

    #[test]
    fn windows_store3_path_over_classic_max_path_opens_and_uses_wal() {
        if !cfg!(windows) { return; }
        let base = TempRoot::new("long-store3-open");
        let mut root = base.0.clone();
        while path_units(&root.join(DB_FILE)) <= 280 {
            root = root.join(format!("긴경로-{}", "x".repeat(40)));
        }
        fs::create_dir_all(&root).expect("create test-owned long workspace directory");
        let database = root.join(DB_FILE);
        assert!(path_units(&database) > 260, "fixture database path should exceed MAX_PATH: {} ({})", database.display(), path_units(&database));
        let probe = root.join("filesystem-long-path-probe.txt");
        let renamed_probe = root.join("filesystem-long-path-renamed.txt");
        fs::write(&probe, b"owned fixture").expect("Rust filesystem APIs support this long path");
        crate::fsx::rename_durable(&probe, &renamed_probe, false).expect("durable stage/publish rename supports this long path");
        let replacement_source = root.join("filesystem-long-path-replacement-source.txt");
        fs::write(&replacement_source, b"replacement").expect("create long-path replacement source");
        let collision = crate::fsx::rename_durable(&replacement_source, &renamed_probe, false).expect_err("no-replace keeps an existing target");
        assert_eq!(collision.kind(), std::io::ErrorKind::AlreadyExists);
        fs::write(&replacement_source, b"replacement").expect("restore replacement source after collision");
        crate::fsx::rename_durable(&replacement_source, &renamed_probe, true).expect("replace an existing long-path target");
        assert_eq!(fs::read(&renamed_probe).expect("read replaced long-path target"), b"replacement");

        let direct = open_connection(&database).expect("win32-longpath VFS opens a database beyond MAX_PATH");
        let direct_mode: String = direct.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)).expect("set WAL beyond MAX_PATH");
        assert_eq!(direct_mode, "wal");
        drop(direct);

        let mut report = crate::RecoveryReport::default();
        let (conn, _) = open_db(&root, &mut report).expect("SQLite open, schema creation, and WAL setup should support this long store3 path");
        assert_eq!(store_version(&conn).expect("store version"), 3);
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0)).expect("read journal mode");
        assert_eq!(mode, "wal");
        drop(conn);

        let (conn, _) = open_db(&root, &mut report).expect("reopen existing long-path store3 workspace");
        assert_eq!(store_version(&conn).expect("reopened store version"), 3);
        drop(conn);

        let short_source = TempRoot::new("long-store1-source");
        seed_store1(&short_source.0, false);
        let migration_base = TempRoot::new("long-store1-migration");
        let mut migration_root = migration_base.0.clone();
        while path_units(&migration_root.join(DB_FILE)) <= 280 {
            migration_root = migration_root.join(format!("마이그레이션-{}", "m".repeat(40)));
        }
        copy_tree(&short_source.0, &migration_root);
        let mut stage_probe = reserve_store_backup_stage(&migration_root, new_id(), 1)
            .expect("reserve a long-path schema-backup staging directory and owner marker");
        write_stage_manifest(&stage_probe, "running", 0, 0).expect("rewrite long-path stage manifest");
        write_stage_manifest(&stage_probe, "complete", 0, 0).expect("complete long-path stage manifest");
        let reservation = OpenOptions::new().write(true).create_new(true).open(&stage_probe.partial_path)
            .expect("reserve long-path candidate file");
        reservation.sync_all().expect("sync long-path candidate reservation");
        drop(reservation);
        stage_probe.owns_partial = true;
        let candidate = open_connection(&stage_probe.partial_path).expect("SQLite open long-path candidate");
        candidate.execute_batch("CREATE TABLE path_probe(value TEXT);").expect("write long-path candidate");
        drop(candidate);
        crate::fsx::sync_file(&stage_probe.partial_path).expect("sync long-path candidate");
        if let Err(error) = crate::fsx::rename_durable(&stage_probe.partial_path, &stage_probe.candidate_path, false) {
            panic!("durably rename long-path candidate {} ({} units) -> {} ({} units): {error:?}; SQLite source={:?}",
                stage_probe.partial_path.display(), path_units(&stage_probe.partial_path),
                stage_probe.candidate_path.display(), path_units(&stage_probe.candidate_path),
                sqlite_path(&stage_probe.partial_path).expect("SQLite path prefix"));
        }
        stage_probe.owns_partial = false;
        stage_probe.owns_candidate = true;
        cleanup_current_stage(&stage_probe).expect("clean owned long-path stage probe");

        let mut migration_report = crate::RecoveryReport::default();
        let (conn, _) = open_db(&migration_root, &mut migration_report)
            .expect("store1 backup creation, verification, and publish should support this long path");
        assert_eq!(store_version(&conn).expect("migrated store version"), 3);
        let ready_backups: i64 = conn.query_row("SELECT count(*) FROM schema_backups WHERE state='ready'", [], |row| row.get(0)).expect("ready backup count");
        assert_eq!(ready_backups, 1, "the long-path backup completed before publication");
    }

    #[test]
    fn partial_pending_recovery_error_marks_counts_unknown_without_numbers() {
        let root = TempRoot::new("pending-recovery-error-unknown");
        let (_blob_hash, paths) = seed_store1(&root.0, true);
        let (temp, _target, _temp_bytes, _target_bytes) = paths.expect("pending paths");
        fs::remove_file(&temp).expect("remove fixture temp file");
        fs::create_dir(&temp).expect("make temp removal fail without touching external paths");

        let mut report = crate::RecoveryReport::default();
        let error = open_db(&root.0, &mut report).expect_err("recovery failure refuses migration");
        let recovery = error.diagnostics.iter().find(|diagnostic| diagnostic.code == "core.pending_recovery_before_migration_failure").expect("unknown recovery diagnostic");
        assert_eq!(recovery.params.get("counts_known").and_then(Value::as_bool), Some(false));
        assert!(!recovery.params.contains_key("removed_pending_temps"));
        assert!(!recovery.params.contains_key("recovered_saves"));
        assert!(recovery.message.contains("exact counts are unknown"));
        assert!(!error.message.contains("recovered_saves_partial="));

        let conn = Connection::open(root.0.join(DB_FILE)).expect("schema1 remains intact");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "1");
        assert_eq!(conn.query_row("SELECT count(*) FROM pending_saves", [], |row| row.get::<_, i64>(0)).expect("pending row remains for retry"), 1);
        assert!(temp.is_dir(), "failed temp removal is preserved for retry");
        assert!(!has_group_id(&conn));
    }

    #[test]
    fn checked_in_store1_fixture_copy_migrates_without_mutating_source() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/integration/baseline/workspace-v1_1");
        assert!(fixture.join(DB_FILE).is_file(), "checked-in v1 fixture is present in the full repository");
        let source_hashes = file_hashes(&fixture);
        let copied = TempRoot::new("checked-in-v1-copy");
        let workspace = copied.0.join("workspace");
        copy_tree(&fixture, &workspace);
        assert_eq!(file_hashes(&workspace), source_hashes, "copy begins byte-identical to the checked-in fixture");
        let remapped_pending = remap_pending_paths_to_copy(&workspace);

        let before = Connection::open(workspace.join(DB_FILE)).expect("open copied fixture db");
        assert_eq!(store_version(&before).expect("fixture version"), 1);
        let counts_before: (i64, i64, i64) = before.query_row(
            "SELECT (SELECT count(*) FROM artifacts), (SELECT count(*) FROM revisions), (SELECT count(*) FROM undo_log)",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).expect("source rows before migration");
        drop(before);

        let mut report = crate::RecoveryReport::default();
        let (conn, _) = open_db(&workspace, &mut report).expect("migrate checked-in fixture copy");
        assert_eq!(store_version(&conn).expect("migrated version"), 3);
        assert_eq!(report.removed_pending_temps, remapped_pending);
        assert_eq!(conn.query_row("SELECT count(*) FROM pending_saves", [], |row| row.get::<_, i64>(0)).expect("pending rows recovered"), 0);
        let counts_after: (i64, i64, i64) = conn.query_row(
            "SELECT (SELECT count(*) FROM artifacts), (SELECT count(*) FROM revisions), (SELECT count(*) FROM undo_log)",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).expect("rows after migration");
        assert_eq!(counts_after, counts_before, "migration preserves fixture content rows");
        let (final_rel, state): (String, String) = conn.query_row("SELECT path,state FROM schema_backups", [], |row| Ok((row.get(0)?, row.get(1)?))).expect("registered backup");
        assert_eq!(state, "ready");
        let backup = Connection::open_with_flags(workspace.join(final_rel), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("published schema1 backup");
        assert_eq!(scalar_string(&backup, "SELECT value FROM meta WHERE key='store_schema_version'"), "1");
        assert_eq!(scalar_string(&backup, "PRAGMA integrity_check"), "ok");
        drop(backup);
        drop(conn); // Publish a closed, checkpointed workspace copy rather than copying a live WAL pair.
        if let Some(output) = std::env::var_os("DESIGNLLM_STORE2_FIXTURE_OUT") {
            let output = PathBuf::from(output);
            assert!(!output.exists(), "refusing to overwrite a prior isolated store2 fixture");
            copy_tree(&workspace, &output);
        }
        assert_eq!(file_hashes(&fixture), source_hashes, "checked-in fixture source was not mutated by copy migration");
    }

    #[test]
    fn migration_ddl_failure_rolls_back_version_and_pin_tables() {
        let root = TempRoot::new("migration-rollback");
        seed_store1(&root.0, false);
        let backup_dir = root.0.join("backups");
        fs::create_dir_all(&backup_dir).expect("backup directory");
        let sentinel = backup_dir.join("preserve-unrelated.txt");
        fs::write(&sentinel, b"unrelated backup sentinel").expect("write sentinel");
        {
            let conn = Connection::open(root.0.join(DB_FILE)).expect("store1 db");
            conn.execute_batch(
                "CREATE TRIGGER fail_store2_version BEFORE UPDATE OF value ON meta \
                 WHEN OLD.key='store_schema_version' AND NEW.value='3' \
                 BEGIN SELECT RAISE(ABORT, 'test migration rollback'); END;",
            ).expect("install rollback trigger");
        }
        let mut open_recovery = crate::RecoveryReport::default();
        let error = match open_db(&root.0, &mut open_recovery) {
            Err(error) => error,
            Ok(_) => panic!("version update trigger should abort the migration"),
        };
        assert!(format!("{error:?}").contains("test migration rollback"), "unexpected migration error: {error:?}");

        let conn = Connection::open(root.0.join(DB_FILE)).expect("store1 db after rollback");
        assert_eq!(scalar_string(&conn, "SELECT value FROM meta WHERE key='store_schema_version'"), "1");
        assert_eq!(conn.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)).unwrap(), APPLICATION_ID, "the failed transaction rolls back the store3 application id");
        assert!(!has_group_id(&conn));
        assert!(!has_table(&conn, "schema_backups"));
        assert!(!has_table(&conn, "schema_backup_blobs"));
        assert_eq!(fs::read(&sentinel).expect("unrelated backup sentinel remains"), b"unrelated backup sentinel");
        let entries: Vec<_> = fs::read_dir(&backup_dir).expect("backup directory after rollback").collect();
        assert_eq!(entries.len(), 1, "rollback removes only its registered candidate/stage; other files remain");
        assert_eq!(entries[0].as_ref().unwrap().file_name(), "preserve-unrelated.txt");
    }

    #[test]
    fn interrupted_restore_staging_is_preserved_and_retry_uses_a_new_stage() {
        let source = TempRoot::new("restore-interrupted-source");
        seed_empty_store1(&source.0);
        let mut report = crate::RecoveryReport::default();
        let (source_db, _) = open_db(&source.0, &mut report).expect("migrate empty v1 source to store3");
        let (backup_id, backup_relative): (String, String) = source_db.query_row(
            "SELECT backup_id,path FROM schema_backups WHERE source_store_version=1 AND state='ready'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).expect("ready v1 checkpoint");
        drop(source_db);
        let backup_path = source.0.join(&backup_relative);
        let backup_bytes_before = fs::read(&backup_path).expect("checkpoint bytes");
        let destination_parent = TempRoot::new("restore-interrupted-destination");
        let destination = destination_parent.0.join("restored");
        fs::write(source.0.join(crate::CORE_LOCK_FILE), b"").expect("create the existing owner-lock file for this direct store fixture");
        let source_before = file_hashes(&source.0);
        let engines: Vec<Arc<dyn EngineAdapter>> = Vec::new();
        let supported = BTreeSet::new();

        let interrupted = prepare_backup_restore_stage(&source.0, &backup_id, &destination, &engines, &supported)
            .expect("prepare isolated restore stage");
        assert!(interrupted.diagnostics.is_empty());
        let interrupted_root = interrupted.staging_root.clone();
        assert!(interrupted_root.join(RESTORE_STAGE_OWNER_FILE).is_file());
        assert!(fs::read_to_string(interrupted_root.join(RESTORE_STAGE_OWNER_FILE)).unwrap().contains("state=ready"));
        drop(interrupted); // Simulate process interruption after staging was verified but before publish.
        assert!(interrupted_root.is_dir(), "an unregistered previous stage is preserved");
        assert!(!destination.exists());
        assert_eq!(file_hashes(&source.0), source_before);
        assert_eq!(fs::read(&backup_path).unwrap(), backup_bytes_before);

        let racing_destination = destination.clone();
        let racing_stage = prepare_backup_restore_stage(&source.0, &backup_id, &racing_destination, &engines, &supported)
            .expect("a later retry can use a new unique stage while an older orphan is preserved");
        assert_ne!(racing_stage.staging_root, interrupted_root);
        assert!(interrupted_root.is_dir());
        let orphan_warning = racing_stage.diagnostics.iter().find(|diagnostic| diagnostic.code == "core.restore_backup_incomplete_left")
            .expect("retry reports the previous restore staging folder");
        assert!(orphan_warning.message.contains(&interrupted_root.display().to_string()));
        fs::create_dir(&racing_destination).expect("simulate destination creation during staging");
        let sentinel = racing_destination.join("sentinel.txt");
        fs::write(&sentinel, b"preserve concurrent destination").expect("create race sentinel");
        mark_backup_restore_stage_ready(&racing_stage).expect("mark only this attempt ready to publish");
        let error = publish_backup_restore_stage(&racing_stage).expect_err("no-replace publish refuses a destination created during staging");
        assert_eq!(error.code, ErrorCode::AlreadyExists);
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve concurrent destination");
        assert!(racing_stage.staging_root.is_dir(), "failed publication preserves this stage for diagnosis");
        assert_eq!(file_hashes(&source.0), source_before);
        assert_eq!(fs::read(&backup_path).unwrap(), backup_bytes_before);
    }

    #[test]
    fn restore_read_snapshot_uses_committed_wal_without_changing_source_files() {
        let source = TempRoot::new("restore-source-wal");
        let destination = TempRoot::new("restore-source-wal-copy");
        let db_path = source.0.join(DB_FILE);
        let bootstrap = Connection::open(&db_path).expect("create test-owned WAL source");
        bootstrap.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE meta (key TEXT PRIMARY KEY,value TEXT NOT NULL);").expect("WAL source schema");
        drop(bootstrap);

        let reader = Connection::open(&db_path).expect("hold an old WAL read snapshot");
        reader.execute_batch("BEGIN DEFERRED;").expect("begin reader snapshot");
        let before_count: i64 = reader.query_row("SELECT count(*) FROM meta", [], |row| row.get(0)).unwrap();
        assert_eq!(before_count, 0);
        let writer = Connection::open(&db_path).expect("open WAL writer");
        writer.execute_batch("PRAGMA wal_autocheckpoint=0;").expect("disable only fixture auto-checkpoint");
        writer.execute("INSERT INTO meta(key,value) VALUES ('committed','visible from WAL')", []).unwrap();
        drop(writer);
        let wal_path = backup_sidecar_path(&db_path, "-wal");
        assert!(wal_path.is_file(), "reader transaction keeps the committed WAL available");
        let source_before = file_hashes(&source.0);

        let snapshot = ReadOnlySqlite::open(&db_path, &destination.0, std::ffi::OsStr::new("registered-backup"))
            .expect("copy the WAL DB privately without touching its source");
        assert_eq!(snapshot.connection.query_row("SELECT value FROM meta WHERE key='committed'", [], |row| row.get::<_, String>(0)).unwrap(), "visible from WAL");
        let normalized = destination.0.join("normalized.sqlite3");
        snapshot.copy_to(&normalized).expect("normalize only the private snapshot into a sidecar-free checkpoint");
        drop(snapshot);
        let normalized_db = open_connection_with_flags(&normalized, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(normalized_db.query_row("SELECT value FROM meta WHERE key='committed'", [], |row| row.get::<_, String>(0)).unwrap(), "visible from WAL");
        drop(normalized_db);

        assert_eq!(file_hashes(&source.0), source_before, "snapshotting reads the DB plus committed WAL without changing source sidecars");
        drop(reader);
    }
}
