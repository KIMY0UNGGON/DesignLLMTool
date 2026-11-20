//! Change preparation: support checks for operations, application through the shared flow model,
//! native-grid rounding with diagnostics, and checks of the candidate model.
//!
//! Operations are the shared `Operation` enum. HWPX supports a subset; every other operation is
//! refused with UNSUPPORTED_OPERATION rather than silently ignored.
use std::collections::{HashMap, HashSet};

use designllm_contracts::{
    new_id, AssetStore, Diagnostic, DltError, DltResult, ErrorCode, FigureLink, FlowBlock, FlowDocument, Operation,
    Paragraph, ParagraphFormat, Size, Style, StylePatch, Table, TextRun, EMU_PER_HWPUNIT, quantize,
};

use crate::diag;
use crate::doc::Loaded;
use crate::figure;
use crate::guard;
use crate::header::{align_is_supported, paragraph_font_size, NativeLine, PHeader, DEFAULT_FONT_SIZE};
use crate::section::{PBlock, PPara};
use crate::text;
use crate::writer::normalized_runs;
use designllm_flow::line_spacing::same_line_spacing;

/// Result of preparing a change: the candidate model, its figure links, and what changed.
pub struct Prepared {
    pub body: FlowDocument,
    pub changed_ids: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub figure_links: Vec<FigureLink>,
}

/// What a change reads besides the model and its operations.
pub struct ChangeInputs<'a> {
    pub assets: &'a dyn AssetStore,
    /// Figure links of the artifact before the change.
    pub figure_links: &'a [FigureLink],
    /// Paragraphs whose runs a rewrite would drop (see `section::locked_paragraph_ids`).
    pub locked_paragraphs: &'a HashSet<String>,
    /// Every block id of the source file, nested ones included (see `section::block_ids_of`). Blocks
    /// added in the session are not in it and may be removed by the operations that remove them.
    pub source_blocks: &'a HashSet<String>,
    /// Pictures of the source that a change may remove (see `lib::removable_pictures`).
    pub removable_pictures: &'a HashSet<String>,
    pub loaded: &'a Loaded,
}

/// Only distinctions lost when projecting native spacing into the model are kept between prepare and save.
pub type NativeLines = std::collections::BTreeMap<String, NativeLine>;

pub fn paragraph_line(p: &Paragraph, source: Option<(&ParagraphFormat, &NativeLine)>, lines: &NativeLines) -> Result<NativeLine, String> {
    if let Some(line) = lines.get(&p.id) {
        if line.approximated() && p.format.line_spacing_percent.is_none() {
            return Ok(line.clone());
        }
    } else if let Some((format, line)) = source {
        if same_line_spacing(&p.format, format) {
            return Ok(line.clone());
        }
    }
    NativeLine::from_format(&p.format)
}

/// A new object's insertion-time line is stored only when it differs from the document default.
pub(crate) fn new_host_line(id: &str, carrier: Option<&Paragraph>, header: &PHeader, lines: &NativeLines) -> DltResult<NativeLine> {
    if let Some(p) = carrier {
        return paragraph_line(p, None, lines).map_err(diag::unsupported);
    }
    lines.get(id).or_else(|| header.default_para().map(|p| &p.line))
        .map(NativeLine::inherited).ok_or_else(|| diag::malformed("header has no paragraph shape"))
}

struct LineContext<'a> {
    header: &'a PHeader,
    paragraphs: HashMap<&'a str, &'a PPara>,
    hosts: HashMap<&'a str, &'a NativeLine>,
    new_base_size: i64,
}

impl<'a> LineContext<'a> {
    fn new(loaded: &'a Loaded) -> Self {
        let header = &loaded.header;
        let mut context = Self {
            header,
            paragraphs: HashMap::new(),
            hosts: HashMap::new(),
            new_base_size: paragraph_font_size(&[], header.char_prs.first().map(|c| c.style.font_size)),
        };
        for section in &loaded.sections {
            context.collect(&section.blocks);
        }
        context
    }

    fn collect(&mut self, blocks: &'a [PBlock]) {
        let mut carrier_line = None;
        for block in blocks {
            let host = match block {
                PBlock::Para(p) => {
                    self.paragraphs.insert(&p.id, p);
                    carrier_line = self.header.para_pr(p.para_pr).map(|shape| &shape.line);
                    continue;
                }
                PBlock::Table(t) => {
                    for cell in t.rows.iter().flatten() {
                        self.collect(&cell.blocks);
                    }
                    t.host_shape.as_ref().map(|s| &s.line).or(carrier_line)
                }
                PBlock::Image(i) => i.host_shape.as_ref().map(|s| &s.line).or(carrier_line),
                PBlock::Opaque(o) => o.para_pr.and_then(|id| self.header.para_pr(id)).map(|s| &s.line),
            };
            if let Some(line) = host {
                self.hosts.insert(block.id(), line);
            }
            carrier_line = None;
        }
    }

