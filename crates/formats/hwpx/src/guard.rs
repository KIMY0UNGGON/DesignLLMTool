//! Refusal guards for the contract v1.2 fields, kinds, interactions, and operations
//! (docs/P0_CONTRACT_V1_2.md section 4).
//!
//! HWPX stores none of these. Every entry point that changes, saves, lays out, or exports a document runs
//! these checks before it touches anything, and refuses with UNSUPPORTED_OPERATION. The message and the
//! blocking diagnostic name the refused fields, so a caller sees what was refused. A value that would be
//! dropped on save or render is never ignored (R09).
//!
//! The field lists come from the contract's own helpers (`Style::non_default_v12_fields`,
//! `StylePatch::set_fields`, `NodeExt::set_fields`, `LayoutSpec::v12_fields`), so a field added later is
//! refused as well. `check_operations` matches every `Operation` variant without a wildcard: a new variant
//! fails to compile until it is listed here.
use designllm_contracts::{
    DltError, DltResult, ErrorCode, FlowBlock, FlowDocument, Interaction, LayoutSpec, Node, NodeKind, Operation, Page,
    Style, StylePatch,
};

use crate::diag;

/// UNSUPPORTED_OPERATION whose message and blocking diagnostic name the refused fields.
fn refuse(subject: &str, names: &[String]) -> DltError {
    diag::error(
        ErrorCode::UnsupportedOperation,
        diag::UNSUPPORTED_OPERATION,
        format!("{subject} not supported for HWPX documents: {}", names.join(", ")),
    )
}

/// Refusal of an operation, naming the operation and the fields that it carries.
fn refuse_operation(name: &str, carried: &[&str]) -> DltError {
    let shown = if carried.is_empty() { name.to_string() } else { format!("{name} ({})", carried.join(", ")) };
    refuse("operation", &[shown])
}

/// A style patch. A non-empty `clear` is refused whatever it clears (docs/P0_CONTRACT_V1_2.md section 4a),
/// and so is a patch that sets a v1.2 field.
fn check_patch(patch: &StylePatch, subject: &str) -> DltResult<()> {
    if !patch.clear.is_empty() {
        let names: Vec<String> = patch.clear.iter().copied().map(|field| field.name().to_string()).collect();
        return Err(refuse("clear of style fields", &names));
    }
    let names: Vec<String> = patch.set_fields().into_iter().filter(|field| field.is_v12()).map(|field| field.name().to_string()).collect();
    if names.is_empty() {
        Ok(())
    } else {
        Err(refuse(subject, &names))
    }
}

/// A style that carries a v1.2 field with a value.
fn check_style(style: &Style, subject: &str) -> DltResult<()> {
    let names: Vec<String> = style.non_default_v12_fields().into_iter().map(|field| field.name().to_string()).collect();
    if names.is_empty() {
        Ok(())
    } else {
        Err(refuse(subject, &names))
    }
}

/// A layout with a v1.2 field, or with mode "grid".
fn check_layout(layout: &LayoutSpec) -> DltResult<()> {
    let names: Vec<String> = layout.v12_fields().into_iter().map(str::to_string).collect();
    if names.is_empty() {
        Ok(())
    } else {
        Err(refuse("layout fields", &names))
    }
}

/// The v1.2 interactions: `OpenUrl` and `Back`.
fn check_interaction(interaction: &Interaction) -> DltResult<()> {
    let name = match interaction {
        Interaction::OpenUrl { .. } => "open_url",
        Interaction::Back => "back",
        Interaction::NavigatePage { .. } | Interaction::ToggleVisibility { .. } | Interaction::SetVariant { .. } => return Ok(()),
    };
    Err(refuse("interaction", &[name.to_string()]))
}

/// A node: its kind, its v1.2 extension fields, its style, layout, interactions, variants, and children.
fn check_node(node: &Node) -> DltResult<()> {
    if node.kind == NodeKind::Path {
        return Err(refuse("node kind", &["NodeKind::Path".to_string()]));
    }
    if node.kind == NodeKind::Video {
        return Err(refuse("node kind", &["NodeKind::Video".to_string()]));
    }
    let fields: Vec<String> = node.ext.set_fields().into_iter().map(str::to_string).collect();
    if !fields.is_empty() {
        return Err(refuse("node fields", &fields));
    }
    check_style(&node.style, "style fields")?;
    if let Some(layout) = &node.layout {
        check_layout(layout)?;
    }
    for interaction in &node.interactions {
        check_interaction(interaction)?;
    }
    for patch in node.variants.values() {
        check_patch(patch, "variant style fields")?;
    }
    for child in &node.children {
        check_node(child)?;
    }
    Ok(())
}

