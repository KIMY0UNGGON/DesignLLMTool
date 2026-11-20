//! Semantic operations on a FlowDocument (all-or-nothing).
use crate::text::{validate_edit_text, LogicalParagraph};
use crate::tree::*;
use designllm_contracts::*;
use std::collections::BTreeSet;

/// Apply operations to the semantic model. Either every operation applies or the document is
/// left unchanged and the first error is returned. Returns changed ids (first occurrence order).
///
/// Supported: ReplaceText, SetText (paragraph or run), SetStyle (run, paragraph, cell, table,
/// image), SetTextStyleRange, SetNamedStyle, SetParagraphFormat, SetPageSetup,
/// SetTableDimensions, InsertFlowBlock, RemoveNode (blocks and runs), SetGeometry (image size).
/// Other operations are rejected with UNSUPPORTED_OPERATION.
pub fn apply_flow_operations(document: &mut FlowDocument, operations: &[Operation]) -> DltResult<Vec<Id>> {
    crate::guard::refuse_v12_operations("Flow documents", operations)?;
    let mut work = document.clone();
    let mut changed = Changed::default();
    for (index, op) in operations.iter().enumerate() {
        apply_one(&mut work, op, &mut changed).map_err(|mut e| {
            e.message = format!("operation {index}: {}", e.message);
            e
        })?;
    }
    // A proportional pitch follows the font sizes that the operations left (task 25).
    for_each_paragraph_mut(&mut work, crate::line_spacing::refresh_pitch);
    *document = work;
    Ok(changed.ids)
}

#[derive(Default)]
struct Changed { ids: Vec<Id>, seen: BTreeSet<Id> }
impl Changed {
    fn add(&mut self, id: &str) { if self.seen.insert(id.to_string()) { self.ids.push(id.to_string()); } }
}

fn not_found(kind: &str, id: &str) -> DltError {
    let mut e = DltError::new(ErrorCode::NotFound, format!("{kind} {id} was not found in the flow document"));
    e.affected_ids = vec![id.to_string()];
    e
}

fn apply_one(doc: &mut FlowDocument, op: &Operation, changed: &mut Changed) -> DltResult<()> {
    match op {
        Operation::ReplaceText { range, text } => replace_text(doc, range, text, changed),
        Operation::SetText { node_id, text } => set_text(doc, node_id, text, changed),
        Operation::SetStyle { node_ids, patch } => {
            if node_ids.is_empty() { return Err(DltError::invalid("set_style needs at least one node id")); }
            for id in node_ids { set_style(doc, id, patch, changed)?; }
            Ok(())
        }
        Operation::SetTextStyleRange { range, patch } => set_text_style_range(doc, range, patch, changed),
        Operation::SetNamedStyle { name, patch, clear_overrides } => set_named_style(doc, name, patch, *clear_overrides, changed),
        Operation::SetParagraphFormat { paragraph_id, format } => {
            validate_paragraph_format(format)?;
            let p = find_paragraph_mut(doc, paragraph_id).ok_or_else(|| not_found("Paragraph", paragraph_id))?;
            let old = std::mem::replace(&mut p.format, format.clone());
            // Without a percentage the pitch is the request: 0 is single spacing, any other pitch is exact. With one the
            // percentage decides, and the pitch is derived from it after the operations.
            match (format.line_spacing_percent, old.line_spacing_percent) {
                (Some(new), Some(before)) if new == before => p.format.line_spacing = old.line_spacing,
                (Some(_), _) => p.format.line_spacing = 0,
                (None, _) => {}
            }
            changed.add(paragraph_id);
            Ok(())
        }
        Operation::SetPageSetup { section_id, setup } => {
            if let Some(problem) = page_setup_problem(setup) { return Err(DltError::invalid(problem)); }
            let section = doc.sections.iter_mut().find(|s| &s.id == section_id).ok_or_else(|| not_found("Section", section_id))?;
            section.setup = setup.clone();
            changed.add(section_id);
            Ok(())
        }
        Operation::SetTableDimensions { table_id, column_widths } => {
            let table = find_table_mut(doc, table_id).ok_or_else(|| not_found("Table", table_id))?;
            if column_widths.len() != table.column_widths.len() {
                return Err(DltError::invalid(format!("Table {table_id} has {} grid columns, {} widths were given", table.column_widths.len(), column_widths.len())));
            }
            if column_widths.iter().any(|w| *w <= 0) { return Err(DltError::invalid("Column widths must be positive")); }
            table.column_widths = column_widths.clone();
            changed.add(table_id);
            Ok(())
        }
        Operation::InsertFlowBlock { section_id, index, block } => insert_block(doc, section_id, *index, block, changed),
        Operation::RemoveNode { node_id } => remove_node(doc, node_id, changed),
        Operation::SetGeometry { node_id, bounds } => {
            let node = find_image_mut(doc, node_id).ok_or_else(|| not_found("Image", node_id))?;
            if bounds.x != 0 || bounds.y != 0 {
                return Err(DltError::unsupported("Flow images are positioned by layout; only width and height can be set (x and y must be 0)"));
            }
            if bounds.width <= 0 || bounds.height <= 0 || !(Size { width: bounds.width, height: bounds.height }).valid() {
                return Err(DltError::invalid("Image size must be positive and within 10 m"));
            }
            node.bounds.width = bounds.width;
            node.bounds.height = bounds.height;
            changed.add(node_id);
            Ok(())
        }
        Operation::ReplaceImageAsset { node_id, .. } => {
            let mut error = DltError::unsupported("replace_image_asset is a Canvas-only operation and is not supported for Flow documents");
            error.affected_ids.push(node_id.clone());
            Err(error)
        }
        other => Err(DltError::unsupported(format!("Operation {} is not applicable to flow documents", other.name()))),
    }
}

