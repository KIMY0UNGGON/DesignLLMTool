//! Figure datasets: raw-cell import, cell classification, canonical version hashes, transform
//! verification, and reconciliation with the immutable source asset.
//!
//! A dataset is the original CSV/TSV asset plus the transforms recorded on it. Every cell must be
//! explained by the source text, by a recorded EditCell, or by an exact UnitScale result.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{
    CellState, DataCell, DataColumn, DataRow, DataTransform, Dataset, DatasetRef, DltError, DltResult, Diagnostic, ErrorCode,
    json_hash, sha256,
};
use serde::Serialize;

use crate::decimal::{Dec, Exact, MAX_DIGITS, ParseClass, unit_scale_exact};
use crate::diag::{self, code};
use crate::table;

/// Origin of a version that a recorded derivation (`service::edit_dataset`) produced from the version it replaces.
pub const DERIVED_ORIGIN: &str = "derived_transform";
/// Dataset origins. Simulated, example and estimated data are never presented as experimental data.
pub const ORIGINS: [&str; 5] = ["user_experimental", "simulation", "example", "image_estimate", DERIVED_ORIGIN];
/// Column types. Anything not declared at import is `text`, which is never read as a number.
pub const COLUMN_TYPES: [&str; 4] = ["category", "numeric", "count", "text"];

/// Declared type for one source column. Units are never inferred from the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDecl {
    pub name: String,
    pub data_type: String,
    pub unit: Option<String>,
}

fn state_of(class: ParseClass) -> CellState {
    match class {
        ParseClass::Missing => CellState::Missing,
        ParseClass::Ambiguous => CellState::Ambiguous,
        ParseClass::Malformed => CellState::Malformed,
    }
}

/// Classifies one raw cell. Returns the state and the canonical decimal text for valid numbers.
/// Blank cells are Missing and are never converted to 0.
pub fn classify_cell(data_type: &str, raw: &str) -> (CellState, Option<String>) {
    if raw.trim().is_empty() {
        return (CellState::Missing, None);
    }
    match data_type {
        "numeric" => match Dec::parse(raw) {
            Ok(d) => (CellState::Valid, Some(d.canonical())),
            Err(e) => (state_of(e.class), None),
        },
        "count" => {
            if !raw.bytes().all(|b| b.is_ascii_digit()) {
                let class = Dec::parse(raw).err().map(|e| e.class).unwrap_or(ParseClass::Malformed);
                return (state_of(class), None);
            }
            match Dec::parse(raw) {
                Ok(d) if d.coef > 0 => (CellState::Valid, Some(d.canonical())),
                _ => (CellState::Malformed, None),
            }
        }
        _ => (CellState::Valid, None),
    }
}

/// Row identity: the `id` column when present, otherwise the 1-based data row number (r0001...).
pub fn source_row_id(index: usize, cells: &[String], id_col: Option<usize>) -> String {
    match id_col {
        Some(c) => cells[c].clone(),
        None => format!("r{:04}", index + 1),
    }
}

/// Column id of the derived output of a UnitScale transform.
pub fn derived_column_id(column_id: &str, output_unit: &str) -> String {
    format!("{column_id}@{output_unit}")
}

/// Id of the column that the AddColumn transform at `index` of a history creates. The index is the transform's position,
/// which never changes and is never reused, so an id that a removal gave up is not given out again.
pub fn added_column_id(index: usize) -> String {
    format!("col{index:04}")
}

/// Id of the `position`-th row that the AddRows transform at `index` of a history creates.
pub fn added_row_id(index: usize, position: usize) -> String {
    format!("row{index:04}-{position:03}")
}

/// The index of the AddRows transform that creates each added row id. One AddRows at most creates an id, because the id names
/// that transform's position (`added_row_id`).
fn added_row_origins(transforms: &[DataTransform]) -> BTreeMap<String, usize> {
    let mut origins = BTreeMap::new();
    for (i, t) in transforms.iter().enumerate() {
        if let DataTransform::AddRows { rows, .. } = t {
            origins.extend((0..rows.len()).map(|k| (added_row_id(i, k), i)));
        }
    }
    origins
}

/// Whether `row_id`, named by the transform at `at`, is a row that an AddRows created. The id alone does not say it: before its
/// AddRows, the id can be held by a source row, which a transform drops before that AddRows gives the id out again.
fn is_added_at(origins: &BTreeMap<String, usize>, row_id: &str, at: usize) -> bool {
    origins.get(row_id).is_some_and(|&added_at| added_at < at)
}

/// Whether the SelectRows at `at`, which names `keep`, keeps the row `row_id` that exists at that point. It keeps the source rows
/// it names and every row that an AddRows before it created. The replay and the transform walk both use this rule.
fn select_rows_keeps(keep: &BTreeSet<&str>, origins: &BTreeMap<String, usize>, row_id: &str, at: usize) -> bool {
    keep.contains(row_id) || is_added_at(origins, row_id, at)
}

pub fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Imports a CSV/TSV source. Every source column is kept. Undeclared columns are `text`.
pub fn import(id: &str, origin: &str, source: &[u8], declared: &[ColumnDecl]) -> DltResult<Dataset> {
    import_table(id, origin, source, declared, table::parse(source).map_err(DltError::invalid)?)
}

pub(crate) fn import_table(id: &str, origin: &str, source: &[u8], declared: &[ColumnDecl], t: table::Table) -> DltResult<Dataset> {
    if id.trim().is_empty() {
        return Err(DltError::invalid("dataset id is empty"));
    }
    if !ORIGINS.contains(&origin) {
        return Err(DltError::invalid(format!("dataset origin \"{origin}\" is not one of {ORIGINS:?}")));
    }
    for d in declared {
        if !COLUMN_TYPES.contains(&d.data_type.as_str()) {
            return Err(DltError::invalid(format!("column \"{}\" type \"{}\" is not one of {COLUMN_TYPES:?}", d.name, d.data_type)));
        }
        if !t.header.contains(&d.name) {
            return Err(DltError::invalid(format!("declared column \"{}\" is not in the source header", d.name)));
        }
    }
    let columns: Vec<DataColumn> = t
        .header
        .iter()
        .map(|name| {
            let (data_type, unit) = declared
                .iter()
                .find(|d| &d.name == name)
                .map(|d| (d.data_type.clone(), d.unit.clone()))
                .unwrap_or_else(|| ("text".to_owned(), None));
            DataColumn { id: name.clone(), name: name.clone(), data_type, unit }
        })
        .collect();
    let id_col = t.header.iter().position(|h| h == "id");
    let mut rows = Vec::with_capacity(t.records.len());
    let mut seen = BTreeSet::new();
    for (k, rec) in t.records.iter().enumerate() {
        let row_id = source_row_id(k, &rec.cells, id_col);
        if row_id.trim().is_empty() {
            return Err(DltError::invalid(format!("source line {} has an empty row id", rec.line)));
        }
        if !seen.insert(row_id.clone()) {
            return Err(DltError::invalid(format!("duplicate row id \"{row_id}\"")));
        }
        let mut cells = BTreeMap::new();
        for (c, col) in columns.iter().enumerate() {
            let raw = rec.cells[c].clone();
            let (state, parsed_decimal) = classify_cell(&col.data_type, &raw);
            cells.insert(col.id.clone(), DataCell { original: raw, parsed_decimal, state });
        }
        rows.push(DataRow { id: row_id, cells });
    }
    let mut ds = Dataset {
        id: id.to_owned(),
        version_hash: String::new(),
        original_asset_hash: sha256(source),
        origin: origin.to_owned(),
        columns,
        rows,
        derived_from: None,
        transforms: Vec::new(),
    };
    set_version(&mut ds)?;
    Ok(ds)
}

#[derive(Serialize)]
struct VersionPayload<'a> {
    id: &'a str,
    origin: &'a str,
    original_asset_hash: &'a str,
    columns: &'a [DataColumn],
    rows: &'a [DataRow],
    derived_from: &'a Option<DatasetRef>,
    transforms: &'a [DataTransform],
}

/// Canonical content hash of a dataset. The `version_hash` field itself is excluded.
pub fn compute_version(ds: &Dataset) -> DltResult<String> {
    json_hash(&VersionPayload {
        id: &ds.id,
        origin: &ds.origin,
        original_asset_hash: &ds.original_asset_hash,
        columns: &ds.columns,
        rows: &ds.rows,
        derived_from: &ds.derived_from,
        transforms: &ds.transforms,
    })
}

pub fn set_version(ds: &mut Dataset) -> DltResult<()> {
    ds.version_hash = compute_version(ds)?;
    Ok(())
}

/// Shape and self-consistency checks that need no source bytes.
pub fn check_shape(ds: &Dataset) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let node = ds.id.as_str();
    if ds.id.trim().is_empty() {
        out.push(diag::blocking(code::DATASET_INVALID, None, "dataset id is empty"));
    }
    if !ORIGINS.contains(&ds.origin.as_str()) {
        out.push(diag::blocking(
            code::DATASET_ORIGIN_INVALID,
            Some(node),
            format!("origin \"{}\" is not one of {ORIGINS:?}", ds.origin),
        ));
    }
    if !is_sha256(&ds.original_asset_hash) {
        out.push(diag::blocking(code::DATASET_INVALID, Some(node), "original_asset_hash is not a SHA-256 hex digest"));
    }
    let mut col_types: BTreeMap<String, String> = BTreeMap::new();
    for c in &ds.columns {
        if c.id.trim().is_empty() || col_types.insert(c.id.clone(), c.data_type.clone()).is_some() {
            out.push(diag::blocking(code::DATASET_INVALID, Some(node), format!("column id \"{}\" is empty or repeated", c.id)));
        }
        if !COLUMN_TYPES.contains(&c.data_type.as_str()) {
            out.push(diag::blocking(
                code::DATA_TYPE_INVALID,
                Some(node),
                format!("column \"{}\" type \"{}\" is not one of {COLUMN_TYPES:?}", c.id, c.data_type),
            ));
        }
    }
    let mut row_ids = BTreeSet::new();
    for r in &ds.rows {
        if !row_ids.insert(r.id.clone()) {
            out.push(diag::blocking(code::DATASET_INVALID, Some(node), format!("row id \"{}\" is repeated", r.id)));
        }
        let keys: BTreeSet<&String> = r.cells.keys().collect();
        let expected: BTreeSet<&String> = col_types.keys().collect();
        if keys != expected {
            out.push(diag::blocking(code::DATASET_ROW_SHAPE, Some(node), format!("row \"{}\" does not have exactly the dataset columns", r.id)));
            continue;
        }
        for c in &ds.columns {
            let Some(cell) = r.cells.get(&c.id) else { continue };
            let (state, parsed) = classify_cell(&c.data_type, &cell.original);
            if state != cell.state || parsed != cell.parsed_decimal {
                out.push(diag::blocking(
                    code::CELL_STATE_MISMATCH,
                    Some(node),
                    format!("cell {}/{} state does not match its source text \"{}\"", r.id, c.id, cell.original),
                ));
            }
        }
    }
    if let Some(parent) = &ds.derived_from {
        if !is_sha256(&parent.version_hash) {
            out.push(diag::blocking(code::DATASET_LINEAGE_INVALID, Some(node), "derived_from.version_hash is not a SHA-256 digest"));
        }
    }
    match compute_version(ds) {
        Ok(v) => {
            // An empty version_hash is refused as well: a stored version always carries its canonical hash.
            if ds.version_hash != v {
                out.push(diag::blocking(
                    code::DATASET_VERSION_MISMATCH,
                    Some(node),
                    "version_hash does not match the canonical content of this dataset",
                ));
            }
        }
        Err(e) => out.push(diag::blocking(code::DATASET_VERSION_MISMATCH, Some(node), e.message)),
    }
    out.extend(check_transforms(ds));
    out
}

