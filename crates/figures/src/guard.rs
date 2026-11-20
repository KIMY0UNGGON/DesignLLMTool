//! P0 refusal guards (docs/P0_CONTRACT_V1_2.md sections 4 and 4a) and the R09 rule for v1.1 values.
//!
//! A figure stores and renders the v1.1 model. Two kinds of item are refused with UNSUPPORTED_OPERATION:
//!
//! - v1.2 fields that the figure engine does not accept on the element they sit on (`field_policy`), variants, and
//!   operations that carry a value. A refused item is refused wherever it appears. Of the `NodeExt` fields, `math` is
//!   accepted on a diagram node of the text kind (a math node, typeset by math.rs) and refused everywhere else.
//! - v1.1 values that the engine stores but never renders or exports: diagram node `children`, `asset`, `layout`,
//!   `interactions` (NavigatePage, ToggleVisibility, SetVariant), and `variants`. A change may not add them. A stored
//!   project keeps them, so opening, laying out, and exporting it are unchanged. Opening and validating such a project
//!   adds one info diagnostic per value ("preserved, not rendered"); nothing is refused for it.
//!
//! The error names every refused item, and the blocking diagnostics name them one by one. Nothing is applied,
//! stored, or written before the check passes, so a refused change leaves the artifact as it was.
//!
//! The full rule set runs in `ops::apply` (each operation) and `FigureEngine::prepare_change` (the whole change,
//! before the first operation). Both get the document, so a patch can be checked against the element it changes. The
//! v1.2 rules alone run on stored documents: `validate::full` (layout, export, encode, validate), `native::parse` (a
//! project file, before any asset is written), and `inspect`, `content_digest`, and `restore`.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{
    Diagnostic, Diagram, DltError, DltResult, Edge, ErrorCode, FigureDocument, FigurePanel, Interaction, LayoutSpec, Node, NodeKind,
    Operation, PanelBody, Style, StyleField, StylePatch,
};

use crate::diag::{self, code};

/// Which rules apply. A stored document is checked for v1.2 values only, so existing data still opens. A change is
/// checked for everything, so it cannot add a value that the engine never renders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Stored,
    Change,
}

/// One refused item: the element it sits on, and what is refused (a field, a variant, or an operation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) element: String,
    pub(crate) what: String,
}

pub(crate) type Refusals = Vec<Refusal>;

fn refuse(out: &mut Refusals, element: &str, what: impl Into<String>) {
    out.push(Refusal { element: element.to_owned(), what: what.into() });
}

/// " (a, b)" for the names whose flag is set, and "" when none is.
fn named(list: &[(&str, bool)]) -> String {
    let names: Vec<&str> = list.iter().filter(|(_, set)| *set).map(|(name, _)| *name).collect();
    if names.is_empty() {
        String::new()
    } else {
        format!(" ({})", names.join(", "))
    }
}

/// Every Style field, in contract order. `field_policy` decides each one, and a test checks the count.
pub(crate) const STYLE_FIELDS: [StyleField; 24] = [
    StyleField::FontFamily,
    StyleField::FontSize,
    StyleField::Color,
    StyleField::Fill,
    StyleField::Stroke,
    StyleField::StrokeWidth,
    StyleField::Bold,
    StyleField::Italic,
    StyleField::Align,
    StyleField::CornerRadius,
    StyleField::Opacity,
    StyleField::Shadows,
    StyleField::Gradient,
    StyleField::LineHeight,
    StyleField::LetterSpacing,
    StyleField::VerticalAlign,
    StyleField::FontWeight,
    StyleField::StrokeDash,
    StyleField::StrokeCap,
    StyleField::StrokeJoin,
    StyleField::Underline,
    StyleField::Strikethrough,
    StyleField::StrokeStart,
    StyleField::StrokeEnd,
];

/// What a style belongs to. Which v1.2 fields an element takes depends on what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Element {
    /// A diagram node of the rectangle kind.
    Rectangle,
    /// A diagram node of the ellipse kind.
    Ellipse,
    /// A connector (a diagram edge): one straight line with an arrow and an optional label.
    Connector,
    /// A plot panel. Its style paints the bars, the legend cells, the axes, the series lines and the error bars.
    PlotPanel,
    /// A diagram or image panel. Its style paints no bars, legend cells or lines.
    OtherPanel,
    /// A patch whose target is not an element of the document, or a named or paragraph style. Every v1.2 element rule
    /// refuses it.
    Unknown,
}

