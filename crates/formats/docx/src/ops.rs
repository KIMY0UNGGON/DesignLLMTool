//! Change operations on the DOCX model (the prepare step).
//!
//! The steps run on a copy of the model. Native records (origin child index, direct formatting,
//! character style, markers) are updated with every model change, so the writer can always rebuild the
//! XML of an edited node from the original bytes. Values are quantized to the native grid before they
//! are stored, and the stored value is the one the file will contain.

use std::collections::BTreeSet;

use designllm_contracts::{
    new_id, sha256, AssetRef, AssetStore, DltError, DltResult, ErrorCode, FigureLink, FlowBlock, FlowDocument, Id, Operation,
    PageSetup, Paragraph, ParagraphFormat, Rect, Size, Style, StylePatch, TextRange, TextRun, EMU_PER_PT,
};
use designllm_flow::tree::{
    find_cell_mut, find_image_mut, find_node, find_paragraph_mut, find_run_mut, find_table_mut, page_setup_problem, FlowNode,
};
use designllm_contracts::media_header::{self, FIGURE_REPRESENTATIONS};
use designllm_flow::{validate_edit_text, LogicalParagraph};

use crate::diag;
use crate::model::{self, Rec, Records};
use crate::png;
use crate::props::{ParaProps, RunProps, PARA_NAMES};
use crate::styles::{Overrides, StyleOverride, StyleTable};
use crate::units;

/// Mutable native state of one artifact during a change.
#[derive(Clone)]
pub struct State {
    pub document: FlowDocument,
    pub records: Records,
    pub overrides: Overrides,
    /// Figure links of the document, sorted by target picture id.
    pub figure_links: Vec<FigureLink>,
}

/// Result of a change: changed ids in first-touch order and the native rounding diagnostics.
pub struct Outcome {
    pub changed: Vec<Id>,
    pub diagnostics: Vec<designllm_contracts::Diagnostic>,
}

/// Applies `operations` to `state` (all or nothing). `base_styles` is the original styles.xml table.
pub fn apply_operations(
    state: &mut State,
    operations: &[Operation],
    base_styles: &StyleTable,
    assets: &dyn AssetStore,
) -> DltResult<Outcome> {
    // P0 refusals run before the first copy or change (docs/P0_CONTRACT_V1_2.md §4). prepare_change calls the same
    // guard first; this call keeps the guarantee for any other caller of this function.
    crate::guard::check_operations(operations)?;
    let mut work = state.clone();
    let mut changed = Changed::default();
    let mut diagnostics = Vec::new();
    let mut styles = base_styles.clone();
    styles.apply_overrides(&work.overrides);
    for (index, op) in operations.iter().enumerate() {
        apply_one(&mut work, op, &mut styles, base_styles, assets, &mut changed, &mut diagnostics).map_err(|mut e| {
            e.message = format!("operation {index}: {}", e.message);
            e
        })?;
    }
    // Effective values follow the (possibly edited) style table; applied direct values are kept.
    let refreshed = refresh(&mut work, &styles);
    for id in refreshed {
        changed.add(&id);
    }
    // Removals drop their links (see `remove_node`). This check only guards the invariant: a link names a picture.
    for link in &work.figure_links {
        if !matches!(find_node(&work.document, &link.target_node_id), Some(FlowNode::Image(_))) {
            return Err(DltError::new(
                ErrorCode::IntegrityError,
                format!("figure link target {} is not a picture of the document", link.target_node_id),
            ));
        }
    }
    *state = work;
    Ok(Outcome { changed: changed.ids, diagnostics })
}

#[derive(Default)]
struct Changed {
    ids: Vec<Id>,
    seen: BTreeSet<Id>,
}

impl Changed {
    fn add(&mut self, id: &str) {
        if self.seen.insert(id.to_string()) {
            self.ids.push(id.to_string());
        }
    }
}

fn not_found(kind: &str, id: &str) -> DltError {
    let mut error = DltError::new(ErrorCode::NotFound, format!("{kind} {id} was not found in the document"));
    error.affected_ids = vec![id.to_string()];
    error
}

fn unsupported_with(code: &str, message: impl Into<String>, id: Option<&Id>) -> DltError {
    let mut error = DltError::unsupported(message);
    error.diagnostics.push(diag::blocking(code, error.message.clone(), id));
    if let Some(id) = id {
        error.affected_ids = vec![id.clone()];
    }
    error
}

/// DOCX does not map `line_spacing_percent` (task 25; its own auto multiples are negative `line_spacing`), so a format
/// that carries it is refused by name instead of being dropped.
fn percent_refusal(format: &ParagraphFormat, id: &str) -> DltResult<()> {
    match format.line_spacing_percent {
        Some(_) => Err(unsupported_with(diag::UNSUPPORTED_FIELD, "line_spacing_percent is not supported for DOCX paragraphs; give line_spacing instead", Some(&id.to_string()))),
        None => Ok(()),
    }
}

/// A page break does not start a page inside a table cell, so DOCX refuses turning one on there (HWP refuses the same request).
fn cell_page_break_refusal(id: &str) -> DltError {
    unsupported_with(diag::UNSUPPORTED_FIELD, "A page break cannot be set on a paragraph inside a table cell in DOCX", Some(&id.to_string()))
}

fn apply_one(
    work: &mut State,
    op: &Operation,
    styles: &mut StyleTable,
    base_styles: &StyleTable,
    assets: &dyn AssetStore,
    changed: &mut Changed,
    diagnostics: &mut Vec<designllm_contracts::Diagnostic>,
) -> DltResult<()> {
    match op {
        Operation::ReplaceText { range, text } => replace_text(work, range, text, changed),
        Operation::SetText { node_id, text } => set_text(work, node_id, text, changed),
        Operation::SetStyle { node_ids, patch } => {
            if node_ids.is_empty() {
                return Err(DltError::invalid("set_style needs at least one node id"));
            }
            for id in node_ids {
                set_style_node(work, id, patch, changed, diagnostics)?;
            }
            Ok(())
        }
        Operation::SetTextStyleRange { range, patch } => set_text_style_range(work, range, patch, changed, diagnostics),
        Operation::SetNamedStyle { name, patch, clear_overrides } => {
            set_named_style(work, base_styles, styles, name, patch, *clear_overrides, changed)
        }
        Operation::SetGeometry { node_id, bounds } => {
            let node = find_image_mut(&mut work.document, node_id).ok_or_else(|| not_found("Image", node_id))?;
            if bounds.x != 0 || bounds.y != 0 {
                return Err(DltError::unsupported("Inline DOCX pictures are positioned by the text flow; only width and height can change"));
            }
            if bounds.width <= 0 || bounds.height <= 0 || !(Size { width: bounds.width, height: bounds.height }).valid() {
                return Err(DltError::invalid("Picture size must be positive and within 10 m"));
            }
            node.bounds.width = bounds.width;
            node.bounds.height = bounds.height;
            changed.add(node_id);
            Ok(())
        }
        Operation::SetParagraphFormat { paragraph_id, format } => {
            set_paragraph_format(work, paragraph_id, format, changed, diagnostics)
        }
        Operation::SetPageSetup { section_id, setup } => set_page_setup(work, section_id, setup, changed, diagnostics),
        Operation::SetTableDimensions { table_id, column_widths } => {
            set_table_dimensions(work, table_id, column_widths, changed, diagnostics)
        }
        Operation::InsertFlowBlock { section_id, index, block } => {
            insert_block(work, section_id, *index, block, styles, assets, changed, diagnostics)
        }
        Operation::RemoveNode { node_id } => remove_node(work, node_id, changed),
        Operation::UpdateFigureLink { link } => update_figure_link(work, link, assets, changed),
        Operation::ReplaceImageAsset { node_id, .. } => Err(unsupported_with(
            diag::PROTECTED_INLINE,
            format!("replace_image_asset on {node_id} is a Canvas-only operation and is not supported for DOCX documents"),
            Some(node_id),
        )),
        other => {
            let name = other.name();
            Err(unsupported_with(
                diag::PROTECTED_INLINE,
                format!("operation {name} is not supported for DOCX documents"),
                None,
            ))
        }
    }
}

