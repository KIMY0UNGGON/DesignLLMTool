//! Canvas operations. `apply` runs on a clone supplied by the engine, so any error leaves the committed document intact.
//! Operations outside the canvas model return UnsupportedOperation instead of being approximated.

use super::figure;
use super::components;
use super::repeat;
use super::guard;
use super::model;
use super::tokens;
use super::model::{
    any_blocking, find_node, find_node_in_doc, find_node_in_doc_mut, find_node_mut, flatten, info_text_problem, subtree_ids,
    validate_bounds, validate_interaction, validate_layout, validate_node, validate_page_size, validate_patch, validate_path, validate_style,
    MAX_ALT_BYTES, MAX_DEPTH, MAX_GRID_COLUMNS, MAX_NODE_NAME_BYTES,
};
use crate::common::{fail, invalid, unsupported, valid_id, valid_key, MAX_NODES, MAX_PAGES, MAX_TEXT_BYTES};
use designllm_contracts::{
    sha256, AssetRef, AssetStore, CanvasBoardMemo, CanvasDocument, CanvasPageGuides, CanvasPageSection, CanvasTable, Diagnostic, DltError, DltResult, ErrorCode, FigureLink, ImageFit, Interaction, LayoutSpec,
    Node, NodeKind, Operation, Page, PathSpec, PixelRect, Rect, Size, StylePatch, TextListKind,
};
use designllm_contracts::media_header::PNG_SIGNATURE;
use std::collections::{BTreeMap, BTreeSet};
use unicode_segmentation::UnicodeSegmentation;

const MAX_REPLACEMENT_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;

/// Applies operations in order and returns the changed IDs. Any failure aborts the whole list.
/// `links` are the artifact's figure links. `assets` supplies the render that an `UpdateFigureLink` names.
pub(crate) fn apply(
    doc: &mut CanvasDocument,
    links: &mut Vec<FigureLink>,
    assets: &dyn AssetStore,
    operations: &[Operation],
) -> DltResult<(Vec<String>, Vec<Diagnostic>)> {
    let mut changed: Vec<String> = Vec::new();
    let mut interaction_sources = Vec::new();
    let mut seen_interaction_sources = BTreeSet::new();
    preflight_image_replacements(doc, links, operations)?;
    components::preflight_table_operations(doc, operations)?;
    let mut refresh_components = false;
    let mut component_diagnostics = Vec::new();
    let mut table_structure_ids = BTreeSet::new();
    let mut table_width_ids = BTreeSet::new();
    let mut board_memo_page_ids = BTreeSet::new();
    let mut guide_page_ids = BTreeSet::new();
    let mut page_section_batches = 0usize;
    let mut table_structures: Vec<&Operation> = Vec::new();
    let mut image_crop_ids = BTreeSet::new();
    for operation in operations {
        match operation {
            Operation::SetCanvasTableStructure { table_id, .. } => {
                if !table_structure_ids.insert(table_id.clone()) {
                    return Err(invalid("TABLE_STRUCTURE_DUPLICATE", "a ChangeSet may replace each Canvas table structure only once"));
                }
                table_structures.push(operation);
            }
            Operation::SetCanvasTableColumnWidths { table_id, .. } => {
                if !table_width_ids.insert(table_id.clone()) {
                    return Err(invalid("TABLE_WIDTH_DUPLICATE", "a ChangeSet may set each Canvas table width only once"));
                }
            }
            Operation::SetCanvasBoardMemos { page_id, .. } => {
                if !board_memo_page_ids.insert(page_id.clone()) {
                    return Err(invalid("BOARD_MEMOS_DUPLICATE", "a ChangeSet may replace each page's board memos only once"));
                }
            }
            Operation::SetCanvasPageGuides { page_id, .. } => {
                if !guide_page_ids.insert(page_id.clone()) {
                    return Err(invalid("CANVAS_GUIDES_DUPLICATE", "a ChangeSet may replace each page's guides only once"));
                }
            }
            Operation::SetCanvasPageSections { .. } => {
                page_section_batches += 1;
                if page_section_batches > 1 {
                    return Err(invalid("PAGE_SECTIONS_DUPLICATE", "a ChangeSet may replace Canvas page sections only once"));
                }
            }
            Operation::SetCanvasImageCrop { node_id, .. } => {
                if !image_crop_ids.insert(node_id.clone()) {
                    return Err(invalid("IMAGE_CROP_DUPLICATE", "a ChangeSet may update each Canvas image crop only once"));
                }
            }
            _ => {}
        }
    }
    if table_structure_ids.iter().any(|table_id| table_width_ids.contains(table_id)) {
        return Err(invalid("TABLE_WIDTH_STRUCTURE_BATCH", "table structure and standalone width changes cannot target the same table in one ChangeSet"));
    }
    let expand_repeats = repeat::needs_expansion(doc, operations);
    for op in operations {
        components::guard_operation(doc, op)?;
        repeat::guard_operation(doc, op)?;
        if matches!(op, Operation::SetCanvasTableStructure { .. }) {
            refresh_components |= components::operation_touches_source(doc, op);
            continue;
        }
        if refresh_components && components::operation_needs_fresh_instance(doc, op) {
            let (ids, diagnostics) = components::refresh_instances(doc)?;
            for id in ids { if !changed.contains(&id) { changed.push(id); } }
            component_diagnostics.extend(diagnostics);
            refresh_components = false;
        }
        let touches_component_source = components::operation_touches_source(doc, op);
        let pending_sources: Vec<String> = match op {
            Operation::AddNode { node, .. } => flatten(std::slice::from_ref(node)).into_iter().map(|node| node.id.clone()).collect(),
            Operation::InsertPage { page, .. } => flatten(&page.nodes).into_iter().map(|node| node.id.clone()).collect(),
            _ => Vec::new(),
        };
        let token_states = components::token_states_before(doc, op);
        for id in apply_one(doc, links, assets, op)? {
            if !changed.contains(&id) {
                changed.push(id);
            }
        }
        components::check_token_state_after(doc, token_states)?;
        for id in components::capture_override(doc, op)? {
            if !changed.contains(&id) { changed.push(id); }
        }
        refresh_components |= touches_component_source;
        for source_id in pending_sources {
            if seen_interaction_sources.insert(source_id.clone()) {
                interaction_sources.push(source_id);
            }
        }
    }
    for operation in table_structures {
        if let Operation::SetCanvasTableStructure { table_id, expected, table } = operation {
            for id in set_canvas_table_structure(doc, table_id, expected, table)? {
                if !changed.contains(&id) { changed.push(id); }
            }
        }
    }
    if refresh_components {
        let (ids, diagnostics) = components::refresh_instances(doc)?;
        for id in ids { if !changed.contains(&id) { changed.push(id); } }
        component_diagnostics.extend(diagnostics);
    }
    // Repeat groups expand again in the same change (D21), after every table and component edit of the batch.
    if expand_repeats {
        let (ids, diagnostics) = repeat::expand(doc)?;
        for id in ids { if !changed.contains(&id) { changed.push(id); } }
        component_diagnostics.extend(diagnostics);
    }
    // Validate only source nodes still present after the complete batch. Removed or replaced intermediates
    // are not observable; nodes with reused IDs are looked up in their final form.
    for source_id in interaction_sources {
        if let Some(node) = find_node_in_doc(doc, &source_id) {
            for interaction in &node.interactions {
                validate_interaction(doc, interaction)?;
            }
        }
    }
    Ok((changed, component_diagnostics))
}
fn table_owner_in_nodes<'a>(nodes: &'a [Node], cell_id: &str) -> Option<&'a Node> {
    for node in nodes {
        if node.ext.table.as_ref().is_some_and(|table| table.rows.iter().any(|row| row.cell_ids.iter().any(|id| id == cell_id))) {
            return Some(node);
        }
        if let Some(owner) = table_owner_in_nodes(&node.children, cell_id) {
            return Some(owner);
        }
    }
    None
}

fn table_owner_in_doc<'a>(doc: &'a CanvasDocument, cell_id: &str) -> Option<&'a Node> {
    doc.pages.iter().find_map(|page| table_owner_in_nodes(&page.nodes, cell_id))
}

fn ensure_table_owner_unlocked(doc: &CanvasDocument, cell_id: &str) -> DltResult<()> {
    if let Some(table) = table_owner_in_doc(doc, cell_id) {
        let metadata = table.ext.table.as_ref().expect("table owner carries table metadata");
        if model::covered_table_cell_ids(metadata).map_err(|problem| invalid("TABLE_CELL_SPANS", problem))?.contains(cell_id) {
            return Err(unsupported("TABLE_CELL_MERGED_COVERED", "a covered table cell cannot be edited until it is unmerged"));
        }
        ensure_unlocked(table)?;
    }
    Ok(())
}