/// The transform history against the current dataset. Structure edits change which rows and columns exist, so each
/// transform is checked against the ids that existed when it was applied. The walk starts from the current ids and undoes
/// the history backwards: an AddColumn or AddRows removes the ids it created (they are named by the position of the
/// transform, see `added_column_id`), and a RemoveColumn or RemoveRow puts back the id it removed.
///
/// A source row is not walked: the walk has no source, so a SelectRows does not say which rows it dropped. The source rows that
/// EditCell and RemoveRow name are checked after the walk instead, as the replay checks them. Such a row must be in the dataset, or
/// a transform must have dropped it, and no transform may name it after the first transform that dropped it. A row id names an
/// added row only after the AddRows that creates it (`is_added_at`), so a source row that holds the same id must be dropped before it.
fn check_transforms(ds: &Dataset) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let node = Some(ds.id.as_str());
    let mismatch = |msg: String| diag::blocking(code::DATASET_TRANSFORM_MISMATCH, node, msg);
    let derived: BTreeSet<String> = ds
        .transforms
        .iter()
        .filter_map(|t| match t {
            DataTransform::UnitScale { column_id, output_unit, .. } => Some(derived_column_id(column_id, output_unit)),
            _ => None,
        })
        .collect();
    // The AddRows that creates each added row. A transform that names a row before its AddRows, or a row no AddRows creates,
    // names a source row (`is_added_at`).
    let origins = added_row_origins(&ds.transforms);
    // The last transform that names each source row.
    let mut last_named: BTreeMap<String, usize> = BTreeMap::new();
    for (i, t) in ds.transforms.iter().enumerate() {
        if let DataTransform::EditCell { row_id, .. } | DataTransform::RemoveRow { row_id, .. } = t {
            if !is_added_at(&origins, row_id, i) {
                last_named.insert(row_id.clone(), i);
            }
        }
    }
    // The first transform that drops each source row: a SelectRows that does not keep it, or its RemoveRow. The walk meets the
    // transforms from the last one, so the last drop that it records is the first.
    let mut first_drop: BTreeMap<String, usize> = BTreeMap::new();
    let mut columns: BTreeSet<String> = ds.columns.iter().map(|c| c.id.clone()).collect();
    let mut rows: BTreeSet<String> = ds.rows.iter().map(|r| r.id.clone()).collect();
    // Walking backwards, the first EditCell met for a cell is its last one. The final text must show it.
    let mut last_edit: BTreeMap<(String, String), String> = BTreeMap::new();
    // A RemoveRow ends the life of its row id in the walk, and an AddRows starts the life of its ids, so walking backwards it ends
    // it as well: an EditCell met before either belongs to an earlier life of the same id, so it is not the final text of the
    // current row.
    let mut closed: BTreeSet<String> = BTreeSet::new();
    for (i, t) in ds.transforms.iter().enumerate().rev() {
        match t {
            DataTransform::SelectRows { row_ids } => {
                let keep: BTreeSet<&str> = row_ids.iter().map(String::as_str).collect();
                for row in last_named.keys().filter(|row| !select_rows_keeps(&keep, &origins, row, i)) {
                    first_drop.insert(row.clone(), i);
                }
            }
            DataTransform::Sort { column_id, .. } => {
                if !columns.contains(column_id) {
                    out.push(mismatch(format!("sort column \"{column_id}\" did not exist when it was sorted")));
                }
            }
            DataTransform::EditCell { row_id, column_id, replacement, .. } => {
                // A source row is checked after the walk. An added row must exist at this point.
                let row_exists = !is_added_at(&origins, row_id, i) || rows.contains(row_id);
                if !row_exists || !columns.contains(column_id) || derived.contains(column_id) {
                    out.push(mismatch(format!(
                        "edit targets a row or column that does not exist at that point, or a derived column: {row_id}/{column_id}"
                    )));
                } else if !closed.contains(row_id) {
                    last_edit.entry((row_id.clone(), column_id.clone())).or_insert_with(|| replacement.clone());
                }
            }
            DataTransform::UnitScale { column_id, numerator, denominator, output_unit } => {
                if !columns.contains(column_id) {
                    out.push(mismatch(format!("unit_scale column \"{column_id}\" did not exist when it was scaled")));
                }
                out.extend(check_unit_scale(ds, column_id, numerator, denominator, output_unit));
            }
            DataTransform::AddColumn { .. } => {
                let id = added_column_id(i);
                if !columns.remove(&id) {
                    out.push(mismatch(format!("column \"{id}\" from AddColumn is not in the dataset after it")));
                }
            }
            DataTransform::AddRows { rows: added, .. } => {
                for k in 0..added.len() {
                    let id = added_row_id(i, k);
                    if !rows.remove(&id) {
                        out.push(mismatch(format!("row \"{id}\" from AddRows is not in the dataset after it")));
                    }
                    closed.insert(id);
                }
            }
            DataTransform::RemoveRow { row_id, .. } => {
                if rows.contains(row_id) {
                    out.push(mismatch(format!("row \"{row_id}\" is still in the dataset after its RemoveRow")));
                }
                rows.insert(row_id.clone());
                if !is_added_at(&origins, row_id, i) {
                    first_drop.insert(row_id.clone(), i);
                }
                closed.insert(row_id.clone());
            }
            DataTransform::RemoveColumn { column_id, .. } => {
                if columns.contains(column_id) {
                    out.push(mismatch(format!("column \"{column_id}\" is still in the dataset after its RemoveColumn")));
                }
                columns.insert(column_id.clone());
            }
        }
    }
    let present: BTreeSet<&str> = ds.rows.iter().map(|r| r.id.as_str()).collect();
    for (row_id, last) in &last_named {
        match first_drop.get(row_id) {
            None if !present.contains(row_id.as_str()) => out.push(mismatch(format!(
                "row \"{row_id}\" is named by a transform, but it is not in the dataset and no transform dropped it"
            ))),
            Some(drop) if last > drop => {
                out.push(mismatch(format!("row \"{row_id}\" is named by a transform after an earlier transform dropped it")));
            }
            _ => {}
        }
        // An AddRows gives out the id of this source row, so the row must be dropped before it, as the replay requires.
        if let Some(&added_at) = origins.get(row_id) {
            if first_drop.get(row_id).is_none_or(|&drop| drop > added_at) {
                out.push(mismatch(format!(
                    "row \"{row_id}\" from AddRows is still a source row at that point: an earlier transform names it, and no SelectRows or RemoveRow dropped it"
                )));
            }
        }
    }
    for ((row_id, column_id), replacement) in &last_edit {
        if let Some(cell) = ds.rows.iter().find(|r| &r.id == row_id).and_then(|r| r.cells.get(column_id)) {
            if &cell.original != replacement {
                out.push(diag::blocking(
                    code::EDIT_MISMATCH,
                    node,
                    format!("cell {row_id}/{column_id} is \"{}\" but its last recorded edit is \"{replacement}\"", cell.original),
                ));
            }
        }
    }
    out
}

fn check_unit_scale(ds: &Dataset, column_id: &str, numerator: &str, denominator: &str, output_unit: &str) -> Vec<Diagnostic> {
    let node = Some(ds.id.as_str());
    let mismatch = |msg: String| diag::blocking(code::DATASET_TRANSFORM_MISMATCH, node, msg);
    let mut out = Vec::new();
    let source_ok = ds.columns.iter().any(|c| c.id == column_id && (c.data_type == "numeric" || c.data_type == "count"));
    if !source_ok {
        out.push(mismatch(format!("unit_scale column \"{column_id}\" is not a numeric column")));
        return out;
    }
    let (num, den) = match (Dec::parse(numerator), Dec::parse(denominator)) {
        (Ok(n), Ok(d)) if d.coef != 0 => (n, d),
        _ => {
            out.push(mismatch(format!("unit_scale factor {numerator}/{denominator} is not a valid non-zero decimal ratio")));
            return out;
        }
    };
    if output_unit.trim().is_empty() {
        out.push(mismatch("unit_scale output_unit is empty".to_owned()));
        return out;
    }
    let did = derived_column_id(column_id, output_unit);
    let Some(dcol) = ds.columns.iter().find(|c| c.id == did) else {
        out.push(mismatch(format!("unit_scale output column \"{did}\" is missing")));
        return out;
    };
    if dcol.data_type != "numeric" || dcol.unit.as_deref() != Some(output_unit) {
        out.push(mismatch(format!("unit_scale output column \"{did}\" must be numeric with unit \"{output_unit}\"")));
    }
    // A derived column is named by its id, as a source column is named by its header (see `compare_with_replay`).
    if dcol.name != did {
        out.push(mismatch(format!("unit_scale output column \"{did}\" is named \"{}\", but a derived column is named by its id", dcol.name)));
    }
    for r in &ds.rows {
        let (Some(src), Some(dst)) = (r.cells.get(column_id), r.cells.get(&did)) else { continue };
        let (exp_state, exp_parsed, exp_original) = match src.state {
            CellState::Valid => match Dec::parse(&src.original).ok().and_then(|d| exact_scale(d.exact(), num, den)) {
                Some(e) => {
                    let text = e.canonical();
                    (CellState::Valid, Some(text.clone()), text)
                }
                None => {
                    out.push(not_exact(node, &r.id, column_id));
                    continue;
                }
            },
            other => (other, None, src.original.clone()),
        };
        if dst.state != exp_state || dst.parsed_decimal != exp_parsed || dst.original != exp_original {
            out.push(mismatch(format!(
                "derived cell {}/{did} is \"{}\", exact unit_scale result is \"{exp_original}\"",
                r.id, dst.original
            )));
        }
    }
    out
}

/// A UnitScale output column: the column it scales and the exact factor `numerator / denominator`.
struct Scale {
    source: String,
    numerator: String,
    denominator: String,
}

/// A refusal of the replay: the diagnostic code and its message.
type Refusal = (&'static str, String);

/// The state that the recorded history leaves when it is replayed over the source rows.
struct Replay {
    /// The row ids in the order the history leaves them.
    order: Vec<String>,
    /// The stored cells of each remaining row, by row id and then column id. A derived column has no stored cell.
    cells: BTreeMap<String, BTreeMap<String, DataCell>>,
    /// The column ids that exist at the end of the history.
    present: BTreeSet<String>,
    /// The UnitScale output columns of the history, by id.
    scales: BTreeMap<String, Scale>,
    /// The AddColumn columns that remain, with the name, type and unit their AddColumn records.
    added: BTreeMap<String, (String, String, Option<String>)>,
}

/// The type of a column as the replay reads it. A source column that a RemoveColumn removed has no recorded type and reads as
/// text. Its cells are then only used as text, and no Sort can order by it.
fn column_type<'a>(types: &'a BTreeMap<String, String>, column_id: &str) -> &'a str {
    types.get(column_id).map_or("text", String::as_str)
}