impl Element {
    /// The elements that exist in a figure, and so the ones a capability reason can name.
    const ALL: [Element; 5] = [Element::Rectangle, Element::Ellipse, Element::Connector, Element::PlotPanel, Element::OtherPanel];

    fn name(self) -> &'static str {
        match self {
            Element::Rectangle => "rectangle nodes",
            Element::Ellipse => "ellipse nodes",
            Element::Connector => "connectors",
            Element::PlotPanel => "plot panels",
            Element::OtherPanel => "diagram and image panels",
            Element::Unknown => "unknown targets",
        }
    }
}

/// Whether the figure engine applies a Style field on one element (the P1 decision table, docs/P1_PLAN.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldPolicy {
    /// Applied and drawn by the renderer on this element.
    Accept,
    /// Refused on this element, with the reason the capability report shows.
    Refuse(&'static str),
}

pub(crate) fn field_policy(field: StyleField, element: Element) -> FieldPolicy {
    use Element::{Connector, Ellipse, OtherPanel, PlotPanel, Rectangle, Unknown};
    use FieldPolicy::{Accept, Refuse};
    match field {
        StyleField::FontFamily | StyleField::FontSize | StyleField::Color | StyleField::Fill | StyleField::Stroke
        | StyleField::StrokeWidth | StyleField::Bold | StyleField::Italic | StyleField::Align => Accept,
        // Valid on every element and drawn by the renderer: opacity paints, dashed strokes, and text metrics.
        StyleField::Opacity | StyleField::StrokeDash | StyleField::LineHeight | StyleField::LetterSpacing => Accept,
        StyleField::CornerRadius => match element {
            Rectangle | PlotPanel => Accept,
            Ellipse => Refuse("ellipses have no corners"),
            Connector => Refuse("connectors have no corners"),
            OtherPanel => Refuse("only plot panels draw corners (bars and legend cells)"),
            Unknown => Refuse("the target is not a figure element"),
        },
        // The node's text is placed inside its box. A connector label and a panel label have no box.
        StyleField::VerticalAlign => match element {
            Rectangle | Ellipse => Accept,
            Connector => Refuse("connector labels have no text box"),
            PlotPanel | OtherPanel => Refuse("panel text has no text box"),
            Unknown => Refuse("the target is not a figure element"),
        },
        // A line end exists only on an open line. A closed shape has none.
        StyleField::StrokeCap => match element {
            Connector | PlotPanel => Accept,
            Rectangle | Ellipse => Refuse("closed shapes have no line ends"),
            OtherPanel => Refuse("only plot panels draw lines"),
            Unknown => Refuse("the target is not a figure element"),
        },
        // A join is a corner. An ellipse has none, and a connector is one straight line.
        StyleField::StrokeJoin => match element {
            Rectangle | PlotPanel => Accept,
            Ellipse => Refuse("ellipses have no corners"),
            Connector => Refuse("a connector is one straight line"),
            OtherPanel => Refuse("only plot panels draw shapes with corners"),
            Unknown => Refuse("the target is not a figure element"),
        },
        StyleField::Shadows | StyleField::Gradient => Refuse("not used in paper figures"),
        StyleField::FontWeight => Refuse("planned for a later phase; bundled fonts are regular and bold only"),
        StyleField::Underline | StyleField::Strikethrough => Refuse("planned for a later phase"),
        StyleField::StrokeStart | StyleField::StrokeEnd => Refuse("line end markers are not supported in paper figures"),
    }
}

/// True when the figure engine refuses the field on the element.
fn is_refused(field: StyleField, element: Element) -> bool {
    matches!(field_policy(field, element), FieldPolicy::Refuse(_))
}