    fn settle_paragraph(&self, p: &mut Paragraph, preceding: &NativeLine, new: bool, requested: bool,
                        lines: &mut NativeLines, diagnostics: &mut Vec<Diagnostic>) -> DltResult<NativeLine> {
        let source = self.paragraphs.get(p.id.as_str()).copied();
        let shape = source.and_then(|op| self.header.para_pr(op.para_pr).map(|s| (&op.format, s)));
        let inherit = new && !requested && p.format.line_spacing_percent.is_none() && p.format.line_spacing == 0;
        if new || requested {
            lines.remove(&p.id);
        }
        let line = if inherit {
            preceding.inherited()
        } else if requested {
            NativeLine::from_format(&p.format).map_err(diag::unsupported)?
        } else {
            paragraph_line(p, shape.map(|(f, s)| (f, &s.line)), lines).map_err(diag::unsupported)?
        };
        if inherit || line.kind == "PERCENT" || line.approximated() {
            let base = match source {
                Some(op) => op.orig_cps.first().and_then(|id| self.header.char_pr(*id)).map_or(DEFAULT_FONT_SIZE, |c| c.style.font_size),
                None => self.new_base_size,
            };
            let size = paragraph_font_size(&p.runs, Some(base));
            p.format.line_spacing_percent = line.percent();
            p.format.line_spacing = line.pitch(size).0;
        }
        if inherit && line.approximated() {
            diagnostics.push(diag::info(diag::LINE_SPACING_APPROXIMATED, "Inherited line spacing has an approximate model pitch", Some(p.id.clone())));
        }
        if line.approximated() || requested {
            // Keep the native choice only when comparing model formats would lose it on the next save.
            let remember = if line.approximated() {
                source.is_none_or(|op| p.format.line_spacing != op.format.line_spacing)
            } else {
                shape.is_some_and(|(old, s)| same_line_spacing(old, &p.format) && (s.line != line || !s.uniform_line))
            };
            if remember {
                lines.insert(p.id.clone(), line.clone());
            } else {
                lines.remove(&p.id);
            }
        }
        Ok(line)
    }

    fn settle(&self, blocks: &mut [FlowBlock], mut preceding: NativeLine, top_level: bool, original: &HashSet<String>,
              explicit: &HashSet<&str>, lines: &mut NativeLines, diagnostics: &mut Vec<Diagnostic>) -> DltResult<()> {
        let mut carrier = false;
        for block in blocks {
            let id = block_id(block);
            let new = !original.contains(id);
            if let FlowBlock::Paragraph(p) = block {
                let requested = explicit.contains(p.id.as_str());
                preceding = self.settle_paragraph(p, &preceding, new, requested, lines, diagnostics)?;
                carrier = top_level && !self.paragraphs.contains_key(p.id.as_str()) && p.format.page_break_before && p.runs.is_empty();
                continue;
            }
            if let FlowBlock::Table(t) = block {
                for cell in t.rows.iter_mut().flatten() {
                    self.settle(&mut cell.blocks, preceding.clone(), false, original, explicit, lines, diagnostics)?;
                }
            }
            let id = block_id(block);
            if let Some(host) = self.hosts.get(id) {
                preceding = (*host).clone();
            } else if matches!(block, FlowBlock::Table(_) | FlowBlock::Image(_)) {
                let default = &self.header.default_para().ok_or_else(|| diag::malformed("header has no paragraph shape"))?.line;
                if !new && !carrier {
                    preceding = lines.get(id).unwrap_or(default).clone();
                }
                preceding = preceding.inherited();
                if &preceding == default {
                    lines.remove(id);
                } else {
                    lines.insert(id.to_string(), preceding.clone());
                }
            }
            carrier = false;
        }
        Ok(())
    }
}