/// Replays the recorded history over the source rows. Each transform is applied to the state that exists at its point of the
/// history, and a transform that the state cannot take is refused:
/// - SelectRows keeps the source rows it names and every row that an AddRows before it created (`select_rows_keeps`). A row it
///   drops is gone for the rest of the history.
/// - RemoveRow names a row that exists and gives its current text in every column.
/// - RemoveColumn names a column that exists under its current name. A unit-scaled column cannot be removed.
/// - AddColumn adds a column after the last one, under the id its position names. That id must not be a column that exists at
///   that point, so a fill never replaces the cells of an existing column. Every row that exists takes its fill.
/// - AddRows places its rows right after their anchor, or at the end. A row takes the text its payload gives for each stored
///   column, and blank text for each column the payload leaves out. A payload names only stored columns.
/// - UnitScale adds its output column, whose source must exist at that point. The output has no stored cells: its cell is the
///   exact scaled cell of its source, so a chain of UnitScales is followed back to the stored column at its start.
/// - EditCell names a row and a stored column that exist, and its previous text is the cell's text at that point. A unit-scaled
///   column follows its source, so the new text must leave each derived cell of its row exact (see `edit_cell`).
/// - Sort orders the rows by the cells they have at that point. It is refused when its column does not exist, has no recorded
///   type, or holds a value that has no sort position.
fn replay_history(ds: &Dataset, t: &table::Table, id_col: Option<usize>) -> Result<Replay, Refusal> {
    let mismatch = |message: String| (code::DATASET_TRANSFORM_MISMATCH, message);
    // The ids that the history creates: an AddColumn's id and a UnitScale's output id. A stored type for such an id belongs to the
    // column the history creates later, so the id has no type until its transform runs.
    let created: BTreeSet<String> = ds
        .transforms
        .iter()
        .enumerate()
        .filter_map(|(i, tr)| match tr {
            DataTransform::AddColumn { .. } => Some(added_column_id(i)),
            DataTransform::UnitScale { column_id, output_unit, .. } => Some(derived_column_id(column_id, output_unit)),
            _ => None,
        })
        .collect();
    // The type of each column that exists at the start of the history. Each AddColumn and UnitScale adds its type when it runs.
    let mut types: BTreeMap<String, String> =
        ds.columns.iter().filter(|c| !created.contains(&c.id)).map(|c| (c.id.clone(), c.data_type.clone())).collect();
    // The name of each column that exists. A source column is named by its header.
    let mut names: BTreeMap<String, String> = t.header.iter().map(|h| (h.clone(), h.clone())).collect();
    // The UnitScale columns that exist at each point, by id. Each is registered when its UnitScale runs.
    let mut scales: BTreeMap<String, Scale> = BTreeMap::new();
    // The source rows in source order, each with its stored cells.
    let mut order: Vec<String> = Vec::with_capacity(t.records.len());
    let mut cells: BTreeMap<String, BTreeMap<String, DataCell>> = BTreeMap::new();
    for (k, rec) in t.records.iter().enumerate() {
        let id = source_row_id(k, &rec.cells, id_col);
        let row: BTreeMap<String, DataCell> =
            t.header.iter().zip(&rec.cells).map(|(name, raw)| (name.clone(), cell_of_text(column_type(&types, name), raw))).collect();
        cells.insert(id.clone(), row);
        order.push(id);
    }
    let mut present: BTreeSet<String> = t.header.iter().cloned().collect();
    let mut added: BTreeMap<String, (String, String, Option<String>)> = BTreeMap::new();
    let origins = added_row_origins(&ds.transforms);
    for (i, tr) in ds.transforms.iter().enumerate() {
        match tr {
            DataTransform::SelectRows { row_ids } => {
                let keep: BTreeSet<&str> = row_ids.iter().map(String::as_str).collect();
                order.retain(|id| select_rows_keeps(&keep, &origins, id, i));
                let live: BTreeSet<String> = order.iter().cloned().collect();
                cells.retain(|id, _| live.contains(id));
            }
            DataTransform::RemoveRow { row_id, previous } => {
                if !order.contains(row_id) {
                    return Err(mismatch(format!("RemoveRow of \"{row_id}\": that row is not in the dataset at that point")));
                }
                let texts = match cells.get(row_id) {
                    Some(row) => row_texts(row_id, row, &scales, &present)?,
                    None => return Err(mismatch(format!("row \"{row_id}\" has no cells at that point"))),
                };
                if texts != *previous {
                    return Err(mismatch(format!("RemoveRow of \"{row_id}\": its previous texts are not the texts of the row at that point")));
                }
                order.retain(|id| id != row_id);
                cells.remove(row_id);
            }
            DataTransform::RemoveColumn { column_id, previous_name } => {
                if !present.contains(column_id) {
                    return Err(mismatch(format!("RemoveColumn of \"{column_id}\": that column is not in the dataset at that point")));
                }
                if scales.contains_key(column_id) || scales.values().any(|s| &s.source == column_id) {
                    return Err(mismatch(format!("column \"{column_id}\" is unit-scaled, so it cannot be removed")));
                }
                if names.get(column_id) != Some(previous_name) {
                    return Err(mismatch(format!("RemoveColumn of \"{column_id}\" names \"{previous_name}\", which is not its name at that point")));
                }
                present.remove(column_id);
                types.remove(column_id);
                names.remove(column_id);
                added.remove(column_id);
                for row in cells.values_mut() {
                    row.remove(column_id);
                }
            }
            DataTransform::AddColumn { name, data_type, unit, fill } => {
                let id = added_column_id(i);
                // The id belongs to this transform's position. A column that already holds it, such as a source column whose
                // header is that id, would have its cells replaced by the fill. Refused before any cell changes.
                if present.contains(&id) {
                    return Err(mismatch(format!("AddColumn id \"{id}\" is already a column at that point, so its fill would replace it")));
                }
                let text = fill.as_deref().unwrap_or("");
                for row in cells.values_mut() {
                    row.insert(id.clone(), cell_of_text(data_type, text));
                }
                types.insert(id.clone(), data_type.clone());
                names.insert(id.clone(), name.clone());
                present.insert(id.clone());
                added.insert(id, (name.clone(), data_type.clone(), unit.clone()));
            }
            DataTransform::AddRows { after_row_id, rows } => {
                let at = match after_row_id {
                    None => order.len(),
                    Some(anchor) => {
                        order
                            .iter()
                            .position(|id| id == anchor)
                            .ok_or_else(|| mismatch(format!("AddRows after \"{anchor}\": that row is not in the dataset at that point")))?
                            + 1
                    }
                };
                for (k, values) in rows.iter().enumerate() {
                    if let Some(name) = values.keys().find(|name| !present.contains(name.as_str()) || scales.contains_key(name.as_str())) {
                        return Err(mismatch(format!("AddRows names \"{name}\", which is not a stored column at that point")));
                    }
                    let id = added_row_id(i, k);
                    if cells.contains_key(&id) {
                        return Err(mismatch(format!("AddRows row id \"{id}\" is already in the dataset")));
                    }
                    let mut row = BTreeMap::new();
                    for name in &present {
                        // A derived column is not stored: its cells are the scaled source cells at each point.
                        if scales.contains_key(name) {
                            continue;
                        }
                        let text = values.get(name).map_or("", String::as_str);
                        row.insert(name.clone(), cell_of_text(column_type(&types, name), text));
                    }
                    cells.insert(id.clone(), row);
                    order.insert(at + k, id);
                }
            }
            DataTransform::UnitScale { column_id, numerator, denominator, output_unit } => {
                if !present.contains(column_id) {
                    return Err(mismatch(format!("unit_scale column \"{column_id}\" did not exist when it was scaled")));
                }
                let derived = derived_column_id(column_id, output_unit);
                if present.contains(&derived) {
                    return Err(mismatch(format!("unit_scale output column \"{derived}\" already exists at that point")));
                }
                // A scaled column is numeric (check_unit_scale): its type is known from this point on.
                let scale = Scale { source: column_id.clone(), numerator: numerator.clone(), denominator: denominator.clone() };
                scales.insert(derived.clone(), scale);
                types.insert(derived.clone(), "numeric".to_owned());
                present.insert(derived);
            }
            DataTransform::EditCell { row_id, column_id, previous, replacement } => {
                if scales.contains_key(column_id) {
                    return Err(mismatch(format!("edit targets the derived column \"{column_id}\"")));
                }
                if !present.contains(column_id) {
                    return Err(mismatch(format!("edit targets column \"{column_id}\", which does not exist at that point")));
                }
                let row = cells
                    .get_mut(row_id)
                    .ok_or_else(|| mismatch(format!("edit targets row \"{row_id}\", which does not exist at that point")))?;
                let current = row.get(column_id).ok_or_else(|| mismatch(format!("row \"{row_id}\" has no cell in \"{column_id}\"")))?;
                if current.original != *previous {
                    return Err((
                        code::EDIT_MISMATCH,
                        format!("edit of {row_id}/{column_id} records previous \"{previous}\", but the cell is \"{}\" at that point", current.original),
                    ));
                }
                row.insert(column_id.clone(), cell_of_text(column_type(&types, column_id), replacement));
                // A unit-scaled column follows its source (`edit_cell`): every derived cell of this row must stay exact.
                for derived in scales.keys() {
                    cell_in(row_id, row, &scales, derived)?;
                }
            }
            DataTransform::Sort { column_id, descending } => {
                if !present.contains(column_id) {
                    return Err(mismatch(format!("sort column \"{column_id}\" did not exist when it was sorted")));
                }
                let Some(data_type) = types.get(column_id) else {
                    return Err(mismatch(format!("sort column \"{column_id}\" has no recorded data type, so its order cannot be replayed")));
                };
                let perm = {
                    let mut keyed: Vec<(&str, Cow<'_, DataCell>)> = Vec::with_capacity(order.len());
                    for id in &order {
                        let row = cells.get(id).ok_or_else(|| mismatch(format!("row \"{id}\" has no cells at that point")))?;
                        keyed.push((id.as_str(), cell_in(id, row, &scales, column_id)?));
                    }
                    sort_permutation(data_type, &keyed, *descending)?
                };
                let mut slots: Vec<Option<String>> = std::mem::take(&mut order).into_iter().map(Some).collect();
                order = perm
                    .into_iter()
                    .map(|p| slots.get_mut(p).and_then(|slot| slot.take()))
                    .collect::<Option<Vec<String>>>()
                    .ok_or_else(|| mismatch("sort order is not a permutation of the rows".to_owned()))?;
            }
        }
    }
    Ok(Replay { order, cells, present, scales, added })
}

/// The text of every column that exists at a point of the history, by column id. A derived column's text is its scaled text.
fn row_texts(
    row_id: &str,
    row: &BTreeMap<String, DataCell>,
    scales: &BTreeMap<String, Scale>,
    present: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>, Refusal> {
    let mut texts = BTreeMap::new();
    for column in present {
        let cell = cell_in(row_id, row, scales, column)?;
        texts.insert(column.clone(), cell.original.clone());
    }
    Ok(texts)
}

/// The cell of `column` in one row of the replayed state. A derived column has no stored cell: its cell is the exact scaled
/// cell of its source, so a chain of UnitScales is followed back to a stored column. A scaled value that is not an exact
/// decimal has no cell, and the replay refuses it.
fn cell_in<'a>(
    row_id: &str,
    row: &'a BTreeMap<String, DataCell>,
    scales: &BTreeMap<String, Scale>,
    column: &str,
) -> Result<Cow<'a, DataCell>, Refusal> {
    let Some(scale) = scales.get(column) else {
        return row
            .get(column)
            .map(Cow::Borrowed)
            .ok_or_else(|| (code::DATASET_TRANSFORM_MISMATCH, format!("row \"{row_id}\" has no cell in column \"{column}\"")));
    };
    let source = cell_in(row_id, row, scales, &scale.source)?;
    match scaled_cell(&source, &scale.numerator, &scale.denominator) {
        Some(cell) => Ok(Cow::Owned(cell)),
        None => Err((
            code::UNIT_SCALE_NOT_EXACT,
            format!("unit_scale of {row_id}/{} is not exact, so {row_id}/{column} has no value", scale.source),
        )),
    }
}