pub(super) fn ensure_node_unlocked_in_doc(doc: &CanvasDocument, node_id: &str) -> DltResult<()> {
    ensure_table_owner_unlocked(doc, node_id)?;
    let node = find_node_in_doc(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    ensure_unlocked(node)
}

fn validate_table_spec(table_id: &str, table: &CanvasTable) -> DltResult<usize> {
    let first = table.rows.first().ok_or_else(|| invalid("TABLE_ROWS", "a Canvas table requires at least one row"))?;
    if table.rows.len() > MAX_NODES {
        return Err(invalid("TABLE_ROWS", "Canvas table has too many rows"));
    }
    let columns = first.cell_ids.len();
    if columns == 0 || columns > MAX_GRID_COLUMNS as usize {
        return Err(invalid("TABLE_COLUMNS", "Canvas table columns must be between 1 and 24"));
    }
    validate_column_widths(table_id, columns, table.column_widths.as_deref())?;
    if table.header_row_id.as_deref().is_some_and(|header| header != first.id) {
        return Err(invalid("TABLE_HEADER_ROW", "only the first row can be the header row"));
    }
    let mut row_ids = BTreeSet::new();
    let mut cell_ids = BTreeSet::new();
    for row in &table.rows {
        if !valid_id(&row.id) || !row_ids.insert(row.id.as_str()) {
            return Err(invalid("TABLE_ROW_ID", "table row ids must be valid and unique"));
        }
        if row.cell_ids.len() != columns {
            return Err(invalid("TABLE_ROW_COLUMNS", "all Canvas table rows must have the same number of cells"));
        }
        for cell_id in &row.cell_ids {
            if !valid_id(cell_id) || !cell_ids.insert(cell_id.as_str()) {
                return Err(invalid("TABLE_CELL_ID", "table cell ids must be valid and unique"));
            }
        }
    }
    model::table_span_grid(table).map_err(|problem| invalid("TABLE_CELL_SPANS", problem))?;
    if table_id.is_empty() {
        return Err(invalid("TABLE_TARGET", "table id is empty"));
    }
    Ok(columns)
}

fn validate_column_widths(table_id: &str, columns: usize, widths: Option<&[u32]>) -> DltResult<()> {
    if let Some(widths) = widths {
        if widths.len() != columns || widths.iter().any(|width| *width == 0) {
            return Err(invalid("TABLE_COLUMN_WIDTHS", format!("table {table_id} requires one positive relative width per column")));
        }
    }
    Ok(())
}

fn inserted_column_index(old: &[String], new: &[String]) -> Option<usize> {
    if new.len() != old.len().checked_add(1)? { return None; }
    let (mut old_index, mut new_index, mut inserted) = (0usize, 0usize, None);
    while old_index < old.len() {
        if old[old_index] == new[new_index] {
            old_index += 1;
            new_index += 1;
        } else if inserted.is_none() {
            inserted = Some(new_index);
            new_index += 1;
        } else {
            return None;
        }
    }
    if new_index < new.len() {
        if inserted.is_some() || new_index + 1 != new.len() { return None; }
        inserted = Some(new_index);
        new_index += 1;
    }
    (old_index == old.len() && new_index == new.len()).then_some(inserted?)
}

fn removed_column_index(old: &[String], new: &[String]) -> Option<usize> {
    if old.len() != new.len().checked_add(1)? { return None; }
    let (mut old_index, mut new_index, mut removed) = (0usize, 0usize, None);
    while new_index < new.len() {
        if old[old_index] == new[new_index] {
            old_index += 1;
            new_index += 1;
        } else if removed.is_none() {
            removed = Some(old_index);
            old_index += 1;
        } else {
            return None;
        }
    }
    if old_index < old.len() {
        if removed.is_some() || old_index + 1 != old.len() { return None; }
        removed = Some(old_index);
        old_index += 1;
    }
    (old_index == old.len() && new_index == new.len()).then_some(removed?)
}

fn structural_column_index(expected: &CanvasTable, table: &CanvasTable, inserted: bool) -> DltResult<usize> {
    let mut found = None;
    for old_row in &expected.rows {
        if let Some(new_row) = table.rows.iter().find(|row| row.id == old_row.id) {
            let index = if inserted {
                inserted_column_index(&old_row.cell_ids, &new_row.cell_ids)
            } else {
                removed_column_index(&old_row.cell_ids, &new_row.cell_ids)
            }.ok_or_else(|| invalid("TABLE_WIDTH_STRUCTURE", "custom column widths require one unambiguous column insertion or removal"))?;
            if found.is_some_and(|current| current != index) {
                return Err(invalid("TABLE_WIDTH_STRUCTURE", "all retained rows must change the same column position"));
            }
            found = Some(index);
        }
    }
    found.ok_or_else(|| invalid("TABLE_WIDTH_STRUCTURE", "custom column widths need at least one retained row to preserve column sizes"))
}

fn validate_structure_width_transition(expected: &CanvasTable, table: &CanvasTable) -> DltResult<()> {
    let old_columns = expected.rows.first().map_or(0, |row| row.cell_ids.len());
    let new_columns = table.rows.first().map_or(0, |row| row.cell_ids.len());
    if (!expected.cell_spans.is_empty() || !table.cell_spans.is_empty())
        && (expected.rows != table.rows || old_columns != new_columns)
    {
        return Err(invalid("TABLE_SPAN_STRUCTURE", "unmerge table cells before adding or removing rows or columns"));
    }
    if old_columns == new_columns {
        if expected.column_widths != table.column_widths {
            return Err(invalid("TABLE_WIDTH_INTENT", "width-only Canvas table changes require SetCanvasTableColumnWidths"));
        }
        return Ok(());
    }
    let Some(old_widths) = expected.column_widths.as_ref() else {
        if table.column_widths.is_none() { return Ok(()); }
        return Err(invalid("TABLE_WIDTH_INTENT", "column structure cannot introduce custom widths without SetCanvasTableColumnWidths"));
    };
    if old_widths.len() != old_columns || old_widths.iter().any(|width| *width == 0) {
        return Err(invalid("TABLE_COLUMN_WIDTHS", "the current custom width vector is invalid"));
    }
    if new_columns + 1 == old_columns {
        let index = structural_column_index(expected, table, false)?;
        let removed = old_widths[index];
        let mut expected_widths = old_widths.clone();
        expected_widths.remove(index);
        let merge_into = if index == 0 { 0 } else { index - 1 };
        expected_widths[merge_into] = expected_widths[merge_into]
            .checked_add(removed)
            .ok_or_else(|| invalid("TABLE_WIDTH_OVERFLOW", "merging a removed column would overflow its adjacent width"))?;
        if table.column_widths.as_ref() != Some(&expected_widths) {
            return Err(invalid("TABLE_WIDTH_STRUCTURE", "removing a custom-width column must merge its weight into the previous column (or the next when removing the first)"));
        }
        return Ok(());
    }
    if old_columns + 1 == new_columns {
        let index = structural_column_index(expected, table, true)?;
        let source = if index == 0 { 0 } else { index - 1 };
        let mut scaled = old_widths.clone();
        if scaled[source] == 1 {
            for width in &mut scaled {
                *width = width.checked_mul(2)
                    .ok_or_else(|| invalid("TABLE_WIDTH_OVERFLOW", "normalizing weights before splitting a column would overflow"))?;
            }
        }
        let width = scaled[source];
        let smaller = width / 2;
        let larger = width - smaller;
        if smaller == 0 {
            return Err(invalid("TABLE_WIDTH_SPLIT", "a custom-width column cannot be split into two positive integer weights"));
        }
        let mut expected_widths = Vec::with_capacity(new_columns);
        for (column, weight) in scaled.into_iter().enumerate() {
            if column == source {
                expected_widths.push(smaller);
                expected_widths.push(larger);
            } else {
                expected_widths.push(weight);
            }
        }
        if table.column_widths.as_ref() != Some(&expected_widths) {
            return Err(invalid("TABLE_WIDTH_STRUCTURE", "adding a custom-width column must split its adjacent weight deterministically"));
        }
        return Ok(());
    }
    Err(invalid("TABLE_WIDTH_STRUCTURE", "custom-width structure changes must add or remove one column at a time"))
}

fn set_canvas_table_structure(doc: &mut CanvasDocument, table_id: &str, expected: &CanvasTable, table: &CanvasTable) -> DltResult<Vec<String>> {
    let columns = validate_table_spec(table_id, table)?;
    let current = find_node_in_doc(doc, table_id).ok_or_else(|| not_found("table", table_id))?;
    if current.kind != NodeKind::Group || current.ext.table.is_none() {
        return Err(unsupported("TABLE_TARGET", format!("node {table_id} is not a Canvas table Group")));
    }
    if current.layout.as_ref().is_none_or(|layout| layout.mode != "grid") {
        return Err(invalid("TABLE_LAYOUT", "Canvas table Group must use a grid layout"));
    }
    if current.ext.table.as_ref() != Some(expected) {
        let message = format!("Canvas table {table_id} changed since the expected structure was read");
        let mut error = DltError::new(ErrorCode::RevisionConflict, message.clone());
        error.affected_ids.push(table_id.to_string());
        error.diagnostics.push(Diagnostic { code: "TABLE_STRUCTURE_STALE".into(), message, node_id: Some(table_id.to_string()), blocking: true, params: Default::default() });
        return Err(error);
    }
    validate_structure_width_transition(expected, table)?;
    ensure_unlocked(current)?;

    let desired_ids: Vec<String> = table.rows.iter().flat_map(|row| row.cell_ids.iter().cloned()).collect();
    let desired_set: BTreeSet<&str> = desired_ids.iter().map(String::as_str).collect();
    let current_ids: BTreeSet<&str> = current.children.iter().map(|child| child.id.as_str()).collect();
    if current_ids.len() != current.children.len() || current_ids != desired_set {
        return Err(invalid(
            "TABLE_STRUCTURE_CHILDREN",
            "add or remove table cell nodes in the same ChangeSet as the table structure update",
        ));
    }

    if expected.rows != table.rows || expected.header_row_id != table.header_row_id || expected.cell_spans != table.cell_spans {
        for child in &current.children {
            ensure_unlocked(child)?;
        }
    }
    if expected == table && current.children.iter().map(|child| child.id.as_str()).eq(desired_ids.iter().map(String::as_str)) {
        return Ok(Vec::new());
    }

    let positions: BTreeMap<String, usize> = current.children.iter().enumerate().map(|(index, child)| (child.id.clone(), index)).collect();
    let mut changed_ids = BTreeSet::from([table_id.to_string()]);
    for row in expected.rows.iter().chain(&table.rows) {
        changed_ids.insert(row.id.clone());
        changed_ids.extend(row.cell_ids.iter().cloned());
    }

    let current = find_node_in_doc_mut(doc, table_id).ok_or_else(|| not_found("table", table_id))?;
    let mut slots: Vec<Option<Node>> = std::mem::take(&mut current.children).into_iter().map(Some).collect();
    let mut reordered = Vec::with_capacity(desired_ids.len());
    for id in &desired_ids {
        let Some(index) = positions.get(id).copied() else {
            return Err(invalid("TABLE_STRUCTURE_CHILDREN", format!("table cell {id} is missing from the ChangeSet result")));
        };
        let Some(child) = slots[index].take() else {
            return Err(invalid("TABLE_STRUCTURE_CHILDREN", format!("table cell {id} is listed more than once")));
        };
        reordered.push(child);
    }
    current.children = reordered;
    current.ext.table = Some(table.clone());
    let layout = current.layout.as_mut().expect("table Group layout was checked before mutation");
    layout.grid_columns = Some(columns as u32);
    for breakpoint in &mut layout.breakpoints {
        if let Some(grid_columns) = &mut breakpoint.grid_columns {
            *grid_columns = columns as u32;
        }
    }
    Ok(changed_ids.into_iter().collect())
}

fn set_canvas_table_column_widths(
    doc: &mut CanvasDocument,
    table_id: &str,
    expected: &Option<Vec<u32>>,
    column_widths: &Option<Vec<u32>>,
) -> DltResult<Vec<String>> {
    let current = find_node_in_doc(doc, table_id).ok_or_else(|| not_found("table", table_id))?;
    if current.kind != NodeKind::Group {
        return Err(unsupported("TABLE_TARGET", format!("node {table_id} is not a Canvas table Group")));
    }
    if current.layout.as_ref().is_none_or(|layout| layout.mode != "grid") {
        return Err(invalid("TABLE_LAYOUT", "Canvas table Group must use a grid layout"));
    }
    let table = current.ext.table.as_ref().ok_or_else(|| unsupported("TABLE_TARGET", format!("node {table_id} is not a Canvas table Group")))?;
    let columns = table.rows.first().map_or(0, |row| row.cell_ids.len());
    validate_column_widths(table_id, columns, column_widths.as_deref())?;
    if table.column_widths.as_ref() != expected.as_ref() {
        let message = format!("Canvas table {table_id} column widths changed since they were read");
        let mut error = DltError::new(ErrorCode::RevisionConflict, message.clone());
        error.affected_ids.push(table_id.to_string());
        error.diagnostics.push(Diagnostic { code: "TABLE_WIDTHS_STALE".into(), message, node_id: Some(table_id.to_string()), blocking: true, params: Default::default() });
        return Err(error);
    }
    ensure_unlocked(current)?;
    if table.column_widths == *column_widths { return Ok(Vec::new()); }
    for child in &current.children { ensure_unlocked(child)?; }
    let mut changed_ids = Vec::with_capacity(current.children.len() + 1);
    changed_ids.push(table_id.to_string());
    changed_ids.extend(current.children.iter().map(|child| child.id.clone()));
    let current = find_node_in_doc_mut(doc, table_id).ok_or_else(|| not_found("table", table_id))?;
    current.ext.table.as_mut().expect("table was checked before mutation").column_widths = column_widths.clone();
    Ok(changed_ids)
}

fn not_found(kind: &str, id: &str) -> DltError {
    let mut error = DltError::new(ErrorCode::NotFound, format!("{kind} {id} does not exist"));
    error.affected_ids.push(id.to_string());
    error.diagnostics.push(Diagnostic {
        code: "TARGET_NOT_FOUND".into(),
        message: format!("{kind} {id} does not exist"),
        node_id: Some(id.to_string()),
        blocking: true,
        params: Default::default(),
    });
    error
}

fn first_blocking(diagnostics: Vec<Diagnostic>) -> DltError {
    let message = diagnostics.iter().find(|d| d.blocking).map(|d| d.message.clone()).unwrap_or_default();
    let mut error = DltError::new(ErrorCode::InvalidArgument, message);
    error.diagnostics = diagnostics;
    error
}

fn image_replacement_error(code: &str, message: impl Into<String>, node_id: &str, error_code: ErrorCode) -> DltError {
    let message = message.into();
    let mut error = DltError::new(error_code, message.clone());
    error.affected_ids.push(node_id.to_string());
    error.diagnostics.push(crate::common::blocking(code, message, Some(node_id.to_string())));
    error
}

fn image_crop_error(code: &str, message: impl Into<String>, node_id: &str, error_code: ErrorCode) -> DltError {
    let message = message.into();
    let mut error = DltError::new(error_code, message.clone());
    error.affected_ids.push(node_id.to_string());
    error.diagnostics.push(crate::common::blocking(code, message, Some(node_id.to_string())));
    error
}

pub(super) fn image_crop_source_dimensions(asset: &AssetRef, assets: &dyn AssetStore, node_id: &str) -> DltResult<(u32, u32)> {
    let canonical = assets.lookup(&asset.sha256).map_err(|_| image_crop_error(
        "IMAGE_CROP_ASSET_MISSING", "the canonical image asset is unavailable", node_id, ErrorCode::NotFound,
    ))?;
    if canonical.sha256 != asset.sha256 || canonical.mime != asset.mime || canonical.bytes != asset.bytes {
        return Err(image_crop_error(
            "IMAGE_CROP_ASSET_METADATA", "the image asset metadata does not match the canonical workspace asset", node_id, ErrorCode::IntegrityError,
        ));
    }
    let bytes = assets.read(&canonical).map_err(|_| image_crop_error(
        "IMAGE_CROP_ASSET_READ", "the canonical image asset could not be read", node_id, ErrorCode::IntegrityError,
    ))?;
    if bytes.len() as u64 != canonical.bytes || sha256(&bytes) != canonical.sha256 {
        return Err(image_crop_error(
            "IMAGE_CROP_ASSET_HASH", "canonical image bytes do not match their stored length and SHA-256", node_id, ErrorCode::IntegrityError,
        ));
    }
    let expected_kind = match canonical.mime.as_str() {
        "image/png" => designllm_render::ImageKind::Png,
        "image/jpeg" => designllm_render::ImageKind::Jpeg,
        "image/bmp" => designllm_render::ImageKind::Bmp,
        "image/gif" => return Err(image_crop_error(
            "IMAGE_CROP_FORMAT_UNSUPPORTED", "Canvas crop does not support GIF because HTML exports may animate it", node_id, ErrorCode::UnsupportedOperation,
        )),
        "image/webp" => designllm_render::ImageKind::Webp,
        mime => return Err(image_crop_error(
            "IMAGE_CROP_FORMAT_UNSUPPORTED", format!("Canvas crop does not support MIME type {mime}"), node_id, ErrorCode::UnsupportedOperation,
        )),
    };
    let mime_matches_bytes = match expected_kind {
        designllm_render::ImageKind::Png => bytes.starts_with(&PNG_SIGNATURE),
        designllm_render::ImageKind::Jpeg => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        designllm_render::ImageKind::Bmp => bytes.starts_with(b"BM") && bytes.len() > 26,
        designllm_render::ImageKind::Webp => bytes.starts_with(b"RIFF") && bytes.get(8..12).is_some_and(|signature| signature == b"WEBP"),
        designllm_render::ImageKind::Gif | designllm_render::ImageKind::Other => false,
    };
    if !mime_matches_bytes {
        return Err(image_crop_error(
            "IMAGE_CROP_MIME_MISMATCH", "canonical image MIME type does not match its source bytes", node_id, ErrorCode::InvalidArgument,
        ));
    }
    if expected_kind == designllm_render::ImageKind::Png && png_is_animated(&bytes).map_err(|error| image_crop_error(
        "IMAGE_CROP_FORMAT_UNSUPPORTED", format!("PNG animation state could not be verified: {}", error.message), node_id, ErrorCode::UnsupportedOperation,
    ))? {
        return Err(image_crop_error(
            "IMAGE_CROP_FORMAT_UNSUPPORTED", "Canvas crop does not support animated PNG because HTML output must preserve animation", node_id, ErrorCode::UnsupportedOperation,
        ));
    }
    let (width, height) = designllm_render::image_pixel_size(&bytes).ok_or_else(|| image_crop_error(
        "IMAGE_CROP_DIMENSIONS", "source image dimensions could not be read", node_id, ErrorCode::InvalidArgument,
    ))?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_REPLACEMENT_IMAGE_PIXELS {
        return Err(image_crop_error(
            "IMAGE_CROP_DIMENSIONS", format!("source image dimensions {width}×{height} are outside the supported range"), node_id, ErrorCode::InvalidArgument,
        ));
    }
    let (decoded_width, decoded_height, decoded_pixels) = designllm_render::decode_image(&bytes).map_err(|error| image_crop_error(
        "IMAGE_CROP_DECODE", format!("source image could not be decoded: {}", error.message), node_id, error.code,
    ))?;
    if (decoded_width, decoded_height) != (width, height) {
        return Err(image_crop_error(
            "IMAGE_CROP_DIMENSIONS", "decoded image dimensions do not match the inspected header", node_id, ErrorCode::IntegrityError,
        ));
    }
    if u64::from(decoded_width) * u64::from(decoded_height) > MAX_REPLACEMENT_IMAGE_PIXELS {
        return Err(image_crop_error(
            "IMAGE_CROP_DIMENSIONS", "decoded image exceeds the 64 megapixel crop limit", node_id, ErrorCode::InvalidArgument,
        ));
    }
    drop(decoded_pixels);
    if expected_kind == designllm_render::ImageKind::Jpeg {
        match designllm_render::jpeg_exif_orientation(&bytes) {
            Ok(None) => {}
            Ok(Some(orientation)) => return Err(image_crop_error(
                "IMAGE_CROP_ORIENTATION_UNSUPPORTED", format!("JPEG EXIF orientation {orientation} is incompatible with source-pixel crop"), node_id, ErrorCode::UnsupportedOperation,
            )),
            Err(error) => return Err(image_crop_error(
                "IMAGE_CROP_ORIENTATION_UNSUPPORTED", format!("JPEG EXIF orientation cannot be verified: {}", error.message), node_id, ErrorCode::UnsupportedOperation,
            )),
        }
    }
    Ok((width, height))
}

fn png_is_animated(bytes: &[u8]) -> DltResult<bool> {
    if !bytes.starts_with(&PNG_SIGNATURE) { return Err(DltError::invalid("PNG signature is invalid")); }
    let mut offset = PNG_SIGNATURE.len();
    loop {
        let header_end = offset.checked_add(8).filter(|end| *end <= bytes.len())
            .ok_or_else(|| DltError::invalid("PNG chunk header is truncated"))?;
        let length = usize::try_from(u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("four-byte PNG chunk length")))
            .map_err(|_| DltError::invalid("PNG chunk length is out of range"))?;
        let chunk_type = &bytes[offset + 4..header_end];
        if chunk_type == b"acTL" { return Ok(true); }
        if chunk_type == b"IDAT" || chunk_type == b"IEND" { return Ok(false); }
        offset = header_end.checked_add(length).and_then(|end| end.checked_add(4))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| DltError::invalid("PNG chunk data is truncated"))?;
    }
}