fn settle_line_spacing(doc: &mut FlowDocument, original: &HashSet<String>, context: &LineContext<'_>, operations: &[Operation],
                       lines: &mut NativeLines, diagnostics: &mut Vec<Diagnostic>) -> DltResult<()> {
    let explicit = operations.iter().filter_map(|op| match op {
        Operation::SetParagraphFormat { paragraph_id, .. } => Some(paragraph_id.as_str()),
        _ => None,
    }).collect();
    let default = context.header.default_para().ok_or_else(|| diag::malformed("header has no paragraph shape"))?;
    for section in &mut doc.sections {
        context.settle(&mut section.blocks, default.line.clone(), true, original, &explicit, lines, diagnostics)?;
    }
    Ok(())
}

const SUPPORTED_IMAGE_MIMES: &[&str] = &["image/png", "image/bmp", "image/jpeg", "image/jpg", "image/gif"];

fn unsupported_op(op: &Operation) -> DltError {
    let name = op.name();
    let node_id = match op {
        Operation::ReplaceImageAsset { node_id, .. } | Operation::SetCanvasImageCrop { node_id, .. } => Some(node_id),
        Operation::SetCanvasBoardMemos { page_id, .. } => Some(page_id),
        Operation::SetCanvasPageGuides { page_id, .. } => Some(page_id),
        Operation::SetCanvasPageSections { sections, expected } => sections.first().or(expected.first()).map(|section| &section.start_page_id),
        _ => None,
    };
    let mut error = diag::unsupported(format!("operation {name} is not supported for HWPX documents"));
    if let Some(node_id) = node_id {
        error.affected_ids.push(node_id.clone());
    }
    error
}

/// Style fields that a run cannot store in HWPX (fill, stroke, and paragraph-only alignment).
fn check_patch(patch: &StylePatch) -> DltResult<()> {
    if patch.fill.is_some() || patch.stroke.is_some() || patch.stroke_width.is_some() {
        return Err(diag::unsupported("fill and stroke styles cannot be stored in HWPX text"));
    }
    if patch.align.is_some() {
        return Err(diag::unsupported("text alignment is a paragraph format in HWPX; use set_paragraph_format"));
    }
    Ok(())
}

fn visit_blocks<'a>(blocks: &'a [FlowBlock], f: &mut dyn FnMut(&'a FlowBlock)) {
    for block in blocks {
        f(block);
        if let FlowBlock::Table(t) = block {
            for cell in t.rows.iter().flatten() {
                visit_blocks(&cell.blocks, f);
            }
        }
    }
}

pub(crate) fn all_blocks(doc: &FlowDocument) -> Vec<&FlowBlock> {
    let mut out = Vec::new();
    for section in &doc.sections {
        visit_blocks(&section.blocks, &mut |b| out.push(b));
    }
    out
}

fn find_paragraph<'a>(doc: &'a FlowDocument, id: &str) -> Option<&'a Paragraph> {
    all_blocks(doc).into_iter().find_map(|b| match b {
        FlowBlock::Paragraph(p) if p.id == id => Some(p),
        _ => None,
    })
}

fn find_table<'a>(doc: &'a FlowDocument, id: &str) -> Option<&'a Table> {
    all_blocks(doc).into_iter().find_map(|b| match b {
        FlowBlock::Table(t) if t.id == id => Some(t),
        _ => None,
    })
}

fn block_ids(doc: &FlowDocument) -> HashSet<String> {
    let mut ids = HashSet::new();
    for section in &doc.sections {
        ids.insert(section.id.clone());
        for block in all_blocks_of(&section.blocks) {
            ids.insert(block_id(block).to_string());
        }
    }
    ids
}

fn all_blocks_of(blocks: &[FlowBlock]) -> Vec<&FlowBlock> {
    let mut out = Vec::new();
    visit_blocks(blocks, &mut |b| out.push(b));
    out
}

fn block_id(block: &FlowBlock) -> &str {
    match block {
        FlowBlock::Paragraph(p) => &p.id,
        FlowBlock::Table(t) => &t.id,
        FlowBlock::Image(n) => &n.id,
        FlowBlock::Opaque { id, .. } => id,
    }
}

/// Table grid: a placement with spans must give every column one unspanned anchor cell.
fn check_grid(table: &Table) -> DltResult<()> {
    let cols = table.column_widths.len();
    let mut occupied: Vec<Vec<bool>> = Vec::new();
    let mut resolved = vec![false; cols];
    for (r, row) in table.rows.iter().enumerate() {
        while occupied.len() <= r {
            occupied.push(vec![false; cols]);
        }
        let mut c = 0usize;
        for cell in row {
            while c < cols && occupied[r][c] {
                c += 1;
            }
            let cs = cell.column_span.max(1) as usize;
            let rs = cell.row_span.max(1) as usize;
            if c + cs > cols {
                return Err(diag::unsupported("a table cell spans beyond the table columns"));
            }
            for rr in r..r + rs {
                while occupied.len() <= rr {
                    occupied.push(vec![false; cols]);
                }
                for cc in c..c + cs {
                    occupied[rr][cc] = true;
                }
            }
            if cs == 1 {
                resolved[c] = true;
            }
            c += cs;
        }
    }
    if cols == 0 || resolved.iter().any(|r| !r) {
        return Err(diag::unsupported("each table column needs a cell that is not merged horizontally"));
    }
    Ok(())
}