#[cfg(test)]
mod canvas_only_operation_tests {
    use super::*;

    #[test]
    fn canvas_page_sections_are_rejected_without_changing_flow_data() {
        let mut document = FlowDocument::default();
        let before = serde_json::to_value(&document).unwrap();
        let error = apply_flow_operations(&mut document, &[Operation::SetCanvasPageSections {
            expected: Vec::new(), sections: Vec::new(),
        }]).expect_err("Canvas page sections do not apply to Flow documents");
        assert_eq!(error.code, ErrorCode::UnsupportedOperation);
        assert!(error.message.contains("set_canvas_page_sections"));
        assert_eq!(serde_json::to_value(&document).unwrap(), before);
    }

    #[test]
    fn canvas_image_crop_is_rejected_without_changing_flow_data() {
        let mut document = FlowDocument::default();
        let before = serde_json::to_value(&document).unwrap();
        let error = apply_flow_operations(&mut document, &[Operation::SetCanvasImageCrop {
            node_id: "image".into(), expected_asset_sha256: "sha".into(), crop_pixel_rect: None,
        }]).expect_err("Canvas image crop does not apply to Flow documents");
        assert_eq!(error.code, ErrorCode::UnsupportedOperation);
        assert!(error.message.contains("set_canvas_image_crop"));
        assert_eq!(serde_json::to_value(&document).unwrap(), before);
    }
}

/// Validate a paragraph format value.
pub fn validate_paragraph_format(format: &ParagraphFormat) -> DltResult<()> {
    if !matches!(format.align.as_str(), "left" | "center" | "right" | "justify") {
        return Err(DltError::invalid(format!("Paragraph align must be left, center, right or justify, got {}", format.align)));
    }
    if format.space_before < 0 || format.space_after < 0 || format.line_spacing < 0 {
        return Err(DltError::invalid("Paragraph spacing values must not be negative"));
    }
    let limit = 1_000 * EMU_PER_MM;
    if [format.space_before, format.space_after, format.line_spacing, format.indent.abs()].iter().any(|v| *v > limit) {
        return Err(DltError::invalid("Paragraph spacing or indent exceeds 1 m"));
    }
    crate::line_spacing::validate_percent(format)
}

pub(crate) fn validate_style_patch(patch: &StylePatch) -> DltResult<()> {
    if let Some(size) = patch.font_size {
        if size <= 0 || size > 1_000 * EMU_PER_PT { return Err(DltError::invalid("Font size must be positive and at most 1000 pt")); }
    }
    if let Some(width) = patch.stroke_width { if width < 0 { return Err(DltError::invalid("Stroke width must not be negative")); } }
    if let Some(family) = &patch.font_family { if family.trim().is_empty() { return Err(DltError::invalid("Font family must not be empty")); } }
    for color in [&patch.color, &patch.fill, &patch.stroke].into_iter().flatten() {
        if !valid_color(color) { return Err(DltError::invalid(format!("Unsupported color value {color}"))); }
    }
    if let Some(align) = &patch.align {
        if !matches!(align.as_str(), "left" | "center" | "right" | "justify") { return Err(DltError::invalid(format!("Unsupported align value {align}"))); }
    }
    Ok(())
}

