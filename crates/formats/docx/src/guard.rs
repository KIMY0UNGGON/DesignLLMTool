//! P0 refusal guards (docs/P0_CONTRACT_V1_2.md §4 and §4a), and the v1.1 values that DOCX does not store (R09).
//!
//! Contract v1.2 adds fields, operations, and variants that the DOCX engine does not support yet. The engine
//! copies fields one at a time, so a new value would be dropped without an error (R09). Every change passes
//! through [`check_operations`] before anything is applied. A v1.2 value, a non-empty `clear`, an operation, or an
//! interaction that this engine cannot perform is refused with UNSUPPORTED_OPERATION. The same holds for a v1.1
//! value that the writer would not store (a picture's layout, variants, interactions, children or other style
//! fields, a new run's fill, stroke or alignment, a table's fill, font or alignment). The message and the
//! diagnostics name each item. A picture's alt text is not refused: it is written as `wp:docPr descr`.

use designllm_contracts::{
    DltError, DltResult, ErrorCode, FlowBlock, Id, Interaction, LayoutSpec, Node, NodeKind, Operation, Style, StyleField, StylePatch,
};
use designllm_package::{is_cfb_container, opc, Package};

use crate::diag;

/// Opens bytes accepted by the DOCX adapter, refusing CFB inputs and OPC-signed packages first.
pub(crate) fn open_input_package(bytes: &[u8]) -> DltResult<Package> {
    if is_cfb_container(bytes) {
        return Err(refuse_input(
            diag::COMPOUND_CONTAINER_UNSUPPORTED,
            "DOCX input uses the OLE Compound File container, not DOCX ZIP; it may be password-encrypted Office or a legacy binary Word document, neither is supported",
        ));
    }
    let package = Package::open(bytes)?;
    if opc::has_digital_signature(&package)? {
        return Err(refuse_input(
            diag::DIGITAL_SIGNATURE_UNSUPPORTED,
            "DOCX package has OPC digital-signature markers; this engine cannot validate or renew signatures, so it is refused",
        ));
    }
    Ok(package)
}

fn refuse_input(code: &str, message: &str) -> DltError {
    let mut error = DltError::new(ErrorCode::UnsupportedOperation, message);
    error.diagnostics.push(diag::blocking(code, message, None));
    error
}

/// Refuses every v1.2 field, operation, and variant in `operations`. `Ok` only when all of them are v1.1.
/// Refusals are listed in operation order, so one error names all of them.
pub fn check_operations(operations: &[Operation]) -> DltResult<()> {
    let mut refusals = Refusals::default();
    for (index, operation) in operations.iter().enumerate() {
        check_operation(operation, index, &mut refusals);
    }
    refusals.into_result()
}

#[derive(Default)]
struct Refusals {
    items: Vec<Refusal>,
}

struct Refusal {
    index: usize,
    label: String,
    node_id: Option<Id>,
}

impl Refusals {
    fn add(&mut self, index: usize, label: impl Into<String>, node_id: Option<&Id>) {
        self.items.push(Refusal { index, label: label.into(), node_id: node_id.cloned() });
    }

    fn into_result(self) -> DltResult<()> {
        if self.items.is_empty() {
            return Ok(());
        }
        let listed: Vec<String> = self.items.iter().map(|r| format!("[{}] {}", r.index, r.label)).collect();
        let mut error = DltError::unsupported(format!(
            "DOCX does not support these v1.2 fields or operations yet, so nothing was applied: {}",
            listed.join(", ")
        ));
        error.diagnostics = self
            .items
            .iter()
            .map(|r| {
                diag::blocking(diag::UNSUPPORTED_FIELD, format!("{} is not supported by the DOCX engine yet", r.label), r.node_id.as_ref())
            })
            .collect();
        let mut ids: Vec<Id> = self.items.iter().filter_map(|r| r.node_id.clone()).collect();
        ids.sort();
        ids.dedup();
        error.affected_ids = ids;
        Err(error)
    }
}

