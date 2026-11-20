//! A style patch that sets and clears the same field (contract v1.2, `StylePatch::check`) is
//! malformed input: core refuses it as INVALID_ARGUMENT, naming each patch by JSON pointer, before
//! any engine sees the change set. Every place a StylePatch can occur is walked: the four style
//! operations and the `variants` of every node an operation carries, nested ones included.
use crate::errors::{invalid, ErrExt};
use crate::strict::MAX_LISTED;
use designllm_contracts::*;

/// `at` is the pointer of the change set inside the request ("" or "/change_set").
pub(crate) fn check_style_patches(operations: &[Operation], at: &str) -> DltResult<()> {
    let mut patches = Vec::new();
    for (index, op) in operations.iter().enumerate() { operation_patches(op, format!("{at}/operations/{index}"), &mut patches); }
    let conflicts: Vec<(String, Vec<String>)> = patches.into_iter()
        .filter(|(_, patch)| patch.check().is_err())
        .map(|(pointer, patch)| (pointer, patch.set_fields().intersection(&patch.clear).map(|f| f.name().to_string()).collect()))
        .collect();
    if conflicts.is_empty() { return Ok(()); }
    let total = conflicts.len();
    let listed: Vec<String> = conflicts.iter().take(MAX_LISTED).map(|(pointer, fields)| format!("{pointer} ({})", fields.join(", "))).collect();
    let more = if total > MAX_LISTED { format!(" and {} more", total - MAX_LISTED) } else { String::new() };
    Err(invalid(format!("{total} style patch(es) set and clear the same field; nothing was done: {}{more}", listed.join(", ")))
        .diags(conflicts.into_iter().take(MAX_LISTED).map(|(pointer, _)| Diagnostic { code: "style_field_conflict".into(), message: pointer, node_id: None, blocking: true, params: Default::default() }).collect()))
}

/// Every variant is listed (no wildcard): a new operation has to say where its patches are.
fn operation_patches<'a>(op: &'a Operation, at: String, out: &mut Vec<(String, &'a StylePatch)>) {
    use Operation::*;
    match op {
        SetStyle { patch, .. } | SetTextStyleRange { patch, .. } | SetNamedStyle { patch, .. } | SetVariantStyle { patch, .. } => out.push((format!("{at}/patch"), patch)),
        AddNode { node, .. } | AddDiagramNode { node, .. } => node_patches(node, format!("{at}/node"), out),
        InsertPage { page, .. } => { for (i, node) in page.nodes.iter().enumerate() { node_patches(node, format!("{at}/page/nodes/{i}"), out); } }
        InsertFlowBlock { block, .. } => block_patches(block, format!("{at}/block"), out),
        AddPanel { panel, .. } => panel_patches(&panel.body, format!("{at}/panel/body"), out),
        ReplaceText { .. } | SetText { .. } | SetGeometry { .. } | RemoveNode { .. } | SetParagraphFormat { .. } | SetPageSetup { .. }
        | SetTableDimensions { .. } | RemovePage { .. } | ReorderPages { .. } | SetPageNotes { .. } | ConnectDiagram { .. } | RemovePanel { .. }
        | ReorderPanels { .. } | SetCaption { .. } | PutEvidence { .. } | SetPlot { .. } | SetPlotDisplay { .. } | SetAxisLabel { .. } | SetAxisTicks { .. } | PutDataset { .. }
        | ArrangePanels { .. } | SetPanelLabel { .. } | UpdateFigureLink { .. } | SetLayout { .. } | SetInteraction { .. } | SetInteractions { .. } | ReplaceWebSource { .. }
        | MappedWebEdit { .. } | SetPageSize { .. } | MoveNode { .. } | SetZOrder { .. } | SetNodeAppearance { .. } | SetNodeInfo { .. } | ReplaceImageAsset { .. }
        | SetCanvasImageCrop { .. }
        // SetNodeList is a content operation and carries no StylePatch to validate here.
        | SetNodeList { .. } | SetNodePath { .. } | SetCanvasTableStructure { .. } | SetCanvasTableColumnWidths { .. } | SetCanvasBoardMemos { .. }
        | SetCanvasPageSections { .. } | SetCanvasPageGuides { .. }
        | RenamePage { .. } | SetPageRole { .. } | InstantiateComponent { .. } | DetachInstance { .. }
        | PutToken { .. } | RemoveToken { .. } | SetTokenMode { .. }
        | BindToken { .. } | UnbindToken { .. }
        | SetDiagramProvenance { .. } | SetCommonLegend { .. } | SetFigureVisualReview { .. }
        | SetFigureImageTransform { .. } | SetFigureImageCalibration { .. }
        | SetGroupRepeat { .. } | SetGroupCondition { .. }
        // SetTheme carries theme colours and fonts, not a StylePatch.
        | SetTheme { .. } => {}
    }
}

fn node_patches<'a>(node: &'a Node, at: String, out: &mut Vec<(String, &'a StylePatch)>) {
    for (name, patch) in &node.variants { out.push((format!("{at}/variants/{}", escape(name)), patch)); }
    for (i, child) in node.children.iter().enumerate() { node_patches(child, format!("{at}/children/{i}"), out); }
}

/// FlowBlock is `{"type", "value"}`: an image block's node, and every block inside table cells.
fn block_patches<'a>(block: &'a FlowBlock, at: String, out: &mut Vec<(String, &'a StylePatch)>) {
    match block {
        FlowBlock::Image(node) => node_patches(node, format!("{at}/value"), out),
        FlowBlock::Table(table) => {
            for (r, row) in table.rows.iter().enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    for (b, inner) in cell.blocks.iter().enumerate() { block_patches(inner, format!("{at}/value/rows/{r}/{c}/blocks/{b}"), out); }
                }
            }
        }
        FlowBlock::Paragraph(_) | FlowBlock::Opaque { .. } => {}
    }
}

/// PanelBody is `{"type", "value"}`: the nodes of a diagram panel.
fn panel_patches<'a>(body: &'a PanelBody, at: String, out: &mut Vec<(String, &'a StylePatch)>) {
    match body {
        PanelBody::Diagram(diagram) => { for (i, node) in diagram.nodes.iter().enumerate() { node_patches(node, format!("{at}/value/nodes/{i}"), out); } }
        PanelBody::Plot(_) | PanelBody::Image(_) => {}
    }
}

/// RFC 6901 escaping of one pointer segment.
fn escape(key: &str) -> String { key.replace('~', "~0").replace('/', "~1") }