/// Paragraph that contains a run or paragraph id.
fn paragraph_of_node<'a>(doc: &'a FlowDocument, id: &str) -> Option<&'a Paragraph> {
    all_blocks(doc).into_iter().find_map(|b| match b {
        FlowBlock::Paragraph(p) if p.id == id || p.runs.iter().any(|r| r.id == id) => Some(p),
        _ => None,
    })
}

/// Pre-checks against the current model and the assets, before the flow model is changed.
fn precheck(body: &FlowDocument, operations: &[Operation], assets: &dyn AssetStore) -> DltResult<()> {
    for op in operations {
        match op {
            Operation::ReplaceText { range, text } => {
                let para = find_paragraph(body, &range.paragraph_id)
                    .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("paragraph {} not found", range.paragraph_id)))?;
                if text.contains('\t') || text.contains('\n') {
                    return Err(diag::unsupported("tab and line-break characters cannot be inserted as text in HWPX"));
                }
                if text::overlaps_atom(&para.runs, range.start, range.end) {
                    return Err(diag::error(ErrorCode::UnsupportedOperation, diag::PROTECTED_ATOM, "the range covers a protected inline object"));
                }
                if range.end > text::total_graphemes(&para.runs) || range.start > range.end {
                    return Err(DltError::invalid("text range is outside the paragraph"));
                }
            }
            Operation::SetText { node_id, text } => {
                let para = paragraph_of_node(body, node_id)
                    .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("paragraph or run {node_id} not found")))?;
                if para.runs.iter().any(|r| r.protected) {
                    return Err(diag::error(ErrorCode::UnsupportedOperation, diag::PROTECTED_ATOM, "set_text would remove protected inline objects"));
                }
                if text.contains('\t') || text.contains('\n') {
                    return Err(diag::unsupported("tab and line-break characters cannot be inserted as text in HWPX"));
                }
            }
            Operation::SetStyle { patch, .. } | Operation::SetTextStyleRange { patch, .. } => check_patch(patch)?,
            Operation::SetNamedStyle { patch, clear_overrides, .. } => {
                if *clear_overrides {
                    return Err(diag::unsupported("clear_overrides is not supported for HWPX named styles"));
                }
                check_patch(patch)?;
            }
            Operation::SetParagraphFormat { paragraph_id, format } => {
                find_paragraph(body, paragraph_id)
                    .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("paragraph {paragraph_id} not found")))?;
                if !align_is_supported(&format.align) {
                    return Err(diag::unsupported(format!("alignment {} is not supported by HWPX", format.align)));
                }
            }
            Operation::SetGeometry { node_id, bounds } => {
                let is_picture = all_blocks(body).into_iter().any(|b| matches!(b, FlowBlock::Image(n) if n.id == *node_id));
                if !is_picture {
                    return Err(diag::unsupported("set_geometry is supported for pictures only in HWPX"));
                }
                if bounds.x != 0 || bounds.y != 0 {
                    return Err(diag::unsupported("picture position is not stored for inline pictures in HWPX"));
                }
            }
            Operation::SetPageSetup { section_id, .. } => {
                if !body.sections.iter().any(|s| &s.id == section_id) {
                    return Err(DltError::new(ErrorCode::NotFound, format!("section {section_id} not found")));
                }
            }
            Operation::SetTableDimensions { table_id, column_widths } => {
                let table = find_table(body, table_id)
                    .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("table {table_id} not found")))?;
                if column_widths.len() != table.column_widths.len() {
                    return Err(diag::unsupported("changing the number of table columns is not supported in HWPX"));
                }
                if column_widths.iter().any(|w| *w <= 0) {
                    return Err(DltError::invalid("table column widths must be positive"));
                }
            }
            Operation::InsertFlowBlock { section_id, block, .. } => {
                if !body.sections.iter().any(|s| &s.id == section_id) {
                    return Err(DltError::new(ErrorCode::NotFound, format!("section {section_id} not found")));
                }
                check_insert_block(block, assets)?;
            }
            // Applied in order by `prepare`, against the candidate state.
            Operation::UpdateFigureLink { .. } => {}
            Operation::RemoveNode { node_id } => {
                let is_picture = all_blocks(body).into_iter().any(|b| matches!(b, FlowBlock::Image(n) if n.id == *node_id));
                let inserted_here = operations
                    .iter()
                    .any(|o| matches!(o, Operation::InsertFlowBlock { block: FlowBlock::Image(n), .. } if n.id == *node_id));
                if !is_picture && !inserted_here {
                    return Err(diag::unsupported("remove_node is supported for pictures only in HWPX"));
                }
            }
            other => return Err(unsupported_op(other)),
        }
    }
    Ok(())
}

