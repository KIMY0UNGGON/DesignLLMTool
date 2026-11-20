//! Node lookup inside a FlowDocument (sections, blocks, runs, tables, cells, images, opaque).
use designllm_contracts::*;
use std::collections::BTreeSet;

/// Read-only view of a node found by id.
#[derive(Debug, Clone, Copy)]
pub enum FlowNode<'a> {
    Section(&'a Section),
    Paragraph(&'a Paragraph),
    Run { paragraph: &'a Paragraph, index: usize },
    Table(&'a Table),
    Cell { table: &'a Table, row: usize, index: usize, cell: &'a TableCell },
    Image(&'a Node),
    Opaque { id: &'a str, label: &'a str },
}

/// Find any node by id.
pub fn find_node<'a>(document: &'a FlowDocument, id: &str) -> Option<FlowNode<'a>> {
    for section in &document.sections {
        if section.id == id { return Some(FlowNode::Section(section)); }
        if let Some(found) = find_in_blocks(&section.blocks, id) { return Some(found); }
    }
    None
}

fn find_in_blocks<'a>(blocks: &'a [FlowBlock], id: &str) -> Option<FlowNode<'a>> {
    for block in blocks {
        match block {
            FlowBlock::Paragraph(p) => {
                if p.id == id { return Some(FlowNode::Paragraph(p)); }
                if let Some(index) = p.runs.iter().position(|r| r.id == id) { return Some(FlowNode::Run { paragraph: p, index }); }
            }
            FlowBlock::Table(t) => {
                if t.id == id { return Some(FlowNode::Table(t)); }
                for (r, row) in t.rows.iter().enumerate() {
                    for (c, cell) in row.iter().enumerate() {
                        if cell.id == id { return Some(FlowNode::Cell { table: t, row: r, index: c, cell }); }
                        if let Some(found) = find_in_blocks(&cell.blocks, id) { return Some(found); }
                    }
                }
            }
            FlowBlock::Image(node) => if node.id == id { return Some(FlowNode::Image(node)); },
            FlowBlock::Opaque { id: oid, label } => if oid == id { return Some(FlowNode::Opaque { id: oid, label }); },
        }
    }
    None
}

/// Find a paragraph (top level or inside table cells).
pub fn find_paragraph<'a>(document: &'a FlowDocument, id: &str) -> Option<&'a Paragraph> {
    match find_node(document, id) { Some(FlowNode::Paragraph(p)) => Some(p), _ => None }
}

pub fn find_paragraph_mut<'a>(document: &'a mut FlowDocument, id: &str) -> Option<&'a mut Paragraph> {
    for section in document.sections.iter_mut() {
        if let Some(p) = paragraph_in_blocks_mut(&mut section.blocks, id) { return Some(p); }
    }
    None
}

fn paragraph_in_blocks_mut<'a>(blocks: &'a mut [FlowBlock], id: &str) -> Option<&'a mut Paragraph> {
    for block in blocks.iter_mut() {
        match block {
            FlowBlock::Paragraph(p) => if p.id == id { return Some(p); },
            FlowBlock::Table(t) => {
                for row in t.rows.iter_mut() {
                    for cell in row.iter_mut() {
                        if let Some(p) = paragraph_in_blocks_mut(&mut cell.blocks, id) { return Some(p); }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Paragraph that owns run `run_id`, with the run index.
pub fn find_run_mut<'a>(document: &'a mut FlowDocument, run_id: &str) -> Option<(&'a mut Paragraph, usize)> {
    for section in document.sections.iter_mut() {
        if let Some(found) = run_in_blocks_mut(&mut section.blocks, run_id) { return Some(found); }
    }
    None
}

fn run_in_blocks_mut<'a>(blocks: &'a mut [FlowBlock], id: &str) -> Option<(&'a mut Paragraph, usize)> {
    for block in blocks.iter_mut() {
        match block {
            FlowBlock::Paragraph(p) => {
                if let Some(index) = p.runs.iter().position(|r| r.id == id) { return Some((p, index)); }
            }
            FlowBlock::Table(t) => {
                for row in t.rows.iter_mut() {
                    for cell in row.iter_mut() {
                        if let Some(found) = run_in_blocks_mut(&mut cell.blocks, id) { return Some(found); }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

pub fn find_table_mut<'a>(document: &'a mut FlowDocument, id: &str) -> Option<&'a mut Table> {
    for section in document.sections.iter_mut() {
        if let Some(t) = table_in_blocks_mut(&mut section.blocks, id) { return Some(t); }
    }
    None
}

fn table_in_blocks_mut<'a>(blocks: &'a mut [FlowBlock], id: &str) -> Option<&'a mut Table> {
    for block in blocks.iter_mut() {
        if let FlowBlock::Table(t) = block {
            if t.id == id { return Some(t); }
            for row in t.rows.iter_mut() {
                for cell in row.iter_mut() {
                    if let Some(found) = table_in_blocks_mut(&mut cell.blocks, id) { return Some(found); }
                }
            }
        }
    }
    None
}

pub fn find_cell_mut<'a>(document: &'a mut FlowDocument, id: &str) -> Option<&'a mut TableCell> {
    for section in document.sections.iter_mut() {
        if let Some(c) = cell_in_blocks_mut(&mut section.blocks, id) { return Some(c); }
    }
    None
}

fn cell_in_blocks_mut<'a>(blocks: &'a mut [FlowBlock], id: &str) -> Option<&'a mut TableCell> {
    for block in blocks.iter_mut() {
        if let FlowBlock::Table(t) = block {
            for row in t.rows.iter_mut() {
                for cell in row.iter_mut() {
                    if cell.id == id { return Some(cell); }
                    if let Some(found) = cell_in_blocks_mut(&mut cell.blocks, id) { return Some(found); }
                }
            }
        }
    }
    None
}

pub fn find_image_mut<'a>(document: &'a mut FlowDocument, id: &str) -> Option<&'a mut Node> {
    for section in document.sections.iter_mut() {
        if let Some(n) = image_in_blocks_mut(&mut section.blocks, id) { return Some(n); }
    }
    None
}