// ----------------------------------------------------------------------------------------------
// Record helpers

fn run_rec_mut<'a>(records: &'a mut Records, id: &str) -> DltResult<(&'a mut BTreeSet<String>, &'a mut Option<String>, &'a mut Vec<usize>, &'a mut Option<usize>)> {
    match records.get_mut(id) {
        Some(Rec::Run { direct, rstyle, lead, origin, .. }) => Ok((direct, rstyle, lead, origin)),
        _ => Err(DltError::new(ErrorCode::IntegrityError, format!("run {id} has no native record"))),
    }
}

fn para_rec_mut<'a>(records: &'a mut Records, id: &str) -> DltResult<(&'a mut BTreeSet<String>, &'a mut Vec<usize>)> {
    match records.get_mut(id) {
        Some(Rec::Paragraph { direct, tail, .. }) => Ok((direct, tail)),
        _ => Err(DltError::new(ErrorCode::IntegrityError, format!("paragraph {id} has no native record"))),
    }
}

fn run_record(records: &Records, id: &str) -> Option<(BTreeSet<String>, Option<String>, Option<usize>, bool)> {
    match records.get(id) {
        Some(Rec::Run { direct, rstyle, origin, atom, .. }) => Some((direct.clone(), rstyle.clone(), *origin, *atom)),
        _ => None,
    }
}

/// Removes run `index` from `p`. Its leading markers move to the next run, or to the paragraph tail.
fn drop_run(p: &mut Paragraph, records: &mut Records, index: usize) -> DltResult<()> {
    let removed = p.runs.remove(index);
    let lead = match records.remove(&removed.id) {
        Some(Rec::Run { lead, .. }) => lead,
        _ => Vec::new(),
    };
    if lead.is_empty() {
        return Ok(());
    }
    if let Some(next) = p.runs.get(index) {
        let next_id = next.id.clone();
        if let Some(Rec::Run { lead: next_lead, .. }) = records.get_mut(&next_id) {
            let mut merged = lead;
            merged.append(next_lead);
            *next_lead = merged;
        }
    } else {
        let (_, tail) = para_rec_mut(records, &p.id)?;
        let mut merged = lead;
        merged.append(tail);
        *tail = merged;
    }
    Ok(())
}

/// Splits the run that begins at grapheme boundary `index` (strictly inside a run). The right part gets a
/// new id and the same origin, character style, and direct formatting.
fn split_at(p: &mut Paragraph, records: &mut Records, index: u32) -> DltResult<()> {
    let lp = LogicalParagraph::new(p);
    lp.check_range(index, index)?;
    if index == 0 || index == lp.len() {
        return Ok(());
    }
    let grapheme = &lp.graphemes[index as usize];
    if grapheme.protected {
        return Ok(());
    }
    let piece = match grapheme.pieces.first() {
        Some(piece) => piece.clone(),
        None => return Ok(()),
    };
    if piece.bytes.start == 0 {
        return Ok(());
    }
    drop(lp);
    let run = &mut p.runs[piece.run];
    let right_text = run.text[piece.bytes.start..].to_string();
    run.text.truncate(piece.bytes.start);
    let left_id = run.id.clone();
    let right_style = run.style.clone();
    let (direct, rstyle, origin, _) = run_record(records, &left_id)
        .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, format!("run {left_id} has no native record")))?;
    let right_id = new_id();
    p.runs.insert(piece.run + 1, TextRun { style: right_style, ..TextRun::new(right_id.clone(), right_text) });
    records.insert(right_id, Rec::Run { origin, atom: false, direct, rstyle, lead: Vec::new() });
    Ok(())
}

// ----------------------------------------------------------------------------------------------
// Text

fn replace_text(work: &mut State, range: &TextRange, text: &str, changed: &mut Changed) -> DltResult<()> {
    validate_edit_text(text)?;
    let p = find_paragraph_mut(&mut work.document, &range.paragraph_id)
        .ok_or_else(|| not_found("Paragraph", &range.paragraph_id))?;
    let (spans, target, new_run_at) = {
        let lp = LogicalParagraph::new(p);
        lp.check_range(range.start, range.end).map_err(|e| e)?;
        if lp.range_has_protected(range.start, range.end) {
            return Err(unsupported_with(
                diag::PROTECTED_INLINE,
                "the range contains a protected inline object; text edits cannot change it",
                Some(&range.paragraph_id),
            ));
        }
        let spans = lp.text_spans(range.start, range.end)?;
        if spans.is_empty() && text.is_empty() {
            return Ok(());
        }
        match spans.first() {
            Some(first) => (spans.clone(), (first.run, first.bytes.start), None),
            None => match lp.insertion_point(range.start) {
                Some(point) => (spans.clone(), point, None),
                None => {
                    // Only protected atoms (or nothing) next to the position: a new run is needed.
                    let at = if range.start == 0 {
                        0
                    } else {
                        lp.graphemes[range.start as usize - 1].pieces.last().map(|piece| piece.run + 1).unwrap_or(0)
                    };
                    (spans.clone(), (at, 0), Some(at))
                }
            },
        }
    };
    let nonempty_before: BTreeSet<Id> = p.runs.iter().filter(|r| !r.protected && !r.text.is_empty()).map(|r| r.id.clone()).collect();
    if let Some(at) = new_run_at {
        // The new run inherits the character style and direct formatting of its neighbour run.
        let neighbour = if at > 0 { at - 1 } else { 0 };
        let (style, neighbour_record) = match p.runs.get(neighbour) {
            Some(r) if !r.protected => (r.style.clone(), run_record(&work.records, &r.id)),
            Some(r) => (Style { ..r.style.clone() }, None),
            None => (Style::default(), None),
        };
        let (direct, rstyle, origin) = match neighbour_record {
            Some((direct, rstyle, origin, false)) => (direct, rstyle, origin),
            _ => (BTreeSet::new(), None, None),
        };
        let run_id = new_id();
        p.runs.insert(at, TextRun { style, ..TextRun::new(run_id.clone(), String::new()) });
        work.records.insert(run_id.clone(), Rec::Run { origin, atom: false, direct, rstyle, lead: Vec::new() });
        changed.add(&run_id);
    }
    for piece in spans.iter().rev() {
        let run = &mut p.runs[piece.run];
        run.text.replace_range(piece.bytes.clone(), "");
    }
    let (target_run, target_byte) = target;
    let target_id = {
        let run = &mut p.runs[target_run];
        run.text.insert_str(target_byte, text);
        run.id.clone()
    };
    changed.add(&target_id);
    for piece in &spans {
        let id = p.runs[piece.run].id.clone();
        changed.add(&id);
    }
    // Remove runs that this edit emptied (not the target, not protected, not already empty).
    let emptied: Vec<usize> = p
        .runs
        .iter()
        .enumerate()
        .filter(|(i, r)| !r.protected && r.text.is_empty() && nonempty_before.contains(&r.id) && *i != target_run)
        .map(|(i, _)| i)
        .collect();
    let mut removed_ids = Vec::new();
    for index in emptied.into_iter().rev() {
        removed_ids.push(p.runs[index].id.clone());
        drop_run(p, &mut work.records, index)?;
    }
    let paragraph_id = p.id.clone();
    for id in removed_ids {
        changed.add(&id);
    }
    changed.add(&paragraph_id);
    Ok(())
}