/// The v1.2 style fields for the capability report (`Feature::Effects`): each accepted field with the elements that take
/// it, and each field refused everywhere with its reason.
pub(crate) fn effects_reason() -> String {
    let mut accepted: Vec<String> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for field in STYLE_FIELDS.iter().copied().filter(|f| f.is_v12()) {
        let on: Vec<&str> = Element::ALL.iter().filter(|e| !is_refused(field, **e)).map(|e| e.name()).collect();
        if on.len() == Element::ALL.len() {
            accepted.push(format!("{} (every element)", field.name()));
        } else if !on.is_empty() {
            accepted.push(format!("{} ({})", field.name(), on.join(", ")));
        } else if let FieldPolicy::Refuse(why) = field_policy(field, Element::Rectangle) {
            refused.push(format!("{} ({why})", field.name()));
        }
    }
    format!("v1.2 style fields accepted on: {}; refused on open and on change: {}", accepted.join(", "), refused.join(", "))
}

fn style(style: &Style, element: Element, element_id: &str, out: &mut Refusals) {
    for field in style.non_default_v12_fields() {
        if is_refused(field, element) {
            refuse(out, element_id, field.name());
        }
    }
}

/// A style patch: every v1.2 field it sets that the element refuses, and every field it clears. `prefix` names a
/// variant ("variant hover ").
fn patch(patch: &StylePatch, element: Element, element_id: &str, prefix: &str, out: &mut Refusals) {
    for field in patch.set_fields() {
        if is_refused(field, element) {
            refuse(out, element_id, format!("{prefix}{}", field.name()));
        }
    }
    for field in &patch.clear {
        refuse(out, element_id, format!("{prefix}clear {}", field.name()));
    }
}

fn layout(spec: &LayoutSpec, element: &str, out: &mut Refusals) {
    for name in spec.v12_fields() {
        refuse(out, element, format!("layout.{name}"));
    }
}

/// The v1.2 interactions (OpenUrl, Back). Refused everywhere.
fn v12_interactions(list: &[Interaction], element: &str, out: &mut Refusals) {
    for interaction in list {
        match interaction {
            Interaction::OpenUrl { .. } => refuse(out, element, "interaction open_url"),
            Interaction::Back => refuse(out, element, "interaction back"),
            Interaction::NavigatePage { .. } | Interaction::ToggleVisibility { .. } | Interaction::SetVariant { .. } => {}
        }
    }
}

/// The name of a v1.1 interaction that the engine stores but never renders (R09), or None.
fn unrendered_interaction(interaction: &Interaction) -> Option<&'static str> {
    match interaction {
        Interaction::NavigatePage { .. } => Some("interaction navigate_page"),
        Interaction::ToggleVisibility { .. } => Some("interaction toggle_visibility"),
        Interaction::SetVariant { .. } => Some("interaction set_variant"),
        Interaction::OpenUrl { .. } | Interaction::Back => None,
    }
}

/// The v1.1 values of one node (not of its children) that the engine stores but never renders or exports (R09), one
/// name per item. A change may not add them. A stored project keeps them, with one info diagnostic each.
fn unrendered_names(n: &Node) -> Vec<String> {
    let mut names = Vec::new();
    if !n.children.is_empty() {
        names.push("children".to_owned());
    }
    if n.asset.is_some() {
        names.push("asset".to_owned());
    }
    if n.layout.is_some() {
        names.push("layout".to_owned());
    }
    names.extend(n.interactions.iter().filter_map(unrendered_interaction).map(str::to_owned));
    names.extend(n.variants.keys().map(|variant| format!("variant {variant}")));
    names
}

/// The element a diagram node is. A node of any other kind is not a figure element, so it gets no v1.2 element rule.
fn element_of_node(n: &Node) -> Element {
    match n.kind {
        NodeKind::Rectangle => Element::Rectangle,
        NodeKind::Ellipse => Element::Ellipse,
        _ => Element::Unknown,
    }
}

/// The element a panel is: a plot panel, or a panel whose body paints no bars or legend cells.
fn element_of_panel(p: &FigurePanel) -> Element {
    match &p.body {
        PanelBody::Plot(_) => Element::PlotPanel,
        PanelBody::Diagram(_) | PanelBody::Image(_) => Element::OtherPanel,
    }
}

/// The `NodeExt` field of a math node (`NodeExt::set_fields` names it so).
const MATH_FIELD: &str = "math";

