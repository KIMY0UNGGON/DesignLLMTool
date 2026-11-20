//! Refusals for contract v1.2 operations and values that the slides engine does not support yet (P0). The whole change
//! is checked before any operation is applied, so a refused change leaves the artifact as it was. Each refusal names
//! the operation or the fields in the message and in the diagnostic. Nothing is dropped silently (R09).

use designllm_contracts::{DltError, DltResult, Operation, Page};
use designllm_pptx::guard::{check_node, check_page, check_patch, check_shadows, refuse};

/// Used in every slides refusal: "... is not supported in slides yet".
const CONTEXT: &str = "in slides";

/// Refuses the change when an operation carries a v1.2 value or is a v1.2 operation. Runs before any operation is
/// applied.
pub(crate) fn check_operations(operations: &[Operation], pages: &[Page]) -> DltResult<()> {
    operations.iter().try_for_each(|operation| check_operation(operation, pages))
}

/// Refuses the pages of a model that carry a v1.2 value. Runs on the candidate state, and before layout and export.
pub(crate) fn check_pages(pages: &[Page]) -> DltResult<()> {
    pages.iter().try_for_each(|page| check_page(page, CONTEXT))
}

fn check_operation(operation: &Operation, pages: &[Page]) -> DltResult<()> {
    match operation {
        Operation::SetStyle { node_ids, patch } => {
            let start = patch.stroke_start.is_some() || patch.clear.contains(&designllm_contracts::StyleField::StrokeStart);
            let end = patch.stroke_end.is_some() || patch.clear.contains(&designllm_contracts::StyleField::StrokeEnd);
            if start || end {
                let names = [("stroke_start", start), ("stroke_end", end)]
                    .into_iter()
                    .filter_map(|(name, set)| set.then_some(name))
                    .collect::<Vec<_>>()
                    .join(", ");
                for id in node_ids {
                    if crate::find_node(pages, id).is_some_and(|(_, node)| node.kind != designllm_contracts::NodeKind::Line) {
                        return Err(refuse(format!("style {names} applies only to line connectors"), Some(id)));
                    }
                }
            }
            // StrokeStart/StrokeEnd are exact for connector lines. The operation applies the patch only to Line nodes;
            // the candidate page guard rejects those fields on every other kind before commit.
            // A shadow depends on the kind of each node, so it is checked per node. Clearing a shadow is always allowed.
            if let Some(shadows) = &patch.shadows {
                for node_id in node_ids {
                    if let Some((_, node)) = crate::find_node(pages, node_id) {
                        check_shadows(shadows, node.kind, node_id, CONTEXT)?;
                    }
                }
            }
            let mut remaining = patch.clone();
            remaining.stroke_start = None;
            remaining.stroke_end = None;
            remaining.shadows = None;
            remaining.clear.retain(|field| {
                !matches!(
                    field,
                    designllm_contracts::StyleField::StrokeStart
                        | designllm_contracts::StyleField::StrokeEnd
                        | designllm_contracts::StyleField::Shadows
                )
            });
            check_patch(&remaining, CONTEXT, node_ids.first().map(String::as_str))
        }
        Operation::SetTextStyleRange { range, patch } => check_patch(patch, CONTEXT, Some(range.paragraph_id.as_str())),
        Operation::AddNode { node, .. } => check_node(node, CONTEXT),
        Operation::InsertPage { page, .. } => check_page(page, CONTEXT),
        Operation::SetPageSize { .. }
        | Operation::MoveNode { .. }
        | Operation::SetZOrder { .. }
        | Operation::SetNodeAppearance { .. }
        | Operation::SetNodeInfo { .. }
        | Operation::SetNodeList { .. }
        | Operation::SetNodePath { .. }
        | Operation::SetCanvasTableColumnWidths { .. }
        | Operation::SetCanvasPageSections { .. }
        | Operation::SetCanvasPageGuides { .. }
        | Operation::SetCanvasImageCrop { .. }
        | Operation::SetInteractions { .. }
        | Operation::RenamePage { .. }
        | Operation::SetPageRole { .. }
        | Operation::InstantiateComponent { .. }
        | Operation::DetachInstance { .. }
        | Operation::SetGroupRepeat { .. }
        | Operation::SetGroupCondition { .. }
        // Figure operations: refused here, before any slide is changed, not left to the operation layer.
        | Operation::SetDiagramProvenance { .. }
        | Operation::SetCommonLegend { .. }
        | Operation::SetFigureVisualReview { .. }
        | Operation::SetFigureImageTransform { .. }
        | Operation::SetFigureImageCalibration { .. } => Err(refuse_operation(operation)),
        _ => Ok(()),
    }
}

/// The name of a v1.2 operation (its JSON `op`), the fields it sets, and the node it targets.
fn refuse_operation(operation: &Operation) -> DltError {
    let name = operation.name();
    let fields = operation_fields(operation);
    let message = if fields.is_empty() {
        format!("operation {name} is not supported {CONTEXT} yet")
    } else {
        format!("operation {name} is not supported {CONTEXT} yet: {}", fields.join(", "))
    };
    refuse(message, operation_node(operation))
}

fn operation_fields(operation: &Operation) -> Vec<&'static str> {
    let set = |pairs: Vec<(&'static str, bool)>| -> Vec<&'static str> {
        pairs.into_iter().filter(|(_, is_set)| *is_set).map(|(name, _)| name).collect()
    };
    match operation {
        Operation::SetNodeAppearance { image_fit, locked, .. } => {
            set(vec![("image_fit", image_fit.is_some()), ("locked", locked.is_some())])
        }
        Operation::SetNodeInfo { alt, name, hidden, .. } => {
            set(vec![("alt", alt.is_some()), ("name", name.is_some()), ("hidden", hidden.is_some())])
        }
        Operation::SetNodeList { list, .. } => set(vec![("list", list.is_some())]),
        Operation::SetNodePath { .. } => vec!["path"],
        Operation::SetCanvasTableColumnWidths { .. } => vec!["column_widths"],
        Operation::SetCanvasImageCrop { .. } => vec!["image crop"],
        Operation::SetInteractions { .. } => vec!["interactions"],
        Operation::RenamePage { name, .. } => set(vec![("name", name.is_some())]),
        _ => Vec::new(),
    }
}

fn operation_node(operation: &Operation) -> Option<&str> {
    match operation {
        Operation::MoveNode { node_id, .. } | Operation::SetZOrder { node_id, .. } => Some(node_id.as_str()),
        Operation::SetNodeAppearance { node_ids, .. } | Operation::SetNodeInfo { node_ids, .. } => {
            node_ids.first().map(String::as_str)
        }
        Operation::SetNodeList { node_id, .. } | Operation::SetNodePath { node_id, .. } => Some(node_id.as_str()),
        Operation::SetCanvasTableColumnWidths { table_id, .. } => Some(table_id.as_str()),
        Operation::SetCanvasImageCrop { node_id, .. } => Some(node_id.as_str()),
        Operation::SetCanvasPageGuides { page_id, .. } => Some(page_id.as_str()),
        Operation::SetCanvasPageSections { sections, expected } => sections.first().or(expected.first()).map(|section| section.start_page_id.as_str()),
        Operation::SetInteractions { node_id, .. } => Some(node_id.as_str()),
        Operation::SetGroupRepeat { node_id, .. } | Operation::SetGroupCondition { node_id, .. } => Some(node_id.as_str()),
        _ => None,
    }
}