fn preflight_image_replacements(doc: &CanvasDocument, links: &[FigureLink], operations: &[Operation]) -> DltResult<()> {
    for operation in operations {
        let Operation::ReplaceImageAsset { node_id, expected_asset_sha256, .. } = operation else { continue };
        components::guard_image_replacement_target(doc, node_id)?;
        if links.iter().any(|link| link.target_node_id.as_str() == node_id.as_str()) {
            return Err(image_replacement_error(
                "IMAGE_REPLACEMENT_FIGURE_LINKED",
                "an image linked to a Figure cannot be replaced until linked-image updates are supported",
                node_id,
                ErrorCode::UnsupportedOperation,
            ));
        }
        let node = find_node_in_doc(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
        if node.kind != NodeKind::Image {
            return Err(image_replacement_error(
                "IMAGE_REPLACEMENT_TARGET",
                "only Canvas Image nodes can have their assets replaced",
                node_id,
                ErrorCode::InvalidArgument,
            ));
        }
        let current_asset = node.asset.as_ref().ok_or_else(|| image_replacement_error(
            "IMAGE_REPLACEMENT_CURRENT_ASSET",
            "the image has no current asset to compare",
            node_id,
            ErrorCode::InvalidArgument,
        ))?;
        if current_asset.sha256.as_str() != expected_asset_sha256.as_str() {
            return Err(image_replacement_error(
                "IMAGE_REPLACEMENT_STALE",
                "the image asset changed since it was read",
                node_id,
                ErrorCode::RevisionConflict,
            ));
        }
    }
    Ok(())
}

fn ensure_image_replacement_ancestors_unlocked(doc: &CanvasDocument, node_id: &str) -> DltResult<()> {
    fn check(nodes: &[Node], target: &str) -> DltResult<bool> {
        for node in nodes {
            if node.id == target { return Ok(true); }
            if check(&node.children, target)? {
                ensure_unlocked(node)?;
                return Ok(true);
            }
        }
        Ok(false)
    }
    for page in &doc.pages {
        if check(&page.nodes, node_id)? { return Ok(()); }
    }
    Err(not_found("node", node_id))
}

fn replace_image_asset(
    doc: &mut CanvasDocument,
    links: &[FigureLink],
    assets: &dyn AssetStore,
    node_id: &str,
    expected_asset_sha256: &str,
    asset_sha256: &str,
) -> DltResult<Vec<String>> {
    let node = find_node_in_doc(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    if node.kind != NodeKind::Image {
        return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_TARGET",
            "only Canvas Image nodes can have their assets replaced",
            node_id,
            ErrorCode::InvalidArgument,
        ));
    }
    let current_asset = node.asset.as_ref().ok_or_else(|| image_replacement_error(
        "IMAGE_REPLACEMENT_CURRENT_ASSET",
        "the image has no current asset to compare",
        node_id,
        ErrorCode::InvalidArgument,
    ))?;
    if current_asset.sha256.as_str() != expected_asset_sha256 {
        return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_STALE",
            "the image asset changed since it was read",
            node_id,
            ErrorCode::RevisionConflict,
        ));
    }
    ensure_image_replacement_ancestors_unlocked(doc, node_id)?;
    ensure_node_unlocked_in_doc(doc, node_id)?;
    if links.iter().any(|link| link.target_node_id.as_str() == node_id) {
        return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_FIGURE_LINKED",
            "an image linked to a Figure cannot be replaced until linked-image updates are supported",
            node_id,
            ErrorCode::UnsupportedOperation,
        ));
    }

    let replacement = assets.lookup(asset_sha256)?;
    if replacement.sha256.as_str() != asset_sha256 {
        return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_ASSET_HASH",
            "the canonical asset lookup returned a different SHA-256",
            node_id,
            ErrorCode::IntegrityError,
        ));
    }
    let expected_kind = match replacement.mime.as_str() {
        "image/png" => designllm_render::ImageKind::Png,
        "image/jpeg" => designllm_render::ImageKind::Jpeg,
        "image/bmp" => designllm_render::ImageKind::Bmp,
        mime => return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_FORMAT_UNSUPPORTED",
            format!("image replacement does not support MIME type {mime}"),
            node_id,
            ErrorCode::UnsupportedOperation,
        )),
    };
    let bytes = assets.read(&replacement)?;
    if bytes.len() as u64 != replacement.bytes || sha256(&bytes) != asset_sha256 {
        return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_ASSET_HASH",
            "canonical image bytes do not match their stored length and SHA-256",
            node_id,
            ErrorCode::IntegrityError,
        ));
    }
    let actual_kind = if bytes.starts_with(&PNG_SIGNATURE) {
        designllm_render::ImageKind::Png
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        designllm_render::ImageKind::Jpeg
    } else if bytes.starts_with(b"BM") {
        designllm_render::ImageKind::Bmp
    } else {
        designllm_render::ImageKind::Other
    };
    if actual_kind != expected_kind {
        return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_MIME_MISMATCH",
            "canonical MIME type does not match the image bytes",
            node_id,
            ErrorCode::InvalidArgument,
        ));
    }
    let Some((width, height)) = designllm_render::image_pixel_size(&bytes) else {
        return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_DIMENSIONS",
            "image dimensions could not be read",
            node_id,
            ErrorCode::InvalidArgument,
        ));
    };
    let pixel_count = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || pixel_count > MAX_REPLACEMENT_IMAGE_PIXELS {
        return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_PIXEL_LIMIT",
            format!("image dimensions {width}×{height} exceed the 64 megapixel limit"),
            node_id,
            ErrorCode::InvalidArgument,
        ));
    }
    let (decoded_width, decoded_height, decoded_pixels) = designllm_render::decode_image(&bytes).map_err(|error| {
        image_replacement_error(
            "IMAGE_REPLACEMENT_DECODE",
            format!("replacement image could not be decoded: {}", error.message),
            node_id,
            error.code,
        )
    })?;
    if (decoded_width, decoded_height) != (width, height) {
        return Err(image_replacement_error(
            "IMAGE_REPLACEMENT_DIMENSIONS",
            "decoded image dimensions do not match the inspected header",
            node_id,
            ErrorCode::IntegrityError,
        ));
    }
    drop(decoded_pixels);

    if let Some(crop) = node.ext.image_crop_pixel_rect {
        if !pixel_rect_within(crop, width, height) {
            return Err(image_replacement_error(
                "IMAGE_REPLACEMENT_CROP_BOUNDS",
                "replacement dimensions do not contain the image's saved crop; reset or adjust the crop first",
                node_id,
                ErrorCode::InvalidArgument,
            ));
        }
        if expected_kind == designllm_render::ImageKind::Jpeg {
            match designllm_render::jpeg_exif_orientation(&bytes) {
                Ok(None) => {}
                Ok(Some(orientation)) => return Err(image_replacement_error(
                    "IMAGE_CROP_ORIENTATION_UNSUPPORTED",
                    format!("JPEG EXIF orientation {orientation} is incompatible with the saved source-pixel crop"),
                    node_id,
                    ErrorCode::UnsupportedOperation,
                )),
                Err(error) => return Err(image_replacement_error(
                    "IMAGE_CROP_ORIENTATION_UNSUPPORTED",
                    format!("JPEG EXIF orientation cannot be verified: {}", error.message),
                    node_id,
                    ErrorCode::UnsupportedOperation,
                )),
            }
        }
    }

    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    node.asset = Some(replacement);
    Ok(vec![node_id.to_string()])
}

pub(super) fn pixel_rect_within(rect: PixelRect, width: u32, height: u32) -> bool {
    rect.width > 0 && rect.height > 0
        && rect.x.checked_add(rect.width).is_some_and(|right| right <= width)
        && rect.y.checked_add(rect.height).is_some_and(|bottom| bottom <= height)
}

fn set_canvas_image_crop(
    doc: &mut CanvasDocument,
    links: &[FigureLink],
    assets: &dyn AssetStore,
    node_id: &str,
    expected_asset_sha256: &str,
    crop_pixel_rect: Option<PixelRect>,
) -> DltResult<Vec<String>> {
    components::guard_image_crop_target(doc, node_id)?;
    let node = find_node_in_doc(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    if node.kind != NodeKind::Image {
        return Err(image_crop_error("IMAGE_CROP_TARGET", "only Canvas Image nodes can be cropped", node_id, ErrorCode::InvalidArgument));
    }
    let asset = node.asset.as_ref().ok_or_else(|| image_crop_error(
        "IMAGE_CROP_ASSET_MISSING", "the image has no current asset", node_id, ErrorCode::InvalidArgument,
    ))?;
    if asset.sha256 != expected_asset_sha256 {
        return Err(image_crop_error("IMAGE_CROP_STALE", "the image asset changed since the crop editor was opened", node_id, ErrorCode::RevisionConflict));
    }
    ensure_image_replacement_ancestors_unlocked(doc, node_id)?;
    ensure_node_unlocked_in_doc(doc, node_id)?;
    if links.iter().any(|link| link.target_node_id == node_id) {
        return Err(image_crop_error(
            "IMAGE_CROP_FIGURE_LINKED", "a Figure-linked image cannot be cropped yet", node_id, ErrorCode::UnsupportedOperation,
        ));
    }
    if let Some(crop) = crop_pixel_rect {
        let (width, height) = image_crop_source_dimensions(asset, assets, node_id)?;
        if !pixel_rect_within(crop, width, height) {
            return Err(image_crop_error(
                "IMAGE_CROP_BOUNDS", format!("crop rectangle is outside the {width}×{height} source image"), node_id, ErrorCode::InvalidArgument,
            ));
        }
    }
    if node.ext.image_crop_pixel_rect == crop_pixel_rect {
        return Ok(Vec::new());
    }
    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    node.ext.image_crop_pixel_rect = crop_pixel_rect;
    Ok(vec![node_id.to_string()])
}

/// Replaces a Canvas Path node's path. The node must be a path, unlocked and outside component instances, and must still
/// hold `expected`. The new path passes the same checks as an inserted one. The bounds stay as they are.
fn set_node_path(doc: &mut CanvasDocument, node_id: &str, expected: &PathSpec, path: &PathSpec) -> DltResult<Vec<String>> {
    let node = find_node_in_doc(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    if node.kind != NodeKind::Path {
        return Err(invalid("NODE_PATH_TARGET", format!("node {node_id} is not a Canvas Path node")));
    }
    ensure_node_unlocked_in_doc(doc, node_id)?;
    if inside_component_instance(doc, node_id) {
        return Err(unsupported(
            repeat::INSTANCE_CHILD_READ_ONLY, format!("component child {node_id} is read-only until an explicit override is applied"),
        ));
    }
    if node.ext.path.as_ref() != Some(expected) {
        return Err(fail(ErrorCode::RevisionConflict, "NODE_PATH_STALE", format!("the path of {node_id} changed since it was read")));
    }
    validate_path(path).map_err(|problem| invalid("NODE_PATH", problem))?;
    if node.ext.path.as_ref() == Some(path) {
        return Ok(Vec::new());
    }
    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    node.ext.path = Some(path.clone());
    Ok(vec![node_id.to_string()])
}

/// Whether a node is a materialized child of a component instance. Such children change only through their source.
fn inside_component_instance(doc: &CanvasDocument, node_id: &str) -> bool {
    fn within(nodes: &[Node], node_id: &str, inside: bool) -> Option<bool> {
        nodes.iter().find_map(|node| {
            if node.id == node_id {
                return Some(inside);
            }
            within(&node.children, node_id, inside || node.ext.instance.is_some())
        })
    }
    doc.pages.iter().find_map(|page| within(&page.nodes, node_id, false)).unwrap_or(false)
}

/// Token updates write through local bindings. Other node edits name their targets directly.
pub(super) fn writes_matching_node(doc: &CanvasDocument, operation: &Operation, matches: impl Fn(&str) -> bool) -> bool {
    let token_id = match operation {
        Operation::PutToken { token_id, .. } => Some(token_id),
        Operation::SetTokenMode { .. } => None,
        _ => return node_targets(operation).into_iter().any(matches),
    };
    doc.bindings.iter().any(|binding| binding.token.source == designllm_contracts::TokenSource::Local
        && token_id.is_none_or(|id| id == &binding.token.token_id) && matches(&binding.node_id))
}

/// The node ids an operation edits directly.
pub(super) fn node_targets(operation: &Operation) -> Vec<&str> {
    use Operation::*;
    match operation {
        SetText { node_id, .. } | SetGeometry { node_id, .. } | RemoveNode { node_id } | SetLayout { node_id, .. }
        | SetInteraction { node_id, .. } | SetInteractions { node_id, .. } | SetVariantStyle { node_id, .. } | SetZOrder { node_id, .. }
        | SetNodeList { node_id, .. } | SetNodePath { node_id, .. } | MoveNode { node_id, .. } | DetachInstance { instance_id: node_id }
        | ReplaceImageAsset { node_id, .. } | SetCanvasImageCrop { node_id, .. }
        | SetGroupRepeat { node_id, .. } | SetGroupCondition { node_id, .. } => vec![node_id.as_str()],
        UpdateFigureLink { link } => vec![link.target_node_id.as_str()],
        ReplaceText { range, .. } => vec![range.paragraph_id.as_str()],
        SetStyle { node_ids, .. } | SetNodeAppearance { node_ids, .. } | SetNodeInfo { node_ids, .. }
        | BindToken { node_ids, .. } | UnbindToken { node_ids, .. } => node_ids.iter().map(String::as_str).collect(),
        SetCanvasTableStructure { table_id, .. } | SetCanvasTableColumnWidths { table_id, .. } => vec![table_id.as_str()],
        // Structural and indirect writes have their own source checks.
        AddNode { .. } | InsertPage { .. } | RemovePage { .. } | SetPageRole { .. } | InstantiateComponent { .. }
        | PutToken { .. } | SetTokenMode { .. } | RemoveToken { .. } => Vec::new(),
        // Page/document metadata or operations belonging to another engine.
        ReorderPages { .. } | SetPageSize { .. } | RenamePage { .. } | SetPageNotes { .. } | SetNamedStyle { .. }
        | SetCanvasBoardMemos { .. } | SetCanvasPageGuides { .. } | SetCanvasPageSections { .. }
        | SetTextStyleRange { .. } | SetParagraphFormat { .. } | SetPageSetup { .. } | SetTableDimensions { .. } | InsertFlowBlock { .. }
        | AddDiagramNode { .. } | ConnectDiagram { .. } | SetDiagramProvenance { .. } | AddPanel { .. } | RemovePanel { .. }
        | ReorderPanels { .. } | ArrangePanels { .. } | SetPanelLabel { .. } | SetCaption { .. } | PutEvidence { .. } | PutDataset { .. }
        | SetPlot { .. } | SetPlotDisplay { .. } | SetAxisLabel { .. } | SetAxisTicks { .. } | SetCommonLegend { .. }
        | SetFigureImageTransform { .. } | SetFigureImageCalibration { .. } | SetFigureVisualReview { .. }
        | ReplaceWebSource { .. } | MappedWebEdit { .. } | SetTheme { .. } => Vec::new(),
    }
}

fn apply_one(doc: &mut CanvasDocument, links: &mut Vec<FigureLink>, assets: &dyn AssetStore, op: &Operation) -> DltResult<Vec<String>> {
    match op {
        Operation::ReplaceText { range, text } => replace_text(doc, &range.paragraph_id, range.start, range.end, text),
        Operation::SetText { node_id, text } => set_text(doc, node_id, text),
        Operation::SetStyle { node_ids, patch } => {
            let mut changed = set_style(doc, node_ids, patch)?;
            for id in tokens::unbind_style_fields(doc, node_ids, patch) {
                if !changed.contains(&id) { changed.push(id); }
            }
            Ok(changed)
        }
        Operation::SetTextStyleRange { .. } => Err(unsupported(
            "TEXT_RANGE_STYLE",
            "per-range text styles are not supported; apply the style to the node",
        )),
        Operation::SetNamedStyle { name, patch, clear_overrides } => set_named_style(doc, name, patch, *clear_overrides),
        Operation::SetGeometry { node_id, bounds } => set_geometry(doc, node_id, bounds),
        Operation::AddNode { page_id, parent_id, node } => {
            if let Some((instance, bindings)) = components::canonicalize_instance_add(doc, page_id, node)? {
                let changed = add_node(doc, page_id, parent_id.as_deref(), &instance)?;
                doc.bindings.extend(bindings);
                Ok(changed)
            } else {
                add_node(doc, page_id, parent_id.as_deref(), node)
            }
        }
        Operation::RemoveNode { node_id } => remove_node(doc, links, node_id),
        Operation::SetLayout { node_id, layout } => {
            let mut changed = set_layout(doc, node_id, layout)?;
            for id in tokens::unbind_layout_fields(doc, node_id) {
                if !changed.contains(&id) { changed.push(id); }
            }
            Ok(changed)
        }
        Operation::SetInteraction { node_id, interaction } => set_interaction(doc, node_id, interaction),
        Operation::SetInteractions { node_id, interactions } => set_interactions(doc, node_id, interactions),
        Operation::SetVariantStyle { node_id, variant, patch } => set_variant_style(doc, node_id, variant, patch),
        Operation::InsertPage { index, page } => insert_page(doc, *index, page),
        Operation::RemovePage { page_id } => remove_page(doc, links, page_id),
        Operation::UpdateFigureLink { link } => figure::update(doc, links, assets, link),
        Operation::ReorderPages { page_ids } => reorder_pages(doc, page_ids),
        Operation::SetPageNotes { page_id, notes } => set_page_notes(doc, page_id, notes),
        Operation::SetCanvasBoardMemos { page_id, expected, memos } => set_canvas_board_memos(doc, page_id, expected, memos),
        Operation::SetCanvasPageGuides { page_id, expected, guides } => set_canvas_page_guides(doc, page_id, expected.as_ref(), guides.as_ref()),
        Operation::SetCanvasPageSections { expected, sections } => set_canvas_page_sections(doc, expected, sections),
        Operation::SetPageSize { page_id, size } => set_page_size(doc, page_id, *size),
        Operation::MoveNode { node_id, page_id, parent_id, index } => move_node(doc, node_id, page_id, parent_id.as_deref(), *index),
        Operation::SetZOrder { node_id, index } => set_z_order(doc, node_id, *index),
        Operation::SetNodeAppearance { node_ids, image_fit, locked } => set_node_appearance(doc, node_ids, *image_fit, *locked),
        Operation::SetNodeInfo { node_ids, alt, name, hidden } => set_node_info(doc, node_ids, alt.clone(), name.clone(), *hidden),
        Operation::ReplaceImageAsset { node_id, expected_asset_sha256, asset_sha256 } => {
            replace_image_asset(doc, links, assets, node_id, expected_asset_sha256, asset_sha256)
        }
        Operation::SetCanvasImageCrop { node_id, expected_asset_sha256, crop_pixel_rect } => {
            set_canvas_image_crop(doc, links, assets, node_id, expected_asset_sha256, *crop_pixel_rect)
        }
        Operation::SetNodePath { node_id, expected, path } => set_node_path(doc, node_id, expected, path),
        Operation::SetNodeList { node_id, list } => set_node_list(doc, node_id, *list),
        Operation::SetCanvasTableColumnWidths { table_id, expected, column_widths } => set_canvas_table_column_widths(doc, table_id, expected, column_widths),
        Operation::RenamePage { page_id, name } => rename_page(doc, page_id, name.clone()),
        Operation::SetPageRole { page_id, role } => {
            if *role == Some(designllm_contracts::PageRole::Component)
                && doc.page_sections.iter().any(|section| section.start_page_id == *page_id)
            {
                return Err(invalid("PAGE_SECTION_COMPONENT_START", "move or remove this page section marker before making its start page a component"));
            }
            components::set_page_role(doc, page_id, *role)
        }
        Operation::InstantiateComponent { page_id, parent_id, instance_id, component_page_id, bounds } => {
            components::instantiate(doc, page_id, parent_id.as_deref(), instance_id, component_page_id, *bounds)
        }
        Operation::DetachInstance { instance_id } => components::detach(doc, instance_id),
        Operation::PutToken { .. } | Operation::RemoveToken { .. } | Operation::SetTokenMode { .. }
        | Operation::BindToken { .. } | Operation::UnbindToken { .. } => tokens::apply(doc, op),
        Operation::SetGroupRepeat { node_id, expected, repeat } => repeat::set_group_repeat(doc, node_id, expected.as_ref(), repeat.as_ref()),
        Operation::SetGroupCondition { node_id, expected, condition } => repeat::set_group_condition(doc, node_id, expected.as_ref(), condition.as_ref()),
        other => Err(unsupported(
            "OPERATION_UNSUPPORTED",
            format!("operation {} is not supported by the canvas engine", other.name()),
        )),
    }
}

fn replace_text(doc: &mut CanvasDocument, paragraph_id: &str, start: u32, end: u32, text: &str) -> DltResult<Vec<String>> {
    ensure_node_unlocked_in_doc(doc, paragraph_id)?;
    let node = find_node_in_doc_mut(doc, paragraph_id).ok_or_else(|| not_found("node", paragraph_id))?;
    if !matches!(node.kind, NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse) {
        return Err(unsupported("NODE_TEXT_UNSUPPORTED", format!("node {paragraph_id} cannot hold text")));
    }
    let graphemes: Vec<&str> = node.text.graphemes(true).collect();
    let (s, e) = (start as usize, end as usize);
    if s > e || e > graphemes.len() {
        return Err(invalid("TEXT_RANGE", "text range is outside the node text"));
    }
    let new_text = format!("{}{}{}", graphemes[..s].concat(), text, graphemes[e..].concat());
    if new_text.len() > MAX_TEXT_BYTES {
        return Err(invalid("TEXT_TOO_LONG", "node text exceeds the size limit"));
    }
    node.text = new_text;
    Ok(vec![paragraph_id.to_string()])
}

fn set_text(doc: &mut CanvasDocument, node_id: &str, text: &str) -> DltResult<Vec<String>> {
    ensure_node_unlocked_in_doc(doc, node_id)?;
    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    if !matches!(node.kind, NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse) {
        return Err(unsupported("NODE_TEXT_UNSUPPORTED", format!("node {node_id} cannot hold text")));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(invalid("TEXT_TOO_LONG", "node text exceeds the size limit"));
    }
    node.text = text.to_string();
    Ok(vec![node_id.to_string()])
}

fn set_style(doc: &mut CanvasDocument, node_ids: &[String], patch: &StylePatch) -> DltResult<Vec<String>> {
    if node_ids.is_empty() {
        return Err(invalid("STYLE_TARGETS", "no nodes were selected"));
    }
    // Refuse fields the target kind cannot draw before changing the working copy.
    guard::check_patch(patch)?;
    validate_patch(patch)?;
    let mut changed = Vec::new();
    let mut seen = BTreeSet::new();
    for id in node_ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        ensure_node_unlocked_in_doc(doc, id)?;
        let node = find_node_in_doc_mut(doc, id).ok_or_else(|| not_found("node", id))?;
        guard::check_patch_kind(patch, node.kind, id)?;
        let mut style = node.style.clone();
        patch.apply(&mut style);
        validate_style(&style)?;
        guard::check_style_kind(&style, node.kind, id)?;
        node.style = style;
        changed.push(id.clone());
    }
    Ok(changed)
}

fn set_named_style(doc: &mut CanvasDocument, name: &str, patch: &StylePatch, clear_overrides: bool) -> DltResult<Vec<String>> {
    if clear_overrides {
        return Err(unsupported(
            "NAMED_STYLE_CLEAR",
            "canvas nodes store inline styles, so clearing overrides by named style does not apply",
        ));
    }
    if !valid_key(name) {
        return Err(invalid("NAMED_STYLE_NAME", "named style name is invalid"));
    }
    guard::check_patch(patch)?;
    let mut style = doc.styles.get(name).cloned().unwrap_or_default();
    patch.apply(&mut style);
    validate_style(&style)?;
    doc.styles.insert(name.to_string(), style);
    Ok(vec![name.to_string()])
}

fn set_geometry(doc: &mut CanvasDocument, node_id: &str, bounds: &Rect) -> DltResult<Vec<String>> {
    if table_owner_in_doc(doc, node_id).is_some() {
        return Err(unsupported("TABLE_CELL_GEOMETRY", "table cell geometry follows its equal grid track; edit the table structure instead"));
    }
    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    ensure_unlocked(node)?;
    validate_bounds(bounds, node.kind)?;
    node.bounds = *bounds;
    Ok(vec![node_id.to_string()])
}

fn depth_of(nodes: &[Node], id: &str, depth: usize) -> Option<usize> {
    for node in nodes {
        if node.id == id {
            return Some(depth);
        }
        if let Some(found) = depth_of(&node.children, id, depth + 1) {
            return Some(found);
        }
    }
    None
}

fn count_subtree(node: &Node) -> usize {
    1 + node.ext.table.as_ref().map_or(0, |table| table.rows.len()) + node.children.iter().map(count_subtree).sum::<usize>()
}

fn add_node(doc: &mut CanvasDocument, page_id: &str, parent_id: Option<&str>, node: &Node) -> DltResult<Vec<String>> {
    let page_index = doc.pages.iter().position(|p| p.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    let depth = match parent_id {
        Some(pid) => {
            let found = depth_of(&doc.pages[page_index].nodes, pid, 0).ok_or_else(|| not_found("node", pid))?;
            found + 1
        }
        None => 0,
    };
    let mut diagnostics = Vec::new();
    validate_node(node, depth, &mut diagnostics);
    if any_blocking(&diagnostics) {
        return Err(first_blocking(diagnostics));
    }

    let existing: BTreeSet<String> = super::model::all_ids(doc).into_iter().collect();
    let mut new_ids = BTreeSet::new();
    subtree_ids(node, &mut new_ids);
    if node.id.contains("::") || (node.ext.instance.is_none() && new_ids.iter().any(|id| id.contains("::"))) {
        return Err(invalid("NODE_ID_RESERVED", "the '::' identifier separator is reserved for component instance children"));
    }
    if new_ids.len() != count_subtree(node) {
        return Err(duplicate_id(&node.id));
    }
    if let Some(id) = new_ids.iter().find(|id| existing.contains(*id)) {
        return Err(duplicate_id(id));
    }
    if existing.len() + new_ids.len() > MAX_NODES + MAX_PAGES {
        return Err(invalid("CANVAS_TOO_MANY_NODES", "canvas would have too many nodes"));
    }

    match parent_id {
        None => doc.pages[page_index].nodes.push(node.clone()),
        Some(pid) => {
            let parent = find_node_mut_in(&mut doc.pages[page_index].nodes, pid).ok_or_else(|| not_found("node", pid))?;
            if !matches!(parent.kind, NodeKind::Group | NodeKind::Rectangle) {
                return Err(invalid("NODE_CHILDREN", "only group and rectangle nodes can contain other nodes"));
            }
            ensure_unlocked(parent)?;
            parent.children.push(node.clone());
        }
    }
    // Interaction targets are validated after the complete operation batch in apply().
    Ok(vec![node.id.clone()])
}

fn duplicate_id(id: &str) -> DltError {
    let mut error = DltError::new(ErrorCode::AlreadyExists, format!("identifier {id} already exists"));
    error.affected_ids.push(id.to_string());
    error.diagnostics.push(Diagnostic {
        code: "DUPLICATE_ID".into(),
        message: format!("identifier {id} already exists"),
        node_id: Some(id.to_string()),
        blocking: true,
        params: Default::default(),
    });
    error
}

fn find_node_mut_in<'a>(nodes: &'a mut [Node], id: &str) -> Option<&'a mut Node> {
    super::model::find_node_mut(nodes, id)
}