/// `math_node` is true for a node placed directly in a diagram (a stored diagram node or an `AddDiagramNode` node): only
/// such a node of the text kind may carry `NodeExt.math` (a Diagram math node, `18`). Node children, canvas nodes, and
/// pages are not drawn as formulas, so math there is refused.
fn node(n: &Node, out: &mut Refusals, scope: Scope, math_node: bool) {
    let id = n.id.as_str();
    if n.kind == NodeKind::Path {
        refuse(out, id, "NodeKind::Path");
    }
    if n.kind == NodeKind::Video {
        refuse(out, id, "NodeKind::Video");
    }
    let element = element_of_node(n);
    style(&n.style, element, id, out);
    for name in n.ext.set_fields() {
        if name == MATH_FIELD {
            if !(math_node && n.kind == NodeKind::Text) {
                refuse(out, id, "math (diagram text nodes only)");
            }
            continue;
        }
        refuse(out, id, name);
    }
    if let Some(spec) = &n.layout {
        layout(spec, id, out);
    }
    v12_interactions(&n.interactions, id, out);
    for (variant, p) in &n.variants {
        patch(p, element, id, &format!("variant {variant} "), out);
    }
    if scope == Scope::Change {
        for name in unrendered_names(n) {
            refuse(out, id, name);
        }
    }
    for child in &n.children {
        node(child, out, scope, false);
    }
}

fn edge(e: &Edge, out: &mut Refusals) {
    style(&e.style, Element::Connector, &e.id, out);
}

fn diagram(dg: &Diagram, out: &mut Refusals, scope: Scope) {
    for n in &dg.nodes {
        node(n, out, scope, true);
    }
    for e in &dg.edges {
        edge(e, out);
    }
}

fn panel(p: &FigurePanel, out: &mut Refusals, scope: Scope) {
    style(&p.style, element_of_panel(p), &p.id, out);
    if let PanelBody::Diagram(dg) = &p.body {
        diagram(dg, out, scope);
    }
}

/// Refusals in a stored figure document: the v1.2 rules. Everything the engine would lay out, export, or write from it.
pub(crate) fn document(doc: &FigureDocument) -> Refusals {
    let mut out = Refusals::new();
    for p in &doc.panels {
        panel(p, &mut out, Scope::Stored);
    }
    out
}

/// Info diagnostics for v1.1 values that a stored figure keeps but never renders or exports. They never block: the
/// file opens and validates as it is, and each diagnostic names the value and its node (R09, coordinator decision).
pub(crate) fn preserved_notes(doc: &FigureDocument) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for p in &doc.panels {
        if let PanelBody::Diagram(dg) = &p.body {
            for n in &dg.nodes {
                preserved_node(n, &mut out);
            }
        }
    }
    out
}

fn preserved_node(n: &Node, out: &mut Vec<Diagnostic>) {
    let id = n.id.as_str();
    for name in unrendered_names(n) {
        out.push(diag::info(
            code::PRESERVED_NOT_RENDERED,
            Some(id),
            format!("{id}: {name} is preserved, not rendered or exported by figure artifacts"),
        ));
    }
    for child in &n.children {
        preserved_node(child, out);
    }
}

/// The element of every id in a document: panels, diagram nodes (with their children), and connectors.
fn element_kinds(doc: &FigureDocument) -> BTreeMap<String, Element> {
    let mut kinds = BTreeMap::new();
    for p in &doc.panels {
        add_panel_kinds(p, &mut kinds);
    }
    kinds
}

fn add_panel_kinds(p: &FigurePanel, kinds: &mut BTreeMap<String, Element>) {
    kinds.insert(p.id.clone(), element_of_panel(p));
    if let PanelBody::Diagram(dg) = &p.body {
        for n in &dg.nodes {
            add_node_kinds(n, kinds);
        }
        for e in &dg.edges {
            kinds.insert(e.id.clone(), Element::Connector);
        }
    }
}

fn add_node_kinds(n: &Node, kinds: &mut BTreeMap<String, Element>) {
    kinds.insert(n.id.clone(), element_of_node(n));
    for child in &n.children {
        add_node_kinds(child, kinds);
    }
}

/// The element of a patch target. An id that is not in the document (or in the change so far) is Unknown.
fn element_of(kinds: &BTreeMap<String, Element>, id: &str) -> Element {
    kinds.get(id).copied().unwrap_or(Element::Unknown)
}

