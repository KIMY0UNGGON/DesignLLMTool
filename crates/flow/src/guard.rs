//! Contract v1.2 refusal guard (P0, docs/P0_CONTRACT_V1_2.md §4 and §4a). Flow documents support
//! none of the v1.2 additions yet, so every one of them is refused by name with
//! UNSUPPORTED_OPERATION before anything changes (R09: nothing is dropped silently):
//! v1.2 style fields with a value, any non-empty `StylePatch.clear`, `NodeKind::Path`, `NodeExt`
//! fields, `Page.name`, the v1.2 `LayoutSpec` fields and mode "grid", `Interaction::{OpenUrl, Back}`
//! and the six v1.2 operations. Shared by the flow operations, flow layout/validation and the
//! format adapters (HWP).
use designllm_contracts::*;

/// Diagnostic code of a v1.2 refusal (same as the renderer's).
pub const V12_UNSUPPORTED: &str = designllm_render::v12::V12_UNSUPPORTED;

/// One refused item: the node (or page/section) it belongs to and the v1.2 names found there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V12Finding { pub node_id: Option<Id>, pub names: Vec<String> }

fn push(out: &mut Vec<V12Finding>, node_id: Option<&str>, names: Vec<String>) {
    if !names.is_empty() { out.push(V12Finding { node_id: node_id.map(str::to_string), names }); }
}

/// v1.2 style fields that carry a value, prefixed ("style.corner_radius").
pub fn style_v12(style: &Style, prefix: &str) -> Vec<String> {
    style.non_default_v12_fields().into_iter().map(|f| format!("{prefix}{}", f.name())).collect()
}

/// v1.2 fields a patch sets, and its `clear` set (refused whole in P0, whatever it names).
pub fn patch_v12(patch: &StylePatch, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = patch.set_fields().into_iter().filter(|f| f.is_v12()).map(|f| format!("{prefix}{}", f.name())).collect();
    if !patch.clear.is_empty() {
        out.push(format!("{prefix}clear [{}]", patch.clear.iter().map(|f| f.name()).collect::<Vec<_>>().join(", ")));
    }
    out
}

fn interaction_v12(interaction: &Interaction) -> Option<&'static str> {
    match interaction {
        Interaction::OpenUrl { .. } => Some("interaction open_url"),
        Interaction::Back => Some("interaction back"),
        Interaction::NavigatePage { .. } | Interaction::ToggleVisibility { .. } | Interaction::SetVariant { .. } => None,
    }
}

/// v1.2 content of a node and its children: kind path, style, NodeExt, layout, interactions, variants.
pub fn node_v12(node: &Node, out: &mut Vec<V12Finding>) {
    let mut names = Vec::new();
    if node.kind == NodeKind::Path { names.push("kind path".to_string()); }
    if node.kind == NodeKind::Video { names.push("kind video".to_string()); }
    names.extend(style_v12(&node.style, "style."));
    names.extend(node.ext.set_fields().into_iter().map(str::to_string));
    if let Some(layout) = &node.layout { names.extend(layout.v12_fields().into_iter().map(|f| format!("layout.{f}"))); }
    names.extend(node.interactions.iter().filter_map(interaction_v12).map(str::to_string));
    for (variant, patch) in &node.variants { names.extend(patch_v12(patch, &format!("variants.{variant}."))); }
    push(out, Some(&node.id), names);
    for child in &node.children { node_v12(child, out); }
}

/// v1.2 content of flow blocks: run styles, table styles, cells, image nodes.
pub fn blocks_v12(blocks: &[FlowBlock], out: &mut Vec<V12Finding>) {
    for block in blocks {
        match block {
            FlowBlock::Paragraph(p) => for run in &p.runs { push(out, Some(&run.id), style_v12(&run.style, "style.")); },
            FlowBlock::Table(t) => {
                push(out, Some(&t.id), style_v12(&t.style, "style."));
                for row in &t.rows { for cell in row { blocks_v12(&cell.blocks, out); } }
            }
            FlowBlock::Image(node) => node_v12(node, out),
            FlowBlock::Opaque { .. } => {}
        }
    }
}