/// The cell that `text` is in a column of `data_type`: its state and canonical value as `classify_cell` gives them.
fn cell_of_text(data_type: &str, text: &str) -> DataCell {
    let (state, parsed_decimal) = classify_cell(data_type, text);
    DataCell { original: text.to_owned(), parsed_decimal, state }
}

/// The exact value of `value * numerator / denominator` when a cell can hold it: a terminating decimal whose canonical text the
/// cell grammar reads back, which is at most `MAX_DIGITS` digits (`Dec::parse`). A larger result is refused like a non-terminating
/// one, so no cell holds a value that a later edit, sort or scale could not read.
fn exact_scale(value: Exact, numerator: Dec, denominator: Dec) -> Option<Exact> {
    let scaled = unit_scale_exact(value, numerator, denominator)?;
    Dec::parse(&scaled.canonical()).ok()?;
    Some(scaled)
}

/// The finding for a scaled value that has no exact decimal of at most `MAX_DIGITS` digits. The service refuses the same scale
/// with InvalidArgument (`diag::error_code_for`), so the check reports it with the same code.
fn not_exact(node: Option<&str>, row_id: &str, column_id: &str) -> Diagnostic {
    diag::blocking(
        code::UNIT_SCALE_NOT_EXACT,
        node,
        format!("unit_scale of {row_id}/{column_id} is not an exact decimal of at most {MAX_DIGITS} digits"),
    )
}

/// The refusal of an edit that would make a scaled value that is not exact. The error carries the diagnostic with its code, as the
/// replay's finding does, so the service and PutDataset report UNIT_SCALE_NOT_EXACT in the same way.
fn not_exact_error(node: &str, row_id: &str, column_id: &str) -> DltError {
    diag::to_error(&[not_exact(Some(node), row_id, column_id)], "The unit scale is not exact")
}

/// The cell of a unit-scaled column for one source cell, as `check_unit_scale` defines it: the exact scaled value, or the
/// blank, ambiguous or malformed state of the source. None when the scaled value is not an exact decimal.
fn scaled_cell(src: &DataCell, numerator: &str, denominator: &str) -> Option<DataCell> {
    match src.state {
        CellState::Valid => {
            let num = Dec::parse(numerator).ok()?;
            let den = Dec::parse(denominator).ok()?;
            let value = Dec::parse(&src.original).ok()?;
            let text = exact_scale(value.exact(), num, den)?.canonical();
            Some(DataCell { original: text.clone(), parsed_decimal: Some(text), state: CellState::Valid })
        }
        other => Some(DataCell { original: src.original.clone(), parsed_decimal: None, state: other }),
    }
}

/// Orders the rows of one column by the cell each row has when the sort is applied. Returns the positions of `rows` in
/// their new order. Numeric and count cells compare by exact decimal value. Category and text cells compare by their
/// original text in byte order. Blank cells come last in both directions. Rows with equal values keep their order. An
/// ambiguous or malformed cell has no position, so the sort is refused. The edit path and the replay both order through
/// this function, so they cannot disagree.
pub(crate) fn sort_permutation(
    data_type: &str,
    rows: &[(&str, Cow<'_, DataCell>)],
    descending: bool,
) -> Result<Vec<usize>, (&'static str, String)> {
    if matches!(data_type, "numeric" | "count") {
        let mut values: Vec<Option<Exact>> = Vec::with_capacity(rows.len());
        for (id, cell) in rows {
            let value = match cell.state {
                CellState::Missing => None,
                CellState::Valid => {
                    let dec = Dec::parse(&cell.original).map_err(|e| {
                        (code::DATA_MALFORMED, format!("row {id}: \"{}\" cannot be ordered: {}", cell.original, e.reason))
                    })?;
                    Some(dec.exact())
                }
                _ => return Err(refused(id, cell)),
            };
            values.push(value);
        }
        // Every value is brought to the largest scale of the column, so each comparison is an integer comparison.
        let scale = values.iter().flatten().map(|e| e.s).max().unwrap_or(0);
        let mut keys: Vec<Option<i128>> = Vec::with_capacity(values.len());
        for value in values {
            let key = match value {
                None => None,
                Some(e) => Some(
                    e.at_scale(scale)
                        .ok_or((code::DATASET_TRANSFORM_MISMATCH, "a sort value is outside the supported range".to_owned()))?,
                ),
            };
            keys.push(key);
        }
        return Ok(ordered_positions(&keys, descending));
    }
    let mut keys: Vec<Option<&str>> = Vec::with_capacity(rows.len());
    for (id, cell) in rows {
        let key = match cell.state {
            CellState::Missing => None,
            CellState::Valid => Some(cell.original.as_str()),
            _ => return Err(refused(id, cell)),
        };
        keys.push(key);
    }
    Ok(ordered_positions(&keys, descending))
}

/// The refusal for a cell that has no sort position.
fn refused(id: &str, cell: &DataCell) -> (&'static str, String) {
    if cell.state == CellState::Ambiguous {
        (code::DATA_AMBIGUOUS, format!("row {id}: \"{}\" is ambiguous, so its sort position is not defined", cell.original))
    } else {
        (code::DATA_MALFORMED, format!("row {id}: \"{}\" is malformed, so its sort position is not defined", cell.original))
    }
}

/// Positions of `keys` in sorted order. Missing keys come last in both directions. Equal keys keep their order, because
/// `sort_by` is stable.
fn ordered_positions<K: Ord>(keys: &[Option<K>], descending: bool) -> Vec<usize> {
    let mut positions: Vec<usize> = (0..keys.len()).collect();
    positions.sort_by(|&a, &b| match (&keys[a], &keys[b]) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            if descending {
                y.cmp(x)
            } else {
                x.cmp(y)
            }
        }
    });
    positions
}

/// Reconciles the stored dataset with the immutable source bytes and its recorded history. The history is replayed over the
/// source, and the stored dataset must be what the replay leaves: the same rows, the same columns, and the same cells
/// (original text, state and exact decimal). A source value may differ from the stored one only where a recorded EditCell
/// explains it. Added, sorted and unit-scaled cells must be the values their transforms give. A stored row order that the
/// replay does not give, with the same rows and cells, is a warning (`code::DATASET_ROW_ORDER_UNRECORDED`) and the stored order is kept.
///
/// Limit: no transform records the type or unit of a source column, and the source bytes carry neither. Opening a file whose
/// source column was given another type or unit, without a recorded transform, reports nothing here, unless the change moves
/// a cell's state (then `check_shape` reports CELL_STATE_MISMATCH). A change of a column's type or unit is refused against
/// the version it replaces (`change_findings`), so such a file comes from before that check or from a hand edit.
///
/// Limit of the origin: the origin of a new asset is the claim of its importer, because the asset has no lineage to check it against.
/// Source bytes that differ only in line endings or in a trailing blank line are a new asset as well, so the same values can enter
/// under another origin from such bytes. The stored origin, unit and `derived_from` are not signed, so a file whose origin, unit or
/// `derived_from` was edited on disk opens without diagnostics here. Opening does not compare them with anything.
pub fn reconcile_source(ds: &Dataset, source: &[u8]) -> Vec<Diagnostic> {
    let node = Some(ds.id.as_str());
    let mut out = Vec::new();
    if sha256(source) != ds.original_asset_hash {
        out.push(diag::blocking(code::DATASET_SOURCE_MISMATCH, node, "source bytes do not match original_asset_hash"));
        return out;
    }
    let removed_columns: BTreeSet<String> = ds
        .transforms
        .iter()
        .filter_map(|t| match t {
            DataTransform::RemoveColumn { column_id, .. } => Some(column_id.clone()),
            _ => None,
        })
        .collect();
    // A MIME-selected TSV can contain commas in its headers. Recover its delimiter
    // from the preserved source columns, never from numeric punctuation in cells.
    let parsed = [',', '\t']
        .into_iter()
        .filter_map(|delimiter| table::parse_delimited(source, delimiter).ok())
        .find(|t| t.header.iter().all(|h| ds.columns.iter().any(|c| &c.id == h) || removed_columns.contains(h)));
    let t = match parsed.ok_or_else(|| "no delimited source matches the preserved columns".to_owned()) {
        Ok(t) => t,
        Err(e) => {
            out.push(diag::blocking(code::DATASET_SOURCE_MISMATCH, node, format!("source cannot be parsed: {e}")));
            return out;
        }
    };
    for h in &t.header {
        if !ds.columns.iter().any(|c| &c.id == h) && !removed_columns.contains(h) {
            out.push(diag::blocking(code::DATASET_SOURCE_MISMATCH, node, format!("source column \"{h}\" is missing from the dataset")));
        }
    }
    let id_col = t.header.iter().position(|h| h == "id");
    let mut source_rows: BTreeSet<String> = BTreeSet::new();
    let mut repeated = false;
    for (k, rec) in t.records.iter().enumerate() {
        let id = source_row_id(k, &rec.cells, id_col);
        if !source_rows.insert(id.clone()) {
            out.push(diag::blocking(code::DATASET_SOURCE_MISMATCH, node, format!("source row id \"{id}\" is repeated")));
            repeated = true;
        }
    }
    if repeated {
        return out;
    }
    for tr in &ds.transforms {
        if let DataTransform::SelectRows { row_ids } = tr {
            for id in row_ids {
                if !source_rows.contains(id) {
                    out.push(diag::blocking(code::DATASET_TRANSFORM_MISMATCH, node, format!("select_rows names \"{id}\", which is not in the source")));
                }
            }
        }
    }
    match replay_history(ds, &t, id_col) {
        Err((c, message)) => out.push(diag::blocking(c, node, message)),
        Ok(replay) => out.extend(compare_with_replay(ds, &replay, &t.header, &source_rows)),
    }
    out
}