/// A page: its name, and the nodes on it.
fn check_page(page: &Page) -> DltResult<()> {
    if page.name.is_some() {
        return Err(refuse("page fields", &["Page.name".to_string()]));
    }
    for node in &page.nodes {
        check_node(node)?;
    }
    Ok(())
}

/// A picture block carries only what HWPX writes: its asset and its size (`writer.rs` `image_paragraph`).
/// Anything else on the node would be dropped on save, so it is refused here, naming each field. The
/// position is kept: the flow places inline pictures and records the position as an information
/// diagnostic (`ops.rs` `round_blocks`).
fn check_picture(node: &Node) -> DltResult<()> {
    let mut names: Vec<String> = Vec::new();
    if node.kind != NodeKind::Image {
        names.push(format!("kind {:?}", node.kind));
    }
    if !node.text.is_empty() {
        names.push("text (alt text)".to_string());
    }
    if node.style != Style::default() {
        names.push("style".to_string());
    }
    if !node.children.is_empty() {
        names.push("children".to_string());
    }
    if node.layout.is_some() {
        names.push("layout".to_string());
    }
    if !node.interactions.is_empty() {
        names.push("interactions".to_string());
    }
    if !node.variants.is_empty() {
        names.push("variants".to_string());
    }
    if names.is_empty() {
        Ok(())
    } else {
        Err(refuse("picture fields", &names))
    }
}

/// A flow block: the styles of its runs and table, and the nodes and fields of a picture, recursively.
fn check_block(block: &FlowBlock) -> DltResult<()> {
    match block {
        FlowBlock::Paragraph(paragraph) => {
            for run in &paragraph.runs {
                check_style(&run.style, "style fields")?;
            }
            Ok(())
        }
        FlowBlock::Table(table) => {
            check_style(&table.style, "style fields")?;
            for cell in table.rows.iter().flatten() {
                for inner in &cell.blocks {
                    check_block(inner)?;
                }
            }
            Ok(())
        }
        FlowBlock::Image(node) => {
            check_node(node)?;
            check_picture(node)
        }
        FlowBlock::Opaque { .. } => Ok(()),
    }
}