/// Refusals in one change: the full rule set. A patch whose set and clear fields collide is INVALID_ARGUMENT
/// (`StylePatch::check`), and that error comes first, as the engine checks a patch before it applies anything. Each
/// patch is checked against the element it names: the document's elements, plus those the change adds before it.
pub(crate) fn operations(doc: &FigureDocument, ops: &[Operation]) -> DltResult<Refusals> {
    let mut out = Refusals::new();
    let mut kinds = element_kinds(doc);
    for op in ops {
        operation(op, &mut out, &mut kinds)?;
    }
    Ok(out)
}

fn operation(op: &Operation, out: &mut Refusals, kinds: &mut BTreeMap<String, Element>) -> DltResult<()> {
    match op {
        Operation::SetStyle { node_ids, patch: p } => {
            p.check()?;
            for id in node_ids {
                patch(p, element_of(kinds, id), id, "", out);
            }
        }
        Operation::SetTextStyleRange { range, patch: p } => {
            p.check()?;
            patch(p, Element::Unknown, &range.paragraph_id, "", out);
        }
        Operation::SetNamedStyle { name, patch: p, .. } => {
            p.check()?;
            patch(p, Element::Unknown, name, "", out);
        }
        Operation::SetVariantStyle { node_id, variant, patch: p } => {
            p.check()?;
            patch(p, element_of(kinds, node_id), node_id, &format!("variant {variant} "), out);
            // The engine never applies a variant, so a variant value is refused whatever it holds (R09).
            refuse(out, node_id, format!("variant {variant}"));
        }
        Operation::AddPanel { panel: p, .. } => {
            panel(p, out, Scope::Change);
            add_panel_kinds(p, kinds);
        }
        Operation::AddDiagramNode { node: n, .. } => {
            node(n, out, Scope::Change, true);
            add_node_kinds(n, kinds);
        }
        Operation::AddNode { node: n, .. } => {
            node(n, out, Scope::Change, false);
            add_node_kinds(n, kinds);
        }
        Operation::ConnectDiagram { edge: e, .. } => {
            edge(e, out);
            kinds.insert(e.id.clone(), Element::Connector);
        }
        Operation::SetLayout { node_id, layout: spec } => {
            layout(spec, node_id, out);
            // Any layout is stored and never rendered (R09).
            refuse(out, node_id, "layout");
        }
        Operation::SetInteraction { node_id, interaction } => {
            v12_interactions(std::slice::from_ref(interaction), node_id, out);
            if let Some(name) = unrendered_interaction(interaction) {
                refuse(out, node_id, name);
            }
        }
        Operation::SetInteractions { node_id, .. } => refuse(out, node_id, "operation set_interactions"),
        Operation::InsertPage { page, .. } => {
            if page.name.is_some() {
                refuse(out, &page.id, "page name");
            }
            for n in &page.nodes {
                node(n, out, Scope::Change, false);
                add_node_kinds(n, kinds);
            }
        }
        Operation::SetPageSize { page_id, .. } => refuse(out, page_id, "operation set_page_size"),
        Operation::MoveNode { node_id, .. } => refuse(out, node_id, "operation move_node"),
        Operation::SetZOrder { node_id, .. } => refuse(out, node_id, "operation set_z_order"),
        Operation::SetNodeAppearance { node_ids, image_fit, locked } => {
            let fields = named(&[("image_fit", image_fit.is_some()), ("locked", locked.is_some())]);
            for id in node_ids {
                refuse(out, id, format!("operation set_node_appearance{fields}"));
            }
        }
        Operation::SetNodeInfo { node_ids, alt, name, hidden } => {
            let fields = named(&[("alt", alt.is_some()), ("name", name.is_some()), ("hidden", hidden.is_some())]);
            for id in node_ids {
                refuse(out, id, format!("operation set_node_info{fields}"));
            }
        }
        Operation::SetNodeList { node_id, .. } => refuse(out, node_id, "operation set_node_list"),
        Operation::SetNodePath { node_id, .. } => refuse(out, node_id, "operation set_node_path"),
        Operation::SetCanvasTableStructure { table_id, .. } => refuse(out, table_id, "operation set_canvas_table_structure"),
        Operation::SetCanvasTableColumnWidths { table_id, .. } => refuse(out, table_id, "operation set_canvas_table_column_widths"),
        Operation::SetCanvasBoardMemos { page_id, .. } => refuse(out, page_id, "operation set_canvas_board_memos"),
        Operation::SetCanvasPageGuides { page_id, .. } => refuse(out, page_id, "operation set_canvas_page_guides"),
        Operation::SetCanvasPageSections { expected, sections } => {
            let start_page_id = sections.first().or(expected.first()).map_or("page_sections", |section| section.start_page_id.as_str());
            refuse(out, start_page_id, "operation set_canvas_page_sections");
        }
        Operation::ReplaceImageAsset { node_id, .. } => refuse(out, node_id, "operation replace_image_asset"),
        Operation::SetCanvasImageCrop { node_id, .. } => refuse(out, node_id, "operation set_canvas_image_crop"),
        Operation::SetPageRole { page_id, .. } => refuse(out, page_id, "operation set_page_role"),
        Operation::InstantiateComponent { instance_id, .. } => refuse(out, instance_id, "operation instantiate_component"),
        Operation::DetachInstance { instance_id } => refuse(out, instance_id, "operation detach_instance"),
        Operation::RenamePage { page_id, .. } => refuse(out, page_id, "operation rename_page"),
        Operation::SetGroupRepeat { node_id, .. } => refuse(out, node_id, "operation set_group_repeat"),
        Operation::SetGroupCondition { node_id, .. } => refuse(out, node_id, "operation set_group_condition"),
        Operation::SetTheme { .. } => refuse(out, "theme", "operation set_theme"),
        // Operations without v1.2 or unrendered content. ops::apply applies them or refuses them as unsupported.
        Operation::ReplaceText { .. }
        | Operation::SetText { .. }
        | Operation::SetGeometry { .. }
        | Operation::RemoveNode { .. }
        | Operation::SetParagraphFormat { .. }
        | Operation::SetPageSetup { .. }
        | Operation::SetTableDimensions { .. }
        | Operation::InsertFlowBlock { .. }
        | Operation::RemovePage { .. }
        | Operation::ReorderPages { .. }
        | Operation::SetPageNotes { .. }
        | Operation::RemovePanel { .. }
        | Operation::ReorderPanels { .. }
        | Operation::SetCaption { .. }
        | Operation::PutEvidence { .. }
        | Operation::SetPlot { .. }
        | Operation::SetPlotDisplay { .. }
        | Operation::SetDiagramProvenance { .. }
        | Operation::SetCommonLegend { .. }
        | Operation::SetFigureVisualReview { .. }
        | Operation::SetFigureImageTransform { .. }
        | Operation::SetFigureImageCalibration { .. }
        | Operation::SetAxisLabel { .. }
        | Operation::SetAxisTicks { .. }
        | Operation::PutDataset { .. }
        | Operation::ArrangePanels { .. }
        | Operation::SetPanelLabel { .. }
        | Operation::UpdateFigureLink { .. }
        | Operation::PutToken { .. }
        | Operation::RemoveToken { .. }
        | Operation::SetTokenMode { .. }
        | Operation::BindToken { .. }
        | Operation::UnbindToken { .. }
        | Operation::ReplaceWebSource { .. }
        | Operation::MappedWebEdit { .. } => {}
    }
    Ok(())
}