fn remove_from(nodes: &mut Vec<Node>, id: &str) -> bool {
    if let Some(pos) = nodes.iter().position(|n| n.id == id) {
        nodes.remove(pos);
        return true;
    }
    for node in nodes.iter_mut() {
        if remove_from(&mut node.children, id) {
            return true;
        }
    }
    false
}

/// Nodes that an interaction points at (NavigatePage points at pages, not nodes).
fn interaction_node_target(interaction: &Interaction) -> Option<&str> {
    match interaction {
        // OpenUrl and Back (contract v1.2) point at no node.
        Interaction::NavigatePage { .. } | Interaction::OpenUrl { .. } | Interaction::Back => None,
        Interaction::ToggleVisibility { node_id } | Interaction::SetVariant { node_id, .. } => Some(node_id.as_str()),
    }
}

fn remove_node(doc: &mut CanvasDocument, links: &mut Vec<FigureLink>, node_id: &str) -> DltResult<Vec<String>> {
    ensure_node_unlocked_in_doc(doc, node_id)?;
    let mut removed = BTreeSet::new();
    let mut found = false;
    for page in &doc.pages {
        if let Some(node) = find_node(&page.nodes, node_id) {
            for inner in flatten(std::slice::from_ref(node)) {
                ensure_unlocked(inner)?;
            }
            subtree_ids(node, &mut removed);
            found = true;
            break;
        }
    }
    if !found {
        return Err(not_found("node", node_id));
    }
    let mut referencing = Vec::new();
    for page in &doc.pages {
        for node in flatten(&page.nodes) {
            if removed.contains(&node.id) {
                continue;
            }
            let refers = node.interactions.iter().any(|i| interaction_node_target(i).is_some_and(|t| removed.contains(t)));
            if refers {
                referencing.push(node.id.clone());
            }
        }
    }
    if !referencing.is_empty() {
        let mut error = invalid(
            "INTERACTION_REFERENCE",
            format!("node {node_id} is the target of an interaction; remove or retarget that interaction first"),
        );
        error.affected_ids = referencing;
        return Err(error);
    }
    for page in doc.pages.iter_mut() {
        if remove_from(&mut page.nodes, node_id) {
            break;
        }
    }
    doc.bindings.retain(|binding| !removed.contains(&binding.node_id));
    figure::prune(links, &removed);
    Ok(removed.into_iter().collect())
}

fn set_layout(doc: &mut CanvasDocument, node_id: &str, layout: &LayoutSpec) -> DltResult<Vec<String>> {
    if table_owner_in_doc(doc, node_id).is_some() {
        return Err(unsupported("TABLE_CELL_LAYOUT", "table cell layout follows its row and column tracks"));
    }
    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    ensure_unlocked(node)?;
    validate_layout(layout, node.kind)?;
    node.layout = Some(layout.clone());
    Ok(vec![node_id.to_string()])
}

fn set_interaction(doc: &mut CanvasDocument, node_id: &str, interaction: &Interaction) -> DltResult<Vec<String>> {
    ensure_node_unlocked_in_doc(doc, node_id)?;
    validate_interaction(doc, interaction)?;
    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    ensure_unlocked(node)?;
    // One click action per node. The list is replaced, so the node always has exactly this action.
    node.interactions = vec![interaction.clone()];
    Ok(vec![node_id.to_string()])
}

fn set_interactions(doc: &mut CanvasDocument, node_id: &str, interactions: &[Interaction]) -> DltResult<Vec<String>> {
    ensure_node_unlocked_in_doc(doc, node_id)?;
    if interactions.iter().filter(|interaction| matches!(interaction, Interaction::OpenUrl { .. })).count() > 1 {
        let mut error = first_blocking(vec![Diagnostic {
            code: "INTERACTION_OPEN_URL_MULTIPLE".into(),
            message: "a node may have at most one OpenUrl interaction".into(),
            node_id: Some(node_id.to_string()),
            blocking: true,
            params: Default::default(),
        }]);
        error.affected_ids.push(node_id.to_string());
        return Err(error);
    }
    for interaction in interactions {
        validate_interaction(doc, interaction)?;
    }
    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    node.interactions = interactions.to_vec();
    Ok(vec![node_id.to_string()])
}

/// Merges two patches applied in order (`base`, then `over`) into one that does the same: a value `over` sets replaces base's,
/// and a field `over` clears loses base's value and stays cleared. A clear from `base` survives unless `over` sets the field.
pub(crate) fn merge_patch(base: &StylePatch, over: &StylePatch) -> DltResult<StylePatch> {
    guard::check_patch(base)?;
    guard::check_patch(over)?;
    let mut merged = StylePatch {
        font_family: over.font_family.clone().or_else(|| base.font_family.clone()),
        font_size: over.font_size.or(base.font_size),
        color: over.color.clone().or_else(|| base.color.clone()),
        fill: over.fill.clone().or_else(|| base.fill.clone()),
        stroke: over.stroke.clone().or_else(|| base.stroke.clone()),
        stroke_width: over.stroke_width.or(base.stroke_width),
        bold: over.bold.or(base.bold),
        italic: over.italic.or(base.italic),
        align: over.align.clone().or_else(|| base.align.clone()),
        corner_radius: over.corner_radius.or(base.corner_radius),
        opacity_milli: over.opacity_milli.or(base.opacity_milli),
        shadows: over.shadows.clone().or_else(|| base.shadows.clone()),
        gradient: over.gradient.clone().or_else(|| base.gradient.clone()),
        line_height_milli: over.line_height_milli.or(base.line_height_milli),
        letter_spacing: over.letter_spacing.or(base.letter_spacing),
        vertical_align: over.vertical_align.or(base.vertical_align),
        font_weight: over.font_weight.or(base.font_weight),
        stroke_dash: over.stroke_dash.or(base.stroke_dash),
        stroke_cap: over.stroke_cap.or(base.stroke_cap),
        stroke_join: over.stroke_join.or(base.stroke_join),
        underline: over.underline.or(base.underline),
        strikethrough: over.strikethrough.or(base.strikethrough),
        stroke_start: over.stroke_start.or(base.stroke_start),
        stroke_end: over.stroke_end.or(base.stroke_end),
        ..StylePatch::default()
    };
    for field in &over.clear {
        drop_value(&mut merged, *field);
    }
    let over_sets = over.set_fields();
    merged.clear = over.clear.iter().copied().chain(base.clear.iter().filter(|field| !over_sets.contains(*field)).copied()).collect();
    Ok(merged)
}