fn set_text(work: &mut State, node_id: &str, text: &str, changed: &mut Changed) -> DltResult<()> {
    validate_edit_text(text)?;
    let p = find_paragraph_mut(&mut work.document, node_id)
        .ok_or_else(|| unsupported_with(diag::PROTECTED_INLINE, format!("set_text targets a paragraph; {node_id} is not one"), Some(&node_id.to_string())))?;
    if p.runs.iter().any(|r| r.protected) {
        // Whole-paragraph replacement would delete the protected inline content (the G0 failure mode).
        return Err(unsupported_with(
            diag::PROTECTED_INLINE,
            format!("paragraph {node_id} contains protected inline content; replace a text range that leaves it in place"),
            Some(&node_id.to_string()),
        ));
    }
    let paragraph_id = p.id.clone();
    let mut lead_markers: Vec<usize> = Vec::new();
    let mut removed = Vec::new();
    for run in p.runs.iter().skip(1) {
        removed.push(run.id.clone());
    }
    for id in &removed {
        if let Some(Rec::Run { lead, .. }) = work.records.remove(id) {
            lead_markers.extend(lead);
        }
    }
    if p.runs.is_empty() {
        let run_id = new_id();
        p.runs.push(TextRun::new(run_id.clone(), String::new()));
        work.records.insert(run_id.clone(), Rec::Run { origin: None, atom: false, direct: BTreeSet::new(), rstyle: None, lead: Vec::new() });
        changed.add(&run_id);
    }
    p.runs.truncate(1);
    let first_id = p.runs[0].id.clone();
    p.runs[0].text = text.to_string();
    if let Some(Rec::Run { lead, .. }) = work.records.get_mut(&first_id) {
        let mut merged = std::mem::take(lead);
        merged.extend(lead_markers);
        *lead = merged;
    }
    for id in removed {
        changed.add(&id);
    }
    changed.add(&first_id);
    changed.add(&paragraph_id);
    Ok(())
}

// ----------------------------------------------------------------------------------------------
// Styles

/// Model color (#RRGGBB) from a request value; the file stores RRGGBB.
fn model_color(value: &str) -> DltResult<String> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(format!("#{}", hex.to_ascii_uppercase()))
    } else {
        Err(DltError::invalid(format!("color must be #RRGGBB, got {value:?}")))
    }
}

/// Rejects patch fields that have no DOCX meaning on text.
fn check_text_patch(patch: &StylePatch) -> DltResult<()> {
    if patch.fill.is_some() || patch.stroke.is_some() || patch.stroke_width.is_some() {
        return Err(DltError::unsupported("fill, stroke and stroke width do not apply to DOCX text"));
    }
    if let Some(size) = patch.font_size {
        if size <= 0 || size > 1_000 * EMU_PER_PT {
            return Err(DltError::invalid("Font size must be positive and at most 1000 pt"));
        }
    }
    if let Some(family) = &patch.font_family {
        if family.trim().is_empty() || family.chars().any(|c| c.is_control()) {
            return Err(DltError::invalid("Font family must be a non-empty name without control characters"));
        }
    }
    if let Some(color) = &patch.color {
        model_color(color)?;
    }
    Ok(())
}

/// Stores the patch on a run and records the fields as direct formatting.
fn apply_run_patch(
    work: &mut State,
    run_index: usize,
    p_id: &str,
    patch: &StylePatch,
    changed: &mut Changed,
    diagnostics: &mut Vec<designllm_contracts::Diagnostic>,
) -> DltResult<()> {
    let p = find_paragraph_mut(&mut work.document, p_id).ok_or_else(|| not_found("Paragraph", p_id))?;
    let run = &mut p.runs[run_index];
    let run_id = run.id.clone();
    let mut names: Vec<&str> = Vec::new();
    if let Some(v) = &patch.font_family {
        run.style.font_family = v.clone();
        names.push("font_family");
    }
    if let Some(v) = patch.font_size {
        let (_, applied) = units::to_half_points(v)?;
        if applied != v {
            diagnostics.push(diag::rounding("font_size", v, applied, "half-point", Some(&run_id)));
        }
        run.style.font_size = applied;
        names.push("font_size");
    }
    if let Some(v) = &patch.color {
        run.style.color = model_color(v)?;
        names.push("color");
    }
    if let Some(v) = patch.bold {
        run.style.bold = v;
        names.push("bold");
    }
    if let Some(v) = patch.italic {
        run.style.italic = v;
        names.push("italic");
    }
    let (direct, _, _, _) = run_rec_mut(&mut work.records, &run_id)?;
    for name in names {
        direct.insert(name.to_string());
    }
    changed.add(&run_id);
    Ok(())
}

fn set_style_node(work: &mut State, id: &str, patch: &StylePatch, changed: &mut Changed, diagnostics: &mut Vec<designllm_contracts::Diagnostic>) -> DltResult<()> {
    check_text_patch(patch)?;
    if find_paragraph_mut(&mut work.document, id).is_some() {
        if let Some(align) = &patch.align {
            set_paragraph_align_for_node(work, id, align, changed)?;
        }
        let indexes: Vec<usize> = {
            let p = find_paragraph_mut(&mut work.document, id).ok_or_else(|| not_found("Paragraph", id))?;
            (0..p.runs.len()).filter(|&i| !p.runs[i].protected).collect()
        };
        let has_atoms = {
            let p = find_paragraph_mut(&mut work.document, id).ok_or_else(|| not_found("Paragraph", id))?;
            p.runs.iter().any(|r| r.protected)
        };
        if has_atoms {
            diagnostics.push(diag::info(diag::PROTECTED_INLINE, "style skipped for protected inline content", Some(&id.to_string())));
        }
        for index in indexes {
            apply_run_patch(work, index, id, patch, changed, diagnostics)?;
        }
        changed.add(id);
        return Ok(());
    }
    if find_image_mut(&mut work.document, id).is_some() || find_table_mut(&mut work.document, id).is_some() || find_cell_mut(&mut work.document, id).is_some() {
        return Err(unsupported_with(
            diag::PROTECTED_INLINE,
            format!("style of node {id} is not supported for DOCX; style paragraphs and text ranges"),
            Some(&id.to_string()),
        ));
    }
    Err(not_found("Node", id))
}

