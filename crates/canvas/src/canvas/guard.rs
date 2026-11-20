//! Refusal guards for contract v1.2 (docs/P0_CONTRACT_V1_2.md §4 and §4a, docs/P1_PLAN.md).
//!
//! The rules follow what the renderer draws (designllm-render `v12`). A value the renderer draws is open here; a value
//! it does not draw is refused by name: UNSUPPORTED_OPERATION, with each refused item in the message and in its own
//! blocking diagnostic. The refusal comes before anything is applied, so no value is dropped silently (R09).
//! OpenUrl and Back are supported for Canvas HTML output. Still refused by kind: text weight/decorations on non-text nodes, gradients on text, lines,
//! and images, shadows on text, lines, and paths, more than one shadow, stroke dash, cap, and join on images, and any v1.2
//! style on a group. A clear is open for every field canvas draws.

use crate::common::blocking;
use designllm_contracts::{
    CanvasDocument, Diagnostic, DltError, DltResult, ErrorCode, Gradient, Interaction, Node, NodeKind, Operation,
    Page, Style, StyleField, StylePatch,
};
use std::collections::BTreeSet;

/// Diagnostic code of a refused field, node kind, or interaction (the figure engine refuses with the same code).
pub(crate) use designllm_contracts::diag_codes::UNSUPPORTED_FIELD;

/// One refused item: the label that names it (for example `patch.underline`) and its node.
#[derive(Debug)]
struct Item {
    label: String,
    node_id: Option<String>,
}

/// Everything a request or a document refuses, collected before any change is made.
#[derive(Debug, Default)]
pub(crate) struct Refusals {
    items: Vec<Item>,
}

impl Refusals {
    fn field(&mut self, label: impl Into<String>, node_id: Option<&str>) {
        self.items.push(Item { label: label.into(), node_id: node_id.map(str::to_string) });
    }

    fn diagnostics(&self) -> Vec<Diagnostic> {
        self.items
            .iter()
            .map(|item| {
                blocking(
                    UNSUPPORTED_FIELD,
                    format!("{} is not supported by the canvas engine yet (contract v1.2)", item.label),
                    item.node_id.clone(),
                )
            })
            .collect()
    }

    /// UNSUPPORTED_OPERATION naming every refused label, one diagnostic per item, and the nodes involved.
    fn into_result(self) -> DltResult<()> {
        if self.items.is_empty() {
            return Ok(());
        }
        let labels: BTreeSet<&str> = self.items.iter().map(|item| item.label.as_str()).collect();
        let message = format!(
            "not supported by the canvas engine yet (contract v1.2): {}",
            labels.into_iter().collect::<Vec<_>>().join(", ")
        );
        let mut error = DltError::new(ErrorCode::UnsupportedOperation, message);
        error.affected_ids = self.items.iter().filter_map(|item| item.node_id.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        error.diagnostics = self.diagnostics();
        Err(error)
    }
}

/// A colour the renderer draws as opaque: `#RRGGBB`, or `#RRGGBBFF`.
fn opaque(color: &str) -> bool {
    let Some(hex) = color.strip_prefix('#') else {
        return false;
    };
    if !hex.is_ascii() {
        return false;
    }
    let rgb_ok = hex.len() == 6 || (hex.len() == 8 && hex[6..].eq_ignore_ascii_case("FF"));
    rgb_ok && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A gradient the renderer draws: exactly two opaque stops in 0..=1000.
fn gradient_drawable(gradient: &Gradient) -> bool {
    gradient.stops.len() == 2 && gradient.stops.iter().all(|stop| stop.pos_milli <= 1000 && opaque(&stop.color))
}

/// Field names of the values a style carries that the renderer does not draw for a node of this kind (`None`: a named
/// style, which no node draws as such). Values with no visible effect on a kind, such as a corner radius on text, are
/// accepted, as the renderer accepts them.
pub(crate) fn undrawn_style_fields(style: &Style, kind: Option<NodeKind>) -> Vec<&'static str> {
    if matches!(kind, Some(NodeKind::Group)) {
        // A group draws no style, so every v1.2 value on it would be dropped.
        return style.non_default_v12_fields().into_iter().map(|field| field.name()).collect();
    }
    let mut out = Vec::new();
    if kind != Some(NodeKind::Line) {
        if style.stroke_start.is_some() { out.push("stroke_start"); }
        if style.stroke_end.is_some() { out.push("stroke_end"); }
    }
    if matches!(kind, Some(NodeKind::Image | NodeKind::Video | NodeKind::Line | NodeKind::Path)) {
        if style.font_weight.is_some() { out.push("font_weight"); }
        if style.underline.is_some() { out.push("underline"); }
        if style.strikethrough.is_some() { out.push("strikethrough"); }
    }
    if let Some(gradient) = &style.gradient {
        if matches!(kind, Some(NodeKind::Text | NodeKind::Line | NodeKind::Image | NodeKind::Video)) || !gradient_drawable(gradient) {
            out.push("gradient");
        }
    }
    if matches!(kind, Some(NodeKind::Image | NodeKind::Video)) {
        // An image has no stroke of its own, so a dash, cap, or join would be dropped.
        if style.stroke_dash.is_some() {
            out.push("stroke_dash");
        }
        if style.stroke_cap.is_some() {
            out.push("stroke_cap");
        }
        if style.stroke_join.is_some() {
            out.push("stroke_join");
        }
    }
    if !style.shadows.is_empty() && (matches!(kind, Some(NodeKind::Text | NodeKind::Line | NodeKind::Path)) || style.shadows.len() > 1) {
        out.push("shadows");
    }
    out
}

/// Fields no primitive draws, plus text fields on a node with no text.
fn patch_undrawn(patch: &StylePatch, kind: Option<NodeKind>) -> Vec<&'static str> {
    let mut out = Vec::new();