/// The blocking findings of a stored dataset against the state its recorded history leaves. Rows, columns and cells are
/// compared with the replay. A derived column is checked against its stored source by `check_unit_scale`, so it is not
/// compared here again.
fn compare_with_replay(ds: &Dataset, replay: &Replay, header: &[String], source_rows: &BTreeSet<String>) -> Vec<Diagnostic> {
    let node = Some(ds.id.as_str());
    let mut out = Vec::new();
    // Rows: the stored rows must be the rows the history leaves, in the order it leaves them.
    let stored_rows: BTreeSet<&str> = ds.rows.iter().map(|r| r.id.as_str()).collect();
    let replayed_rows: BTreeSet<&str> = replay.order.iter().map(String::as_str).collect();
    if stored_rows != replayed_rows {
        let missing = replayed_rows.difference(&stored_rows).count();
        let extra = stored_rows.difference(&replayed_rows).count();
        out.push(diag::blocking(
            code::DATASET_ROWS_MISMATCH,
            node,
            format!("dataset rows differ from the rows the recorded history leaves: {missing} missing, {extra} unexplained"),
        ));
    } else {
        let stored_order: Vec<&str> = ds.rows.iter().map(|r| r.id.as_str()).collect();
        let replayed_order: Vec<&str> = replay.order.iter().map(String::as_str).collect();
        if stored_order != replayed_order {
            out.push(diag::info(
                code::DATASET_ROW_ORDER_UNRECORDED,
                node,
                "row order differs from the order that replaying the recorded transforms (including Sort) produces; the stored order is kept",
            ));
        }
    }
    // Columns: every stored column must be one the history leaves, with the name, type and unit its transform records.
    let header_names: BTreeSet<&str> = header.iter().map(String::as_str).collect();
    let stored_columns: BTreeSet<&str> = ds.columns.iter().map(|c| c.id.as_str()).collect();
    let replayed_columns: BTreeSet<&str> = replay.present.iter().map(String::as_str).collect();
    for id in stored_columns.difference(&replayed_columns) {
        out.push(diag::blocking(code::DATASET_TRANSFORM_MISMATCH, node, format!("column \"{id}\" is not one that the recorded history leaves")));
    }
    for id in replayed_columns.difference(&stored_columns) {
        // A source column that the dataset lacks is reported by the source check in `reconcile_source`.
        if !header_names.contains(id) {
            out.push(diag::blocking(code::DATASET_TRANSFORM_MISMATCH, node, format!("column \"{id}\" that the recorded history leaves is missing")));
        }
    }
    for column in &ds.columns {
        if let Some((name, data_type, unit)) = replay.added.get(&column.id) {
            if column.name != *name || column.data_type != *data_type || column.unit != *unit {
                out.push(diag::blocking(
                    code::DATASET_TRANSFORM_MISMATCH,
                    node,
                    format!("column \"{}\" differs from the AddColumn that creates it", column.id),
                ));
            }
        } else if header_names.contains(column.id.as_str()) && column.name != column.id {
            out.push(diag::blocking(
                code::DATASET_SOURCE_MISMATCH,
                node,
                format!("column \"{}\" is named \"{}\", but its source header is \"{}\"", column.id, column.name, column.id),
            ));
        }
    }
    // Cells: each stored cell must equal the replayed cell. A source cell that differs is allowed only through an EditCell.
    let edited: BTreeSet<(&str, &str)> = ds
        .transforms
        .iter()
        .filter_map(|t| match t {
            DataTransform::EditCell { row_id, column_id, .. } => Some((row_id.as_str(), column_id.as_str())),
            _ => None,
        })
        .collect();
    for row in &ds.rows {
        let Some(replayed) = replay.cells.get(&row.id) else { continue };
        for column in &ds.columns {
            if !replay.present.contains(&column.id) || replay.scales.contains_key(&column.id) {
                continue;
            }
            let Some(stored) = row.cells.get(&column.id) else { continue };
            let expected = match cell_in(&row.id, replayed, &replay.scales, &column.id) {
                Ok(cell) => cell,
                Err((c, message)) => {
                    out.push(diag::blocking(c, node, message));
                    continue;
                }
            };
            if *stored == *expected {
                continue;
            }
            let from_source = source_rows.contains(&row.id)
                && header_names.contains(column.id.as_str())
                && !edited.contains(&(row.id.as_str(), column.id.as_str()));
            let finding = if from_source { code::DATASET_SOURCE_MISMATCH } else { code::DATASET_TRANSFORM_MISMATCH };
            out.push(diag::blocking(
                finding,
                node,
                format!("cell {}/{} is {} but the recorded history gives {}", row.id, column.id, describe(stored), describe(&expected)),
            ));
        }
    }
    out
}

/// The blocking findings that replacing the version `prev` (None for a new dataset) by `next` adds. `prev` is the version of the
/// same id, or the dataset that `next` derives from. `prev_source` and `source` are the original bytes of each version. Without
/// the source of a version, its row order is not checked here: the document check reports a missing asset. Rules:
/// - `next` keeps the recorded transform history of `prev`. The type or unit of a column may not change: no recorded transform
///   changes either, so no change explains one.
/// - `next` carries the origin that `resulting_origin` gives it. A change of the data is a recorded derivation (`origin_change_allowed`).
/// - A version whose stored order the history does not produce (a legacy file) keeps that stored order. Its rows must be in the
///   order that the transforms it adds give when they are applied to the stored order: a Sort orders equal values by their stored
///   order, and an AddRows places its rows after their anchor. A recorded Sort over equal values therefore keeps the tied rows in
///   their stored order, and the warning stays for them, because the history orders them otherwise.
/// - Any other version may not give the rows an order that its recorded history does not produce.
pub fn change_findings(prev: Option<&Dataset>, prev_source: Option<&[u8]>, next: &Dataset, source: Option<&[u8]>) -> Vec<Diagnostic> {
    let node = Some(next.id.as_str());
    let mut out = Vec::new();
    if let Some(prev) = prev {
        // The source is part of the dataset: another asset under the same values would change the cells without any transform.
        if prev.original_asset_hash != next.original_asset_hash {
            out.push(diag::blocking(
                code::DATASET_SOURCE_MISMATCH,
                node,
                format!("\"{}\" is read from a different source than \"{}\", so its cells would change without a recorded transform", next.id, prev.id),
            ));
            return out;
        }
        if !history_extends(prev, next) {
            out.push(diag::blocking(
                code::DATASET_TRANSFORM_MISMATCH,
                node,
                format!("\"{}\" does not keep the recorded transform history of \"{}\"", next.id, prev.id),
            ));
            return out;
        }
        // A column whose type or unit changes with no recorded transform is the cause of the change, so it is reported first. The
        // origin of a changed dataset follows from that change, and is not reported on top of it.
        for column in &next.columns {
            let Some(old) = prev.columns.iter().find(|c| c.id == column.id) else { continue };
            if old.data_type != column.data_type || old.unit != column.unit {
                out.push(diag::blocking(
                    code::DATASET_TRANSFORM_MISMATCH,
                    node,
                    format!("column \"{}\" changes its type or unit, and no recorded transform explains the change", column.id),
                ));
            }
        }
        if out.is_empty() {
            let expected = resulting_origin(prev, next);
            if expected != next.origin.as_str() {
                out.push(diag::blocking(
                    code::DATASET_ORIGIN_INVALID,
                    node,
                    format!(
                        "\"{}\" has origin \"{}\", but its change gives \"{expected}\": changed data is derived_transform, and unchanged data keeps the origin of \"{}\"",
                        next.id, next.origin, prev.id
                    ),
                ));
                return out;
            }
            if !origin_change_allowed(prev, next) {
                out.push(diag::blocking(
                    code::DATASET_TRANSFORM_MISMATCH,
                    node,
                    format!("\"{}\" changes its data, and no recorded transform that extends the history of \"{}\" explains it", next.id, prev.id),
                ));
                return out;
            }
        }
    }
    let Some(source) = source else { return out };
    match prev {
        Some(prev) if prev_source.is_some_and(|bytes| has_unrecorded_order(prev, bytes)) => match expected_order(prev, next) {
            None => out.push(diag::blocking(
                code::DATASET_TRANSFORM_MISMATCH,
                node,
                "the transforms this version adds cannot be applied to the stored order of its dataset, so the change is refused",
            )),
            Some(expected) => {
                let actual: Vec<&str> = next.rows.iter().map(|r| r.id.as_str()).collect();
                if expected.iter().map(String::as_str).ne(actual.iter().copied()) {
                    out.push(diag::blocking(
                        code::DATASET_ROWS_MISMATCH,
                        node,
                        "the rows do not follow from the transforms this version adds, applied to the stored order of its dataset (a Sort orders equal values by their stored order, and AddRows places rows after their anchor), so the change is refused",
                    ));
                }
            }
        },
        _ => {
            if has_unrecorded_order(next, source) {
                out.push(diag::blocking(
                    code::DATASET_ROWS_MISMATCH,
                    node,
                    "the change gives the rows an order that the recorded transforms (including Sort) do not produce",
                ));
            }
        }
    }
    out
}

/// The stored row order that the transforms `next` adds give when they are applied, in order, to the stored rows of `prev`, through
/// `apply_edits`. A Sort orders by the stored values, and equal values keep their stored order. An AddRows places its rows after
/// their anchor in the stored order. None when a transform cannot be applied to that state. Requires that `next` keeps the history
/// of `prev`.
fn expected_order(prev: &Dataset, next: &Dataset) -> Option<Vec<String>> {
    let added = &next.transforms[prev.transforms.len()..];
    if added.is_empty() {
        return Some(prev.rows.iter().map(|r| r.id.clone()).collect());
    }
    let applied = apply_edits(prev, added).ok()?;
    Some(applied.rows.into_iter().map(|r| r.id).collect())
}

/// True when the stored row order of `ds` differs from the replayed order, while the rows and cells agree.
fn has_unrecorded_order(ds: &Dataset, source: &[u8]) -> bool {
    reconcile_source(ds, source).iter().any(|d| d.code == code::DATASET_ROW_ORDER_UNRECORDED)
}

/// A cell as the diagnostics show it: its text, its state and its exact decimal.
fn describe(cell: &DataCell) -> String {
    format!("\"{}\" ({:?}, decimal {})", cell.original, cell.state, cell.parsed_decimal.as_deref().unwrap_or("none"))
}

/// True when `new.transforms` starts with every transform already recorded on `old`.
pub fn history_extends(old: &Dataset, new: &Dataset) -> bool {
    if new.transforms.len() < old.transforms.len() {
        return false;
    }
    old.transforms.iter().zip(new.transforms.iter()).all(|(a, b)| same_json(a, b))
}

/// True when both values serialize and their JSON is equal. A value that does not serialize is equal to nothing, not to
/// another value that fails the same way.
fn same_json<T: Serialize + ?Sized>(a: &T, b: &T) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The origin that `next` carries as a version of `prev`: the version it replaces under the same id, or the parent it derives from.
/// `derived_transform` when the data changes (`data_changed`), from whatever origin `prev` had. The origin of `prev` when it does not,
/// so a transform that changes no cell, row order or column with values does not relabel the data. The service gives its edits this
/// origin, and PutDataset refuses any other, so the same change has the same origin on either path.
pub fn resulting_origin<'a>(prev: &'a Dataset, next: &Dataset) -> Cow<'a, str> {
    if data_changed(prev, next) {
        Cow::Borrowed(DERIVED_ORIGIN)
    } else {
        Cow::Borrowed(prev.origin.as_str())
    }
}

/// True when `next` may replace `prev` under the same id, or derive a new id from it, as far as its origin goes. It must carry
/// `resulting_origin`. A change of the data is a recorded derivation: it names the version it derives from and extends its history.
/// A copy under a new id therefore keeps its parent's origin only while it changes no data.
pub fn origin_change_allowed(prev: &Dataset, next: &Dataset) -> bool {
    if resulting_origin(prev, next) != next.origin.as_str() {
        return false;
    }
    !data_changed(prev, next)
        || (next.derived_from.as_ref().is_some_and(|parent| parent.dataset_id == prev.id && parent.version_hash == prev.version_hash)
            && next.transforms.len() > prev.transforms.len())
}