/// v1.2 content of a whole flow document (blocks and named styles).
pub fn document_v12(document: &FlowDocument) -> Vec<V12Finding> {
    let mut out = Vec::new();
    for section in &document.sections { blocks_v12(&section.blocks, &mut out); }
    for (name, style) in &document.styles { push(&mut out, None, style_v12(style, &format!("named style {name}: style."))); }
    out
}

/// v1.2 content of one operation: the v1.2 operations by name, and every Style, StylePatch,
/// Node, Page, LayoutSpec or Interaction the operation carries.
pub fn operation_v12(op: &Operation) -> Vec<V12Finding> {
    let mut out = Vec::new();
    let whole = |out: &mut Vec<V12Finding>, name: &str, node: Option<&str>| push(out, node, vec![format!("operation {name}")]);
    match op {
        Operation::SetStyle { node_ids, patch } => push(&mut out, node_ids.first().map(String::as_str), patch_v12(patch, "patch.")),
        Operation::SetTextStyleRange { range, patch } => push(&mut out, Some(&range.paragraph_id), patch_v12(patch, "patch.")),
        Operation::SetNamedStyle { name, patch, .. } => push(&mut out, None, patch_v12(patch, &format!("named style {name}: patch."))),
        Operation::SetVariantStyle { node_id, patch, .. } => push(&mut out, Some(node_id), patch_v12(patch, "patch.")),
        Operation::AddNode { node, .. } | Operation::AddDiagramNode { node, .. } => node_v12(node, &mut out),
        Operation::InsertFlowBlock { block, .. } => blocks_v12(std::slice::from_ref(block), &mut out),
        Operation::InsertPage { page, .. } => {
            if page.name.is_some() { push(&mut out, Some(&page.id), vec!["page.name".to_string()]); }
            for node in &page.nodes { node_v12(node, &mut out); }
        }
        Operation::SetLayout { node_id, layout } => push(&mut out, Some(node_id), layout.v12_fields().into_iter().map(|f| format!("layout.{f}")).collect()),
        Operation::SetInteraction { node_id, interaction } => push(&mut out, Some(node_id), interaction_v12(interaction).map(str::to_string).into_iter().collect()),
        Operation::SetInteractions { node_id, .. } => whole(&mut out, "set_interactions", Some(node_id)),
        Operation::AddPanel { panel, .. } => {
            push(&mut out, Some(&panel.id), style_v12(&panel.style, "style."));
            if let PanelBody::Diagram(d) = &panel.body {
                for node in &d.nodes { node_v12(node, &mut out); }
                for edge in &d.edges { push(&mut out, Some(&edge.id), style_v12(&edge.style, "style.")); }
            }
        }
        Operation::ConnectDiagram { edge, .. } => push(&mut out, Some(&edge.id), style_v12(&edge.style, "style.")),
        Operation::SetPageSize { page_id, .. } => whole(&mut out, "set_page_size", Some(page_id)),
        Operation::MoveNode { node_id, .. } => whole(&mut out, "move_node", Some(node_id)),
        Operation::SetZOrder { node_id, .. } => whole(&mut out, "set_z_order", Some(node_id)),
        Operation::SetNodeAppearance { node_ids, .. } => whole(&mut out, "set_node_appearance", node_ids.first().map(String::as_str)),
        Operation::SetNodeInfo { node_ids, .. } => whole(&mut out, "set_node_info", node_ids.first().map(String::as_str)),
        Operation::SetNodeList { node_id, .. } => whole(&mut out, "set_node_list", Some(node_id)),
        Operation::SetNodePath { node_id, .. } => whole(&mut out, "set_node_path", Some(node_id)),
        Operation::SetCanvasTableStructure { table_id, .. } => whole(&mut out, "set_canvas_table_structure", Some(table_id)),
        Operation::SetCanvasTableColumnWidths { table_id, .. } => whole(&mut out, "set_canvas_table_column_widths", Some(table_id)),
        Operation::SetCanvasImageCrop { node_id, .. } => whole(&mut out, "set_canvas_image_crop", Some(node_id)),
        Operation::SetPageRole { page_id, .. } => whole(&mut out, "set_page_role", Some(page_id)),
        Operation::InstantiateComponent { instance_id, .. } => whole(&mut out, "instantiate_component", Some(instance_id)),
        Operation::DetachInstance { instance_id } => whole(&mut out, "detach_instance", Some(instance_id)),
        Operation::RenamePage { page_id, .. } => whole(&mut out, "rename_page", Some(page_id)),
        Operation::SetGroupRepeat { node_id, .. } => whole(&mut out, "set_group_repeat", Some(node_id)),
        Operation::SetGroupCondition { node_id, .. } => whole(&mut out, "set_group_condition", Some(node_id)),
        // A slides theme: no flow node carries it.
        Operation::SetTheme { .. } => whole(&mut out, "set_theme", None),
        Operation::ReplaceText { .. } | Operation::SetText { .. } | Operation::SetGeometry { .. } | Operation::RemoveNode { .. }
        | Operation::SetParagraphFormat { .. } | Operation::SetPageSetup { .. } | Operation::SetTableDimensions { .. }
        | Operation::RemovePage { .. } | Operation::ReorderPages { .. } | Operation::SetPageNotes { .. } | Operation::RemovePanel { .. }
        | Operation::ReorderPanels { .. } | Operation::SetCaption { .. } | Operation::PutEvidence { .. } | Operation::SetPlot { .. } | Operation::SetPlotDisplay { .. }
        | Operation::SetAxisLabel { .. } | Operation::SetAxisTicks { .. } | Operation::PutDataset { .. } | Operation::ArrangePanels { .. }
        | Operation::SetPanelLabel { .. } | Operation::UpdateFigureLink { .. } | Operation::ReplaceWebSource { .. } | Operation::MappedWebEdit { .. }
        | Operation::PutToken { .. } | Operation::RemoveToken { .. } | Operation::SetTokenMode { .. }
        | Operation::BindToken { .. } | Operation::UnbindToken { .. }
          | Operation::ReplaceImageAsset { .. } | Operation::SetCanvasBoardMemos { .. }
          | Operation::SetCanvasPageSections { .. } | Operation::SetCanvasPageGuides { .. }
        | Operation::SetDiagramProvenance { .. } | Operation::SetCommonLegend { .. } | Operation::SetFigureVisualReview { .. }
        | Operation::SetFigureImageTransform { .. } | Operation::SetFigureImageCalibration { .. } => {}
    }
    out
}