    if matches!(kind, Some(NodeKind::Image | NodeKind::Video | NodeKind::Line | NodeKind::Path | NodeKind::Group)) {
        if patch.font_weight.is_some() { out.push("font_weight"); }
        if patch.underline.is_some() { out.push("underline"); }
        if patch.strikethrough.is_some() { out.push("strikethrough"); }
    }
    if patch.gradient.as_ref().is_some_and(|gradient| !gradient_drawable(gradient)) {
        out.push("gradient");
    }
    if patch.shadows.as_ref().is_some_and(|shadows| shadows.len() > 1) {
        out.push("shadows");
    }
    out
}

/// Every field a patch touches that is not drawn: a clear of font weight, underline, or strikethrough (`patch.clear.<field>`),
/// and each value `patch_undrawn` names (`patch.<field>`). A clear of any other field is open (contract v1.2 §4a).
fn patch_items(out: &mut Refusals, patch: &StylePatch, node_id: Option<&str>, kind: Option<NodeKind>) {
    for field in &patch.clear {
        if kind.is_some_and(|node_kind| node_kind != NodeKind::Line)
            && matches!(field, StyleField::StrokeStart | StyleField::StrokeEnd)
        {
            out.field(format!("patch.clear.{}", field.name()), node_id);
        }
        if matches!(kind, Some(NodeKind::Image | NodeKind::Video | NodeKind::Line | NodeKind::Path | NodeKind::Group))
            && matches!(field, StyleField::FontWeight | StyleField::Underline | StyleField::Strikethrough)
        {
            out.field(format!("patch.clear.{}", field.name()), node_id);
        }
    }
    for field in patch_undrawn(patch, kind) {
        out.field(format!("patch.{field}"), node_id);
    }
}

/// Back is a local HTML page-history action (contract v1.2, P2).
fn interaction_items(_out: &mut Refusals, interaction: &Interaction, _node_id: Option<&str>) {
    match interaction {
        Interaction::Back | Interaction::OpenUrl { .. } | Interaction::NavigatePage { .. } | Interaction::ToggleVisibility { .. } | Interaction::SetVariant { .. } => {}
    }
}

/// A node, its subtree, and everything it carries that the renderer does not draw: style values by kind, interactions,
/// and variants.
fn node_items(out: &mut Refusals, node: &Node) {
    let id = Some(node.id.as_str());
    if node.ext.list.is_some() && node.kind != NodeKind::Text {
        out.field("node.list", id);
    }
    // Math is a figure Diagram node field (open/18); the canvas renderer does not typeset it.
    if node.ext.math.is_some() {
        out.field("node.math", id);
    }
    for field in undrawn_style_fields(&node.style, Some(node.kind)) {
        out.field(format!("node.style.{field}"), id);
    }
    for interaction in &node.interactions {
        interaction_items(out, interaction, id);
    }
    for variant in node.variants.values() {
        patch_items(out, variant, id, Some(node.kind));
    }
    for child in &node.children {
        node_items(out, child);
    }
}

