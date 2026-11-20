use std::collections::BTreeMap;

use designllm_contracts::*;
use crate::{dataset, diag, table};

/// Reads immutable user-supplied tabular data. Numeric syntax can be recognized;
/// measurement units and whether the experiment is valid cannot be inferred.
pub fn import_csv(asset: &AssetRef, assets: &dyn AssetStore) -> DltResult<Dataset> {
    let delimiter = delimiter_of(asset)?;
    let bytes = verified_bytes(asset, assets)?;
    let parsed = table::parse_delimited(&bytes, delimiter).map_err(DltError::invalid)?;
    let declared = parsed.header.iter().enumerate().map(|(index, name)| {
        let values: Vec<_> = parsed.records.iter().map(|row| row.cells[index].as_str()).filter(|v| !v.trim().is_empty()).collect();
        let data_type = if name == "id" { "category" }
            else if name == "n" && !values.is_empty() && values.iter().all(|v| dataset::classify_cell("count", v).0 == CellState::Valid) { "count" }
            else if !values.is_empty() && values.iter().all(|v| dataset::classify_cell("numeric", v).0 == CellState::Valid) { "numeric" }
            else { "text" };
        dataset::ColumnDecl { name: name.clone(), data_type: data_type.into(), unit: None }
    }).collect::<Vec<_>>();
    dataset::import_table(&format!("data-{}", &asset.sha256[..16]), "user_experimental", &bytes, &declared, parsed)
}

/// Reads CSV/TSV with the column types and units the caller declares. Nothing is inferred.
/// `columns` must name every header column exactly once; matching uses `DataColumn.name`, in any order, and
/// `DataColumn.id` is ignored (ids are assigned as in `import_csv`). A unit is allowed only on numeric columns.
/// Cells keep their source text and are classified as in `import_csv`: a value that does not fit its declared
/// type stays Malformed or Ambiguous, and nothing is coerced or filled with zero.
pub fn import_csv_declared(asset: &AssetRef, assets: &dyn AssetStore, columns: &[DataColumn]) -> DltResult<Dataset> {
    let delimiter = delimiter_of(asset)?;
    let bytes = verified_bytes(asset, assets)?;
    let parsed = table::parse_delimited(&bytes, delimiter).map_err(DltError::invalid)?;
    let declared = declarations(&parsed.header, columns)?;
    dataset::import_table(&format!("data-{}", &asset.sha256[..16]), "user_experimental", &bytes, &declared, parsed)
}

/// Matches declarations to the header: every header column once, no unknown names, no repeats.
fn declarations(header: &[String], columns: &[DataColumn]) -> DltResult<Vec<dataset::ColumnDecl>> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for column in columns {
        *counts.entry(column.name.as_str()).or_insert(0) += 1;
    }
    let repeated: Vec<&str> = counts.iter().filter(|(_, n)| **n > 1).map(|(name, _)| *name).collect();
    if !repeated.is_empty() {
        return Err(DltError::invalid(format!("columns declared more than once: {}", repeated.join(", "))));
    }
    let unknown: Vec<&str> = counts.keys().copied().filter(|name| !header.iter().any(|h| h.as_str() == *name)).collect();
    if !unknown.is_empty() {
        return Err(DltError::invalid(format!("declared columns that the source does not have: {}", unknown.join(", "))));
    }
    let missing: Vec<&str> = header.iter().map(String::as_str).filter(|h| !counts.contains_key(h)).collect();
    if !missing.is_empty() {
        return Err(DltError::invalid(format!("source columns without a declaration: {}", missing.join(", "))));
    }
    let mut out = Vec::with_capacity(header.len());
    for name in header {
        let column = columns
            .iter()
            .find(|c| c.name == *name)
            .ok_or_else(|| DltError::new(ErrorCode::Internal, format!("declaration for \"{name}\" disappeared")))?;
        if !dataset::COLUMN_TYPES.contains(&column.data_type.as_str()) {
            return Err(DltError::invalid(format!(
                "column \"{name}\" has data_type \"{}\"; supported types are {}",
                column.data_type,
                dataset::COLUMN_TYPES.join(", ")
            )));
        }
        if let Some(unit) = &column.unit {
            if column.data_type != "numeric" {
                return Err(DltError::invalid(format!("column \"{name}\": a unit is allowed only on numeric columns")));
            }
            if unit.trim().is_empty() {
                return Err(DltError::invalid(format!("column \"{name}\": unit is empty; omit it instead")));
            }
        }
        out.push(dataset::ColumnDecl { name: name.clone(), data_type: column.data_type.clone(), unit: column.unit.clone() });
    }
    Ok(out)
}

fn delimiter_of(asset: &AssetRef) -> DltResult<char> {
    match asset.mime.split(';').next().unwrap_or("").trim() {
        "text/csv" | "application/csv" => Ok(','),
        "text/tab-separated-values" => Ok('\t'),
        other => Err(DltError::unsupported(format!("Expected CSV or TSV MIME, found {other}"))),
    }
}

/// Source bytes, checked against the asset's length and hash before anything is parsed.
fn verified_bytes(asset: &AssetRef, assets: &dyn AssetStore) -> DltResult<Vec<u8>> {
    let bytes = assets.read(asset)?;
    if bytes.len() as u64 != asset.bytes || sha256(&bytes) != asset.sha256 {
        return Err(DltError::new(ErrorCode::IntegrityError, "CSV source bytes do not match their asset reference"));
    }
    Ok(bytes)
}

/// Derives an immutable version using the same exact parser as figure validation. The edits are applied in order, each
/// checked against the version the edits before it produced, and each one is recorded in the history. A new column or
/// row takes its id from the position of its edit in the history (`dataset::added_column_id`), so an id is never given
/// out twice and replaying the history gives the same ids.
pub fn edit_dataset(base: &Dataset, edits: &[DataTransform]) -> DltResult<Dataset> {
    let diagnostics = dataset::check_shape(base);
    if diag::has_blocking(&diagnostics) { return Err(diag::to_error(&diagnostics, "Invalid base dataset")); }
    if base.version_hash != dataset::compute_version(base)? {
        return Err(DltError::new(ErrorCode::IntegrityError, "The base dataset needs its canonical version hash"));
    }
    if edits.is_empty() { return Err(DltError::invalid("At least one cell edit is required")); }
    // `apply_edits` refuses a row selection, as PutDataset does.
    let mut next = dataset::apply_edits(base, edits)?;
    // The origin is the one PutDataset computes for the same change (`dataset::resulting_origin`).
    next.origin = dataset::resulting_origin(base, &next).into_owned();
    next.derived_from = Some(DatasetRef { dataset_id: base.id.clone(), version_hash: base.version_hash.clone() });
    dataset::set_version(&mut next)?;
    let diagnostics = dataset::check_shape(&next);
    if diag::has_blocking(&diagnostics) { return Err(diag::to_error(&diagnostics, "Derived dataset failed validation")); }
    Ok(next)
}