fn check_operation(operation: &Operation, index: usize, out: &mut Refusals) {
    match operation {
        Operation::SetStyle { node_ids, patch } => check_patch(patch, "", index, single(node_ids), out),
        Operation::SetTextStyleRange { range, patch } => check_patch(patch, "", index, Some(&range.paragraph_id), out),
        Operation::SetNamedStyle { patch, .. } => check_patch(patch, "", index, None, out),
        Operation::SetVariantStyle { node_id, patch, .. } => check_patch(patch, "", index, Some(node_id), out),
        Operation::SetLayout { node_id, layout } => check_layout(layout, index, Some(node_id), out),
        Operation::SetInteraction { node_id, interaction } => check_interaction(interaction, index, Some(node_id), out),
        Operation::SetInteractions { node_id, .. } => out.add(index, "operation set_interactions", Some(node_id)),
        Operation::AddNode { node, .. } | Operation::AddDiagramNode { node, .. } => check_node(node, index, out),
        Operation::InsertFlowBlock { block, .. } => check_block(block, index, out),
        Operation::InsertPage { page, .. } => {
            if page.name.is_some() {
                out.add(index, "Page.name", Some(&page.id));
            }
            for node in &page.nodes {
                check_node(node, index, out);
            }
        }
        Operation::SetPageSize { page_id, .. } => out.add(index, "operation set_page_size", Some(page_id)),
        Operation::MoveNode { node_id, .. } => out.add(index, "operation move_node", Some(node_id)),
        Operation::SetZOrder { node_id, .. } => out.add(index, "operation set_z_order", Some(node_id)),
        Operation::SetNodeAppearance { node_ids, .. } => out.add(index, "operation set_node_appearance", single(node_ids)),
        Operation::SetNodeInfo { node_ids, .. } => out.add(index, "operation set_node_info", single(node_ids)),
        Operation::SetNodeList { node_id, .. } => out.add(index, "operation set_node_list", Some(node_id)),
        Operation::SetNodePath { node_id, .. } => out.add(index, "operation set_node_path", Some(node_id)),
        Operation::SetCanvasTableStructure { table_id, .. } => out.add(index, "operation set_canvas_table_structure", Some(table_id)),
        Operation::SetCanvasTableColumnWidths { table_id, .. } => out.add(index, "operation set_canvas_table_column_widths", Some(table_id)),
        Operation::SetCanvasImageCrop { node_id, .. } => out.add(index, "operation set_canvas_image_crop", Some(node_id)),
        Operation::SetPageRole { page_id, .. } => out.add(index, "operation set_page_role", Some(page_id)),
        Operation::InstantiateComponent { instance_id, .. } => out.add(index, "operation instantiate_component", Some(instance_id)),
        Operation::DetachInstance { instance_id } => out.add(index, "operation detach_instance", Some(instance_id)),
        Operation::RenamePage { page_id, .. } => out.add(index, "operation rename_page", Some(page_id)),
        Operation::SetGroupRepeat { node_id, .. } => out.add(index, "operation set_group_repeat", Some(node_id)),
        Operation::SetGroupCondition { node_id, .. } => out.add(index, "operation set_group_condition", Some(node_id)),
        Operation::SetTheme { .. } => out.add(index, "operation set_theme", None),
        // No v1.2 payload is read here. ConnectDiagram and AddPanel carry a Style inside their edges and panels, but
        // the engine refuses those operations as unsupported before it reads them. Every variant is listed (no
        // wildcard), so a new operation fails to compile until someone decides what to check.
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
        | Operation::AddPanel { .. }
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

/// A patch: each v1.2 field it sets, and every field it clears. P0 refuses any non-empty `clear`, whatever
/// the field (docs/P0_CONTRACT_V1_2.md §4a).
fn check_patch(patch: &StylePatch, scope: &str, index: usize, node_id: Option<&Id>, out: &mut Refusals) {
    for field in &patch.clear {
        out.add(index, format!("{scope}StylePatch.clear.{}", style_key(*field)), node_id);
    }
    for field in patch.set_fields() {
        if field.is_v12() {
            out.add(index, format!("{scope}StylePatch.{}", style_key(field)), node_id);
        }
    }
}

/// A style value: each v1.2 field that carries a value.
fn check_style(style: &Style, index: usize, node_id: Option<&Id>, out: &mut Refusals) {
    for field in style.non_default_v12_fields() {
        out.add(index, format!("Style.{}", style_key(field)), node_id);
    }
}

fn check_layout(layout: &LayoutSpec, index: usize, node_id: Option<&Id>, out: &mut Refusals) {
    for name in layout.v12_fields() {
        out.add(index, format!("LayoutSpec.{name}"), node_id);
    }
}

fn check_interaction(interaction: &Interaction, index: usize, node_id: Option<&Id>, out: &mut Refusals) {
    match interaction {
        Interaction::OpenUrl { .. } => out.add(index, "Interaction.open_url", node_id),
        Interaction::Back => out.add(index, "Interaction.back", node_id),
        Interaction::NavigatePage { .. } | Interaction::ToggleVisibility { .. } | Interaction::SetVariant { .. } => {}
    }
}

/// A node and everything it carries: its kind, its NodeExt fields, its style, layout, interactions, variants,
/// and children.
fn check_node(node: &Node, index: usize, out: &mut Refusals) {
    let id = Some(&node.id);
    if node.kind == NodeKind::Path {
        out.add(index, "NodeKind.path", id);
    }
    if node.kind == NodeKind::Video {
        out.add(index, "NodeKind.video", id);
    }
    for name in node.ext.set_fields() {
        out.add(index, format!("NodeExt.{name}"), id);
    }
    check_style(&node.style, index, id, out);
    if let Some(layout) = &node.layout {
        check_layout(layout, index, id, out);
    }
    for interaction in &node.interactions {
        check_interaction(interaction, index, id, out);
    }
    for (variant, patch) in &node.variants {
        check_patch(patch, &format!("variant {variant}: "), index, id, out);
    }
    for child in &node.children {
        check_node(child, index, out);
    }
}

/// A flow block: the styles of its runs and tables, and the nodes of its pictures, recursively.
fn check_block(block: &FlowBlock, index: usize, out: &mut Refusals) {
    match block {
        FlowBlock::Paragraph(paragraph) => {
            for run in &paragraph.runs {
                check_style(&run.style, index, Some(&run.id), out);
                check_run_v11(&run.style, index, Some(&run.id), out);
            }
        }
        FlowBlock::Table(table) => {
            check_style(&table.style, index, Some(&table.id), out);
            check_table_v11(&table.style, index, Some(&table.id), out);
            for row in &table.rows {
                for cell in row {
                    for child in &cell.blocks {
                        check_block(child, index, out);
                    }
                }
            }
        }
        FlowBlock::Image(node) => {
            check_node(node, index, out);
            check_picture_v11(node, index, out);
        }
        FlowBlock::Opaque { .. } => {}
    }
}

/// v1.1 values of a new text run that the writer does not keep. Its run style is rebuilt from the font, size, color,
/// bold and italic only (ops.rs, prepare_new_block). A value equal to the default is stored as the default, so it is
/// not a loss; any other value is refused.
fn check_run_v11(style: &Style, index: usize, node_id: Option<&Id>, out: &mut Refusals) {
    let base = Style::default();
    if style.fill != base.fill {
        out.add(index, "run Style.fill", node_id);
    }
    if style.stroke != base.stroke {
        out.add(index, "run Style.stroke", node_id);
    }
    if style.stroke_width != base.stroke_width {
        out.add(index, "run Style.stroke_width", node_id);
    }
    if style.align != base.align {
        out.add(index, "run Style.align", node_id);
    }
}

/// v1.1 values of a new table's style that the writer does not keep. `new_table` writes the border only: `stroke` and
/// `stroke_width`, and only when `stroke` is a `#RRGGBB` color. The fill, the font and the alignment are not written.
fn check_table_v11(style: &Style, index: usize, node_id: Option<&Id>, out: &mut Refusals) {
    let base = Style::default();
    let not_written = [
        ("fill", style.fill != base.fill),
        ("font_family", style.font_family != base.font_family),
        ("font_size", style.font_size != base.font_size),
        ("color", style.color != base.color),
        ("bold", style.bold != base.bold),
        ("italic", style.italic != base.italic),
        ("align", style.align != base.align),
    ];
    for (name, differs) in not_written {
        if differs {
            out.add(index, format!("table Style.{name}"), node_id);
        }
    }
    if style.stroke.len() != 7 {
        out.add(index, "table Style.stroke", node_id);
        if style.stroke_width != base.stroke_width {
            out.add(index, "table Style.stroke_width", node_id);
        }
    }
}

/// v1.1 values of a new picture that the writer does not keep. `new_picture` writes the extent, the alignment, the
/// image and the alt text (`descr`). The picture's other style fields, its layout, variants, interactions and children
/// are not written. `bounds.x` and `bounds.y` are not refused: the flow places the picture, and existing callers send
/// them (tests/integration figure_links.rs and p0_baseline.rs).
fn check_picture_v11(node: &Node, index: usize, out: &mut Refusals) {
    let id = Some(&node.id);
    if !matches!(node.kind, NodeKind::Image | NodeKind::Path) {
        out.add(index, format!("Node.kind={}", kind_key(node.kind)), id);
    }
    let base = Style::default();
    let style = &node.style;
    let not_written = [
        ("fill", style.fill != base.fill),
        ("stroke", style.stroke != base.stroke),
        ("stroke_width", style.stroke_width != base.stroke_width),
        ("font_family", style.font_family != base.font_family),
        ("font_size", style.font_size != base.font_size),
        ("color", style.color != base.color),
        ("bold", style.bold != base.bold),
        ("italic", style.italic != base.italic),
    ];
    for (name, differs) in not_written {
        if differs {
            out.add(index, format!("picture Style.{name}"), id);
        }
    }
    if node.layout.is_some() {
        out.add(index, "Node.layout", id);
    }
    if !node.variants.is_empty() {
        out.add(index, "Node.variants", id);
    }
    if !node.interactions.is_empty() {
        out.add(index, "Node.interactions", id);
    }
    if !node.children.is_empty() {
        out.add(index, "Node.children", id);
    }
}

/// The name of a node kind, as the contract spells it.
fn kind_key(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Text => "text",
        NodeKind::Rectangle => "rectangle",
        NodeKind::Ellipse => "ellipse",
        NodeKind::Line => "line",
        NodeKind::Image => "image",
        NodeKind::Group => "group",
        NodeKind::Video => "video",
        NodeKind::Path => "path",
    }
}

/// The id of an operation that names exactly one node; `None` when it names several.
fn single(ids: &[Id]) -> Option<&Id> {
    match ids {
        [only] => Some(only),
        _ => None,
    }
}

/// The JSON name of a Style field. `StylePatch` and `Style` use the same names, except that the contract's
/// `StyleField::Opacity` is stored as `opacity_milli`.
fn style_key(field: StyleField) -> &'static str {
    use StyleField as F;
    match field {
        F::FontFamily => "font_family",
        F::FontSize => "font_size",
        F::Color => "color",
        F::Fill => "fill",
        F::Stroke => "stroke",
        F::StrokeWidth => "stroke_width",
        F::Bold => "bold",
        F::Italic => "italic",
        F::Align => "align",
        F::CornerRadius => "corner_radius",
        F::Opacity => "opacity_milli",
        F::Shadows => "shadows",
        F::Gradient => "gradient",
        F::LineHeight => "line_height_milli",
        F::LetterSpacing => "letter_spacing",
        F::VerticalAlign => "vertical_align",
        F::FontWeight => "font_weight",
        F::StrokeDash => "stroke_dash",
        F::StrokeCap => "stroke_cap",
        F::StrokeJoin => "stroke_join",
        F::Underline => "underline",
        F::Strikethrough => "strikethrough",
        F::StrokeStart => "stroke_start",
        F::StrokeEnd => "stroke_end",
    }
}