/// Drops the value a patch holds for one field, so a clear that follows leaves the field at its default.
fn drop_value(patch: &mut StylePatch, field: designllm_contracts::StyleField) {
    use designllm_contracts::StyleField;
    match field {
        StyleField::FontFamily => patch.font_family = None,
        StyleField::FontSize => patch.font_size = None,
        StyleField::Color => patch.color = None,
        StyleField::Fill => patch.fill = None,
        StyleField::Stroke => patch.stroke = None,
        StyleField::StrokeWidth => patch.stroke_width = None,
        StyleField::Bold => patch.bold = None,
        StyleField::Italic => patch.italic = None,
        StyleField::Align => patch.align = None,
        StyleField::CornerRadius => patch.corner_radius = None,
        StyleField::Opacity => patch.opacity_milli = None,
        StyleField::Shadows => patch.shadows = None,
        StyleField::Gradient => patch.gradient = None,
        StyleField::LineHeight => patch.line_height_milli = None,
        StyleField::LetterSpacing => patch.letter_spacing = None,
        StyleField::VerticalAlign => patch.vertical_align = None,
        StyleField::FontWeight => patch.font_weight = None,
        StyleField::StrokeDash => patch.stroke_dash = None,
        StyleField::StrokeCap => patch.stroke_cap = None,
        StyleField::StrokeJoin => patch.stroke_join = None,
        StyleField::Underline => patch.underline = None,
        StyleField::Strikethrough => patch.strikethrough = None,
        StyleField::StrokeStart => patch.stroke_start = None,
        StyleField::StrokeEnd => patch.stroke_end = None,
    }
}

fn set_variant_style(doc: &mut CanvasDocument, node_id: &str, variant: &str, patch: &StylePatch) -> DltResult<Vec<String>> {
    if !valid_key(variant) {
        return Err(invalid("VARIANT_KEY", "variant name is invalid"));
    }
    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    ensure_unlocked(node)?;
    let base = node.variants.get(variant).cloned().unwrap_or_default();
    let merged = merge_patch(&base, patch)?;
    validate_patch(&merged)?;
    guard::check_patch_kind(&merged, node.kind, node_id)?;
    let mut effective = node.style.clone();
    merged.apply(&mut effective);
    guard::check_style_kind(&effective, node.kind, node_id)?;
    node.variants.insert(variant.to_string(), merged);
    Ok(vec![node_id.to_string()])
}

fn insert_page(doc: &mut CanvasDocument, index: u32, page: &Page) -> DltResult<Vec<String>> {
    if doc.pages.len() >= MAX_PAGES {
        return Err(invalid("CANVAS_TOO_MANY_PAGES", "canvas already has the maximum number of pages"));
    }
    let index = index as usize;
    if index > doc.pages.len() {
        return Err(invalid("PAGE_INDEX", "page index is beyond the end of the page list"));
    }
    if !valid_id(&page.id) {
        return Err(invalid("NODE_ID", "page id is empty or contains invalid characters"));
    }
    validate_page_size(page.size)?;
    if page.notes.len() > MAX_TEXT_BYTES {
        return Err(invalid("PAGE_NOTES_TOO_LONG", "page notes exceed the size limit"));
    }
    let mut diagnostics = Vec::new();
    for node in &page.nodes {
        validate_node(node, 0, &mut diagnostics);
    }
    if any_blocking(&diagnostics) {
        return Err(first_blocking(diagnostics));
    }
    let existing: BTreeSet<String> = super::model::all_ids(doc).into_iter().collect();
    let mut new_ids = BTreeSet::new();
    new_ids.insert(page.id.clone());
    for node in &page.nodes {
        subtree_ids(node, &mut new_ids);
    }
    if page.nodes.iter().flat_map(|node| flatten(std::slice::from_ref(node))).any(|node| node.id.contains("::")) {
        return Err(invalid("NODE_ID_RESERVED", "the '::' identifier separator is reserved for component instance children"));
    }
    let new_count = 1 + page.nodes.iter().map(count_subtree).sum::<usize>();
    if new_ids.len() != new_count {
        return Err(duplicate_id(&page.id));
    }
    if let Some(id) = new_ids.iter().find(|id| existing.contains(*id)) {
        return Err(duplicate_id(id));
    }
    doc.pages.insert(index, page.clone());
    // Interaction targets are validated after the complete operation batch in apply().
    Ok(vec![page.id.clone()])
}