/// Colors accepted by flow edits: #RGB, #RRGGBB, #RRGGBBAA, none, transparent.
pub fn valid_color(value: &str) -> bool {
    if matches!(value, "none" | "transparent") { return true; }
    let Some(hex) = value.strip_prefix('#') else { return false };
    matches!(hex.len(), 3 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

fn replace_text(doc: &mut FlowDocument, range: &TextRange, text: &str, changed: &mut Changed) -> DltResult<()> {
    validate_edit_text(text)?;
    let p = find_paragraph_mut(doc, &range.paragraph_id).ok_or_else(|| not_found("Paragraph", &range.paragraph_id))?;
    let lp = LogicalParagraph::new(p);
    let spans = lp.text_spans(range.start, range.end)?;
    if spans.is_empty() && text.is_empty() { return Ok(()); }
    let nonempty_before: BTreeSet<Id> = p.runs.iter().filter(|r| !r.text.is_empty()).map(|r| r.id.clone()).collect();
    let target = match spans.first() {
        Some(first) => (first.run, first.bytes.start),
        None => match lp.insertion_point(range.start) {
            Some(point) => point,
            None => {
                // Only protected atoms (or nothing) around: add a new unprotected run.
                let at = if range.start == 0 { 0 } else {
                    lp.graphemes[range.start as usize - 1].pieces.last().map(|p| p.run + 1).unwrap_or(0)
                };
                let style = p.runs.get(at.saturating_sub(1)).or_else(|| p.runs.get(at)).map(|r| r.style.clone()).unwrap_or_default();
                let run = TextRun { style, ..TextRun::new(new_id(), String::new()) };
                changed.add(&run.id);
                p.runs.insert(at, run);
                (at, 0)
            }
        },
    };
    for piece in spans.iter().rev() {
        let run = &mut p.runs[piece.run];
        run.text.replace_range(piece.bytes.clone(), "");
        changed.add(&run.id.clone());
    }
    let run = &mut p.runs[target.0];
    run.text.insert_str(target.1, text);
    changed.add(&run.id.clone());
    // Drop runs that this edit emptied, keeping the insertion run and at least one run.
    let target_id = p.runs[target.0].id.clone();
    let emptied: BTreeSet<Id> = p.runs.iter()
        .filter(|r| r.text.is_empty() && !r.protected && nonempty_before.contains(&r.id) && r.id != target_id)
        .map(|r| r.id.clone()).collect();
    if !emptied.is_empty() { p.runs.retain(|r| !emptied.contains(&r.id)); }
    changed.add(&range.paragraph_id);
    Ok(())
}

fn set_text(doc: &mut FlowDocument, node_id: &str, text: &str, changed: &mut Changed) -> DltResult<()> {
    validate_edit_text(text)?;
    if let Some(p) = find_paragraph_mut(doc, node_id) {
        if p.runs.iter().any(|r| r.protected) {
            return Err(DltError::unsupported(format!("Paragraph {node_id} contains protected inline objects; use replace_text on its text ranges")));
        }
        for run in &p.runs { changed.add(&run.id); }
        if p.runs.is_empty() {
            p.runs.push(TextRun::new(new_id(), String::new()));
        }
        p.runs.truncate(1);
        p.runs[0].text = text.to_string();
        changed.add(&p.runs[0].id.clone());
        changed.add(node_id);
        return Ok(());
    }
    if let Some((p, index)) = find_run_mut(doc, node_id) {
        if p.runs[index].protected { return Err(DltError::unsupported(format!("Run {node_id} is a protected inline object"))); }
        p.runs[index].text = text.to_string();
        changed.add(node_id);
        changed.add(&p.id.clone());
        return Ok(());
    }
    Err(DltError::unsupported(format!("set_text targets a paragraph or text run; {node_id} is neither")))
}

fn set_style(doc: &mut FlowDocument, id: &str, patch: &StylePatch, changed: &mut Changed) -> DltResult<()> {
    validate_style_patch(patch)?;
    if let Some((p, index)) = find_run_mut(doc, id) {
        patch.apply(&mut p.runs[index].style);
        changed.add(id);
        return Ok(());
    }
    if let Some(p) = find_paragraph_mut(doc, id) {
        for run in p.runs.iter_mut() { patch.apply(&mut run.style); changed.add(&run.id); }
        if let Some(align) = &patch.align { p.format.align = align.clone(); }
        changed.add(id);
        return Ok(());
    }
    if let Some(cell) = find_cell_mut(doc, id) {
        let mut ids = Vec::new();
        style_blocks(&mut cell.blocks, patch, &mut ids);
        for i in ids { changed.add(&i); }
        changed.add(id);
        return Ok(());
    }
    if let Some(table) = find_table_mut(doc, id) {
        patch.apply(&mut table.style);
        changed.add(id);
        return Ok(());
    }
    if let Some(node) = find_image_mut(doc, id) {
        patch.apply(&mut node.style);
        changed.add(id);
        return Ok(());
    }
    Err(not_found("Node", id))
}

fn style_blocks(blocks: &mut [FlowBlock], patch: &StylePatch, ids: &mut Vec<Id>) {
    for block in blocks.iter_mut() {
        match block {
            FlowBlock::Paragraph(p) => {
                for run in p.runs.iter_mut() { patch.apply(&mut run.style); ids.push(run.id.clone()); }
                if let Some(align) = &patch.align { p.format.align = align.clone(); }
                ids.push(p.id.clone());
            }
            FlowBlock::Table(t) => for row in t.rows.iter_mut() { for cell in row.iter_mut() { style_blocks(&mut cell.blocks, patch, ids); } },
            _ => {}
        }
    }
}

/// Split the run that contains logical boundary `index` strictly inside it. The left part
/// keeps the id; the right part gets a new id. Returns the new run id when a split happened.
fn split_at(p: &mut Paragraph, index: u32) -> DltResult<Option<Id>> {
    let lp = LogicalParagraph::new(p);
    lp.check_range(index, index)?;
    if index == 0 || index == lp.len() { return Ok(None); }
    let next = &lp.graphemes[index as usize];
    if next.protected { return Ok(None); }
    let piece = &next.pieces[0];
    if piece.bytes.start == 0 { return Ok(None); }
    let run = &p.runs[piece.run];
    let right = TextRun { style: run.style.clone(), ..TextRun::new(new_id(), run.text[piece.bytes.start..].to_string()) };
    let id = right.id.clone();
    p.runs[piece.run].text.truncate(piece.bytes.start);
    p.runs.insert(piece.run + 1, right);
    Ok(Some(id))
}

fn set_text_style_range(doc: &mut FlowDocument, range: &TextRange, patch: &StylePatch, changed: &mut Changed) -> DltResult<()> {
    validate_style_patch(patch)?;
    let p = find_paragraph_mut(doc, &range.paragraph_id).ok_or_else(|| not_found("Paragraph", &range.paragraph_id))?;
    LogicalParagraph::new(p).check_range(range.start, range.end)?;
    if range.start == range.end { return Err(DltError::invalid("set_text_style_range needs a non-empty range")); }
    for id in [split_at(p, range.end)?, split_at(p, range.start)?].into_iter().flatten() { changed.add(&id); }
    let lp = LogicalParagraph::new(p);
    let runs: BTreeSet<usize> = lp.all_spans(range.start, range.end)?.into_iter().map(|s| s.run).collect();
    for index in runs {
        patch.apply(&mut p.runs[index].style);
        changed.add(&p.runs[index].id.clone());
    }
    changed.add(&range.paragraph_id);
    Ok(())
}

fn set_named_style(doc: &mut FlowDocument, name: &str, patch: &StylePatch, clear_overrides: bool, changed: &mut Changed) -> DltResult<()> {
    validate_style_patch(patch)?;
    let old = doc.styles.get(name).cloned().ok_or_else(|| not_found("Named style", name))?;
    let mut new = old.clone();
    patch.apply(&mut new);
    let mut touched = Vec::new();
    for_each_paragraph_mut(doc, |p| {
        if p.style_ref.as_deref() != Some(name) { return; }
        for run in p.runs.iter_mut() {
            if patch_inherited(&mut run.style, &old, &new, patch, clear_overrides) { touched.push(run.id.clone()); }
        }
        if let Some(align) = &patch.align {
            if clear_overrides || p.format.align == old.align { p.format.align = align.clone(); }
        }
        touched.push(p.id.clone());
    });
    doc.styles.insert(name.to_string(), new);
    for id in touched { changed.add(&id); }
    Ok(())
}

/// Apply patched fields to a run when the run inherits them (value equals the old named style)
/// or when overrides are cleared. Returns true if anything changed.
fn patch_inherited(style: &mut Style, old: &Style, new: &Style, patch: &StylePatch, clear: bool) -> bool {
    let before = style.clone();
    macro_rules! field {
        ($f:ident) => { if patch.$f.is_some() && (clear || style.$f == old.$f) { style.$f = new.$f.clone(); } };
    }
    field!(font_family); field!(font_size); field!(color); field!(fill); field!(stroke);
    field!(stroke_width); field!(bold); field!(italic); field!(align);
    *style != before
}

fn insert_block(doc: &mut FlowDocument, section_id: &str, index: u32, block: &FlowBlock, changed: &mut Changed) -> DltResult<()> {
    let existing: BTreeSet<Id> = collect_ids(doc).into_iter().collect();
    let mut new_ids = Vec::new();
    collect_block_ids(std::slice::from_ref(block), &mut new_ids);
    let mut fresh = BTreeSet::new();
    for id in &new_ids {
        if id.is_empty() { return Err(DltError::invalid("Inserted block contains an empty id")); }
        if existing.contains(id) || !fresh.insert(id.clone()) {
            let mut e = DltError::new(ErrorCode::AlreadyExists, format!("Id {id} already exists"));
            e.affected_ids = vec![id.clone()];
            return Err(e);
        }
    }
    validate_new_block(block)?;
    let section = doc.sections.iter_mut().find(|s| s.id == section_id).ok_or_else(|| not_found("Section", section_id))?;
    if index as usize > section.blocks.len() {
        return Err(DltError::invalid(format!("Insert index {index} is past the {} blocks of section {section_id}", section.blocks.len())));
    }
    section.blocks.insert(index as usize, block.clone());
    for id in new_ids { changed.add(&id); }
    changed.add(section_id);
    Ok(())
}

fn validate_new_block(block: &FlowBlock) -> DltResult<()> {
    match block {
        FlowBlock::Paragraph(p) => {
            validate_paragraph_format(&p.format)?;
            for run in &p.runs { if !run.protected { validate_edit_text(&run.text)?; } }
            Ok(())
        }
        FlowBlock::Table(t) => {
            if t.rows.is_empty() || t.column_widths.is_empty() { return Err(DltError::invalid("A new table needs at least one row and one column width")); }
            if let Some(problem) = table_problem(t) { return Err(DltError::invalid(problem)); }
            for row in &t.rows { for cell in row { for b in &cell.blocks { validate_new_block(b)?; } } }
            Ok(())
        }
        FlowBlock::Image(node) => {
            if node.kind != NodeKind::Image {
                let mut e = DltError::unsupported(format!("Flow image blocks store image nodes only, not kind {:?}", node.kind));
                e.affected_ids = vec![node.id.clone()];
                return Err(e);
            }
            if node.asset.is_none() { return Err(DltError::invalid("A new image block needs an asset")); }
            if node.bounds.width <= 0 || node.bounds.height <= 0 { return Err(DltError::invalid("A new image block needs a positive size")); }
            // Canvas node fields that a flow image has no use for are refused, not kept unseen (R09).
            let unused: Vec<&str> = [("children", !node.children.is_empty()), ("interactions", !node.interactions.is_empty()), ("variants", !node.variants.is_empty()), ("layout", node.layout.is_some())]
                .into_iter().filter(|(_, set)| *set).map(|(name, _)| name).collect();
            if !unused.is_empty() {
                let mut e = DltError::unsupported(format!("Flow image blocks do not use {}", unused.join(", ")));
                e.affected_ids = vec![node.id.clone()];
                return Err(e);
            }
            Ok(())
        }
        FlowBlock::Opaque { .. } => Err(DltError::unsupported("Opaque blocks preserve imported content and cannot be inserted")),
    }
}

fn remove_node(doc: &mut FlowDocument, node_id: &str, changed: &mut Changed) -> DltResult<()> {
    #[derive(PartialEq)]
    enum Kind { Run, Block, Section, Cell, Missing }
    let kind = match find_node(doc, node_id) {
        Some(FlowNode::Run { .. }) => Kind::Run,
        Some(FlowNode::Paragraph(_)) | Some(FlowNode::Table(_)) | Some(FlowNode::Image(_)) | Some(FlowNode::Opaque { .. }) => Kind::Block,
        Some(FlowNode::Section(_)) => Kind::Section,
        Some(FlowNode::Cell { .. }) => Kind::Cell,
        None => Kind::Missing,
    };
    match kind {
        Kind::Run => {
            let (p, index) = find_run_mut(doc, node_id).ok_or_else(|| not_found("Run", node_id))?;
            p.runs.remove(index);
            let pid = p.id.clone();
            changed.add(node_id);
            changed.add(&pid);
            Ok(())
        }
        Kind::Block => {
            let removed = remove_block(doc, node_id).ok_or_else(|| not_found("Block", node_id))?;
            let mut ids = Vec::new();
            collect_block_ids(std::slice::from_ref(&removed), &mut ids);
            for id in ids { changed.add(&id); }
            Ok(())
        }
        Kind::Section => Err(DltError::unsupported("Removing a whole section is not supported")),
        Kind::Cell => Err(DltError::unsupported("Removing a single table cell changes the table grid and is not supported")),
        Kind::Missing => Err(not_found("Node", node_id)),
    }
}