/// The columns of `ds` that hold data: at least one cell of the column is not missing.
fn data_columns(ds: &Dataset) -> Vec<&DataColumn> {
    ds.columns.iter().filter(|c| ds.rows.iter().any(|r| r.cells.get(&c.id).is_some_and(|cell| cell.state != CellState::Missing))).collect()
}

/// True when the data of `next` differs from `prev`: its row ids or their order, the cells of a data column, or a data column that
/// one of them has and the other lacks (with its name, type and unit). A column whose cells are all missing carries no data, so adding
/// or removing one changes no data. The transform count does not decide it: a transform that changes none of these changes no data.
pub fn data_changed(prev: &Dataset, next: &Dataset) -> bool {
    let before = data_columns(prev);
    let after = data_columns(next);
    let same_columns = before.len() == after.len()
        && before.iter().zip(&after).all(|(a, b)| a.id == b.id && a.name == b.name && a.data_type == b.data_type && a.unit == b.unit);
    if !same_columns || prev.rows.len() != next.rows.len() {
        return true;
    }
    prev.rows.iter().zip(&next.rows).any(|(a, b)| a.id != b.id || after.iter().any(|c| a.cells.get(&c.id) != b.cells.get(&c.id)))
}

const SELECTION_REFUSED: &str = "This command accepts only cell, structure, sort and unit scale edits";

/// Applies `edits` in order to a copy of `base`, as the service applies an edit: each edit is checked against the state that the
/// edits before it produced, and is recorded in the history. `service::edit_dataset`, PutDataset and the apply check of a legacy
/// dataset (`expected_order`) all apply edits through this function, so they refuse and place rows in the same way.
pub(crate) fn apply_edits(base: &Dataset, edits: &[DataTransform]) -> DltResult<Dataset> {
    // A row selection is a view of the rows, not an edit, so no version adds one. Core refuses it for figure.dataset.edit too.
    // The rows of a selection that an older file stores are still replayed (`replay_history`).
    if edits.iter().any(|edit| matches!(edit, DataTransform::SelectRows { .. })) {
        return Err(DltError::unsupported(SELECTION_REFUSED));
    }
    let mut next = base.clone();
    for edit in edits {
        apply_transform(&mut next, edit)?;
        next.transforms.push(edit.clone());
    }
    Ok(next)
}

/// Applies one edit to `next`. The edit is not yet in `next.transforms`, so its position is `next.transforms.len()`.
fn apply_transform(next: &mut Dataset, edit: &DataTransform) -> DltResult<()> {
    match edit {
        DataTransform::EditCell { row_id, column_id, previous, replacement } => edit_cell(next, row_id, column_id, previous, replacement),
        DataTransform::AddColumn { name, data_type, unit, fill } => add_column(next, name, data_type, unit.as_deref(), fill.as_deref()),
        DataTransform::AddRows { after_row_id, rows } => add_rows(next, after_row_id.as_deref(), rows),
        DataTransform::RemoveRow { row_id, previous } => remove_row(next, row_id, previous),
        DataTransform::RemoveColumn { column_id, previous_name } => remove_column(next, column_id, previous_name),
        DataTransform::Sort { column_id, descending } => sort_rows(next, column_id, *descending),
        DataTransform::SelectRows { .. } => Err(DltError::unsupported(SELECTION_REFUSED)),
        DataTransform::UnitScale { column_id, numerator, denominator, output_unit } => {
            unit_scale(next, column_id, numerator, denominator, output_unit)
        }
    }
}

/// Adds the output column of a unit scale of a numeric or count column: the exact scaled cell of each row, as `check_unit_scale`
/// verifies it. A scaled value that is not an exact decimal is refused, and nothing changes.
fn unit_scale(next: &mut Dataset, column_id: &str, numerator: &str, denominator: &str, output_unit: &str) -> DltResult<()> {
    let source = next
        .columns
        .iter()
        .find(|c| c.id == column_id)
        .ok_or_else(|| DltError::new(ErrorCode::NotFound, "Dataset column not found"))?;
    if !matches!(source.data_type.as_str(), "numeric" | "count") {
        return Err(DltError::unsupported(format!("column \"{column_id}\" is not numeric, so it cannot be scaled")));
    }
    match (Dec::parse(numerator), Dec::parse(denominator)) {
        (Ok(_), Ok(d)) if d.coef != 0 => {}
        _ => return Err(DltError::invalid(format!("unit_scale factor {numerator}/{denominator} is not a valid non-zero decimal ratio"))),
    }
    if output_unit.trim().is_empty() {
        return Err(DltError::invalid("A unit scale needs an output unit"));
    }
    let derived = derived_column_id(column_id, output_unit);
    if next.columns.iter().any(|c| c.id == derived) {
        return Err(DltError::invalid(format!("Column id \"{derived}\" is already taken")));
    }
    // A derived column is named by its id, so a column that already has that name would be a second column of the same name.
    if next.columns.iter().any(|c| c.name == derived) {
        return Err(DltError::invalid(format!("A column named \"{derived}\" already exists")));
    }
    let mut scaled = Vec::with_capacity(next.rows.len());
    for row in &next.rows {
        let src = row
            .cells
            .get(column_id)
            .ok_or_else(|| DltError::new(ErrorCode::Internal, format!("Row {} has no cell in column {column_id}", row.id)))?;
        let cell = scaled_cell(src, numerator, denominator).ok_or_else(|| not_exact_error(&next.id, &row.id, column_id))?;
        scaled.push(cell);
    }
    for (row, cell) in next.rows.iter_mut().zip(scaled) {
        row.cells.insert(derived.clone(), cell);
    }
    next.columns.push(DataColumn { id: derived.clone(), name: derived, data_type: "numeric".to_owned(), unit: Some(output_unit.to_owned()) });
    Ok(())
}

/// True when a UnitScale transform of `ds` produces the column `column_id`.
fn is_derived(ds: &Dataset, column_id: &str) -> bool {
    ds.transforms.iter().any(|t| matches!(t, DataTransform::UnitScale { column_id: source, output_unit, .. } if derived_column_id(source, output_unit) == column_id))
}

/// True when an AddColumn of `ds` creates the column `column_id`. The id of an added column is the position of its AddColumn.
fn is_added(ds: &Dataset, column_id: &str) -> bool {
    ds.transforms.iter().enumerate().any(|(i, t)| matches!(t, DataTransform::AddColumn { .. }) && added_column_id(i) == column_id)
}

/// The stored cell for `text` in a column of `data_type`. Blank text is a missing cell. Ambiguous or malformed text is refused.
fn cell_of(data_type: &str, text: &str, what: &str) -> DltResult<DataCell> {
    let (state, parsed_decimal) = classify_cell(data_type, text);
    if matches!(state, CellState::Ambiguous | CellState::Malformed) {
        return Err(DltError::new(ErrorCode::DataAmbiguous, format!("{what} is not a supported unambiguous {data_type} value")));
    }
    Ok(DataCell { original: text.to_owned(), parsed_decimal, state })
}

/// Replaces one cell. A unit-scaled column follows its source: after the new text is stored, the derived cells of the row are
/// recomputed from their sources, as the replay reads them. A new value whose scaled result is not an exact decimal is refused.
fn edit_cell(next: &mut Dataset, row_id: &str, column_id: &str, previous: &str, replacement: &str) -> DltResult<()> {
    let data_type = next.columns.iter().find(|c| c.id == column_id)
        .map(|c| c.data_type.clone())
        .ok_or_else(|| DltError::new(ErrorCode::NotFound, "Dataset column not found"))?;
    if is_derived(next, column_id) {
        return Err(DltError::unsupported("Edit the source of a derived column instead"));
    }
    let at = next.rows.iter().position(|r| r.id == row_id)
        .ok_or_else(|| DltError::new(ErrorCode::NotFound, "Dataset cell not found"))?;
    let cell = next.rows[at].cells.get_mut(column_id)
        .ok_or_else(|| DltError::new(ErrorCode::NotFound, "Dataset cell not found"))?;
    if cell.original != previous {
        return Err(DltError::new(ErrorCode::RevisionConflict, format!("Cell {row_id}/{column_id} changed since it was read")));
    }
    *cell = cell_of(&data_type, replacement, &format!("Cell {row_id}/{column_id}"))?;
    refresh_scaled_cells(next, at)
}

/// Recomputes the derived cells of one row from their sources, in the order the UnitScales were recorded, so that a chain reads
/// the output of the scale before it. A source that no longer scales exactly is refused. The caller discards its copy of the
/// dataset when this fails, so no row is left half-updated.
fn refresh_scaled_cells(next: &mut Dataset, at: usize) -> DltResult<()> {
    let row = &mut next.rows[at];
    for tr in &next.transforms {
        let DataTransform::UnitScale { column_id, numerator, denominator, output_unit } = tr else { continue };
        let src = row
            .cells
            .get(column_id)
            .ok_or_else(|| DltError::new(ErrorCode::Internal, format!("Row {} has no cell in column {column_id}", row.id)))?;
        let cell = scaled_cell(src, numerator, denominator).ok_or_else(|| not_exact_error(&next.id, &row.id, column_id))?;
        row.cells.insert(derived_column_id(column_id, output_unit), cell);
    }
    Ok(())
}

/// A new column after the last one. Every existing row gets `fill` (blank when absent). The name must be new.
fn add_column(next: &mut Dataset, name: &str, data_type: &str, unit: Option<&str>, fill: Option<&str>) -> DltResult<()> {
    if name.trim().is_empty() {
        return Err(DltError::invalid("A new column needs a name"));
    }
    if next.columns.iter().any(|c| c.name == name) {
        return Err(DltError::invalid(format!("A column named \"{name}\" already exists")));
    }
    if !COLUMN_TYPES.contains(&data_type) {
        return Err(DltError::invalid(format!("Column type \"{data_type}\" is not one of {:?}", COLUMN_TYPES)));
    }
    let id = added_column_id(next.transforms.len());
    if next.columns.iter().any(|c| c.id == id) {
        return Err(DltError::invalid(format!("Column id \"{id}\" is already taken")));
    }
    let cell = cell_of(data_type, fill.unwrap_or(""), &format!("The fill of column \"{name}\""))?;
    for row in &mut next.rows {
        row.cells.insert(id.clone(), cell.clone());
    }
    next.columns.push(DataColumn { id, name: name.to_owned(), data_type: data_type.to_owned(), unit: unit.map(str::to_owned) });
    Ok(())
}