/// One blocking diagnostic per refused item, on the element it sits on.
pub(crate) fn diagnostics(refusals: &Refusals) -> Vec<Diagnostic> {
    refusals
        .iter()
        .map(|r| {
            let code_name = if r.what.starts_with("operation ") { code::UNSUPPORTED_OPERATION } else { code::UNSUPPORTED_FIELD };
            diag::blocking(code_name, Some(r.element.as_str()), format!("{}: {} is not supported by figure artifacts", r.element, r.what))
        })
        .collect()
}

/// One UNSUPPORTED_OPERATION error. Its message names every refused item, and `diagnostics` lists each one.
pub(crate) fn to_error(refusals: &Refusals) -> DltError {
    let mut names: Vec<String> = Vec::new();
    for r in refusals {
        let name = format!("{} on {}", r.what, r.element);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let affected: BTreeSet<String> = refusals.iter().map(|r| r.element.clone()).collect();
    DltError {
        code: ErrorCode::UnsupportedOperation,
        message: format!("not supported by figure artifacts: {}", names.join(", ")),
        current_revision: None,
        affected_ids: affected.into_iter().collect(),
        retryable: false,
        diagnostics: diagnostics(refusals),
    }
}

fn check(refusals: &Refusals) -> DltResult<()> {
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(to_error(refusals))
    }
}