/// A page and its nodes.
fn page_items(out: &mut Refusals, page: &Page) {
    for node in &page.nodes {
        node_items(out, node);
    }
}

fn document_items(doc: &CanvasDocument) -> Refusals {
    let mut out = Refusals::default();
    for page in &doc.pages {
        page_items(&mut out, page);
    }
    for (name, style) in &doc.styles {
        for field in undrawn_style_fields(style, None) {
            out.field(format!("named_style.{name}.{field}"), None);
        }
    }
    out
}

fn operation_items(out: &mut Refusals, operation: &Operation) {
    match operation {
        Operation::SetStyle { node_ids, patch } => {
            if node_ids.is_empty() {
                patch_items(out, patch, None, None);
            }
            for id in node_ids {
                patch_items(out, patch, Some(id.as_str()), None);
            }
        }
        Operation::SetTextStyleRange { patch, .. } | Operation::SetNamedStyle { patch, .. } => patch_items(out, patch, None, None),
        Operation::SetVariantStyle { node_id, patch, .. } => patch_items(out, patch, Some(node_id.as_str()), None),
        Operation::AddNode { node, .. } => node_items(out, node),
        Operation::InsertPage { page, .. } => page_items(out, page),
        Operation::SetInteraction { node_id, interaction } => interaction_items(out, interaction, Some(node_id.as_str())),
        Operation::SetInteractions { node_id, interactions } => {
            for interaction in interactions {
                interaction_items(out, interaction, Some(node_id.as_str()));
            }
        }
        _ => {}
    }
}

/// Refuses an operation list before anything is applied: values the renderer does not draw, and cleared fields.
/// Kind-specific values are checked where the node is known (`check_style_kind`).
pub(crate) fn check_operations(operations: &[Operation]) -> DltResult<()> {
    let mut out = Refusals::default();
    for operation in operations {
        operation_items(&mut out, operation);
    }
    out.into_result()
}

fn nodes_have_table_spans(nodes: &[Node]) -> bool {
    let mut pending: Vec<&Node> = nodes.iter().collect();
    while let Some(node) = pending.pop() {
        if node.ext.table.as_ref().is_some_and(|table| !table.cell_spans.is_empty()) {
            return true;
        }
        pending.extend(node.children.iter());
    }
    false
}

fn document_has_table_spans(doc: &CanvasDocument) -> bool {
    doc.pages.iter().any(|page| nodes_have_table_spans(&page.nodes))
}

/// Refuses a document that carries a value the renderer does not draw. Used when a file is opened, a candidate is made, a
/// project is saved, exported, or laid out, so no path can drop such a value silently.
pub(crate) fn check_document(doc: &CanvasDocument) -> DltResult<()> {
    check_refused_values(doc)?;
    // Repeat planning reads table rows and cells, so with a repeat group the table shape is checked first too.
    if document_has_table_spans(doc) || super::repeat::has_repeat_groups(doc) {
        super::model::into_result(super::model::validate_document(doc))?;
    }
    super::model::into_result(super::components::validate_document(doc))?;
    super::model::into_result(super::repeat::validate_document(doc)).map(|_| ())
}

/// Only the refused values of `check_document` (no table, component, or repeat model validation), as the other engines
/// check their refused v1.2 values when a body is classified for storage (`persisted_features`).
pub(crate) fn check_refused_values(doc: &CanvasDocument) -> DltResult<()> {
    document_items(doc).into_result()
}

/// The same refusals as blocking diagnostics, for `validate`.
pub(crate) fn document_diagnostics(doc: &CanvasDocument) -> Vec<Diagnostic> {
    let mut diagnostics = document_items(doc).diagnostics();
    diagnostics.extend(super::components::validate_document(doc));
    diagnostics.extend(super::repeat::validate_document(doc));
    diagnostics
}

/// Refuses a patch that sets a value no kind draws, or clears any field.
pub(crate) fn check_patch(patch: &StylePatch) -> DltResult<()> {
    let mut out = Refusals::default();
    patch_items(&mut out, patch, None, None);
    out.into_result()
}

pub(crate) fn check_patch_kind(patch: &StylePatch, kind: NodeKind, node_id: &str) -> DltResult<()> {
    let mut out = Refusals::default();
    patch_items(&mut out, patch, Some(node_id), Some(kind));
    out.into_result()
}

