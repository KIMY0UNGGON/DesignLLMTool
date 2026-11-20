//! Read-only public API for registered schema-migration checkpoints.

use designllm_contracts::DltResult;
use serde::Serialize;
use std::path::Path;

/// A committed migration checkpoint that can be selected by the new-folder restore API.
///
/// `file_available` only reports that the registered file exists at its verified in-workspace
/// path. Restore performs the full integrity, blob-pin, feature, and snapshot validation again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationCheckpointSummary {
    pub backup_id: String,
    pub source_store_version: i64,
    pub restore_min_store_version: i64,
    pub created_at_ms: i64,
    pub file_available: bool,
}

/// Lists committed checkpoints from an already migrated Store3 workspace without opening or
/// migrating it. The source must be closed so Core can take its owner lock for a consistent
/// read-only SQLite snapshot.
pub fn list_migration_checkpoints(source_root: &Path) -> DltResult<Vec<MigrationCheckpointSummary>> {
    crate::store::list_migration_checkpoints(source_root)
}