fn set_paragraph_align_for_node(work: &mut State, id: &str, align: &str, changed: &mut Changed) -> DltResult<()> {
    if !matches!(align, "left" | "center" | "right" | "justify") {
        return Err(DltError::invalid(format!("align must be left, center, right or justify, got {align}")));
    }
    let p = find_paragraph_mut(&mut work.document, id).ok_or_else(|| not_found("Paragraph", id))?;
    p.format.align = align.to_string();
    let (direct, _) = para_rec_mut(&mut work.records, id)?;
    direct.insert("align".to_string());
    changed.add(id);
    Ok(())
}

fn set_text_style_range(
    work: &mut State,
    range: &TextRange,
    patch: &StylePatch,
    changed: &mut Changed,
    diagnostics: &mut Vec<designllm_contracts::Diagnostic>,
) -> DltResult<()> {
    check_text_patch(patch)?;
    if patch.align.is_some() {
        return Err(DltError::unsupported("alignment is a paragraph property; use set_paragraph_format"));
    }
    if range.start >= range.end {
        return Err(DltError::invalid("set_text_style_range needs a non-empty range"));
    }
    let p = find_paragraph_mut(&mut work.document, &range.paragraph_id)
        .ok_or_else(|| not_found("Paragraph", &range.paragraph_id))?;
    LogicalParagraph::new(p).check_range(range.start, range.end)?;
    let skipped_atoms = LogicalParagraph::new(p).range_has_protected(range.start, range.end);
    split_at(p, &mut work.records, range.end)?;
    split_at(p, &mut work.records, range.start)?;
    let p = find_paragraph_mut(&mut work.document, &range.paragraph_id)
        .ok_or_else(|| not_found("Paragraph", &range.paragraph_id))?;
    let spans = LogicalParagraph::new(p).all_spans(range.start, range.end)?;
    let mut indexes: Vec<usize> = spans.iter().map(|s| s.run).collect();
    indexes.sort_unstable();
    indexes.dedup();
    let paragraph_id = range.paragraph_id.clone();
    for index in indexes {
        let is_atom = work.document_run_is_protected(&paragraph_id, index);
        if is_atom {
            if skipped_atoms {
                diagnostics.push(diag::info(
                    diag::PROTECTED_INLINE,
                    "style skipped for protected inline content inside the range",
                    Some(&paragraph_id),
                ));
            }
            continue;
        }
        apply_run_patch(work, index, &paragraph_id, patch, changed, diagnostics)?;
    }
    changed.add(&paragraph_id);
    Ok(())
}

impl State {
    fn document_run_is_protected(&self, paragraph_id: &str, index: usize) -> bool {
        for section in &self.document.sections {
            if let Some(p) = find_para_in_blocks(&section.blocks, paragraph_id) {
                return p.runs.get(index).map(|r| r.protected).unwrap_or(false);
            }
        }
        false
    }
}