fn image_in_blocks_mut<'a>(blocks: &'a mut [FlowBlock], id: &str) -> Option<&'a mut Node> {
    for block in blocks.iter_mut() {
        match block {
            FlowBlock::Image(node) => if node.id == id { return Some(node); },
            FlowBlock::Table(t) => {
                for row in t.rows.iter_mut() {
                    for cell in row.iter_mut() {
                        if let Some(found) = image_in_blocks_mut(&mut cell.blocks, id) { return Some(found); }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Remove a block (paragraph, table, image or opaque) wherever it is. Returns the block.
pub fn remove_block(document: &mut FlowDocument, id: &str) -> Option<FlowBlock> {
    for section in document.sections.iter_mut() {
        if let Some(b) = remove_in_blocks(&mut section.blocks, id) { return Some(b); }
    }
    None
}

fn remove_in_blocks(blocks: &mut Vec<FlowBlock>, id: &str) -> Option<FlowBlock> {
    if let Some(index) = blocks.iter().position(|b| block_id(b) == id) { return Some(blocks.remove(index)); }
    for block in blocks.iter_mut() {
        if let FlowBlock::Table(t) = block {
            for row in t.rows.iter_mut() {
                for cell in row.iter_mut() {
                    if let Some(found) = remove_in_blocks(&mut cell.blocks, id) { return Some(found); }
                }
            }
        }
    }
    None
}

/// Id of a block.
pub fn block_id(block: &FlowBlock) -> &str {
    match block {
        FlowBlock::Paragraph(p) => &p.id,
        FlowBlock::Table(t) => &t.id,
        FlowBlock::Image(n) => &n.id,
        FlowBlock::Opaque { id, .. } => id,
    }
}

/// All ids in document order (sections, blocks, runs, cells, nested blocks).
pub fn collect_ids(document: &FlowDocument) -> Vec<Id> {
    let mut out = Vec::new();
    for section in &document.sections {
        out.push(section.id.clone());
        collect_block_ids(&section.blocks, &mut out);
    }
    out
}

pub fn collect_block_ids(blocks: &[FlowBlock], out: &mut Vec<Id>) {
    for block in blocks {
        out.push(block_id(block).to_string());
        match block {
            FlowBlock::Paragraph(p) => out.extend(p.runs.iter().map(|r| r.id.clone())),
            FlowBlock::Table(t) => {
                for row in &t.rows {
                    for cell in row {
                        out.push(cell.id.clone());
                        collect_block_ids(&cell.blocks, out);
                    }
                }
            }
            FlowBlock::Image(n) => out.extend(n.children.iter().map(|c| c.id.clone())),
            FlowBlock::Opaque { .. } => {}
        }
    }
}

/// Visit every paragraph in document order.
pub fn for_each_paragraph<'a>(document: &'a FlowDocument, mut f: impl FnMut(&'a Paragraph)) {
    fn walk<'a>(blocks: &'a [FlowBlock], f: &mut impl FnMut(&'a Paragraph)) {
        for block in blocks {
            match block {
                FlowBlock::Paragraph(p) => f(p),
                FlowBlock::Table(t) => for row in &t.rows { for cell in row { walk(&cell.blocks, f); } },
                _ => {}
            }
        }
    }
    for section in &document.sections { walk(&section.blocks, &mut f); }
}

/// Visit every paragraph mutably.
pub fn for_each_paragraph_mut(document: &mut FlowDocument, mut f: impl FnMut(&mut Paragraph)) {
    fn walk(blocks: &mut [FlowBlock], f: &mut impl FnMut(&mut Paragraph)) {
        for block in blocks.iter_mut() {
            match block {
                FlowBlock::Paragraph(p) => f(p),
                FlowBlock::Table(t) => for row in t.rows.iter_mut() { for cell in row.iter_mut() { walk(&mut cell.blocks, f); } },
                _ => {}
            }
        }
    }
    for section in document.sections.iter_mut() { walk(&mut section.blocks, &mut f); }
}

/// Structural validation: empty or duplicate ids, table grid problems, images without assets,
/// unusable page setups. Each problem is a blocking diagnostic.
pub fn validate_document(document: &FlowDocument) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for id in collect_ids(document) {
        if id.is_empty() {
            out.push(diag("empty_id", "A flow node has an empty id", None, true));
        } else if !seen.insert(id.clone()) {
            out.push(diag("duplicate_id", format!("Id {id} is used more than once"), Some(id), true));
        }
    }
    for section in &document.sections {
        if let Some(problem) = page_setup_problem(&section.setup) {
            out.push(diag("invalid_page_setup", problem, Some(section.id.clone()), true));
        }
        validate_blocks(&section.blocks, &mut out);
    }
    out.extend(crate::guard::document_v12_diagnostics("Flow documents", document));
    out
}

fn validate_blocks(blocks: &[FlowBlock], out: &mut Vec<Diagnostic>) {
    for block in blocks {
        match block {
            FlowBlock::Table(t) => {
                if let Some(problem) = table_problem(t) { out.push(diag("table_grid_mismatch", problem, Some(t.id.clone()), true)); }
                for row in &t.rows { for cell in row { validate_blocks(&cell.blocks, out); } }
            }
            FlowBlock::Image(n) => {
                if n.asset.is_none() { out.push(diag("image_missing_asset", format!("Image {} has no asset", n.id), Some(n.id.clone()), true)); }
                if n.bounds.width <= 0 || n.bounds.height <= 0 { out.push(diag("image_invalid_size", format!("Image {} has a non-positive size", n.id), Some(n.id.clone()), true)); }
            }
            _ => {}
        }
    }
}

/// Problem with a page setup, if any.
pub fn page_setup_problem(setup: &PageSetup) -> Option<String> {
    if !setup.size.valid() { return Some(format!("Page size {}x{} EMU is not valid", setup.size.width, setup.size.height)); }
    let margins = [setup.margin_left, setup.margin_top, setup.margin_right, setup.margin_bottom];
    if margins.iter().any(|m| *m < 0) { return Some("Page margins must not be negative".into()); }
    if setup.margin_left.saturating_add(setup.margin_right) >= setup.size.width || setup.margin_top.saturating_add(setup.margin_bottom) >= setup.size.height {
        return Some("Page margins leave no content area".into());
    }
    None
}

/// Problem with a table grid (HTML-style spans: covered positions have no cell entry).
pub fn table_problem(table: &Table) -> Option<String> {
    if table.column_widths.iter().any(|w| *w <= 0) { return Some(format!("Table {} has a non-positive column width", table.id)); }
    let rows = table.rows.len();
    let cols = table.column_widths.len();
    let mut occupied = vec![vec![false; cols.max(1)]; rows];
    for (r, row) in table.rows.iter().enumerate() {
        let mut c = 0usize;
        for cell in row {
            if cell.column_span == 0 || cell.row_span == 0 { return Some(format!("Cell {} has a zero span", cell.id)); }
            while c < cols && occupied[r][c] { c += 1; }
            let cs = cell.column_span as usize;
            let rs = cell.row_span as usize;
            if cols > 0 && c + cs > cols { return Some(format!("Row {r} of table {} needs more than {cols} grid columns", table.id)); }
            if r + rs > rows { return Some(format!("Cell {} spans past the last row", cell.id)); }
            for rr in r..r + rs { for cc in c..(c + cs).min(cols) { occupied[rr][cc] = true; } }
            c += cs;
        }
    }
    None
}

pub(crate) fn diag(code: &str, message: impl Into<String>, node_id: Option<Id>, blocking: bool) -> Diagnostic {
    Diagnostic { code: code.into(), message: message.into(), node_id, blocking, params: Default::default() }
}