/// New rows after `after_row_id` (the end when None). A column a row leaves out is a missing cell. A derived column
/// (UnitScale output) takes its value from the same row's source cell, the way the unit scale of an existing row does.
fn add_rows(next: &mut Dataset, after_row_id: Option<&str>, rows: &[BTreeMap<String, String>]) -> DltResult<()> {
    if rows.is_empty() {
        return Err(DltError::invalid("A row addition needs at least one row"));
    }
    let at = match after_row_id {
        None => next.rows.len(),
        Some(id) => next.rows.iter().position(|r| r.id == id).map(|p| p + 1)
            .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("Dataset row \"{id}\" not found")))?,
    };
    let index = next.transforms.len();
    let scales: Vec<(String, String, String, String)> = next
        .transforms
        .iter()
        .filter_map(|t| match t {
            DataTransform::UnitScale { column_id, numerator, denominator, output_unit } => {
                let derived = derived_column_id(column_id, output_unit);
                next.columns.iter().any(|c| c.id == derived).then(|| (column_id.clone(), derived, numerator.clone(), denominator.clone()))
            }
            _ => None,
        })
        .collect();
    let mut created = Vec::with_capacity(rows.len());
    for (k, values) in rows.iter().enumerate() {
        let id = added_row_id(index, k);
        if next.rows.iter().any(|r| r.id == id) {
            return Err(DltError::invalid(format!("Row id \"{id}\" is already taken")));
        }
        let mut cells: BTreeMap<String, DataCell> = BTreeMap::new();
        for (column_id, text) in values {
            if scales.iter().any(|(_, derived, _, _)| derived == column_id) {
                return Err(DltError::unsupported("Edit the source of a derived column instead"));
            }
            let column = next.columns.iter().find(|c| &c.id == column_id)
                .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("Dataset column \"{column_id}\" not found")))?;
            cells.insert(column_id.clone(), cell_of(&column.data_type, text, &format!("The new row's cell {column_id}"))?);
        }
        for column in &next.columns {
            if cells.contains_key(&column.id) || scales.iter().any(|(_, derived, _, _)| derived == &column.id) {
                continue;
            }
            cells.insert(column.id.clone(), cell_of(&column.data_type, "", "A blank cell")?);
        }
        for (source, derived, numerator, denominator) in &scales {
            let src = cells.get(source).cloned()
                .unwrap_or(DataCell { original: String::new(), parsed_decimal: None, state: CellState::Missing });
            let scaled = match src.state {
                CellState::Valid => {
                    let exact = Dec::parse(&src.original).ok().and_then(|d| {
                        exact_scale(d.exact(), Dec::parse(numerator).ok()?, Dec::parse(denominator).ok()?)
                    });
                    let Some(e) = exact else {
                        return Err(not_exact_error(&next.id, &id, source));
                    };
                    let text = e.canonical();
                    DataCell { original: text.clone(), parsed_decimal: Some(text), state: CellState::Valid }
                }
                other => DataCell { original: src.original.clone(), parsed_decimal: None, state: other },
            };
            cells.insert(derived.clone(), scaled);
        }
        created.push(DataRow { id, cells });
    }
    for (offset, row) in created.into_iter().enumerate() {
        next.rows.insert(at + offset, row);
    }
    Ok(())
}

/// Removes a row. `previous` must hold the row's current text in every column (column id to original text).
fn remove_row(next: &mut Dataset, row_id: &str, previous: &BTreeMap<String, String>) -> DltResult<()> {
    let at = next.rows.iter().position(|r| r.id == row_id)
        .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("Dataset row \"{row_id}\" not found")))?;
    let row = &next.rows[at];
    let unchanged = row.cells.len() == previous.len() && row.cells.iter().all(|(column_id, cell)| previous.get(column_id) == Some(&cell.original));
    if !unchanged {
        return Err(DltError::new(ErrorCode::RevisionConflict, format!("Row \"{row_id}\" changed since it was read")));
    }
    next.rows.remove(at);
    Ok(())
}

/// Removes a column and its cells. `previous_name` must be the column's current name. A unit-scaled column (its source
/// or its output) cannot be removed, because the unit scale is recorded against it.
fn remove_column(next: &mut Dataset, column_id: &str, previous_name: &str) -> DltResult<()> {
    let at = next.columns.iter().position(|c| c.id == column_id)
        .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("Dataset column \"{column_id}\" not found")))?;
    if next.columns[at].name != previous_name {
        return Err(DltError::new(ErrorCode::RevisionConflict, format!("Column \"{column_id}\" was renamed since it was read")));
    }
    let scaled = next.transforms.iter().any(|t| matches!(t, DataTransform::UnitScale { column_id: source, output_unit, .. }
        if source == column_id || derived_column_id(source, output_unit) == column_id));
    if scaled {
        return Err(DltError::unsupported("A unit-scaled column cannot be removed"));
    }
    // A sort orders by the column's type. An added column keeps its type in its AddColumn, so its sort still replays after the
    // removal. An original column keeps its type nowhere once RemoveColumn drops it, so a sort that orders by it could not be
    // replayed: that removal is refused, and nothing changes.
    let sorted = next.transforms.iter().any(|t| matches!(t, DataTransform::Sort { column_id: key, .. } if key == column_id));
    if sorted && !is_added(next, column_id) {
        return Err(DltError::unsupported("An original column that a Sort orders by cannot be removed: its sort could not be replayed without its type"));
    }
    next.columns.remove(at);
    for row in &mut next.rows {
        row.cells.remove(column_id);
    }
    Ok(())
}