fn check_insert_block(block: &FlowBlock, assets: &dyn AssetStore) -> DltResult<()> {
    match block {
        FlowBlock::Paragraph(p) => {
            if p.runs.iter().any(|r| r.protected) {
                return Err(diag::unsupported("inline objects can only be inserted as separate blocks in HWPX"));
            }
            if p.runs.iter().any(|r| r.text.contains('\t') || r.text.contains('\n')) {
                return Err(diag::unsupported("tab and line-break characters cannot be inserted as text in HWPX"));
            }
            Ok(())
        }
        FlowBlock::Image(node) => {
            let asset = node.asset.as_ref().ok_or_else(|| diag::unsupported("image block without an asset"))?;
            if !SUPPORTED_IMAGE_MIMES.contains(&asset.mime.as_str()) {
                return Err(diag::unsupported(format!("image type {} cannot be embedded in HWPX", asset.mime)));
            }
            let bytes = assets.read(asset).map_err(|_| diag::invalid(diag::UNSUPPORTED_OPERATION, "the image asset is not available"))?;
            if designllm_contracts::sha256(&bytes) != asset.sha256 {
                return Err(diag::invalid(diag::UNSUPPORTED_OPERATION, "the image bytes do not match their asset hash"));
            }
            Ok(())
        }
        FlowBlock::Table(t) => {
            check_grid(t)?;
            for cell in t.rows.iter().flatten() {
                for inner in &cell.blocks {
                    match inner {
                        FlowBlock::Opaque { .. } => {
                            return Err(diag::unsupported("opaque blocks cannot be inserted into a table cell"))
                        }
                        other => check_insert_block(other, assets)?,
                    }
                }
            }
            Ok(())
        }
        FlowBlock::Opaque { .. } => Err(diag::unsupported("inserting opaque blocks is not supported in HWPX")),
    }
}

/// Rounds a native-grid value and records the change when it moved.
fn round_grid(value: i64, node: &str, what: &str, diags: &mut Vec<Diagnostic>) -> DltResult<i64> {
    let applied = quantize(value, EMU_PER_HWPUNIT)?;
    if applied != value {
        diags.push(diag::info(
            diag::UNIT_ROUNDING,
            format!("{what}: requested {value} EMU, applied {applied} EMU (native grid {EMU_PER_HWPUNIT} EMU)"),
            Some(node.to_string()),
        ));
    }
    Ok(applied)
}

fn round_style(style: &mut Style, node: &str, diags: &mut Vec<Diagnostic>) -> DltResult<()> {
    style.font_size = round_grid(style.font_size, node, "font size", diags)?;
    Ok(())
}

fn round_format(format: &mut ParagraphFormat, node: &str, diags: &mut Vec<Diagnostic>) -> DltResult<()> {
    format.space_before = round_grid(format.space_before, node, "space before", diags)?;
    format.space_after = round_grid(format.space_after, node, "space after", diags)?;
    format.indent = round_grid(format.indent, node, "indent", diags)?;
    Ok(())
}