/// Refuses a style that the node's kind does not draw (for example a gradient on text), after a change is applied to it.
pub(crate) fn check_style_kind(style: &Style, kind: NodeKind, node_id: &str) -> DltResult<()> {
    let mut out = Refusals::default();
    for field in undrawn_style_fields(style, Some(kind)) {
        out.field(format!("node.style.{field}"), Some(node_id));
    }
    out.into_result()
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::{CanvasTable, CanvasTableRow, CanvasTableSpan, LayoutSpec, NodeExt, Page, Rect, Size, EMU_PER_CSS_PX};

    fn nested_table_span_document() -> CanvasDocument {
        let cell = |id: &str| Node {
            id: id.into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x: 0, y: 0, width: 120 * EMU_PER_CSS_PX, height: 36 * EMU_PER_CSS_PX },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt::default(),
        };
        let table = Node {
            id: "table".into(),
            kind: NodeKind::Group,
            bounds: Rect { x: 0, y: 0, width: 240 * EMU_PER_CSS_PX, height: 36 * EMU_PER_CSS_PX },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![cell("anchor"), cell("covered")],
            layout: Some(LayoutSpec {
                mode: "grid".into(),
                height_mode: "hug".into(),
                grid_columns: Some(2),
                gap: 0,
                row_gap: Some(0),
                ..Default::default()
            }),
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt {
                table: Some(CanvasTable {
                    rows: vec![CanvasTableRow { id: "row".into(), cell_ids: vec!["anchor".into(), "covered".into()] }],
                    header_row_id: None,
                    column_widths: None,
                    cell_spans: [("anchor".into(), CanvasTableSpan { row_span: 1, column_span: 2 })].into_iter().collect(),
                }),
                ..NodeExt::default()
            },
        };
        let parent = Node {
            id: "parent".into(),
            kind: NodeKind::Group,
            bounds: Rect { x: 0, y: 0, width: 300 * EMU_PER_CSS_PX, height: 80 * EMU_PER_CSS_PX },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![table],
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt::default(),
        };
        CanvasDocument {
            pages: vec![Page { nodes: vec![parent], ..Page::new("page", Size::slide()) }],
            ..Default::default()
        }
    }

    #[test]
    fn non_span_document_keeps_the_legacy_document_guard_path() {
        let document = CanvasDocument {
            pages: vec![Page {
                nodes: vec![Node {
                    id: String::new(),
                    kind: NodeKind::Rectangle,
                    bounds: Rect { x: 0, y: 0, width: 1, height: 1 },
                    text: String::new(),
                    style: Style::default(),
                    asset: None,
                    children: Vec::new(),
                    layout: None,
                    interactions: Vec::new(),
                    variants: Default::default(),
                    ext: Default::default(),
                }],
                ..Page::new("page", Size::slide())
            }],
            ..Default::default()
        };

        assert!(check_document(&document).is_ok());
    }

    #[test]
    fn nested_span_validation_and_removal_restore_legacy_guard() {
        let mut document = nested_table_span_document();
        assert!(document_has_table_spans(&document), "nested table metadata must activate model validation");
        assert!(check_document(&document).is_ok(), "a nested span with valid IDs and empty covered content passes validation");

        document.pages[0].nodes[0].children[0].children[1].text = "must remain visible".into();
        assert!(check_document(&document).is_ok(), "nested covered plain text is validated and retained");
        document.pages[0].nodes[0].children[0].children[1].style.color = "#CC0000".into();
        let error = check_document(&document).expect_err("covered style differences remain blocked");
        assert!(error.diagnostics.iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_HIDDEN_DATA"));
        document.pages[0].nodes[0].children[0].children[1].style = Style::default();
        document.pages[0].nodes[0].children[0].children[1].text.clear();

        document.pages[0].nodes[0].children[0].ext.table.as_mut().unwrap().cell_spans.clear();
        document.pages[0].nodes[0].id = "<uuid-N>".into();
        assert!(!document_has_table_spans(&document), "removing the last span must restore the legacy guard path");
        assert!(super::super::model::validate_document(&document).iter().any(|diagnostic| diagnostic.code == "NODE_ID"));
        assert!(check_document(&document).is_ok(), "placeholder IDs remain outside the legacy renderer-refusal guard");
    }
}