/// UNSUPPORTED_OPERATION naming every v1.2 field or operation found (one blocking diagnostic per
/// node, `affected_ids` = those nodes); Ok when there is none. `who` names the engine.
pub fn refuse_v12(who: &str, found: Vec<(Option<usize>, V12Finding)>) -> DltResult<()> {
    if found.is_empty() { return Ok(()); }
    let detail = found.iter().map(|(k, f)| {
        let at = match (k, &f.node_id) { (Some(k), Some(n)) => format!("operation {k} ({n})"), (Some(k), None) => format!("operation {k}"), (None, Some(n)) => n.clone(), (None, None) => "document".into() };
        format!("{at}: {}", f.names.join(", "))
    }).collect::<Vec<_>>().join("; ");
    let mut e = DltError::unsupported(format!("{who} do not support these contract v1.2 fields or operations yet: {detail}"));
    e.affected_ids = found.iter().filter_map(|(_, f)| f.node_id.clone()).collect();
    e.affected_ids.dedup();
    e.diagnostics = found.into_iter().map(|(_, f)| Diagnostic { code: V12_UNSUPPORTED.into(), message: format!("{who} do not support {} yet", f.names.join(", ")), node_id: f.node_id, blocking: true, params: Default::default() }).collect();
    Err(e)
}

/// Refuse a change set that carries any v1.2 content (call before changing anything).
pub fn refuse_v12_operations(who: &str, operations: &[Operation]) -> DltResult<()> {
    refuse_v12(who, operations.iter().enumerate().flat_map(|(k, op)| operation_v12(op).into_iter().map(move |f| (Some(k), f))).collect())
}

/// Blocking diagnostics for v1.2 content found in a document (validation).
pub fn document_v12_diagnostics(who: &str, document: &FlowDocument) -> Vec<Diagnostic> {
    match refuse_v12(who, document_v12(document).into_iter().map(|f| (None, f)).collect()) { Ok(()) => vec![], Err(e) => e.diagnostics }
}