fn round_blocks(blocks: &mut [FlowBlock], diags: &mut Vec<Diagnostic>) -> DltResult<()> {
    for block in blocks {
        match block {
            FlowBlock::Paragraph(p) => {
                let id = p.id.clone();
                for run in &mut p.runs {
                    round_style(&mut run.style, &id, diags)?;
                }
                round_format(&mut p.format, &id, diags)?;
            }
            FlowBlock::Table(t) => {
                let id = t.id.clone();
                for w in &mut t.column_widths {
                    *w = round_grid(*w, &id, "column width", diags)?;
                }
                for cell in t.rows.iter_mut().flatten() {
                    round_blocks(&mut cell.blocks, diags)?;
                }
            }
            FlowBlock::Image(node) => {
                let id = node.id.clone();
                node.bounds.width = round_grid(node.bounds.width, &id, "picture width", diags)?;
                node.bounds.height = round_grid(node.bounds.height, &id, "picture height", diags)?;
                if node.bounds.x != 0 || node.bounds.y != 0 {
                    // HWPX stores a picture as an inline block; its position is not an attribute.
                    diags.push(diag::info(
                        diag::UNMODELED_PRESERVED,
                        "picture position is not stored for inline pictures in HWPX; placed in the paragraph flow",
                        Some(id.clone()),
                    ));
                    node.bounds.x = 0;
                    node.bounds.y = 0;
                }
            }
            FlowBlock::Opaque { .. } => {}
        }
    }
    Ok(())
}

/// Native-grid rounding of every value the HWPX writer stores.
fn round_document(doc: &mut FlowDocument, diags: &mut Vec<Diagnostic>) -> DltResult<()> {
    for section in &mut doc.sections {
        let id = section.id.clone();
        let setup = &mut section.setup;
        setup.size = Size {
            width: round_grid(setup.size.width, &id, "page width", diags)?,
            height: round_grid(setup.size.height, &id, "page height", diags)?,
        };
        setup.margin_left = round_grid(setup.margin_left, &id, "left margin", diags)?;
        setup.margin_right = round_grid(setup.margin_right, &id, "right margin", diags)?;
        setup.margin_top = round_grid(setup.margin_top, &id, "top margin", diags)?;
        setup.margin_bottom = round_grid(setup.margin_bottom, &id, "bottom margin", diags)?;
        round_blocks(&mut section.blocks, diags)?;
    }
    for (name, style) in doc.styles.iter_mut() {
        round_style(style, name, diags)?;
    }
    Ok(())
}

/// Style values that HWPX cannot store must keep the model defaults. Picture and table styles are
/// never stored, so any change to them is refused rather than dropped.
fn check_representable(doc: &FlowDocument) -> DltResult<()> {
    let default = Style::default();
    let check = |style: &Style| -> DltResult<()> {
        if style.fill != default.fill || style.stroke != default.stroke || style.stroke_width != default.stroke_width || style.align != default.align {
            return Err(diag::unsupported("fill, stroke, and alignment styles cannot be stored in HWPX text"));
        }
        Ok(())
    };
    for style in doc.styles.values() {
        check(style)?;
    }
    for block in all_blocks(doc) {
        match block {
            FlowBlock::Paragraph(p) => {
                for run in &p.runs {
                    check(&run.style)?;
                }
                if !align_is_supported(&p.format.align) {
                    return Err(diag::unsupported(format!("alignment {} is not supported by HWPX", p.format.align)));
                }
            }
            FlowBlock::Image(node) => {
                if node.style != default {
                    return Err(diag::unsupported("picture styles cannot be stored in HWPX"));
                }
            }
            FlowBlock::Table(t) => {
                if t.style != default {
                    return Err(diag::unsupported("table styles cannot be stored in HWPX"));
                }
            }
            FlowBlock::Opaque { .. } => {}
        }
    }
    Ok(())
}

/// Inserted tables get an empty paragraph in every empty cell, so the reparsed cell has content.
fn normalize_new_tables(doc: &mut FlowDocument, original: &HashSet<String>) {
    fn walk(blocks: &mut [FlowBlock], original: &HashSet<String>) {
        for block in blocks {
            if let FlowBlock::Table(t) = block {
                let is_new = !original.contains(&t.id);
                for cell in t.rows.iter_mut().flatten() {
                    if is_new && cell.blocks.is_empty() {
                        cell.blocks.push(FlowBlock::Paragraph(Paragraph {
                            id: new_id(),
                            runs: Vec::new(),
                            style_ref: None,
                            format: ParagraphFormat::default(),
                        }));
                    }
                    walk(&mut cell.blocks, original);
                }
            }
        }
    }
    for section in &mut doc.sections {
        walk(&mut section.blocks, original);
    }
}

/// Refusal for a block that the change would remove and HWPX cannot remove.
fn removal_refused(body: &FlowDocument, id: &str) -> DltError {
    if all_blocks(body).into_iter().any(|b| matches!(b, FlowBlock::Image(n) if n.id == id)) {
        diag::unsupported(format!(
            "removing picture {id} is not supported in HWPX: only a top-level picture that is the sole content of its paragraph, with a unique paragraph anchor, can be removed"
        ))
    } else {
        diag::unsupported(format!("the change would remove {id}, which HWPX cannot remove"))
    }
}