fn find_para_in_blocks<'a>(blocks: &'a [FlowBlock], id: &str) -> Option<&'a Paragraph> {
    for block in blocks {
        match block {
            FlowBlock::Paragraph(p) if p.id == id => return Some(p),
            FlowBlock::Table(t) => {
                for row in &t.rows {
                    for cell in row {
                        if let Some(p) = find_para_in_blocks(&cell.blocks, id) {
                            return Some(p);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn set_named_style(
    work: &mut State,
    base_styles: &StyleTable,
    styles: &mut StyleTable,
    name: &str,
    patch: &StylePatch,
    clear_overrides: bool,
    changed: &mut Changed,
) -> DltResult<()> {
    check_text_patch(patch)?;
    let id = base_styles
        .resolve_name(name)
        .ok_or_else(|| not_found("Named style", name))?;
    let kind = base_styles.styles.get(&id).map(|d| d.kind.clone()).unwrap_or_default();
    if patch.align.is_some() && kind != "paragraph" {
        return Err(DltError::unsupported("align applies to paragraph styles only"));
    }
    let entry = work.overrides.entry(id.clone()).or_insert_with(StyleOverride::default);
    if let Some(v) = &patch.font_family {
        entry.rpr.font_family = Some(v.clone());
    }
    if let Some(v) = patch.font_size {
        entry.rpr.font_size = Some(units::to_half_points(v)?.1);
    }
    if let Some(v) = &patch.color {
        entry.rpr.color = Some(model_color(v)?);
    }
    if let Some(v) = patch.bold {
        entry.rpr.bold = Some(v);
    }
    if let Some(v) = patch.italic {
        entry.rpr.italic = Some(v);
    }
    if let Some(v) = &patch.align {
        entry.ppr.align = Some(v.clone());
    }
    *styles = base_styles.clone();
    styles.apply_overrides(&work.overrides);

    // Clearing overrides removes the direct formatting of the affected runs and paragraphs.
    if clear_overrides {
        for section in &mut work.document.sections {
            clear_direct_for_style(&mut section.blocks, false, styles, &mut work.records, &id, &kind, changed)?;
        }
    }
    changed.add(&id);
    Ok(())
}

/// `in_cell` marks blocks inside a table cell. There a direct page break off stays when the style chain would turn the
/// break on without it: the style's page break must not reach the cell, which `cell_page_break_refusal` refuses for every
/// other path. Any other direct page break is cleared like the rest of the direct formatting.
fn clear_direct_for_style(
    blocks: &mut [FlowBlock],
    in_cell: bool,
    styles: &StyleTable,
    records: &mut Records,
    style_id: &str,
    kind: &str,
    changed: &mut Changed,
) -> DltResult<()> {
    for block in blocks.iter_mut() {
        match block {
            FlowBlock::Paragraph(p) => {
                let para_style = p.style_ref.as_deref() == Some(style_id) && kind == "paragraph";
                if para_style {
                    let (direct, _) = para_rec_mut(records, &p.id)?;
                    let keep_off = in_cell
                        && direct.contains("page_break_before")
                        && !p.format.page_break_before
                        && styles.para_format(p.style_ref.as_deref(), &ParaProps::default()).page_break_before;
                    direct.clear();
                    if keep_off {
                        direct.insert("page_break_before".to_string());
                    }
                    changed.add(&p.id);
                }
                for run in &p.runs {
                    if run.protected {
                        continue;
                    }
                    let matches = if para_style {
                        true
                    } else if kind == "character" {
                        matches!(records.get(&run.id), Some(Rec::Run { rstyle: Some(r), .. }) if r == style_id)
                    } else {
                        false
                    };
                    if matches {
                        let (direct, _, _, _) = run_rec_mut(records, &run.id)?;
                        direct.clear();
                        changed.add(&run.id);
                    }
                }
            }
            FlowBlock::Table(t) => {
                for row in t.rows.iter_mut() {
                    for cell in row.iter_mut() {
                        clear_direct_for_style(&mut cell.blocks, true, styles, records, style_id, kind, changed)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Field-wise equality of paragraph formats (the contracts type does not derive PartialEq).
fn same_format(a: &ParagraphFormat, b: &ParagraphFormat) -> bool {
    a.align == b.align
        && a.space_before == b.space_before
        && a.space_after == b.space_after
        && a.line_spacing == b.line_spacing
        && a.indent == b.indent
        && a.page_break_before == b.page_break_before
}

/// Recomputes effective values of every paragraph and run from the style table. Direct values stay.
/// Returns the ids whose modelled values changed.
pub fn refresh(work: &mut State, styles: &StyleTable) -> Vec<Id> {
    let mut changed = Vec::new();
    let snapshot = work.records.clone();
    for section in &mut work.document.sections {
        refresh_blocks(&mut section.blocks, &snapshot, styles, &mut changed);
    }
    work.document.styles = styles.styles.keys().map(|id| (id.clone(), styles.model_style(id))).collect();
    changed
}

fn refresh_blocks(blocks: &mut [FlowBlock], snapshot: &Records, styles: &StyleTable, changed: &mut Vec<Id>) {
    for block in blocks.iter_mut() {
        match block {
            FlowBlock::Paragraph(p) => {
                let direct: BTreeSet<String> = match snapshot.get(&p.id) {
                    Some(Rec::Paragraph { direct, .. }) => direct.clone(),
                    _ => BTreeSet::new(),
                };
                let mut direct_pp = ParaProps::default();
                if direct.contains("align") {
                    direct_pp.align = Some(p.format.align.clone());
                }
                if direct.contains("space_before") {
                    direct_pp.space_before = Some(p.format.space_before);
                }
                if direct.contains("space_after") {
                    direct_pp.space_after = Some(p.format.space_after);
                }
                if direct.contains("line_spacing") {
                    direct_pp.line_spacing = Some(p.format.line_spacing);
                }
                if direct.contains("indent") {
                    direct_pp.indent = Some(p.format.indent);
                }
                if direct.contains("page_break_before") {
                    direct_pp.page_break_before = Some(p.format.page_break_before);
                }
                let format = styles.para_format(p.style_ref.as_deref(), &direct_pp);
                if !same_format(&format, &p.format) {
                    p.format = format;
                    changed.push(p.id.clone());
                }
                for run in p.runs.iter_mut() {
                    if run.protected {
                        continue;
                    }
                    let (run_direct, rstyle) = match snapshot.get(&run.id) {
                        Some(Rec::Run { direct, rstyle, .. }) => (direct.clone(), rstyle.clone()),
                        _ => (BTreeSet::new(), None),
                    };
                    let mut direct_rp = RunProps::default();
                    if run_direct.contains("font_family") {
                        direct_rp.font_family = Some(run.style.font_family.clone());
                    }
                    if run_direct.contains("font_size") {
                        direct_rp.font_size = Some(run.style.font_size);
                    }
                    if run_direct.contains("color") {
                        direct_rp.color = Some(run.style.color.clone());
                    }
                    if run_direct.contains("bold") {
                        direct_rp.bold = Some(run.style.bold);
                    }
                    if run_direct.contains("italic") {
                        direct_rp.italic = Some(run.style.italic);
                    }
                    let mut style = styles.run_style(p.style_ref.as_deref(), rstyle.as_deref(), &direct_rp);
                    style.fill = run.style.fill.clone();
                    style.stroke = run.style.stroke.clone();
                    style.stroke_width = run.style.stroke_width;
                    style.align = run.style.align.clone();
                    if style != run.style {
                        run.style = style;
                        changed.push(run.id.clone());
                    }
                }
            }
            FlowBlock::Table(t) => {
                for row in t.rows.iter_mut() {
                    for cell in row.iter_mut() {
                        refresh_blocks(&mut cell.blocks, snapshot, styles, changed);
                    }
                }
            }
            _ => {}
        }
    }
}

// ----------------------------------------------------------------------------------------------
// Paragraph, page, table

fn set_paragraph_format(
    work: &mut State,
    paragraph_id: &str,
    format: &ParagraphFormat,
    changed: &mut Changed,
    diagnostics: &mut Vec<designllm_contracts::Diagnostic>,
) -> DltResult<()> {
    percent_refusal(format, paragraph_id)?;
    if !matches!(format.align.as_str(), "left" | "center" | "right" | "justify") {
        return Err(DltError::invalid(format!("align must be left, center, right or justify, got {}", format.align)));
    }
    let current = find_paragraph_mut(&mut work.document, paragraph_id)
        .ok_or_else(|| not_found("Paragraph", paragraph_id))?
        .format
        .line_spacing;
    if format.line_spacing < 0 && format.line_spacing != current {
        return Err(DltError::unsupported(
            "automatic multiple line spacing cannot be set through this operation; use 0 for single or an exact pitch",
        ));
    }
    let limit = 1_000 * designllm_contracts::EMU_PER_MM;
    if [format.space_before, format.space_after, format.indent.abs(), format.line_spacing.abs()].iter().any(|v| *v > limit) {
        return Err(DltError::invalid("Paragraph spacing or indent exceeds 1 m"));
    }
    if format.space_before < 0 || format.space_after < 0 {
        return Err(DltError::invalid("Paragraph spacing before and after must not be negative"));
    }
    let mut applied = format.clone();
    let mut round = |field: &str, requested: i64| -> DltResult<i64> {
        if requested == 0 {
            return Ok(0);
        }
        let (_, value) = units::to_twips(requested)?;
        if value != requested {
            diagnostics.push(diag::rounding(field, requested, value, "twip", Some(&paragraph_id.to_string())));
        }
        Ok(value)
    };
    applied.space_before = round("space_before", format.space_before)?;
    applied.space_after = round("space_after", format.space_after)?;
    applied.indent = round("indent", format.indent)?;
    applied.line_spacing = if format.line_spacing > 0 { round("line_spacing", format.line_spacing)? } else { format.line_spacing };
    // Inside a table cell a page break does not start a page: turning one on is refused, as HWP refuses it.
    let top_level = work.document.sections.iter().any(|s| s.blocks.iter().any(|b| matches!(b, FlowBlock::Paragraph(q) if q.id == paragraph_id)));
    let p = find_paragraph_mut(&mut work.document, paragraph_id).ok_or_else(|| not_found("Paragraph", paragraph_id))?;
    if format.page_break_before && !p.format.page_break_before && !top_level {
        return Err(cell_page_break_refusal(paragraph_id));
    }
    let page_break_changed = p.format.page_break_before != applied.page_break_before;
    p.format = applied;
    let (direct, _) = para_rec_mut(&mut work.records, paragraph_id)?;
    for name in PARA_NAMES {
        direct.insert(name.to_string());
    }
    // The page break becomes direct formatting when this request changes it or the paragraph already has it directly.
    // Otherwise the inherited value stays as the file has it, and nothing is rewritten.
    if page_break_changed || direct.contains("page_break_before") {
        direct.insert("page_break_before".to_string());
    }
    changed.add(paragraph_id);
    Ok(())
}

fn set_page_setup(
    work: &mut State,
    section_id: &str,
    setup: &PageSetup,
    changed: &mut Changed,
    diagnostics: &mut Vec<designllm_contracts::Diagnostic>,
) -> DltResult<()> {
    if let Some(problem) = page_setup_problem(setup) {
        return Err(DltError::invalid(problem));
    }
    let section = work
        .document
        .sections
        .iter_mut()
        .find(|s| s.id == section_id)
        .ok_or_else(|| not_found("Section", section_id))?;
    let mut round = |field: &str, requested: i64| -> DltResult<i64> {
        let (_, value) = units::to_twips(requested)?;
        if value != requested {
            diagnostics.push(diag::rounding(field, requested, value, "twip", Some(&section_id.to_string())));
        }
        Ok(value)
    };
    let applied = PageSetup {
        size: Size { width: round("page_width", setup.size.width)?, height: round("page_height", setup.size.height)? },
        margin_left: round("margin_left", setup.margin_left)?,
        margin_top: round("margin_top", setup.margin_top)?,
        margin_right: round("margin_right", setup.margin_right)?,
        margin_bottom: round("margin_bottom", setup.margin_bottom)?,
    };
    if let Some(problem) = page_setup_problem(&applied) {
        return Err(DltError::invalid(problem));
    }
    section.setup = applied;
    changed.add(section_id);
    Ok(())
}

fn set_table_dimensions(
    work: &mut State,
    table_id: &str,
    column_widths: &[i64],
    changed: &mut Changed,
    diagnostics: &mut Vec<designllm_contracts::Diagnostic>,
) -> DltResult<()> {
    let table = find_table_mut(&mut work.document, table_id).ok_or_else(|| not_found("Table", table_id))?;
    if column_widths.len() != table.column_widths.len() {
        return Err(DltError::invalid(format!(
            "table {table_id} has {} grid columns, {} widths were given",
            table.column_widths.len(),
            column_widths.len()
        )));
    }
    let mut applied = Vec::with_capacity(column_widths.len());
    for (index, width) in column_widths.iter().enumerate() {
        if *width <= 0 {
            return Err(DltError::invalid("column widths must be positive"));
        }
        let (_, value) = units::to_twips(*width)?;
        if value <= 0 {
            return Err(DltError::invalid("column width rounds to zero twips"));
        }
        if value != *width {
            diagnostics.push(diag::rounding(&format!("column_{index}"), *width, value, "twip", Some(&table_id.to_string())));
        }
        applied.push(value);
    }
    table.column_widths = applied;
    changed.add(table_id);
    Ok(())
}

// ----------------------------------------------------------------------------------------------
// Figure links

/// Gives a picture the bytes of a figure render and records the link (docs/FIGURE_LINKS.md).
///
/// The picture keeps its place and width. Its height becomes `width x pixel height / pixel width`, rounded half-up
/// to EMU, so the render keeps its proportions. The target must be a picture of the candidate state, so a picture
/// inserted earlier in the same change set can be linked. A later operation in the same change may still resize the
/// picture; the link is not re-checked against the new size.
fn update_figure_link(work: &mut State, link: &FigureLink, assets: &dyn AssetStore, changed: &mut Changed) -> DltResult<()> {
    match find_node(&work.document, &link.target_node_id) {
        Some(FlowNode::Image(_)) => {}
        Some(_) => return Err(invalid_with_id(format!("node {} is not a picture", link.target_node_id), &link.target_node_id)),
        None => return Err(not_found("Picture", &link.target_node_id)),
    }
    if !FIGURE_REPRESENTATIONS.contains(&link.representation.as_str()) {
        return Err(DltError::unsupported(format!(
            "figure representation {:?} is not supported for DOCX pictures; supported: {}",
            link.representation,
            FIGURE_REPRESENTATIONS.join(", ")
        )));
    }
    let stored = assets.lookup(&link.render_hash)?;
    if stored.mime != "image/png" {
        return Err(DltError::invalid(format!(
            "render {} is {}, but representation png needs image/png",
            link.render_hash, stored.mime
        )));
    }
    let bytes = assets.read(&stored)?;
    if sha256(&bytes) != link.render_hash {
        return Err(DltError::new(ErrorCode::IntegrityError, "the render bytes do not match their hash"));
    }
    let (pixel_width, pixel_height) =
        png::pixel_size(&bytes).ok_or_else(|| DltError::invalid("the render is not a readable PNG"))?;
    let node = find_image_mut(&mut work.document, &link.target_node_id)
        .ok_or_else(|| not_found("Picture", &link.target_node_id))?;
    let height = ratio_height(node.bounds.width, pixel_width, pixel_height)?;
    if height <= 0 || !(Size { width: node.bounds.width, height }).valid() {
        return Err(invalid_with_id(
            format!("picture {} would be {height} EMU high; it must be positive and within 10 m", link.target_node_id),
            &link.target_node_id,
        ));
    }
    node.bounds.height = height;
    node.asset = Some(AssetRef { sha256: link.render_hash.clone(), mime: "image/png".to_string(), bytes: bytes.len() as u64 });
    changed.add(&link.target_node_id);
    work.figure_links.retain(|existing| existing.target_node_id != link.target_node_id);
    work.figure_links.push(link.clone());
    work.figure_links.sort_by(|a, b| a.target_node_id.cmp(&b.target_node_id));
    Ok(())
}

/// `width x pixel_height / pixel_width` in EMU, rounded half-up (`media_header::aspect_height`). The caller refuses
/// a height that is not positive.
fn ratio_height(width: i64, pixel_width: u32, pixel_height: u32) -> DltResult<i64> {
    match media_header::aspect_height(width, pixel_width, pixel_height) {
        Ok(height) => Ok(height),
        Err(media_header::AspectError::NonPositive) => Ok(0),
        Err(media_header::AspectError::OutOfRange) => Err(DltError::invalid("picture height is out of range")),
    }
}

fn invalid_with_id(message: impl Into<String>, id: &Id) -> DltError {
    let mut error = DltError::invalid(message);
    error.affected_ids = vec![id.clone()];
    error
}

// ----------------------------------------------------------------------------------------------
// Insert and remove

const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/bmp"];

#[allow(clippy::too_many_arguments)]
fn insert_block(
    work: &mut State,
    section_id: &str,
    index: u32,
    block: &FlowBlock,
    styles: &StyleTable,
    assets: &dyn AssetStore,
    changed: &mut Changed,
    diagnostics: &mut Vec<designllm_contracts::Diagnostic>,
) -> DltResult<()> {
    let section_count = work.document.sections.len();
    let section_position = work.document.sections.iter().position(|s| s.id == section_id).ok_or_else(|| not_found("Section", section_id))?;
    let existing = model::collect_ids(&work.document)?;
    let mut new_records = Records::new();
    let mut new_changed = Vec::new();
    let mut prepared = block.clone();
    prepare_new_block(&mut prepared, styles, assets, &existing, &mut new_records, &mut new_changed, diagnostics)?;
    let section = &mut work.document.sections[section_position];
    if index as usize > section.blocks.len() {
        return Err(DltError::invalid(format!("insert index {index} is past the {} blocks of section {section_id}", section.blocks.len())));
    }
    if section_position + 1 < section_count && index as usize == section.blocks.len() {
        return Err(DltError::invalid(
            "inserting after the section break paragraph of a section would move the block into the next section; insert before it",
        ));
    }
    section.blocks.insert(index as usize, prepared);
    work.records.extend(new_records);
    for id in new_changed {
        changed.add(&id);
    }
    changed.add(section_id);
    Ok(())
}

/// Validates a new block, quantizes its native values, and creates native records for its nodes.
fn prepare_new_block(
    block: &mut FlowBlock,
    styles: &StyleTable,
    assets: &dyn AssetStore,
    existing: &BTreeSet<Id>,
    records: &mut Records,
    changed: &mut Vec<Id>,
    diagnostics: &mut Vec<designllm_contracts::Diagnostic>,
) -> DltResult<()> {
    let mut fresh = BTreeSet::new();
    let mut check_id = |id: &mut Id| -> DltResult<()> {
        if id.is_empty() {
            *id = new_id();
        }
        if existing.contains(id) || !fresh.insert(id.clone()) {
            let mut error = DltError::new(ErrorCode::AlreadyExists, format!("id {id} already exists"));
            error.affected_ids = vec![id.clone()];
            return Err(error);
        }
        Ok(())
    };
    match block {
        FlowBlock::Paragraph(p) => {
            check_id(&mut p.id)?;
            percent_refusal(&p.format, &p.id)?;
            if let Some(style) = &p.style_ref {
                // A style without `w:type` is a paragraph style (ECMA-376, as `styles::list_flow_styles` lists it).
                match styles.styles.get(style).map(|def| def.kind.as_str()) {
                    None => return Err(DltError::invalid(format!("paragraph style {style} is not defined in this document"))),
                    Some("paragraph" | "") => {}
                    Some(kind) => return Err(DltError::invalid(format!("style {style} is a {kind} style, not a paragraph style"))),
                }
            }
            let mut direct_pp = ParaProps::default();
            let mut quantized = p.format.clone();
            if !matches!(quantized.align.as_str(), "left" | "center" | "right" | "justify") {
                return Err(DltError::invalid(format!("align must be left, center, right or justify, got {}", quantized.align)));
            }
            quantize_format(&mut quantized, &p.id, diagnostics)?;
            let inherited = styles.para_format(p.style_ref.as_deref(), &ParaProps::default());
            let mut names = BTreeSet::new();
            if quantized.align != inherited.align {
                names.insert("align".to_string());
                direct_pp.align = Some(quantized.align.clone());
            }
            if quantized.space_before != inherited.space_before {
                names.insert("space_before".to_string());
            }
            if quantized.space_after != inherited.space_after {
                names.insert("space_after".to_string());
            }
            if quantized.line_spacing != inherited.line_spacing {
                names.insert("line_spacing".to_string());
            }
            if quantized.indent != inherited.indent {
                names.insert("indent".to_string());
            }
            if quantized.page_break_before != inherited.page_break_before {
                names.insert("page_break_before".to_string());
            }
            // Explicit paragraph format from the request is direct formatting even when it equals the default.
            for name in PARA_NAMES {
                names.insert(name.to_string());
            }
            p.format = quantized;
            changed.push(p.id.clone());
            records.insert(p.id.clone(), Rec::Paragraph { path: None, direct: names, tail: Vec::new() });
            let mut run_ids = BTreeSet::new();
            for run in p.runs.iter_mut() {
                if run.id.is_empty() {
                    run.id = new_id();
                }
                check_id(&mut run.id)?;
                run_ids.insert(run.id.clone());
                if run.protected {
                    return Err(DltError::unsupported("a new paragraph cannot contain protected inline content"));
                }
                validate_edit_text(&run.text)?;
                let inherited_run = styles.run_style(p.style_ref.as_deref(), None, &RunProps::default());
                let mut direct = BTreeSet::new();
                let mut rp = RunProps::default();
                if run.style.font_family != inherited_run.font_family {
                    direct.insert("font_family".to_string());
                    rp.font_family = Some(run.style.font_family.clone());
                }
                if run.style.font_size != inherited_run.font_size {
                    let (_, applied) = units::to_half_points(run.style.font_size)?;
                    run.style.font_size = applied;
                    direct.insert("font_size".to_string());
                }
                if run.style.color != inherited_run.color {
                    direct.insert("color".to_string());
                }
                if run.style.bold != inherited_run.bold {
                    direct.insert("bold".to_string());
                }
                if run.style.italic != inherited_run.italic {
                    direct.insert("italic".to_string());
                }
                run.style = styles.run_style(p.style_ref.as_deref(), None, &RunProps {
                    font_family: direct.contains("font_family").then(|| run.style.font_family.clone()),
                    font_size: direct.contains("font_size").then_some(run.style.font_size),
                    color: direct.contains("color").then(|| run.style.color.clone()),
                    bold: direct.contains("bold").then_some(run.style.bold),
                    italic: direct.contains("italic").then_some(run.style.italic),
                });
                records.insert(run.id.clone(), Rec::Run { origin: None, atom: false, direct, rstyle: None, lead: Vec::new() });
                changed.push(run.id.clone());
            }
        }
        FlowBlock::Table(t) => {
            check_id(&mut t.id)?;
            let cols = t.column_widths.len();
            if t.rows.is_empty() || cols == 0 {
                return Err(DltError::invalid("a new table needs at least one row and one column width"));
            }
            let mut applied = Vec::with_capacity(cols);
            for (i, width) in t.column_widths.iter().enumerate() {
                if *width <= 0 {
                    return Err(DltError::invalid("column widths must be positive"));
                }
                let (_, value) = units::to_twips(*width)?;
                if value != *width {
                    diagnostics.push(diag::rounding(&format!("column_{i}"), *width, value, "twip", Some(&t.id)));
                }
                applied.push(value.max(1));
            }
            t.column_widths = applied;
            for row in t.rows.iter_mut() {
                let spans: u32 = row.iter().map(|c| c.column_span).sum();
                if spans as usize != cols {
                    return Err(DltError::invalid(format!("table row covers {spans} columns, the table has {cols}")));
                }
                for cell in row.iter_mut() {
                    if cell.row_span != 1 || cell.column_span == 0 {
                        return Err(DltError::unsupported("vertical merges cannot be created with insert_flow_block; insert unmerged cells"));
                    }
                    check_id(&mut cell.id)?;
                    for child in cell.blocks.iter_mut() {
                        if let FlowBlock::Paragraph(cp) = child {
                            // The refusal names the paragraph, so it gets the id `prepare_new_block` would give it.
                            if cp.id.is_empty() {
                                cp.id = new_id();
                            }
                            if cp.format.page_break_before {
                                return Err(cell_page_break_refusal(&cp.id));
                            }
                        }
                        prepare_new_block(child, styles, assets, existing, records, changed, diagnostics)?;
                    }
                    if cell.blocks.is_empty() {
                        return Err(DltError::invalid("a table cell needs at least one block"));
                    }
                }
            }
            records.insert(t.id.clone(), Rec::Table { path: None, hidden: false });
            changed.push(t.id.clone());
        }
        FlowBlock::Image(node) => {
            check_id(&mut node.id)?;
            let asset = node.asset.clone().ok_or_else(|| DltError::invalid("a new picture needs an asset"))?;
            if !IMAGE_MIMES.contains(&asset.mime.as_str()) {
                return Err(DltError::unsupported(format!("picture type {} cannot be inserted into DOCX", asset.mime)));
            }
            if node.bounds.width <= 0 || node.bounds.height <= 0 || !(Size { width: node.bounds.width, height: node.bounds.height }).valid() {
                return Err(DltError::invalid("a new picture needs a positive size within 10 m"));
            }
            let bytes = assets.read(&asset)?;
            if designllm_contracts::sha256(&bytes) != asset.sha256 {
                return Err(DltError::new(ErrorCode::IntegrityError, "picture bytes do not match the asset hash"));
            }
            if node.bounds.x != 0 || node.bounds.y != 0 {
                diagnostics.push(diag::info(
                    diag::INLINE_PICTURE_POSITION_IGNORED,
                    format!("DOCX inline pictures follow text flow; requested position ({}, {}) EMU is ignored", node.bounds.x, node.bounds.y),
                    Some(&node.id),
                ));
            }
            node.bounds = Rect { x: 0, y: 0, width: node.bounds.width, height: node.bounds.height };
            records.insert(node.id.clone(), Rec::Image { path: None });
            changed.push(node.id.clone());
        }
        FlowBlock::Opaque { .. } => {
            return Err(DltError::unsupported("opaque blocks preserve imported content and cannot be inserted"));
        }
    }
    Ok(())
}

fn quantize_format(format: &mut ParagraphFormat, id: &Id, diagnostics: &mut Vec<designllm_contracts::Diagnostic>) -> DltResult<()> {
    let mut round = |field: &str, requested: i64| -> DltResult<i64> {
        if requested == 0 {
            return Ok(0);
        }
        let (_, value) = units::to_twips(requested)?;
        if value != requested {
            diagnostics.push(diag::rounding(field, requested, value, "twip", Some(id)));
        }
        Ok(value)
    };
    format.space_before = round("space_before", format.space_before)?;
    format.space_after = round("space_after", format.space_after)?;
    format.indent = round("indent", format.indent)?;
    if format.line_spacing > 0 {
        format.line_spacing = round("line_spacing", format.line_spacing)?;
    }
    Ok(())
}

/// Removes a block or a run. Protected content and cells are refused, because removing them would drop
/// XML that the model does not represent.
fn remove_node(work: &mut State, node_id: &str, changed: &mut Changed) -> DltResult<()> {
    // Run?
    if let Some((p, index)) = find_run_mut(&mut work.document, node_id) {
        if p.runs[index].protected {
            return Err(unsupported_with(diag::PROTECTED_INLINE, "protected inline content cannot be removed", Some(&node_id.to_string())));
        }
        let paragraph_id = p.id.clone();
        drop_run(p, &mut work.records, index)?;
        changed.add(node_id);
        changed.add(&paragraph_id);
        return Ok(());
    }
    // Section? cell? opaque?
    for section in &work.document.sections {
        if section.id == node_id {
            return Err(DltError::unsupported("removing a whole section is not supported"));
        }
    }
    let mut removed_ids = Vec::new();
    for section_index in 0..work.document.sections.len() {
        let is_last = section_index + 1 == work.document.sections.len();
        let section = &mut work.document.sections[section_index];
        if let Some(index) = section.blocks.iter().position(|b| model::block_id(b).map(|id| id == node_id).unwrap_or(false)) {
            if matches!(section.blocks[index], FlowBlock::Opaque { .. }) {
                return Err(unsupported_with(diag::PROTECTED_BLOCK, "opaque content cannot be removed", Some(&node_id.to_string())));
            }
            if !is_last && index + 1 == section.blocks.len() {
                return Err(DltError::unsupported("removing the section break paragraph would merge two sections"));
            }
            let block = section.blocks.remove(index);
            collect_block_ids(&block, &mut removed_ids);
            guard_removable(&block, &work.records)?;
            // A removed picture takes its figure link with it.
            work.figure_links.retain(|link| !removed_ids.contains(&link.target_node_id));
            for id in &removed_ids {
                work.records.remove(id);
            }
            for id in removed_ids {
                changed.add(&id);
            }
            changed.add(&section.id.clone());
            return Ok(());
        }
        if remove_from_cells(&mut section.blocks, node_id, &mut removed_ids, &work.records)?.is_some() {
            work.figure_links.retain(|link| !removed_ids.contains(&link.target_node_id));
            for id in &removed_ids {
                work.records.remove(id);
            }
            for id in removed_ids {
                changed.add(&id);
            }
            return Ok(());
        }
    }
    Err(not_found("Node", node_id))
}

fn guard_removable(block: &FlowBlock, records: &Records) -> DltResult<()> {
    match block {
        FlowBlock::Opaque { id, .. } => Err(unsupported_with(diag::PROTECTED_BLOCK, "opaque content cannot be removed", Some(id))),
        FlowBlock::Paragraph(p) => {
            if p.runs.iter().any(|r| r.protected) {
                return Err(unsupported_with(diag::PROTECTED_INLINE, "a paragraph with protected inline content cannot be removed", Some(&p.id)));
            }
            if let Some(Rec::Paragraph { tail, .. }) = records.get(&p.id) {
                if !tail.is_empty() {
                    return Err(unsupported_with(diag::PROTECTED_INLINE, "a paragraph with preserved markers cannot be removed", Some(&p.id)));
                }
            }
            for run in &p.runs {
                if let Some(Rec::Run { lead, .. }) = records.get(&run.id) {
                    if !lead.is_empty() {
                        return Err(unsupported_with(diag::PROTECTED_INLINE, "a paragraph with preserved markers cannot be removed", Some(&p.id)));
                    }
                }
            }
            Ok(())
        }
        FlowBlock::Table(t) => {
            if matches!(records.get(&t.id), Some(Rec::Table { hidden: true, .. })) {
                return Err(unsupported_with(diag::IRREGULAR_TABLE, "a table with merged cells cannot be removed", Some(&t.id)));
            }
            for row in &t.rows {
                for cell in row {
                    for child in &cell.blocks {
                        guard_removable(child, records)?;
                    }
                }
            }
            Ok(())
        }
        FlowBlock::Image(_) => Ok(()),
    }
}

fn collect_block_ids(block: &FlowBlock, out: &mut Vec<Id>) {
    match block {
        FlowBlock::Paragraph(p) => {
            out.push(p.id.clone());
            out.extend(p.runs.iter().map(|r| r.id.clone()));
        }
        FlowBlock::Table(t) => {
            out.push(t.id.clone());
            for row in &t.rows {
                for cell in row {
                    out.push(cell.id.clone());
                    for child in &cell.blocks {
                        collect_block_ids(child, out);
                    }
                }
            }
        }
        FlowBlock::Image(n) => out.push(n.id.clone()),
        FlowBlock::Opaque { id, .. } => out.push(id.clone()),
    }
}

/// Removes a block nested in table cells. Returns `Some(())` when the block was found and removed.
fn remove_from_cells(blocks: &mut [FlowBlock], node_id: &str, removed: &mut Vec<Id>, records: &Records) -> DltResult<Option<()>> {
    for block in blocks.iter_mut() {
        if let FlowBlock::Table(t) = block {
            for row in t.rows.iter_mut() {
                for cell in row.iter_mut() {
                    if let Some(index) = cell.blocks.iter().position(|b| model::block_id(b).map(|id| id == node_id).unwrap_or(false)) {
                        if cell.blocks.len() == 1 {
                            return Err(DltError::invalid("a table cell must keep at least one block"));
                        }
                        if matches!(cell.blocks[index], FlowBlock::Opaque { .. }) {
                            let id = model::block_id(&cell.blocks[index]).cloned().unwrap_or_default();
                            return Err(unsupported_with(diag::PROTECTED_BLOCK, "opaque content cannot be removed", Some(&id)));
                        }
                        guard_removable(&cell.blocks[index], records)?;
                        let block = cell.blocks.remove(index);
                        collect_block_ids(&block, removed);
                        return Ok(Some(()));
                    }
                    if let Some(found) = remove_from_cells(&mut cell.blocks, node_id, removed, records)? {
                        return Ok(Some(found));
                    }
                }
            }
        }
    }
    Ok(None)
}