/// Refuses every operation that carries a contract v1.2 field, kind, interaction, or operation. Run before
/// the flow model applies any operation, so a refused change leaves the document as it was.
pub fn check_operations(operations: &[Operation]) -> DltResult<()> {
    for op in operations {
        match op {
            Operation::SetStyle { patch, .. } | Operation::SetTextStyleRange { patch, .. } | Operation::SetNamedStyle { patch, .. } => {
                check_patch(patch, "style fields")?
            }
            Operation::SetVariantStyle { patch, .. } => check_patch(patch, "variant style fields")?,
            Operation::AddNode { node, .. } | Operation::AddDiagramNode { node, .. } => check_node(node)?,
            Operation::InsertFlowBlock { block, .. } => check_block(block)?,
            Operation::InsertPage { page, .. } => check_page(page)?,
            Operation::SetLayout { layout, .. } => check_layout(layout)?,
            Operation::SetInteraction { interaction, .. } => check_interaction(interaction)?,
            Operation::SetInteractions { .. } => return Err(refuse_operation("set_interactions", &["interactions"])),
            Operation::SetPageSize { .. } => return Err(refuse_operation("set_page_size", &["size"])),
            Operation::MoveNode { .. } => return Err(refuse_operation("move_node", &[])),
            Operation::SetZOrder { .. } => return Err(refuse_operation("set_z_order", &[])),
            Operation::SetNodeAppearance { image_fit, locked, .. } => {
                let mut carried = Vec::new();
                if image_fit.is_some() {
                    carried.push("image_fit");
                }
                if locked.is_some() {
                    carried.push("locked");
                }
                return Err(refuse_operation("set_node_appearance", &carried));
            }
            Operation::SetNodeInfo { alt, name, hidden, .. } => {
                let mut carried = Vec::new();
                if alt.is_some() {
                    carried.push("alt");
                }
                if name.is_some() {
                    carried.push("name");
                }
                if hidden.is_some() {
                    carried.push("hidden");
                }
                return Err(refuse_operation("set_node_info", &carried));
            }
            Operation::SetNodeList { .. } => return Err(refuse_operation("set_node_list", &["list"])),
            Operation::SetNodePath { .. } => return Err(refuse_operation("set_node_path", &["path"])),
            Operation::SetCanvasTableStructure { .. } => return Err(refuse_operation("set_canvas_table_structure", &["table"])),
            Operation::SetCanvasTableColumnWidths { .. } => return Err(refuse_operation("set_canvas_table_column_widths", &["table column widths"])),
            Operation::SetCanvasImageCrop { .. } => return Err(refuse_operation("set_canvas_image_crop", &["image crop"])),
            Operation::SetPageRole { .. } => return Err(refuse_operation("set_page_role", &["component page role"])),
            Operation::InstantiateComponent { .. } => return Err(refuse_operation("instantiate_component", &["component instance"])),
            Operation::DetachInstance { .. } => return Err(refuse_operation("detach_instance", &["component instance"])),
            Operation::RenamePage { .. } => return Err(refuse_operation("rename_page", &["name"])),
            Operation::SetGroupRepeat { .. } => return Err(refuse_operation("set_group_repeat", &["repeat group"])),
            Operation::SetGroupCondition { .. } => return Err(refuse_operation("set_group_condition", &["group condition"])),
            Operation::SetTheme { .. } => return Err(refuse_operation("set_theme", &["slide theme"])),
            // No contract v1.2 content: these operations are checked by the operation layer.
            Operation::ReplaceText { .. }
            | Operation::SetText { .. }
            | Operation::SetGeometry { .. }
            | Operation::RemoveNode { .. }
            | Operation::SetParagraphFormat { .. }
            | Operation::SetPageSetup { .. }
            | Operation::SetTableDimensions { .. }
            | Operation::RemovePage { .. }
            | Operation::ReorderPages { .. }
            | Operation::SetPageNotes { .. }
            | Operation::ConnectDiagram { .. }
            | Operation::AddPanel { .. }
            | Operation::RemovePanel { .. }
            | Operation::ReorderPanels { .. }
            | Operation::SetCaption { .. }
            | Operation::PutEvidence { .. }
            | Operation::SetPlot { .. }
            | Operation::SetPlotDisplay { .. }
            | Operation::SetAxisLabel { .. }
            | Operation::SetAxisTicks { .. }
            | Operation::PutDataset { .. }
            | Operation::ArrangePanels { .. }
            | Operation::SetPanelLabel { .. }
            | Operation::UpdateFigureLink { .. }
            | Operation::ReplaceWebSource { .. }
            | Operation::MappedWebEdit { .. }
            | Operation::PutToken { .. }
            | Operation::RemoveToken { .. }
            | Operation::SetTokenMode { .. }
            | Operation::BindToken { .. }
            | Operation::UnbindToken { .. }
              | Operation::SetCanvasBoardMemos { .. }
              | Operation::SetCanvasPageSections { .. }
              | Operation::SetCanvasPageGuides { .. }
            | Operation::SetDiagramProvenance { .. } | Operation::SetCommonLegend { .. } | Operation::SetFigureVisualReview { .. }
            | Operation::SetFigureImageTransform { .. } | Operation::SetFigureImageCalibration { .. }
            | Operation::ReplaceImageAsset { .. } => {}
        }
    }
    Ok(())
}

/// Refuses a document body that carries a contract v1.2 field or kind (named styles, runs, tables, pictures).
/// Run before a save, a render, or an export, so no such value is written or drawn without notice.
pub fn check_body(document: &FlowDocument) -> DltResult<()> {
    for style in document.styles.values() {
        check_style(style, "style fields")?;
    }
    for section in &document.sections {
        for block in &section.blocks {
            check_block(block)?;
        }
    }
    Ok(())
}