/// A paragraph whose runs a rewrite would drop keeps its runs: the candidate must not change them.
fn check_locked_runs(body: &FlowDocument, doc: &FlowDocument, locked: &HashSet<String>) -> DltResult<()> {
    if locked.is_empty() {
        return Ok(());
    }
    for block in all_blocks(doc) {
        let FlowBlock::Paragraph(p) = block else { continue };
        if !locked.contains(&p.id) {
            continue;
        }
        if let Some(original) = find_paragraph(body, &p.id) {
            if normalized_runs(&original.runs) != normalized_runs(&p.runs) {
                return Err(match restyled_by_named_style(body, doc, original, p) {
                    Some(name) => locked_runs_named_style(name, &p.id),
                    None => diag::locked_runs(&p.id),
                });
            }
        }
    }
    Ok(())
}

/// The named style whose change alone restyled a locked paragraph: the paragraph keeps its style, and its runs are the
/// original runs with only the fields they inherit from the style's old definition taken from the new one. Text or
/// direct character-style edits of the paragraph give `None`.
fn restyled_by_named_style<'d>(body: &FlowDocument, doc: &'d FlowDocument, original: &Paragraph, p: &'d Paragraph) -> Option<&'d str> {
    let name = p.style_ref.as_deref()?;
    if original.style_ref.as_deref() != Some(name) {
        return None;
    }
    let (old, new) = (body.styles.get(name)?, doc.styles.get(name)?);
    if old == new {
        return None;
    }
    let inherited = original
        .runs
        .iter()
        .map(|r| Some(TextRun { style: inherit(&r.style, old, new)?, ..r.clone() }))
        .collect::<Option<Vec<_>>>()?;
    (normalized_runs(&inherited) == normalized_runs(&p.runs)).then_some(name)
}

/// `style` with every field that equals the old named style taken from the new one, field by field over the serialized
/// style. The other fields are the run's direct formatting and stay.
fn inherit(style: &Style, old: &Style, new: &Style) -> Option<Style> {
    use serde_json::{to_value, Map, Value};
    let (Value::Object(own), Value::Object(old), Value::Object(new)) = (to_value(style).ok()?, to_value(old).ok()?, to_value(new).ok()?) else {
        return None;
    };
    let mut out = Map::new();
    for key in own.keys().chain(old.keys()).chain(new.keys()) {
        let value = if own.get(key) == old.get(key) { new.get(key) } else { own.get(key) };
        if let Some(value) = value {
            out.insert(key.clone(), value.clone());
        }
    }
    serde_json::from_value(Value::Object(out)).ok()
}

/// Refusal of a named style change that would restyle a paragraph whose runs carry unmodeled markup. HWPX keeps a run's
/// character shape on the run, so the change reaches the paragraph only through a rewrite of its runs.
fn locked_runs_named_style(name: &str, paragraph_id: &str) -> DltError {
    diag::error(
        ErrorCode::UnsafeRoundtrip,
        diag::UNSAFE_ROUNDTRIP,
        format!(
            "named style {name} cannot be changed: paragraph {paragraph_id} uses it, and HWPX keeps each run's character shape on the run (hp:run/@charPrIDRef), so the change would rewrite that paragraph's runs, which carry attributes or namespace declarations the model does not carry, and drop them"
        ),
    )
}