/// Err when a stored document holds a refused v1.2 item.
pub(crate) fn require_document(doc: &FigureDocument) -> DltResult<()> {
    check(&document(doc))
}

/// Err when a change holds a refused item. Called before any operation is applied, with the document the change applies
/// to.
pub(crate) fn require_operations(doc: &FigureDocument, ops: &[Operation]) -> DltResult<()> {
    check(&operations(doc, ops)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every element, Unknown included.
    const ELEMENTS: [Element; 6] =
        [Element::Rectangle, Element::Ellipse, Element::Connector, Element::PlotPanel, Element::OtherPanel, Element::Unknown];

    #[test]
    fn style_fields_lists_each_variant_once() {
        let names: BTreeSet<&str> = STYLE_FIELDS.iter().map(|f| f.name()).collect();
        assert_eq!(names.len(), STYLE_FIELDS.len());
        assert_eq!(STYLE_FIELDS.len(), 24, "StyleField has 24 variants: list a new one in STYLE_FIELDS and field_policy");
    }

    #[test]
    fn v11_fields_are_accepted_on_every_element() {
        for field in STYLE_FIELDS.iter().copied().filter(|f| !f.is_v12()) {
            for element in ELEMENTS {
                assert_eq!(field_policy(field, element), FieldPolicy::Accept, "{field} on {}", element.name());
            }
        }
    }

    #[test]
    fn every_refusal_has_a_reason() {
        for field in STYLE_FIELDS {
            for element in ELEMENTS {
                if let FieldPolicy::Refuse(why) = field_policy(field, element) {
                    assert!(!why.is_empty(), "{field} on {} is refused without a reason", element.name());
                }
            }
        }
    }

    /// The P1 decision table: the elements that take each v1.2 field (docs/P1_PLAN.md).
    #[test]
    fn the_element_rules_match_the_decision_table() {
        let on = |field: StyleField| -> Vec<Element> {
            ELEMENTS.iter().copied().filter(|e| field_policy(field, *e) == FieldPolicy::Accept).collect()
        };
        assert_eq!(on(StyleField::Opacity), ELEMENTS.to_vec());
        assert_eq!(on(StyleField::StrokeDash), ELEMENTS.to_vec());
        assert_eq!(on(StyleField::LineHeight), ELEMENTS.to_vec());
        assert_eq!(on(StyleField::LetterSpacing), ELEMENTS.to_vec());
        assert_eq!(on(StyleField::CornerRadius), vec![Element::Rectangle, Element::PlotPanel]);
        assert_eq!(on(StyleField::VerticalAlign), vec![Element::Rectangle, Element::Ellipse]);
        assert_eq!(on(StyleField::StrokeCap), vec![Element::Connector, Element::PlotPanel]);
        assert_eq!(on(StyleField::StrokeJoin), vec![Element::Rectangle, Element::PlotPanel]);
        for field in [StyleField::Shadows, StyleField::Gradient, StyleField::FontWeight, StyleField::Underline, StyleField::Strikethrough, StyleField::StrokeStart, StyleField::StrokeEnd] {
            assert!(on(field).is_empty(), "{field} is refused everywhere");
        }
    }

    #[test]
    fn the_effects_reason_names_the_elements_of_each_accepted_field_and_the_reason_of_each_refusal() {
        let reason = effects_reason();
        for text in [
            "opacity_milli (every element)",
            "stroke_dash (every element)",
            "line_height_milli (every element)",
            "letter_spacing (every element)",
            "corner_radius (rectangle nodes, plot panels)",
            "vertical_align (rectangle nodes, ellipse nodes)",
            "stroke_cap (connectors, plot panels)",
            "stroke_join (rectangle nodes, plot panels)",
            "shadows (not used in paper figures)",
            "gradient (not used in paper figures)",
            "font_weight (planned for a later phase",
            "underline (planned for a later phase)",
            "strikethrough (planned for a later phase)",
        ] {
            assert!(reason.contains(text), "{text} is missing from: {reason}");
        }
    }
}