/// Orders the rows by the values of one column as they are now. The order is the one `sort_permutation` gives,
/// which is also the order the replay of the recorded history produces. A later cell edit does not reorder the rows; the
/// next Sort does.
fn sort_rows(next: &mut Dataset, column_id: &str, descending: bool) -> DltResult<()> {
    let data_type = next
        .columns
        .iter()
        .find(|c| c.id == column_id)
        .map(|c| c.data_type.clone())
        .ok_or_else(|| DltError::new(ErrorCode::NotFound, "Dataset column not found"))?;
    let perm = {
        let mut keyed: Vec<(&str, Cow<'_, DataCell>)> = Vec::with_capacity(next.rows.len());
        for row in &next.rows {
            let cell = row
                .cells
                .get(column_id)
                .ok_or_else(|| DltError::new(ErrorCode::Internal, format!("Row {} has no cell in column {column_id}", row.id)))?;
            keyed.push((row.id.as_str(), Cow::Borrowed(cell)));
        }
        sort_permutation(&data_type, &keyed, descending)
            .map_err(|(_, message)| DltError::new(ErrorCode::DataAmbiguous, message))?
    };
    let mut slots: Vec<Option<DataRow>> = std::mem::take(&mut next.rows).into_iter().map(Some).collect();
    for p in perm {
        let row = slots
            .get_mut(p)
            .and_then(|slot| slot.take())
            .ok_or_else(|| DltError::new(ErrorCode::Internal, "Sort order is not a permutation of the rows"))?;
        next.rows.push(row);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decls() -> Vec<ColumnDecl> {
        vec![
            ColumnDecl { name: "condition".into(), data_type: "category".into(), unit: None },
            ColumnDecl { name: "mean".into(), data_type: "numeric".into(), unit: Some("ratio".into()) },
            ColumnDecl { name: "sd".into(), data_type: "numeric".into(), unit: Some("ratio".into()) },
            ColumnDecl { name: "n".into(), data_type: "count".into(), unit: None },
        ]
    }

    #[test]
    fn missing_ambiguous_and_malformed_never_become_zero() {
        assert_eq!(classify_cell("numeric", ""), (CellState::Missing, None));
        assert_eq!(classify_cell("numeric", "1,234").0, CellState::Ambiguous);
        assert_eq!(classify_cell("numeric", "0.1.2").0, CellState::Malformed);
        assert_eq!(classify_cell("numeric", ".20"), (CellState::Valid, Some("0.2".into())));
        assert_eq!(classify_cell("count", "2.5").0, CellState::Malformed);
        assert_eq!(classify_cell("count", "0").0, CellState::Malformed);
        assert_eq!(classify_cell("count", "3"), (CellState::Valid, Some("3".into())));
    }

    #[test]
    fn import_keeps_original_text_and_hashes_the_source() {
        let src = b"condition,mean,sd,n,unit\nbaseline,0.10,0.01,3,ratio\ntreated,0.20,0.02,3,ratio\n";
        let ds = import("ds", "user_experimental", src, &decls()).unwrap();
        assert_eq!(ds.rows.len(), 2);
        assert_eq!(ds.rows[0].id, "r0001");
        assert_eq!(ds.rows[0].cells["mean"].original, "0.10");
        assert_eq!(ds.rows[0].cells["mean"].parsed_decimal.as_deref(), Some("0.1"));
        assert_eq!(ds.rows[0].cells["unit"].original, "ratio", "undeclared column is text and kept");
        assert_eq!(ds.original_asset_hash, sha256(src));
        assert!(check_shape(&ds).is_empty());
        assert!(reconcile_source(&ds, src).is_empty());
    }

    #[test]
    fn version_hash_is_stable_and_detects_content_changes() {
        let src = b"condition,mean,sd,n\nA,0.10,0.01,3\n";
        let a = import("ds", "example", src, &decls()).unwrap();
        let b = import("ds", "example", src, &decls()).unwrap();
        assert_eq!(a.version_hash, b.version_hash);
        let mut changed = a.clone();
        changed.rows[0].cells.get_mut("mean").unwrap().original = "0.11".into();
        assert_ne!(compute_version(&changed).unwrap(), a.version_hash);
        assert!(check_shape(&changed).iter().any(|d| d.code == code::DATASET_VERSION_MISMATCH));
    }

    #[test]
    fn unexplained_edits_and_fabricated_states_are_blocking() {
        let src = b"condition,mean,sd,n\nA,0.10,0.01,3\n";
        let mut ds = import("ds", "example", src, &decls()).unwrap();
        ds.rows[0].cells.get_mut("mean").unwrap().original = "0.99".into();
        ds.rows[0].cells.get_mut("mean").unwrap().parsed_decimal = Some("0.99".into());
        set_version(&mut ds).unwrap();
        let codes: Vec<String> = reconcile_source(&ds, src).into_iter().map(|d| d.code).collect();
        assert!(codes.contains(&code::DATASET_SOURCE_MISMATCH.to_owned()), "{codes:?}");

        let mut forged = import("ds", "example", src, &decls()).unwrap();
        forged.rows[0].cells.get_mut("mean").unwrap().state = CellState::Missing;
        assert!(check_shape(&forged).iter().any(|d| d.code == code::CELL_STATE_MISMATCH));
    }

    #[test]
    fn recorded_edit_explains_the_difference_and_must_match() {
        let src = b"condition,mean,sd,n\nA,0.10,0.01,3\n";
        let mut ds = import("ds", "example", src, &decls()).unwrap();
        ds.rows[0].cells.get_mut("mean").unwrap().original = "0.12".into();
        ds.rows[0].cells.get_mut("mean").unwrap().parsed_decimal = Some("0.12".into());
        ds.transforms.push(DataTransform::EditCell {
            row_id: "r0001".into(),
            column_id: "mean".into(),
            previous: "0.10".into(),
            replacement: "0.12".into(),
        });
        set_version(&mut ds).unwrap();
        assert!(reconcile_source(&ds, src).is_empty(), "{:?}", reconcile_source(&ds, src));
        assert!(check_shape(&ds).is_empty());
    }

    #[test]
    fn unit_scale_output_is_verified_exactly() {
        let src = b"condition,mean,sd,n\nA,0.25,0.01,3\n";
        let mut ds = import("ds", "example", src, &decls()).unwrap();
        ds.columns.push(DataColumn {
            id: derived_column_id("mean", "mmol"),
            name: derived_column_id("mean", "mmol"),
            data_type: "numeric".into(),
            unit: Some("mmol".into()),
        });
        for r in ds.rows.iter_mut() {
            r.cells.insert(
                derived_column_id("mean", "mmol"),
                DataCell { original: "250".into(), parsed_decimal: Some("250".into()), state: CellState::Valid },
            );
        }
        ds.transforms.push(DataTransform::UnitScale {
            column_id: "mean".into(),
            numerator: "1000".into(),
            denominator: "1".into(),
            output_unit: "mmol".into(),
        });
        set_version(&mut ds).unwrap();
        assert!(check_shape(&ds).is_empty(), "{:?}", check_shape(&ds));
        ds.rows[0].cells.get_mut(&derived_column_id("mean", "mmol")).unwrap().original = "251".into();
        assert!(check_shape(&ds).iter().any(|d| d.code == code::DATASET_TRANSFORM_MISMATCH));
    }

    #[test]
    fn select_rows_keeps_only_the_named_rows_and_nothing_else_is_dropped() {
        let src = b"condition,mean,sd,n\nA,0.10,0.01,3\nB,0.20,0.02,3\nC,0.30,0.03,3\n";
        let full = import("ds", "example", src, &decls()).unwrap();

        // Two rows kept by an explicit SelectRows: consistent.
        let mut kept = full.clone();
        kept.rows.retain(|r| r.id != "r0002");
        kept.transforms.push(DataTransform::SelectRows { row_ids: vec!["r0001".into(), "r0003".into()] });
        set_version(&mut kept).unwrap();
        assert!(reconcile_source(&kept, src).is_empty(), "{:?}", reconcile_source(&kept, src));

        // The same two rows without a recorded selection: an unexplained loss of data.
        let mut silent = kept.clone();
        silent.transforms.clear();
        set_version(&mut silent).unwrap();
        assert!(reconcile_source(&silent, src).iter().any(|d| d.code == code::DATASET_ROWS_MISMATCH));

        // A selection that names a row the source does not have is refused.
        let mut unknown = full.clone();
        unknown.transforms.push(DataTransform::SelectRows { row_ids: vec!["r9999".into()] });
        unknown.rows.clear();
        set_version(&mut unknown).unwrap();
        assert!(reconcile_source(&unknown, src).iter().any(|d| d.code == code::DATASET_TRANSFORM_MISMATCH));
    }

    /// Source for the row-order tests: three rows, in this order, with ids r0001..r0003.
    const ORDER_SRC: &[u8] = b"condition,mean,sd,n\nA,0.10,0.01,3\nB,0.20,0.02,3\nC,0.30,0.03,3\n";

    /// Codes of the blocking diagnostics that reconcile_source reports for `ds` against `src`.
    fn blocking_codes(ds: &Dataset, src: &[u8]) -> Vec<String> {
        reconcile_source(ds, src).into_iter().filter(|d| d.blocking).map(|d| d.code).collect()
    }

    /// Codes of the non-blocking diagnostics that reconcile_source reports for `ds` against `src`.
    fn warning_codes(ds: &Dataset, src: &[u8]) -> Vec<String> {
        reconcile_source(ds, src).into_iter().filter(|d| !d.blocking).map(|d| d.code).collect()
    }

    #[test]
    fn reordered_source_rows_without_a_recorded_transform_are_a_warning_that_keeps_the_order() {
        let mut ds = import("ds", "example", ORDER_SRC, &decls()).unwrap();
        ds.rows.swap(0, 1);
        set_version(&mut ds).unwrap();
        assert!(check_shape(&ds).is_empty(), "{:?}", check_shape(&ds));
        assert!(blocking_codes(&ds, ORDER_SRC).is_empty(), "{:?}", reconcile_source(&ds, ORDER_SRC));
        assert_eq!(warning_codes(&ds, ORDER_SRC), vec![code::DATASET_ROW_ORDER_UNRECORDED.to_owned()]);
        assert_eq!(ds.rows[0].id, "r0002", "the stored order is not changed");
    }

    #[test]
    fn an_added_row_must_sit_right_after_its_anchor() {
        let base = import("ds", "example", ORDER_SRC, &decls()).unwrap();
        let added = DataRow { id: added_row_id(0, 0), cells: base.rows[0].cells.clone() };
        // The payload is the added row's text, so the stored cells are the cells its AddRows gives.
        let payload: BTreeMap<String, String> = base.rows[0].cells.iter().map(|(name, cell)| (name.clone(), cell.original.clone())).collect();
        let transform = DataTransform::AddRows { after_row_id: Some("r0001".into()), rows: vec![payload] };

        let mut after_anchor = base.clone();
        after_anchor.rows.insert(1, added.clone());
        after_anchor.transforms.push(transform.clone());
        set_version(&mut after_anchor).unwrap();
        assert!(reconcile_source(&after_anchor, ORDER_SRC).is_empty(), "{:?}", reconcile_source(&after_anchor, ORDER_SRC));

        let mut at_end = base.clone();
        at_end.rows.push(added);
        at_end.transforms.push(transform);
        set_version(&mut at_end).unwrap();
        assert!(blocking_codes(&at_end, ORDER_SRC).is_empty(), "{:?}", reconcile_source(&at_end, ORDER_SRC));
        assert_eq!(warning_codes(&at_end, ORDER_SRC), vec![code::DATASET_ROW_ORDER_UNRECORDED.to_owned()]);
    }

    #[test]
    fn a_filter_keeps_the_source_order() {
        let mut kept = import("ds", "example", ORDER_SRC, &decls()).unwrap();
        kept.rows.retain(|r| r.id != "r0002");
        kept.transforms.push(DataTransform::SelectRows { row_ids: vec!["r0001".into(), "r0003".into()] });
        set_version(&mut kept).unwrap();
        assert!(reconcile_source(&kept, ORDER_SRC).is_empty(), "{:?}", reconcile_source(&kept, ORDER_SRC));

        let mut reversed = kept.clone();
        reversed.rows.reverse();
        set_version(&mut reversed).unwrap();
        assert!(blocking_codes(&reversed, ORDER_SRC).is_empty(), "{:?}", reconcile_source(&reversed, ORDER_SRC));
        assert_eq!(warning_codes(&reversed, ORDER_SRC), vec![code::DATASET_ROW_ORDER_UNRECORDED.to_owned()]);
    }

    #[test]
    fn a_sort_orders_the_rows_and_a_stored_order_that_differs_from_it_is_a_warning() {
        // Descending by mean: r0003 (0.30), r0002 (0.20), r0001 (0.10). The stored rows are already in that order.
        let mut sorted = import("ds", "example", ORDER_SRC, &decls()).unwrap();
        sorted.rows.reverse();
        sorted.transforms.push(DataTransform::Sort { column_id: "mean".into(), descending: true });
        set_version(&mut sorted).unwrap();
        assert!(check_shape(&sorted).is_empty(), "{:?}", check_shape(&sorted));
        assert!(blocking_codes(&sorted, ORDER_SRC).is_empty(), "{:?}", reconcile_source(&sorted, ORDER_SRC));

        // The same rows in source order do not follow the recorded sort: a warning on open. A change may not introduce it.
        let mut unsorted = sorted.clone();
        unsorted.rows.reverse();
        set_version(&mut unsorted).unwrap();
        assert!(blocking_codes(&unsorted, ORDER_SRC).is_empty(), "{:?}", reconcile_source(&unsorted, ORDER_SRC));
        assert_eq!(warning_codes(&unsorted, ORDER_SRC), vec![code::DATASET_ROW_ORDER_UNRECORDED.to_owned()]);
    }

    #[test]
    fn an_anchor_missing_at_that_point_is_refused() {
        let base = import("ds", "example", ORDER_SRC, &decls()).unwrap();
        let previous: BTreeMap<String, String> = base.rows[1].cells.iter().map(|(name, cell)| (name.clone(), cell.original.clone())).collect();
        let mut ds = base;
        ds.rows.retain(|r| r.id != "r0002");
        ds.transforms.push(DataTransform::RemoveRow { row_id: "r0002".into(), previous });
        ds.transforms.push(DataTransform::AddRows { after_row_id: Some("r0002".into()), rows: vec![BTreeMap::new()] });
        set_version(&mut ds).unwrap();
        assert_eq!(blocking_codes(&ds, ORDER_SRC), vec![code::DATASET_TRANSFORM_MISMATCH.to_owned()]);
    }

    /// A value that cannot be serialized, so that a history holding it cannot be compared.
    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(<S::Error as serde::ser::Error>::custom("no JSON form"))
        }
    }

    #[test]
    fn values_that_fail_to_serialize_are_never_the_same_history() {
        assert!(!same_json(&Unserializable, &Unserializable), "two failures are not equal");
        let sort = DataTransform::Sort { column_id: "mean".into(), descending: false };
        assert!(same_json(&sort, &sort));
        let other = DataTransform::Sort { column_id: "mean".into(), descending: true };
        assert!(!same_json(&sort, &other));
    }

    #[test]
    fn an_empty_version_hash_is_refused_by_the_shape_check() {
        let src = b"condition,mean,sd,n\nA,0.10,0.01,3\n";
        let mut ds = import("ds", "example", src, &decls()).unwrap();
        ds.version_hash.clear();
        assert!(check_shape(&ds).iter().any(|d| d.code == code::DATASET_VERSION_MISMATCH));
    }

    #[test]
    fn the_origin_changes_only_through_a_recorded_derivation_of_the_same_id() {
        let src = b"condition,mean,sd,n\nA,0.10,0.01,3\n";
        let base = import("ds", "simulation", src, &decls()).unwrap();
        let parent = DatasetRef { dataset_id: base.id.clone(), version_hash: base.version_hash.clone() };

        assert!(origin_change_allowed(&base, &base), "an unchanged origin");
        let mut relabel = base.clone();
        relabel.origin = "user_experimental".into();
        assert!(!origin_change_allowed(&base, &relabel), "a relabel is not a derivation");

        // A recorded edit that changes a cell is a derivation of the same id.
        let mut recorded = base.clone();
        recorded.origin = DERIVED_ORIGIN.into();
        recorded.derived_from = Some(parent);
        recorded.rows[0].cells.get_mut("mean").unwrap().original = "0.12".into();
        recorded.rows[0].cells.get_mut("mean").unwrap().parsed_decimal = Some("0.12".into());
        recorded.transforms.push(DataTransform::EditCell {
            row_id: "r0001".into(),
            column_id: "mean".into(),
            previous: "0.10".into(),
            replacement: "0.12".into(),
        });
        assert!(origin_change_allowed(&base, &recorded), "a recorded derivation of the same id");

        // The same transform with no change of the data relabels nothing (a no-op edit, a sort of rows in order).
        let mut no_change = recorded.clone();
        no_change.rows = base.rows.clone();
        assert!(!origin_change_allowed(&base, &no_change), "a transform that changes no cell, row order or column relabels nothing");

        let mut unrecorded = recorded.clone();
        unrecorded.transforms.clear();
        assert!(!origin_change_allowed(&base, &unrecorded), "no transform records the derivation");

        let mut not_from_parent = recorded.clone();
        not_from_parent.derived_from = Some(DatasetRef { dataset_id: base.id.clone(), version_hash: sha256(b"old") });
        assert!(!origin_change_allowed(&base, &not_from_parent), "the derivation must name the version it replaces");

        let mut copy = recorded.clone();
        copy.id = "copy".into();
        assert!(origin_change_allowed(&base, &copy), "a derived copy whose data changed is derived_transform");
        let mut relabelled_copy = copy.clone();
        relabelled_copy.origin = "user_experimental".into();
        assert!(!origin_change_allowed(&base, &relabelled_copy), "a derived copy cannot take its parent's data back as experimental");
        let mut kept = base.clone();
        kept.id = "copy".into();
        assert!(origin_change_allowed(&base, &kept), "a copy that keeps the origin");
    }
}