/// Applies `operations` to a copy of `body` and returns the candidate with its figure links and
/// diagnostics. Flow operations run in order. An `UpdateFigureLink` runs against the candidate state
/// left by the operations before it, and a failure anywhere refuses the whole change. The contract v1.2
/// guards run first, before the flow model applies anything.
pub fn prepare(body: &FlowDocument, operations: &[Operation], inputs: &ChangeInputs<'_>, lines: &mut NativeLines) -> DltResult<Prepared> {
    guard::check_operations(operations)?;
    guard::check_body(body)?;
    let original_ids = block_ids(body);
    let mut doc = body.clone();
    let mut figure_links = inputs.figure_links.to_vec();
    let mut changed_ids: Vec<String> = Vec::new();
    let context = LineContext::new(inputs.loaded);
    let mut diagnostics = Vec::new();
    let mut settled_ids = None;
    // Resolve an insertion before a later operation can change its predecessor. Forward insertions
    // in the same section can be resolved together in document order.
    let mut batches = operations.chunk_by(|previous, next| match previous {
        Operation::InsertFlowBlock { section_id, index, .. } => matches!(next, Operation::InsertFlowBlock {
            section_id: next_section, index: next_index, ..
        } if section_id == next_section && index < next_index),
        _ => true,
    }).peekable();
    loop {
        let batch = batches.next().unwrap_or_default();
        precheck(&doc, batch, inputs.assets)?;
        let mut start = 0;
        for (index, op) in batch.iter().enumerate() {
            if let Operation::UpdateFigureLink { link } = op {
                changed_ids.extend(designllm_flow::apply_flow_operations(&mut doc, &batch[start..index])?);
                start = index + 1;
                figure::apply_link(&mut doc, &mut figure_links, link, inputs.assets, &mut changed_ids)?;
            }
        }
        changed_ids.extend(designllm_flow::apply_flow_operations(&mut doc, &batch[start..])?);
        let known = settled_ids.as_ref().unwrap_or(&original_ids);
        normalize_new_tables(&mut doc, known);
        if batches.peek().is_none() {
            round_document(&mut doc, &mut diagnostics)?;
        }
        settle_line_spacing(&mut doc, known, &context, batch, lines, &mut diagnostics)?;
        if batches.peek().is_none() {
            break;
        }
        settled_ids = Some(block_ids(&doc));
    }
    let mut seen = HashSet::new();
    changed_ids.retain(|id| seen.insert(id.clone()));

    // Every block of the source must survive the change, except a picture the change removed on purpose
    // (the removable set admits only pictures that can be removed without renumbering other anchors).
    let after_ids = block_ids(&doc);
    if let Some(lost) = original_ids
        .iter()
        .find(|id| inputs.source_blocks.contains(*id) && !after_ids.contains(*id) && !inputs.removable_pictures.contains(*id))
    {
        return Err(removal_refused(body, lost));
    }
    for table in all_blocks(&doc).into_iter().filter_map(|b| match b {
        FlowBlock::Table(t) => Some(t),
        _ => None,
    }) {
        if !original_ids.contains(&table.id) {
            check_grid(table)?;
        }
    }
    check_locked_runs(body, &doc, inputs.locked_paragraphs)?;
    lines.retain(|id, _| after_ids.contains(id));
    check_representable(&doc)?;
    // A figure link follows its picture: a link whose picture the change removed goes with it.
    let pictures: HashSet<String> = all_blocks(&doc)
        .into_iter()
        .filter_map(|b| match b {
            FlowBlock::Image(n) => Some(n.id.clone()),
            _ => None,
        })
        .collect();
    figure_links.retain(|link| pictures.contains(&link.target_node_id));
    Ok(Prepared { body: doc, changed_ids, diagnostics, figure_links })
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::{TableCell, TextRun};

    #[test]
    fn canvas_page_sections_have_an_explicit_unsupported_operation_error() {
        let error = unsupported_op(&Operation::SetCanvasPageSections { expected: Vec::new(), sections: Vec::new() });
        assert_eq!(error.code, ErrorCode::UnsupportedOperation);
        assert!(error.message.contains("set_canvas_page_sections"));
    }

    fn table(widths: Vec<i64>, rows: Vec<Vec<(u32, u32)>>) -> Table {
        Table {
            id: "t".into(),
            column_widths: widths,
            rows: rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|(cs, rs)| TableCell { id: new_id(), blocks: vec![], column_span: cs, row_span: rs })
                        .collect()
                })
                .collect(),
            style: Style::default(),
        }
    }

    #[test]
    fn grid_requires_an_unspanned_cell_in_every_column() {
        assert!(check_grid(&table(vec![10, 10], vec![vec![(1, 1), (1, 1)]])).is_ok());
        assert!(check_grid(&table(vec![10, 10], vec![vec![(2, 1)]])).is_err());
    }

    #[test]
    fn protected_atoms_block_overlapping_replacements() {
        let runs = vec![
            TextRun::new("a", "가"),
            TextRun { protected: true, ..TextRun::new("b", text::OBJECT_CHAR) },
        ];
        assert!(text::overlaps_atom(&runs, 1, 2));
        assert!(!text::overlaps_atom(&runs, 0, 1));
    }

    #[test]
    fn rounding_records_the_applied_value() {
        let mut diags = Vec::new();
        let applied = round_grid(720_000, "n", "left margin", &mut diags).expect("round");
        assert_eq!(applied, 719_963);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, diag::UNIT_ROUNDING);
        assert!(diags[0].message.contains("requested 720000"));
    }
}