fn remove_page(doc: &mut CanvasDocument, links: &mut Vec<FigureLink>, page_id: &str) -> DltResult<Vec<String>> {
    if doc.pages.len() == 1 {
        return Err(invalid("PAGE_LAST", "a canvas must keep at least one page"));
    }
    let index = doc.pages.iter().position(|p| p.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    if doc.board_memos.iter().any(|memo| memo.page_id == page_id) {
        let mut error = invalid("PAGE_BOARD_MEMOS", "remove this page's board memos before deleting the page");
        error.affected_ids.push(page_id.to_owned());
        return Err(error);
    }
    if let Some(section) = doc.page_sections.iter().find(|section| section.start_page_id == page_id) {
        let mut error = invalid("PAGE_SECTION_START", "move or remove this page section marker before deleting its start page");
        error.affected_ids.extend([page_id.to_owned(), section.id.clone()]);
        return Err(error);
    }
    let mut removed = BTreeSet::new();
    removed.insert(page_id.to_string());
    for node in flatten(&doc.pages[index].nodes) {
        removed.insert(node.id.clone());
    }
    let mut referencing = Vec::new();
    for (i, page) in doc.pages.iter().enumerate() {
        if i == index {
            continue;
        }
        for node in flatten(&page.nodes) {
            let refers = node.interactions.iter().any(|interaction| match interaction {
                Interaction::NavigatePage { page_id: target } => removed.contains(target),
                other => interaction_node_target(other).is_some_and(|t| removed.contains(t)),
            });
            if refers {
                referencing.push(node.id.clone());
            }
        }
    }
    if !referencing.is_empty() {
        let mut error = invalid(
            "INTERACTION_REFERENCE",
            format!("page {page_id} is the target of an interaction; remove or retarget that interaction first"),
        );
        error.affected_ids = referencing;
        return Err(error);
    }
    doc.pages.remove(index);
    doc.layout_guides.remove(page_id);
    doc.bindings.retain(|binding| !removed.contains(&binding.node_id));
    figure::prune(links, &removed);
    Ok(removed.into_iter().collect())
}

fn reorder_pages(doc: &mut CanvasDocument, page_ids: &[String]) -> DltResult<Vec<String>> {
    if page_ids.len() != doc.pages.len() {
        return Err(invalid("PAGE_ORDER", "reorder must list every page exactly once"));
    }
    let mut seen = BTreeSet::new();
    let mut reordered = Vec::with_capacity(page_ids.len());
    for id in page_ids {
        if !seen.insert(id.clone()) {
            return Err(invalid("PAGE_ORDER", "reorder lists a page more than once"));
        }
        let page = doc.pages.iter().find(|p| &p.id == id).ok_or_else(|| not_found("page", id))?;
        reordered.push(page.clone());
    }
    doc.pages = reordered;
    // Section markers keep their stable start_page_id. Membership is derived from this new page order.
    Ok(page_ids.to_vec())
}

fn set_page_notes(doc: &mut CanvasDocument, page_id: &str, notes: &str) -> DltResult<Vec<String>> {
    if notes.len() > MAX_TEXT_BYTES {
        return Err(invalid("PAGE_NOTES_TOO_LONG", "page notes exceed the size limit"));
    }
    let page = doc.pages.iter_mut().find(|p| p.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    page.notes = notes.to_string();
    Ok(vec![page_id.to_string()])
}

fn set_canvas_board_memos(
    doc: &mut CanvasDocument,
    page_id: &str,
    expected: &[CanvasBoardMemo],
    memos: &[CanvasBoardMemo],
) -> DltResult<Vec<String>> {
    if !doc.pages.iter().any(|page| page.id == page_id) {
        return Err(not_found("page", page_id));
    }
    let current: Vec<CanvasBoardMemo> = doc.board_memos.iter().filter(|memo| memo.page_id == page_id).cloned().collect();
    if current.as_slice() != expected {
        let message = format!("Canvas board memos on page {page_id} changed since the expected snapshot was read");
        let mut error = DltError::new(ErrorCode::RevisionConflict, message.clone());
        error.affected_ids.push(page_id.to_owned());
        error.diagnostics.push(Diagnostic {
            code: "BOARD_MEMOS_STALE".into(),
            message,
            node_id: Some(page_id.to_owned()),
            blocking: true,
            params: Default::default(),
        });
        return Err(error);
    }
    if expected.iter().chain(memos).any(|memo| memo.page_id != page_id) {
        return Err(invalid("BOARD_MEMO_PAGE", "a page-scoped memo change contains another page's memo"));
    }
    if memos.len() > MAX_NODES {
        return Err(invalid("CANVAS_TOO_MANY_BOARD_MEMOS", "page has too many board memos"));
    }
    if current.as_slice() == memos {
        return Ok(Vec::new());
    }

    let old_ids: BTreeSet<&str> = current.iter().map(|memo| memo.id.as_str()).collect();
    let mut existing: BTreeSet<String> = model::all_ids(doc).into_iter().filter(|id| !old_ids.contains(id.as_str())).collect();
    let mut new_ids = BTreeSet::new();
    for memo in memos {
        if !valid_id(&memo.id) {
            return Err(invalid("BOARD_MEMO_ID", "board memo identifier is invalid"));
        }
        if !new_ids.insert(memo.id.as_str()) || !existing.insert(memo.id.clone()) {
            return Err(invalid("DUPLICATE_ID", format!("identifier {} is already used", memo.id)));
        }
    }

    let mut changed_ids = BTreeSet::from([page_id.to_owned()]);
    changed_ids.extend(current.iter().map(|memo| memo.id.clone()));
    changed_ids.extend(memos.iter().map(|memo| memo.id.clone()));

    let mut replaced = false;
    let mut next = Vec::with_capacity(doc.board_memos.len() + memos.len());
    for memo in std::mem::take(&mut doc.board_memos) {
        if memo.page_id == page_id {
            if !replaced {
                next.extend(memos.iter().cloned());
                replaced = true;
            }
        } else {
            next.push(memo);
        }
    }
    if !replaced {
        next.extend(memos.iter().cloned());
    }
    doc.board_memos = next;
    Ok(changed_ids.into_iter().collect())
}

fn set_canvas_page_guides(
    doc: &mut CanvasDocument,
    page_id: &str,
    expected: Option<&CanvasPageGuides>,
    guides: Option<&CanvasPageGuides>,
) -> DltResult<Vec<String>> {
    let page = doc.pages.iter().find(|page| page.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    let current = doc.layout_guides.get(page_id);
    if current != expected {
        let message = format!("Canvas page guides on page {page_id} changed since the expected snapshot was read");
        let mut error = DltError::new(ErrorCode::RevisionConflict, message.clone());
        error.affected_ids.push(page_id.to_owned());
        error.diagnostics.push(Diagnostic {
            code: "CANVAS_GUIDES_STALE".into(),
            message,
            node_id: Some(page_id.to_owned()),
            blocking: true,
            params: Default::default(),
        });
        return Err(error);
    }
    if let Some(guides) = guides {
        if let Some((code, message)) = model::page_guides_problem(page.size, guides) {
            return Err(invalid(code, message));
        }
    }
    if current == guides {
        return Ok(Vec::new());
    }
    match guides {
        Some(guides) => { doc.layout_guides.insert(page_id.to_owned(), guides.clone()); }
        None => { doc.layout_guides.remove(page_id); }
    }
    Ok(vec![page_id.to_owned()])
}

fn set_canvas_page_sections(
    doc: &mut CanvasDocument,
    expected: &[CanvasPageSection],
    sections: &[CanvasPageSection],
) -> DltResult<Vec<String>> {
    if doc.page_sections.as_slice() != expected {
        let message = "Canvas page sections changed since the expected snapshot was read";
        let mut error = DltError::new(ErrorCode::RevisionConflict, message);
        error.affected_ids = doc.page_sections.iter().map(|section| section.id.clone()).collect();
        error.diagnostics.push(Diagnostic {
            code: "PAGE_SECTIONS_STALE".into(),
            message: message.into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        });
        return Err(error);
    }
    if sections.len() > MAX_PAGES {
        return Err(invalid("PAGE_SECTIONS_TOO_MANY", "canvas has too many page sections"));
    }

    let old_ids: BTreeSet<&str> = doc.page_sections.iter().map(|section| section.id.as_str()).collect();
    let mut existing: BTreeSet<String> = model::all_ids(doc).into_iter().filter(|id| !old_ids.contains(id.as_str())).collect();
    let mut section_ids = BTreeSet::new();
    let mut start_page_ids = BTreeSet::new();
    let mut next = sections.to_vec();
    for section in &next {
        if !valid_id(&section.id) {
            return Err(invalid("PAGE_SECTION_ID", "page section identifier is invalid"));
        }
        if !section_ids.insert(section.id.as_str()) || !existing.insert(section.id.clone()) {
            return Err(invalid("DUPLICATE_ID", format!("identifier {} is already used", section.id)));
        }
        if section.name.is_empty() {
            return Err(invalid("PAGE_SECTION_NAME", "page section name cannot be empty"));
        }
        if let Some(problem) = info_text_problem(&section.name, MAX_NODE_NAME_BYTES) {
            return Err(invalid("PAGE_SECTION_NAME", format!("page section name {problem}")));
        }
        let page = doc.pages.iter().find(|page| page.id == section.start_page_id)
            .ok_or_else(|| invalid("PAGE_SECTION_START", "page section start refers to a missing page"))?;
        if page.role == Some(designllm_contracts::PageRole::Component) {
            return Err(invalid("PAGE_SECTION_COMPONENT_START", "a component page cannot start a section"));
        }
        if !start_page_ids.insert(section.start_page_id.as_str()) {
            return Err(invalid("PAGE_SECTION_DUPLICATE", "only one page section may start at a page"));
        }
    }
    next.sort_by(|left, right| left.id.cmp(&right.id));
    if doc.page_sections == next {
        return Ok(Vec::new());
    }

    let mut changed_ids = BTreeSet::new();
    changed_ids.extend(doc.page_sections.iter().map(|section| section.id.clone()));
    changed_ids.extend(doc.page_sections.iter().map(|section| section.start_page_id.clone()));
    changed_ids.extend(next.iter().map(|section| section.id.clone()));
    changed_ids.extend(next.iter().map(|section| section.start_page_id.clone()));
    doc.page_sections = next;
    Ok(changed_ids.into_iter().collect())
}

/// Refuses a change to a locked node. A locked node changes only through SetNodeAppearance, which can unlock it.
pub(super) fn ensure_unlocked(node: &Node) -> DltResult<()> {
    if node.ext.locked == Some(true) {
        let message = format!("node {} is locked", node.id);
        let mut error = DltError::new(ErrorCode::ContentLocked, message.clone());
        error.affected_ids.push(node.id.clone());
        error.diagnostics.push(Diagnostic { code: "NODE_LOCKED".into(), message, node_id: Some(node.id.clone()), blocking: true, params: Default::default() });
        return Err(error);
    }
    Ok(())
}

/// Removes a node with its subtree from a list and returns it.
fn take_from(nodes: &mut Vec<Node>, id: &str) -> Option<Node> {
    if let Some(pos) = nodes.iter().position(|n| n.id == id) {
        return Some(nodes.remove(pos));
    }
    for node in nodes.iter_mut() {
        if let Some(found) = take_from(&mut node.children, id) {
            return Some(found);
        }
    }
    None
}

/// The list that holds a node: the page's top-level list or its parent's children.
fn sibling_list_mut<'a>(nodes: &'a mut Vec<Node>, id: &str) -> Option<&'a mut Vec<Node>> {
    if nodes.iter().any(|n| n.id == id) {
        return Some(nodes);
    }
    for node in nodes.iter_mut() {
        if let Some(list) = sibling_list_mut(&mut node.children, id) {
            return Some(list);
        }
    }
    None
}

/// Levels in a subtree, counting the node itself. A leaf is one level.
fn subtree_depth(node: &Node) -> usize {
    1 + node.children.iter().map(subtree_depth).max().unwrap_or(0)
}

fn set_page_size(doc: &mut CanvasDocument, page_id: &str, size: Size) -> DltResult<Vec<String>> {
    validate_page_size(size)?;
    if let Some((code, reason)) = doc.layout_guides.get(page_id).and_then(|guides| model::page_guides_problem(size, guides)) {
        let mut error = invalid(
            code,
            format!("SetPageSize refused for page {page_id} ({}x{} EMU): its layout guides no longer fit ({reason})", size.width, size.height),
        );
        error.affected_ids.push(page_id.to_owned());
        return Err(error);
    }
    let page = doc.pages.iter_mut().find(|p| p.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    page.size = size;
    Ok(vec![page_id.to_string()])
}

/// Moves a node with its subtree to `index` among the children of `parent_id` (None: the top level of the page).
/// The index counts the list after the node has left its old place. Cycles and too deep trees are refused.
fn move_node(doc: &mut CanvasDocument, node_id: &str, page_id: &str, parent_id: Option<&str>, index: u32) -> DltResult<Vec<String>> {
    if table_owner_in_doc(doc, node_id).is_some() {
        return Err(unsupported("TABLE_CELL_MOVE", "table cells move with their table row or column structure"));
    }
    let dest = doc.pages.iter().position(|p| p.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    let source = find_node_in_doc(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    ensure_unlocked(source)?;
    let mut subtree = BTreeSet::new();
    subtree_ids(source, &mut subtree);
    let levels = subtree_depth(source);
    let new_depth = match parent_id {
        Some(pid) => {
            if subtree.contains(pid) {
                return Err(invalid("MOVE_CYCLE", "a node cannot move into its own subtree"));
            }
            let parent = find_node(&doc.pages[dest].nodes, pid).ok_or_else(|| not_found("node", pid))?;
            if !matches!(parent.kind, NodeKind::Group | NodeKind::Rectangle) {
                return Err(invalid("NODE_CHILDREN", "only group and rectangle nodes can contain other nodes"));
            }
            ensure_unlocked(parent)?;
            depth_of(&doc.pages[dest].nodes, pid, 0).ok_or_else(|| not_found("node", pid))? + 1
        }
        None => 0,
    };
    if new_depth + levels > MAX_DEPTH + 1 {
        return Err(invalid("NODE_DEPTH", "node tree is too deep"));
    }
    let mut moved = None;
    for page in doc.pages.iter_mut() {
        if let Some(node) = take_from(&mut page.nodes, node_id) {
            moved = Some(node);
            break;
        }
    }
    let node = moved.ok_or_else(|| not_found("node", node_id))?;
    match parent_id {
        None => {
            let list = &mut doc.pages[dest].nodes;
            if index as usize > list.len() {
                return Err(invalid("MOVE_INDEX", "index is beyond the end of the page's list"));
            }
            list.insert(index as usize, node);
        }
        Some(pid) => {
            let parent = find_node_mut(&mut doc.pages[dest].nodes, pid).ok_or_else(|| not_found("node", pid))?;
            if index as usize > parent.children.len() {
                return Err(invalid("MOVE_INDEX", "index is beyond the end of the child list"));
            }
            parent.children.insert(index as usize, node);
        }
    }
    Ok(vec![node_id.to_string()])
}

/// Moves a node to `index` among its current siblings.
fn set_z_order(doc: &mut CanvasDocument, node_id: &str, index: u32) -> DltResult<Vec<String>> {
    if table_owner_in_doc(doc, node_id).is_some() {
        return Err(unsupported("TABLE_CELL_ORDER", "table cell order follows its row and column structure"));
    }
    for page in doc.pages.iter_mut() {
        if let Some(list) = sibling_list_mut(&mut page.nodes, node_id) {
            let pos = list.iter().position(|n| n.id == node_id).ok_or_else(|| not_found("node", node_id))?;
            ensure_unlocked(&list[pos])?;
            if index as usize >= list.len() {
                return Err(invalid("Z_INDEX", "index is outside the sibling list"));
            }
            let node = list.remove(pos);
            list.insert(index as usize, node);
            return Ok(vec![node_id.to_string()]);
        }
    }
    Err(not_found("node", node_id))
}

/// Image fit (images only) and the lock flag. Locking is a flag on the node; a locked node is changed only by this
/// operation, which can unlock it.
fn set_node_appearance(
    doc: &mut CanvasDocument,
    node_ids: &[String],
    image_fit: Option<ImageFit>,
    locked: Option<bool>,
) -> DltResult<Vec<String>> {
    if node_ids.is_empty() {
        return Err(invalid("APPEARANCE_TARGETS", "no nodes were selected"));
    }
    let mut changed = Vec::new();
    let mut seen = BTreeSet::new();
    for id in node_ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        ensure_table_owner_unlocked(doc, id)?;
        let node = find_node_in_doc_mut(doc, id).ok_or_else(|| not_found("node", id))?;
        if let Some(fit) = image_fit {
            ensure_unlocked(node)?;
            if node.kind != NodeKind::Image {
                return Err(invalid("NODE_IMAGE_FIT", format!("node {id} is not an image")));
            }
            node.ext.image_fit = Some(fit);
        }
        if let Some(lock) = locked {
            node.ext.locked = Some(lock);
        }
        changed.push(id.clone());
    }
    Ok(changed)
}

/// Alternative text, layer name (empty text clears), and visibility. Locked nodes are refused.
fn set_node_info(
    doc: &mut CanvasDocument,
    node_ids: &[String],
    alt: Option<String>,
    name: Option<String>,
    hidden: Option<bool>,
) -> DltResult<Vec<String>> {
    if node_ids.is_empty() {
        return Err(invalid("INFO_TARGETS", "no nodes were selected"));
    }
    if let Some(text) = &alt {
        if let Some(problem) = info_text_problem(text, MAX_ALT_BYTES) {
            return Err(invalid("NODE_ALT", format!("alternative text {problem}")));
        }
    }
    if let Some(text) = &name {
        if let Some(problem) = info_text_problem(text, MAX_NODE_NAME_BYTES) {
            return Err(invalid("NODE_NAME", format!("layer name {problem}")));
        }
    }
    let mut changed = Vec::new();
    let mut seen = BTreeSet::new();
    for id in node_ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        if hidden.is_some() && table_owner_in_doc(doc, id).is_some() {
            return Err(unsupported("TABLE_CELL_HIDDEN", "table cells cannot be hidden individually"));
        }
        ensure_node_unlocked_in_doc(doc, id)?;
        let node = find_node_in_doc_mut(doc, id).ok_or_else(|| not_found("node", id))?;
        if let Some(text) = &alt {
            node.ext.alt = (!text.is_empty()).then(|| text.clone());
        }
        if let Some(text) = &name {
            node.ext.name = (!text.is_empty()).then(|| text.clone());
        }
        if let Some(value) = hidden {
            node.ext.hidden = Some(value);
        }
        changed.push(id.clone());
    }
    Ok(changed)
}

fn set_node_list(doc: &mut CanvasDocument, node_id: &str, list: Option<TextListKind>) -> DltResult<Vec<String>> {
    let node = find_node_in_doc_mut(doc, node_id).ok_or_else(|| not_found("node", node_id))?;
    if node.kind != NodeKind::Text {
        return Err(unsupported("NODE_LIST_UNSUPPORTED", format!("list formatting requires a text node: {node_id}")));
    }
    ensure_unlocked(node)?;
    node.ext.list = list;
    Ok(vec![node_id.to_string()])
}

fn rename_page(doc: &mut CanvasDocument, page_id: &str, name: Option<String>) -> DltResult<Vec<String>> {
    if let Some(text) = &name {
        if let Some(problem) = info_text_problem(text, MAX_NODE_NAME_BYTES) {
            return Err(invalid("PAGE_NAME", format!("page name {problem}")));
        }
    }
    let page = doc.pages.iter_mut().find(|p| p.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    page.name = name.filter(|text| !text.is_empty());
    Ok(vec![page_id.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::{AssetRef, CanvasBoardMemoKind, ImageFit, Interaction, NodeExt, PageRole, PixelRect, Style, StylePatch};

    struct ReplacementAssets {
        canonical: AssetRef,
        bytes: Vec<u8>,
    }

    impl AssetStore for ReplacementAssets {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> {
            Err(DltError::unsupported("asset imports are not used by replacement tests"))
        }
        fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
            if asset.sha256.as_str() != self.canonical.sha256.as_str() {
                return Err(DltError::new(ErrorCode::NotFound, asset.sha256.clone()));
            }
            Ok(self.bytes.clone())
        }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
            if hash == self.canonical.sha256.as_str() {
                Ok(self.canonical.clone())
            } else {
                Err(DltError::new(ErrorCode::NotFound, hash.to_string()))
            }
        }
    }

    fn replacement_assets(bytes: Vec<u8>, mime: &str) -> ReplacementAssets {
        ReplacementAssets {
            canonical: AssetRef { sha256: sha256(&bytes), mime: mime.into(), bytes: bytes.len() as u64 },
            bytes,
        }
    }

    fn tiny_bmp() -> Vec<u8> {
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&62u32.to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&2i32.to_le_bytes());
        bmp.extend_from_slice(&1i32.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 24]);
        bmp.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        bmp
    }

    fn image_document(asset: AssetRef) -> CanvasDocument {
        let node = Node {
            id: "image".into(),
            kind: NodeKind::Image,
            bounds: Rect { x: 10, y: 20, width: 300, height: 200 },
            text: String::new(),
            style: Style {
                fill: "#AABBCC".into(),
                stroke: "#112233".into(),
                align: "center".into(),
                opacity_milli: Some(750),
                ..Style::default()
            },
            asset: Some(asset),
            children: Vec::new(),
            layout: None,
            interactions: vec![Interaction::OpenUrl { url: "https://example.test/image".into() }],
            variants: [("dark".into(), StylePatch { fill: Some("#334455".into()), ..Default::default() })].into_iter().collect(),
            ext: NodeExt { alt: Some("original alternative".into()), image_fit: Some(ImageFit::Contain), ..Default::default() },
        };
        CanvasDocument {
            pages: vec![Page { nodes: vec![node], ..Page::new("page", Size::slide()) }],
            ..Default::default()
        }
    }

    fn replacement_op(node_id: &str, expected: &str, replacement: &AssetRef) -> Operation {
        Operation::ReplaceImageAsset {
            node_id: node_id.into(),
            expected_asset_sha256: expected.into(),
            asset_sha256: replacement.sha256.clone(),
        }
    }

    fn image_crop_op(expected: &str, crop: Option<PixelRect>) -> Operation {
        Operation::SetCanvasImageCrop { node_id: "image".into(), expected_asset_sha256: expected.into(), crop_pixel_rect: crop }
    }

    fn assert_replacement_refused(
        mut document: CanvasDocument,
        mut links: Vec<FigureLink>,
        assets: &dyn AssetStore,
        operation: Operation,
        code: ErrorCode,
        diagnostic: &str,
    ) {
        let before = serde_json::to_value((&document, &links)).expect("before snapshot");
        let error = apply(&mut document, &mut links, assets, &[operation]).expect_err("unsafe image replacement is refused");
        assert_eq!(error.code, code, "{error:?}");
        assert!(error.diagnostics.iter().any(|entry| entry.code == diagnostic), "{error:?}");
        assert_eq!(serde_json::to_value((&document, &links)).expect("after snapshot"), before, "a refusal changes neither document nor links");
    }

    fn assert_image_crop_refused(
        mut document: CanvasDocument,
        mut links: Vec<FigureLink>,
        assets: &dyn AssetStore,
        operation: Operation,
        code: ErrorCode,
        diagnostic: &str,
    ) {
        let before = serde_json::to_value((&document, &links)).expect("before crop snapshot");
        let error = apply(&mut document, &mut links, assets, &[operation]).expect_err("unsafe image crop is refused");
        assert_eq!(error.code, code, "{error:?}");
        assert!(error.diagnostics.iter().any(|entry| entry.code == diagnostic), "{error:?}");
        assert_eq!(serde_json::to_value((&document, &links)).expect("after crop refusal"), before, "refusal preserves node and links");
    }

    fn span_table_document() -> CanvasDocument {
        let cell = |id: &str| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect {
                x: 0, y: 0, width: 120 * designllm_contracts::EMU_PER_CSS_PX,
                height: 36 * designllm_contracts::EMU_PER_CSS_PX,
            },
            text: String::new(), style: Style::default(), asset: None, children: vec![],
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: vec![], variants: Default::default(), ext: Default::default(),
        };
        let table = Node {
            id: "table".into(), kind: NodeKind::Group, bounds: Rect {
                x: 0, y: 0, width: 240 * designllm_contracts::EMU_PER_CSS_PX,
                height: 36 * designllm_contracts::EMU_PER_CSS_PX,
            },
            text: String::new(), style: Style::default(), asset: None, children: vec![cell("c1"), cell("c2")],
            layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..Default::default() }),
            interactions: vec![], variants: Default::default(),
            ext: designllm_contracts::NodeExt {
                table: Some(CanvasTable {
                    rows: vec![designllm_contracts::CanvasTableRow { id: "r1".into(), cell_ids: vec!["c1".into(), "c2".into()] }],
                    header_row_id: None, column_widths: None, cell_spans: Default::default(),
                }),
                ..Default::default()
            },
        };
        CanvasDocument { pages: vec![Page {
            nodes: vec![table],
            ..Page::new("page", Size::slide())
        }], ..Default::default() }
    }

    #[test]
    fn merge_keeps_base_values_that_the_patch_does_not_set() {
        let base = StylePatch { color: Some("#111111".into()), bold: Some(true), ..Default::default() };
        let over = StylePatch { bold: Some(false), ..Default::default() };
        let merged = merge_patch(&base, &over).unwrap();
        assert_eq!(merged.color.as_deref(), Some("#111111"));
        assert_eq!(merged.bold, Some(false));
    }

    #[test]
    fn merge_applies_a_later_clear_and_a_later_set_in_order() {
        use designllm_contracts::StyleField;
        // A later clear drops the earlier set, and the field stays cleared.
        let base = StylePatch { color: Some("#111111".into()), fill: Some("#FFFFFF".into()), ..Default::default() };
        let clears_color = StylePatch { clear: [StyleField::Color].into_iter().collect(), ..Default::default() };
        let merged = merge_patch(&base, &clears_color).unwrap();
        assert_eq!((merged.color.as_deref(), merged.fill.as_deref()), (None, Some("#FFFFFF")));
        assert!(merged.clear.contains(&StyleField::Color));
        // A later set cancels an earlier clear of the same field.
        let clears_fill = StylePatch { clear: [StyleField::Fill].into_iter().collect(), ..Default::default() };
        let sets_fill = StylePatch { fill: Some("#000000".into()), ..Default::default() };
        let merged = merge_patch(&clears_fill, &sets_fill).unwrap();
        assert_eq!(merged.fill.as_deref(), Some("#000000"));
        assert!(!merged.clear.contains(&StyleField::Fill));
        // The merged patch gives the same style as the two patches applied one after the other.
        let style = || designllm_contracts::Style { color: "#222222".into(), fill: "#444444".into(), ..Default::default() };
        let mut in_sequence = style();
        base.apply(&mut in_sequence);
        clears_color.apply(&mut in_sequence);
        let mut once = style();
        merge_patch(&base, &clears_color).unwrap().apply(&mut once);
        assert_eq!(serde_json::to_value(&in_sequence).unwrap(), serde_json::to_value(&once).unwrap());
    }

    #[test]
    fn merge_keeps_typography_values_and_clears() {
        let v12 = StylePatch { underline: Some(true), ..Default::default() };
        assert_eq!(merge_patch(&StylePatch::default(), &v12).unwrap().underline, Some(true));
        let clears_weight = StylePatch { clear: [designllm_contracts::StyleField::FontWeight].into_iter().collect(), ..Default::default() };
        assert!(merge_patch(&StylePatch::default(), &clears_weight).unwrap().clear.contains(&designllm_contracts::StyleField::FontWeight));
    }

    #[test]
    fn op_names_are_snake_case_tags() {
        let op = Operation::SetPageNotes { page_id: "p".into(), notes: String::new() };
        assert_eq!(op.name(), "set_page_notes");
    }

    #[test]
    fn board_memo_page_snapshot_supports_crud_and_page_delete_fails_closed() {
        struct NoAssets;
        impl AssetStore for NoAssets {
            fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no assets")) }
            fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> { Err(DltError::unsupported("no assets")) }
            fn lookup(&self, hash: &str) -> DltResult<AssetRef> { Err(DltError::new(ErrorCode::NotFound, hash)) }
        }

        let mut doc = span_table_document();
        doc.pages[0].notes = "speaker notes stay on the page".into();
        doc.pages.push(Page::new("other-page", Size::slide()));
        let memo = CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: "memo-1".into(), page_id: "page".into(),
            bounds: Rect { x: -100, y: 200, width: 300_000, height: 180_000 },
            text: "draft".into(), color: "#FFF2CC".into(),
        };
        let assets = NoAssets;
        let mut links = Vec::new();
        apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasBoardMemos {
            page_id: "page".into(), expected: Vec::new(), memos: vec![memo.clone()],
        }]).expect("create page-local board memo");
        assert_eq!(doc.board_memos, vec![memo.clone()]);
        assert_eq!(doc.pages[0].notes, "speaker notes stay on the page");
        assert!(!model::validate_document(&doc).iter().any(|diagnostic| diagnostic.blocking));

        let before_stale = serde_json::to_value(&doc).unwrap();
        let stale = apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasBoardMemos {
            page_id: "page".into(), expected: Vec::new(), memos: Vec::new(),
        }]).expect_err("stale page memo snapshots are refused");
        assert_eq!(stale.code, ErrorCode::RevisionConflict);
        assert_eq!(serde_json::to_value(&doc).unwrap(), before_stale, "stale memo update is atomic");

        let before_page_delete = serde_json::to_value(&doc).unwrap();
        let delete_error = apply(&mut doc, &mut links, &assets, &[Operation::RemovePage { page_id: "page".into() }])
            .expect_err("page deletion cannot discard board memos");
        assert_eq!(delete_error.code, ErrorCode::InvalidArgument);
        assert_eq!(serde_json::to_value(&doc).unwrap(), before_page_delete, "refused page deletion preserves the memo");

        let mut updated = memo.clone();
        updated.text = "edited\ntext".into();
        updated.color = "#DDEEFF".into();
        updated.bounds.x = -50_000;
        apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasBoardMemos {
            page_id: "page".into(), expected: vec![memo], memos: vec![updated.clone()],
        }]).expect("update page-local board memo");
        assert_eq!(doc.board_memos, vec![updated.clone()]);

        apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasBoardMemos {
            page_id: "page".into(), expected: vec![updated], memos: Vec::new(),
        }]).expect("delete page-local board memo");
        assert!(doc.board_memos.is_empty());
        assert_eq!(doc.pages[0].notes, "speaker notes stay on the page");
        apply(&mut doc, &mut links, &assets, &[Operation::RemovePage { page_id: "page".into() }])
            .expect("page can be removed after its memos were explicitly removed");
        assert_eq!(doc.pages[0].id, "other-page");
    }

    #[test]
    fn page_sections_are_snapshot_atomic_and_start_markers_follow_page_ids() {
        struct NoAssets;
        impl AssetStore for NoAssets {
            fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no assets")) }
            fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> { Err(DltError::unsupported("no assets")) }
            fn lookup(&self, hash: &str) -> DltResult<AssetRef> { Err(DltError::new(ErrorCode::NotFound, hash)) }
        }

        let mut doc = span_table_document();
        doc.pages.push(Page::new("page-2", Size::slide()));
        doc.pages.push(Page::new("page-3", Size::slide()));
        let section_a = CanvasPageSection { id: "section-a".into(), name: "Part A".into(), start_page_id: "page".into() };
        let section_b = CanvasPageSection { id: "section-b".into(), name: "Part B".into(), start_page_id: "page-3".into() };
        let assets = NoAssets;
        let mut links = Vec::new();

        apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasPageSections {
            expected: Vec::new(), sections: vec![section_b.clone(), section_a.clone()],
        }]).expect("create page section markers");
        assert_eq!(doc.page_sections, vec![section_a.clone(), section_b.clone()], "stored snapshots use canonical section ID order");
        assert!(!model::validate_document(&doc).iter().any(|diagnostic| diagnostic.blocking));

        let before_stale = serde_json::to_value(&doc).unwrap();
        let stale = apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasPageSections { expected: Vec::new(), sections: Vec::new() }])
            .expect_err("stale full section snapshot is rejected");
        assert_eq!(stale.code, ErrorCode::RevisionConflict);
        assert!(stale.diagnostics.iter().any(|diagnostic| diagnostic.code == "PAGE_SECTIONS_STALE"));
        assert_eq!(serde_json::to_value(&doc).unwrap(), before_stale, "stale section replacement is atomic");

        let duplicate_start = apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasPageSections {
            expected: vec![section_a.clone(), section_b.clone()],
            sections: vec![section_a.clone(), CanvasPageSection { start_page_id: "page".into(), ..section_b.clone() }],
        }]).expect_err("a page can start only one section");
        assert_eq!(duplicate_start.diagnostics.first().map(|diagnostic| diagnostic.code.as_str()), Some("PAGE_SECTION_DUPLICATE"));
        assert_eq!(serde_json::to_value(&doc).unwrap(), before_stale, "invalid section starts are atomic");

        let reordered = apply(&mut doc, &mut links, &assets, &[Operation::ReorderPages {
            page_ids: vec!["page-3".into(), "page-2".into(), "page".into()],
        }]).expect("reorder actual pages");
        assert!(reordered.0.contains(&"page-3".to_owned()));
        assert_eq!(doc.page_sections.iter().map(|section| section.start_page_id.as_str()).collect::<Vec<_>>(), ["page", "page-3"],
            "section markers remain attached to page IDs when pages reorder");
        assert!(!model::validate_document(&doc).iter().any(|diagnostic| diagnostic.blocking));

        let before_component_role = serde_json::to_value(&doc).unwrap();
        let component_start = apply(&mut doc, &mut links, &assets, &[Operation::SetPageRole {
            page_id: "page-3".into(), role: Some(designllm_contracts::PageRole::Component),
        }]).expect_err("component pages cannot start sections");
        assert_eq!(component_start.code, ErrorCode::InvalidArgument);
        assert_eq!(serde_json::to_value(&doc).unwrap(), before_component_role);

        let before_start_delete = serde_json::to_value(&doc).unwrap();
        let start_delete = apply(&mut doc, &mut links, &assets, &[Operation::RemovePage { page_id: "page-3".into() }])
            .expect_err("a section start page cannot be deleted silently");
        assert_eq!(start_delete.diagnostics.first().map(|diagnostic| diagnostic.code.as_str()), Some("PAGE_SECTION_START"));
        assert_eq!(serde_json::to_value(&doc).unwrap(), before_start_delete);

        apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasPageSections {
            expected: vec![section_a, section_b], sections: Vec::new(),
        }]).expect("remove section markers while keeping their pages");
        assert!(doc.page_sections.is_empty());
        assert_eq!(doc.pages.len(), 3);
        assert!(!model::validate_document(&doc).iter().any(|diagnostic| diagnostic.blocking));
    }

    #[test]
    fn page_guides_are_expected_snapshot_atomic_and_follow_page_ids() {
        struct NoAssets;
        impl AssetStore for NoAssets {
            fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no assets")) }
            fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> { Err(DltError::unsupported("no assets")) }
            fn lookup(&self, hash: &str) -> DltResult<AssetRef> { Err(DltError::new(ErrorCode::NotFound, hash)) }
        }

        let mut doc = span_table_document();
        doc.pages.push(Page::new("page-2", Size::slide()));
        let guides = CanvasPageGuides {
            columns: Some(designllm_contracts::CanvasAxisGuides {
                count: 3, spacing_emu: 20_000, leading_margin_emu: 100_000, trailing_margin_emu: 100_000,
            }),
            rows: Some(designllm_contracts::CanvasAxisGuides {
                count: 2, spacing_emu: 20_000, leading_margin_emu: 100_000, trailing_margin_emu: 100_000,
            }),
        };
        let assets = NoAssets;
        let mut links = Vec::new();

        let created = apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasPageGuides {
            page_id: "page".into(), expected: None, guides: Some(guides.clone()),
        }]).expect("create page-local guides");
        assert_eq!(created.0, vec!["page"]);
        assert_eq!(doc.layout_guides.get("page"), Some(&guides));
        assert!(!model::validate_document(&doc).iter().any(|diagnostic| diagnostic.blocking));

        let before_stale = serde_json::to_value(&doc).unwrap();
        let stale = apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasPageGuides {
            page_id: "page".into(), expected: None, guides: None,
        }]).expect_err("stale page guide settings are refused");
        assert_eq!(stale.code, ErrorCode::RevisionConflict);
        assert!(stale.diagnostics.iter().any(|diagnostic| diagnostic.code == "CANVAS_GUIDES_STALE"));
        assert_eq!(serde_json::to_value(&doc).unwrap(), before_stale, "stale guide update is atomic");

        let invalid = CanvasPageGuides {
            columns: Some(designllm_contracts::CanvasAxisGuides {
                count: 2, spacing_emu: i64::MAX, leading_margin_emu: 0, trailing_margin_emu: 0,
            }),
            rows: None,
        };
        let error = apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasPageGuides {
            page_id: "page".into(), expected: Some(guides.clone()), guides: Some(invalid),
        }]).expect_err("guide settings that consume the page are refused");
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.diagnostics.iter().any(|diagnostic| diagnostic.code == "CANVAS_GUIDES_AREA"));
        assert_eq!(serde_json::to_value(&doc).unwrap(), before_stale, "invalid guide update is atomic");

        let removed = apply(&mut doc, &mut links, &assets, &[Operation::RemovePage { page_id: "page".into() }])
            .expect("page deletion removes its page-local guides atomically");
        assert!(removed.0.contains(&"page".to_owned()));
        assert!(!doc.layout_guides.contains_key("page"));
        assert_eq!(doc.pages[0].id, "page-2");
    }

    #[test]
    fn set_page_size_refuses_a_shrink_that_breaks_page_guides_and_keeps_the_page() {
        struct NoAssets;
        impl AssetStore for NoAssets {
            fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no assets")) }
            fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> { Err(DltError::unsupported("no assets")) }
            fn lookup(&self, hash: &str) -> DltResult<AssetRef> { Err(DltError::new(ErrorCode::NotFound, hash)) }
        }

        let mut doc = span_table_document();
        let assets = NoAssets;
        let mut links = Vec::new();
        let guides = CanvasPageGuides {
            columns: Some(designllm_contracts::CanvasAxisGuides {
                count: 1, spacing_emu: 0, leading_margin_emu: 10_000, trailing_margin_emu: 10_000,
            }),
            rows: None,
        };
        apply(&mut doc, &mut links, &assets, &[Operation::SetCanvasPageGuides {
            page_id: "page".into(), expected: None, guides: Some(guides),
        }]).expect("guides that leave room for one track");
        let before = serde_json::to_value(&doc).unwrap();
        let height = doc.pages[0].size.height;
        let error = apply(&mut doc, &mut links, &assets, &[Operation::SetPageSize {
            page_id: "page".into(), size: Size { width: 15_000, height },
        }]).expect_err("a shrink that leaves no track room is refused");
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(error.affected_ids, vec!["page".to_owned()]);
        assert!(error.diagnostics.iter().any(|diagnostic| diagnostic.code == "CANVAS_GUIDES_AREA"));
        assert!(error.message.contains("refused for page page (15000x"), "{}", error.message);
        assert!(error.message.contains("guide spacing and margins must leave at least 1 EMU for each track"), "{}", error.message);
        assert_eq!(serde_json::to_value(&doc).unwrap(), before, "refused resize is atomic");
    }

    #[test]
    fn guide_count_message_names_the_shared_limit() {
        let too_many = CanvasPageGuides {
            columns: Some(designllm_contracts::CanvasAxisGuides {
                count: designllm_contracts::layout_guides::MAX_GUIDE_COUNT + 1,
                spacing_emu: 0, leading_margin_emu: 0, trailing_margin_emu: 0,
            }),
            rows: None,
        };
        let (code, message) = model::page_guides_problem(Size { width: 100_000, height: 100_000 }, &too_many)
            .expect("a count above the limit is refused");
        assert_eq!(code, "CANVAS_GUIDES_COUNT");
        assert!(message.contains(&designllm_contracts::layout_guides::MAX_GUIDE_COUNT.to_string()), "{message}");
    }

    #[test]
    fn spans_can_be_applied_and_covered_cells_cannot_be_edited() {
        use designllm_contracts::CanvasTableSpan;

        let mut doc = span_table_document();
        let expected = doc.pages[0].nodes[0].ext.table.clone().unwrap();
        let mut merged = expected.clone();
        merged.cell_spans.insert("c1".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        let changed = set_canvas_table_structure(&mut doc, "table", &expected, &merged).expect("valid spans can be applied");
        assert!(changed.iter().any(|id| id == "table"));
        assert_eq!(doc.pages[0].nodes[0].ext.table.as_ref(), Some(&merged));
        assert!(!model::validate_document(&doc).iter().any(|diagnostic| diagnostic.blocking), "merged table remains valid");

        let before = serde_json::to_value(&doc).expect("snapshot before covered edit");
        let error = set_text(&mut doc, "c2", "hidden edit").expect_err("covered content must wait for unmerge");
        assert_eq!(error.code, ErrorCode::UnsupportedOperation);
        assert_eq!(doc.pages[0].nodes[0].children[1].text, "");
        assert_eq!(serde_json::to_value(&doc).expect("snapshot after covered edit refusal"), before);
    }

    #[test]
    fn rows_and_columns_cannot_change_while_a_table_span_is_active() {
        use designllm_contracts::CanvasTableSpan;

        let doc = span_table_document();
        let mut expected = doc.pages[0].nodes[0].ext.table.clone().unwrap();
        expected.cell_spans.insert("c1".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        let mut next = expected.clone();
        next.rows[0].cell_ids.push("c3".into());
        let error = validate_structure_width_transition(&expected, &next).expect_err("unmerge before changing columns");
        assert_eq!(error.diagnostics[0].code, "TABLE_SPAN_STRUCTURE");
    }

    #[test]
    fn image_crop_sets_and_resets_source_pixels_without_changing_other_node_data() {
        let bytes = designllm_render::png_from_rgba(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]).expect("source PNG");
        let assets = replacement_assets(bytes, "image/png");
        let original = image_document(assets.canonical.clone());
        let before = serde_json::to_value(&original).expect("original image document");
        let crop = PixelRect { x: 1, y: 0, width: 1, height: 1 };
        let mut document = original.clone();
        let mut links = Vec::new();
        let (changed, diagnostics) = apply(&mut document, &mut links, &assets, &[image_crop_op(&assets.canonical.sha256, Some(crop))])
            .expect("valid non-destructive source-pixel crop");
        assert_eq!(changed, vec!["image".to_string()]);
        assert!(diagnostics.is_empty());
        let after = serde_json::to_value(&document).expect("cropped image document");
        let mut expected = before["pages"][0]["nodes"][0].clone();
        expected["image_crop_pixel_rect"] = serde_json::json!({"x": 1, "y": 0, "width": 1, "height": 1});
        assert_eq!(after["pages"][0]["nodes"][0], expected, "crop changes only the persisted source-pixel rectangle");
        assert_eq!(document.pages[0].nodes[0].asset.as_ref(), Some(&assets.canonical), "the original asset remains referenced");

        let (changed, diagnostics) = apply(&mut document, &mut links, &assets, &[image_crop_op(&assets.canonical.sha256, None)])
            .expect("None explicitly restores the full source image");
        assert_eq!(changed, vec!["image".to_string()]);
        assert!(diagnostics.is_empty());
        assert_eq!(serde_json::to_value(&document).expect("reset image document"), before);
    }

    #[test]
    fn image_crop_rejects_stale_bounds_locks_figure_links_and_component_images_atomically() {
        let bytes = designllm_render::png_from_rgba(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]).expect("source PNG");
        let assets = replacement_assets(bytes, "image/png");
        let base = image_document(assets.canonical.clone());
        let crop = Some(PixelRect { x: 1, y: 0, width: 1, height: 1 });

        assert_image_crop_refused(
            base.clone(), Vec::new(), &assets,
            Operation::SetCanvasImageCrop { node_id: "image".into(), expected_asset_sha256: "stale".into(), crop_pixel_rect: crop },
            ErrorCode::RevisionConflict, "IMAGE_CROP_STALE",
        );
        assert_image_crop_refused(
            base.clone(), Vec::new(), &assets,
            image_crop_op(&assets.canonical.sha256, Some(PixelRect { x: 2, y: 0, width: 1, height: 1 })),
            ErrorCode::InvalidArgument, "IMAGE_CROP_BOUNDS",
        );

        let mut locked = base.clone();
        locked.pages[0].nodes[0].ext.locked = Some(true);
        assert_image_crop_refused(locked, Vec::new(), &assets, image_crop_op(&assets.canonical.sha256, crop), ErrorCode::ContentLocked, "NODE_LOCKED");

        let mut parent_locked = base.clone();
        let image = parent_locked.pages[0].nodes.pop().expect("image node");
        parent_locked.pages[0].nodes.push(Node {
            id: "parent".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 500, height: 500 },
            text: String::new(), style: Style::default(), asset: None, children: vec![image], layout: None,
            interactions: Vec::new(), variants: Default::default(), ext: NodeExt { locked: Some(true), ..Default::default() },
        });
        assert_image_crop_refused(parent_locked, Vec::new(), &assets, image_crop_op(&assets.canonical.sha256, crop), ErrorCode::ContentLocked, "NODE_LOCKED");

        let link = FigureLink { figure_id: "figure".into(), figure_revision: 1, target_node_id: "image".into(), representation: "png".into(), render_hash: "render".into() };
        assert_image_crop_refused(base.clone(), vec![link], &assets, image_crop_op(&assets.canonical.sha256, crop), ErrorCode::UnsupportedOperation, "IMAGE_CROP_FIGURE_LINKED");

        let mut component = base;
        component.pages[0].role = Some(PageRole::Component);
        assert_image_crop_refused(component, Vec::new(), &assets, image_crop_op(&assets.canonical.sha256, crop), ErrorCode::UnsupportedOperation, "IMAGE_CROP_COMPONENT_SOURCE_UNSUPPORTED");

        let mut instance = image_document(assets.canonical.clone());
        let mut child = instance.pages[0].nodes.pop().expect("image node");
        child.id = "instance::image".into();
        instance.pages[0].nodes.push(Node {
            id: "instance".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 500, height: 500 },
            text: String::new(), style: Style::default(), asset: None, children: vec![child], layout: None,
            interactions: Vec::new(), variants: Default::default(),
            ext: NodeExt { instance: Some(designllm_contracts::InstanceRef { component_page_id: "component-page".into(), overrides: Default::default() }), ..Default::default() },
        });
        assert_image_crop_refused(
            instance,
            Vec::new(),
            &assets,
            Operation::SetCanvasImageCrop { node_id: "instance::image".into(), expected_asset_sha256: assets.canonical.sha256.clone(), crop_pixel_rect: crop },
            ErrorCode::UnsupportedOperation,
            "IMAGE_CROP_INSTANCE_UNSUPPORTED",
        );
    }

    #[test]
    fn crop_format_guard_distinguishes_static_png_from_apng() {
        let static_png = designllm_render::png_from_rgba(1, 1, &[1, 2, 3, 255]).expect("static PNG");
        assert!(!png_is_animated(&static_png).expect("static PNG chunk walk"));

        let mut apng_prefix = b"\x89PNG\r\n\x1a\n".to_vec();
        apng_prefix.extend_from_slice(&8u32.to_be_bytes());
        apng_prefix.extend_from_slice(b"acTL");
        apng_prefix.extend_from_slice(&2u32.to_be_bytes());
        apng_prefix.extend_from_slice(&0u32.to_be_bytes());
        apng_prefix.extend_from_slice(&[0; 4]);
        assert!(png_is_animated(&apng_prefix).expect("animated control chunk is detected before frame data"));
    }

    #[test]
    fn image_crop_refuses_a_plausible_header_with_undecodable_payload() {
        let complete = designllm_render::png_from_rgba(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]).expect("complete PNG");
        let idat_header = complete.windows(4).position(|chunk_type| chunk_type == b"IDAT").expect("PNG IDAT chunk") + 4;
        let truncated = complete[..idat_header].to_vec();
        assert_eq!(designllm_render::image_pixel_size(&truncated), Some((2, 1)), "the header-only probe accepts the advertised dimensions");
        assert!(!png_is_animated(&truncated).expect("IHDR and IDAT headers are complete"), "the format guard reaches IDAT without mistaking the truncated data for animation");
        assert!(designllm_render::decode_image(&truncated).is_err(), "the decoder rejects the missing PNG payload");
        let assets = replacement_assets(truncated, "image/png");
        let document = image_document(assets.canonical.clone());
        let mut saved_crop = document.clone();
        saved_crop.pages[0].nodes[0].ext.image_crop_pixel_rect = Some(PixelRect { x: 0, y: 0, width: 1, height: 1 });
        assert!(super::super::image_crop_diagnostics(&saved_crop, &assets).iter().any(|diagnostic| diagnostic.code == "IMAGE_CROP_DECODE"),
            "the Canvas preview/check path reports undecodable crop source bytes");
        assert_image_crop_refused(
            document,
            Vec::new(),
            &assets,
            image_crop_op(&assets.canonical.sha256, Some(PixelRect { x: 0, y: 0, width: 1, height: 1 })),
            ErrorCode::InvalidArgument,
            "IMAGE_CROP_DECODE",
        );
    }

    #[test]
    fn image_replacement_refuses_to_invalidate_a_saved_crop() {
        let source = designllm_render::png_from_rgba(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]).expect("source PNG");
        let original_assets = replacement_assets(source, "image/png");
        let mut document = image_document(original_assets.canonical.clone());
        document.pages[0].nodes[0].ext.image_crop_pixel_rect = Some(PixelRect { x: 1, y: 0, width: 1, height: 1 });
        let replacement_assets = replacement_assets(designllm_render::png_from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(), "image/png");
        assert_replacement_refused(
            document,
            Vec::new(),
            &replacement_assets,
            replacement_op("image", &original_assets.canonical.sha256, &replacement_assets.canonical),
            ErrorCode::InvalidArgument,
            "IMAGE_REPLACEMENT_CROP_BOUNDS",
        );
    }

    #[test]
    fn image_replacement_decodes_png_jpeg_and_bmp_and_changes_only_the_asset() {
        let original_asset = AssetRef { sha256: sha256(b"original image"), mime: "image/png".into(), bytes: 14 };
        let original = image_document(original_asset.clone());
        let before = serde_json::to_value(&original).expect("original image document");
        let png = designllm_render::png_from_rgba(1, 1, &[20, 40, 60, 255]).expect("PNG fixture");
        for (mime, bytes) in [
            ("image/png", png),
            // Test-only 1×1 JPEG generated in memory with Windows System.Drawing; no external image asset.
            ("image/jpeg", include_bytes!("../../tests/fixtures/image-replacement-1x1.jpg").to_vec()),
            ("image/bmp", tiny_bmp()),
        ] {
            let assets = replacement_assets(bytes, mime);
            let replacement = assets.canonical.clone();
            let mut document = original.clone();
            let mut links = Vec::new();
            let (changed, diagnostics) = apply(
                &mut document,
                &mut links,
                &assets,
                &[replacement_op("image", &original_asset.sha256, &replacement)],
            ).expect("a canonical decoded image can replace an existing Canvas image");
            assert_eq!(changed, vec!["image".to_string()]);
            assert!(diagnostics.is_empty());
            let after = serde_json::to_value(&document).expect("replaced image document");
            let mut expected = before["pages"][0]["nodes"][0].clone();
            expected["asset"] = serde_json::to_value(&replacement).expect("canonical asset reference");
            assert_eq!(after["pages"][0]["nodes"][0], expected, "replacement preserves node ID, bounds, alt, fit, style, and interactions");
            assert!(links.is_empty());
        }
    }

    #[test]
    fn image_replacement_refuses_stale_locked_non_image_and_figure_linked_targets_atomically() {
        let old = AssetRef { sha256: sha256(b"old image"), mime: "image/png".into(), bytes: 9 };
        let assets = replacement_assets(designllm_render::png_from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(), "image/png");
        let operation = |expected: &str| replacement_op("image", expected, &assets.canonical);

        assert_replacement_refused(
            image_document(old.clone()), Vec::new(), &assets, operation("different"),
            ErrorCode::RevisionConflict, "IMAGE_REPLACEMENT_STALE",
        );

        let mut locked = image_document(old.clone());
        locked.pages[0].nodes[0].ext.locked = Some(true);
        assert_replacement_refused(
            locked, Vec::new(), &assets, operation(&old.sha256), ErrorCode::ContentLocked, "NODE_LOCKED",
        );

        let mut wrong_kind = image_document(old.clone());
        wrong_kind.pages[0].nodes[0].kind = NodeKind::Rectangle;
        assert_replacement_refused(
            wrong_kind, Vec::new(), &assets, operation(&old.sha256), ErrorCode::InvalidArgument, "IMAGE_REPLACEMENT_TARGET",
        );

        let link = FigureLink {
            figure_id: "figure".into(),
            figure_revision: 1,
            target_node_id: "image".into(),
            representation: "png".into(),
            render_hash: "f".repeat(64),
        };
        assert_replacement_refused(
            image_document(old.clone()), vec![link], &assets, operation(&old.sha256),
            ErrorCode::UnsupportedOperation, "IMAGE_REPLACEMENT_FIGURE_LINKED",
        );
    }

    #[test]
    fn image_replacement_refuses_an_unlocked_image_under_a_locked_group() {
        let old = AssetRef { sha256: sha256(b"old image"), mime: "image/png".into(), bytes: 9 };
        let assets = replacement_assets(designllm_render::png_from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(), "image/png");
        let mut document = image_document(old.clone());
        let image = document.pages[0].nodes.pop().expect("image node");
        assert_ne!(image.ext.locked, Some(true), "the target image itself is unlocked");
        document.pages[0].nodes.push(Node {
            id: "locked-parent".into(),
            kind: NodeKind::Group,
            bounds: Rect { x: 0, y: 0, width: 1000, height: 1000 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![image],
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt { locked: Some(true), ..Default::default() },
        });
        let before = serde_json::to_value(&document).expect("before snapshot");
        let mut links = Vec::new();
        let error = apply(
            &mut document,
            &mut links,
            &assets,
            &[replacement_op("image", &old.sha256, &assets.canonical)],
        ).expect_err("a locked ancestor protects the nested image");
        assert_eq!(error.code, ErrorCode::ContentLocked);
        assert!(error.diagnostics.iter().any(|entry| entry.code == "NODE_LOCKED" && entry.node_id.as_deref() == Some("locked-parent")));
        assert_eq!(error.affected_ids, vec!["locked-parent".to_string()]);
        assert_eq!(serde_json::to_value(&document).expect("after refusal"), before, "the original asset and node remain unchanged");
        assert!(links.is_empty());
    }

    #[test]
    fn image_replacement_refuses_figure_links_and_component_source_or_instance_images() {
        use designllm_contracts::{InstanceRef, PageRole};

        let old = AssetRef { sha256: sha256(b"old image"), mime: "image/png".into(), bytes: 9 };
        let assets = replacement_assets(designllm_render::png_from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(), "image/png");

        let mut source = image_document(old.clone());
        source.pages[0].role = Some(PageRole::Component);
        assert_replacement_refused(
            source, Vec::new(), &assets, replacement_op("image", &old.sha256, &assets.canonical),
            ErrorCode::UnsupportedOperation, "IMAGE_REPLACE_COMPONENT_SOURCE_UNSUPPORTED",
        );

        let mut component_source = image_document(old.clone());
        component_source.pages[0].id = "component".into();
        component_source.pages[0].role = Some(PageRole::Component);
        let mut materialized_image = component_source.pages[0].nodes[0].clone();
        materialized_image.id = "instance::image".into();
        let instance = Node {
            id: "instance".into(),
            kind: NodeKind::Group,
            bounds: Rect { x: 0, y: 0, width: 1000, height: 1000 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![materialized_image],
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt { instance: Some(InstanceRef { component_page_id: "component".into(), overrides: Default::default() }), ..Default::default() },
        };
        let mut instance_document = component_source;
        instance_document.pages.push(Page {
            nodes: vec![instance],
            ..Page::new("regular", Size::slide())
        });
        assert_replacement_refused(
            instance_document, Vec::new(), &assets, replacement_op("instance::image", &old.sha256, &assets.canonical),
            ErrorCode::UnsupportedOperation, "IMAGE_REPLACE_INSTANCE_UNSUPPORTED",
        );
    }

    #[test]
    fn image_replacement_checks_canonical_bytes_mime_decoder_and_pixel_limit() {
        let old = AssetRef { sha256: sha256(b"old image"), mime: "image/png".into(), bytes: 9 };
        let png = designllm_render::png_from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();

        let mut tampered = replacement_assets(png.clone(), "image/png");
        *tampered.bytes.last_mut().unwrap() ^= 1;
        assert_replacement_refused(
            image_document(old.clone()), Vec::new(), &tampered,
            replacement_op("image", &old.sha256, &tampered.canonical),
            ErrorCode::IntegrityError, "IMAGE_REPLACEMENT_ASSET_HASH",
        );

        let wrong_mime = replacement_assets(png.clone(), "image/jpeg");
        assert_replacement_refused(
            image_document(old.clone()), Vec::new(), &wrong_mime,
            replacement_op("image", &old.sha256, &wrong_mime.canonical),
            ErrorCode::InvalidArgument, "IMAGE_REPLACEMENT_MIME_MISMATCH",
        );

        let unsupported = replacement_assets(b"<svg></svg>".to_vec(), "image/svg+xml");
        assert_replacement_refused(
            image_document(old.clone()), Vec::new(), &unsupported,
            replacement_op("image", &old.sha256, &unsupported.canonical),
            ErrorCode::UnsupportedOperation, "IMAGE_REPLACEMENT_FORMAT_UNSUPPORTED",
        );

        let mut truncated = png.clone();
        truncated.truncate(24);
        let truncated = replacement_assets(truncated, "image/png");
        assert_replacement_refused(
            image_document(old.clone()), Vec::new(), &truncated,
            replacement_op("image", &old.sha256, &truncated.canonical),
            ErrorCode::InvalidArgument, "IMAGE_REPLACEMENT_DECODE",
        );

        let mut oversized_png = png;
        oversized_png[16..20].copy_from_slice(&((64 * 1024 * 1024u32) + 1).to_be_bytes());
        oversized_png[20..24].copy_from_slice(&1u32.to_be_bytes());
        let oversized = replacement_assets(oversized_png, "image/png");
        assert_replacement_refused(
            image_document(old.clone()), Vec::new(), &oversized,
            replacement_op("image", &old.sha256, &oversized.canonical),
            ErrorCode::InvalidArgument, "IMAGE_REPLACEMENT_PIXEL_LIMIT",
        );
    }
}

#[cfg(test)]
#[path = "ops_gradient_clear_tests.rs"]
mod gradient_clear_tests;
