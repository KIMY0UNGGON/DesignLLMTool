//! Figures inside documents (docs/FIGURE_LINKS.md). A picture of a figure remembers which figure
//! revision it shows. Inserting one, and replacing it when the figure has a newer revision, are
//! always the user's request: nothing here runs on its own, and a figure that changes leaves every
//! document that shows it as it was until the user asks for the update.
//!
//! A request takes three host calls: read the figure (its current revision and physical size),
//! render it (`figure.render_link`, an immutable PNG), then one change set on the document.

use std::collections::BTreeMap;

use designllm_contracts::media_header::{self, FIGURE_REPRESENTATIONS};
use designllm_contracts::{
    new_id, Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AssetRef, DltError, ErrorCode,
    Feature, FigureLink, FlowBlock, FlowDocument, Intent, Node, NodeKind, Operation, Rect, Size,
    Style,
};
use eframe::egui::{self, Ui};
use serde::Deserialize;
use serde_json::json;

use super::{DesktopApp, PendingAction};

/// Resolution of a figure render that goes into a document. 300 dpi is the usual print quality;
/// the picture is placed at the figure's physical size or smaller, never larger.
const LINK_DPI: u32 = 300;

/// The only representation the document engines support.
const PNG: &str = FIGURE_REPRESENTATIONS[0];

/// Where a new picture of a figure goes, decided when the user clicks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Placement {
    /// On a page of a canvas or a slide deck.
    Page { page_id: String, size: Size },
    /// Between the blocks of a flowing document, as wide as the section's text area allows.
    Flow { section_id: String, index: u32, content_width: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinkIntent {
    /// Put a new picture of the figure here.
    Insert(Placement),
    /// Give this picture the figure's current render.
    Update { target_node_id: String, representation: String },
}

/// What the user asked for, fixed at the click: the document at the revision the user was looking
/// at, and the figure. The change set is sent against exactly that revision, so a document that
/// moved on in the meantime refuses it instead of being overwritten.
#[derive(Clone, Debug)]
pub(super) struct LinkRequest {
    pub parent_id: String,
    pub parent_revision: u64,
    pub figure_id: String,
    pub intent: LinkIntent,
}

/// The figure as read just before it is rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FigureFacts {
    pub id: String,
    pub revision: u64,
    pub size: Size,
}

/// The reply of `figure.render_link`: the immutable PNG and its hash.
#[derive(Clone, Debug, Deserialize)]
pub(super) struct RenderedLink {
    pub asset: AssetRef,
    pub render_hash: String,
}

/// `numerator / denominator` rounded half up (`media_header::div_round_half_up`). Without a positive
/// denominator or with a negative numerator there is no proportion, and the result is 0 (callers raise it
/// to 1 EMU); a result beyond `i64` saturates.
fn div_round(numerator: i128, denominator: i128) -> i64 {
    if denominator <= 0 || numerator < 0 {
        return 0;
    }
    media_header::div_round_half_up(numerator, denominator).and_then(|value| i64::try_from(value).ok()).unwrap_or(i64::MAX)
}

/// The height of a picture of `width` that keeps the figure's proportions (EMU, half up). The
/// document engine applies the same rule to the render's pixel size when it links the picture.
pub(super) fn proportional_height(width: i64, figure: Size) -> i64 {
    div_round(i128::from(width) * i128::from(figure.height), i128::from(figure.width))
}

/// A picture on a page: the figure's physical size, shrunk to fit inside the page with a margin
/// of a tenth of the shorter side, and centred.
pub(super) fn page_bounds(page: Size, figure: Size) -> Rect {
    let margin = page.width.min(page.height) / 10;
    let room_width = (page.width - 2 * margin).max(1);
    let room_height = (page.height - 2 * margin).max(1);
    let fitting_height = (i128::from(room_height) * i128::from(figure.width))
        .checked_div(i128::from(figure.height))
        .and_then(|value| i64::try_from(value).ok())
        .unwrap_or(i64::MAX);
    let width = figure.width.min(room_width).min(fitting_height).max(1);
    let height = proportional_height(width, figure).max(1);
    Rect {
        x: (page.width - width) / 2,
        y: (page.height - height) / 2,
        width,
        height,
    }
}

/// A picture in a flowing document: as wide as the text area, but never wider than the figure's
/// physical size (a larger picture would be an enlarged raster). Flow engines place it themselves,
/// so only the size matters.
pub(super) fn flow_bounds(content_width: i64, figure: Size) -> Rect {
    let width = figure.width.min(content_width).max(1);
    Rect {
        x: 0,
        y: 0,
        width,
        height: proportional_height(width, figure).max(1),
    }
}

/// The section and top-level block that hold `id` (a block, a run, a table cell or anything nested
/// in one).
fn locate_block(document: &FlowDocument, id: &str) -> Option<(usize, usize)> {
    fn holds(block: &FlowBlock, id: &str) -> bool {
        match block {
            FlowBlock::Paragraph(paragraph) => {
                paragraph.id == id || paragraph.runs.iter().any(|run| run.id == id)
            }
            FlowBlock::Image(node) => node.id == id,
            FlowBlock::Table(table) => {
                table.id == id
                    || table.rows.iter().flatten().any(|cell| {
                        cell.id == id || cell.blocks.iter().any(|block| holds(block, id))
                    })
            }
            FlowBlock::Opaque { id: opaque, .. } => opaque == id,
        }
    }
    document.sections.iter().enumerate().find_map(|(section, content)| {
        content
            .blocks
            .iter()
            .position(|block| holds(block, id))
            .map(|block| (section, block))
    })
}

/// After the selected block, or at the end of the last section when nothing is selected. A
/// selection inside a table puts the picture after the table. A section that another section
/// follows ends in its section-break paragraph, so the picture goes before that last block.
pub(super) fn flow_placement(document: &FlowDocument, anchor: Option<&str>) -> Option<Placement> {
    let last = document.sections.len().checked_sub(1)?;
    let (section_index, index) = match anchor.and_then(|id| locate_block(document, id)) {
        Some((section, block)) => (section, block + 1),
        None => (last, document.sections[last].blocks.len()),
    };
    let section = &document.sections[section_index];
    let index = if section_index < last && index >= section.blocks.len() {
        section.blocks.len().saturating_sub(1)
    } else {
        index
    };
    let setup = &section.setup;
    Some(Placement::Flow {
        section_id: section.id.clone(),
        index: u32::try_from(index).ok()?,
        content_width: (setup.size.width - setup.margin_left - setup.margin_right).max(1),
    })
}

/// Where a picture goes in `artifact`: the page on screen for canvas and slides, after the
/// selection for flowing documents. `None` for artifacts that cannot hold pictures.
pub(super) fn placement(artifact: &Artifact, page_index: u32, anchor: Option<&str>) -> Option<Placement> {
    let page = |pages: &[designllm_contracts::Page]| {
        pages.get(page_index as usize).map(|page| Placement::Page {
            page_id: page.id.clone(),
            size: page.size,
        })
    };
    match &artifact.body {
        ArtifactBody::Canvas(document) => page(&document.pages),
        ArtifactBody::Slides(document) => page(&document.pages),
        ArtifactBody::Flow(document) => flow_placement(document, anchor),
        ArtifactBody::Figure(_) | ArtifactBody::WebCode(_) => None,
    }
}

/// The change set that puts a picture of the figure into the document: the engine's ordinary
/// picture insertion first, then the link that names the new picture. One undo step. Returns the
/// operations and the id of the new picture.
pub(super) fn insertion(
    placement: &Placement,
    figure: &FigureFacts,
    rendered: &RenderedLink,
) -> (Vec<Operation>, String) {
    let id = new_id();
    let bounds = match placement {
        Placement::Page { size, .. } => page_bounds(*size, figure.size),
        Placement::Flow { content_width, .. } => flow_bounds(*content_width, figure.size),
    };
    let node = Node {
        id: id.clone(),
        kind: NodeKind::Image,
        bounds,
        text: String::new(),
        style: Style::default(),
        asset: Some(rendered.asset.clone()),
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: BTreeMap::new(),
        ext: Default::default(),
    };
    let insert = match placement {
        Placement::Page { page_id, .. } => Operation::AddNode {
            page_id: page_id.clone(),
            parent_id: None,
            node,
        },
        Placement::Flow { section_id, index, .. } => Operation::InsertFlowBlock {
            section_id: section_id.clone(),
            index: *index,
            block: FlowBlock::Image(node),
        },
    };
    let link = FigureLink {
        figure_id: figure.id.clone(),
        figure_revision: figure.revision,
        target_node_id: id.clone(),
        representation: PNG.into(),
        render_hash: rendered.render_hash.clone(),
    };
    (vec![insert, Operation::UpdateFigureLink { link }], id)
}

/// The change set that gives an already linked picture the figure's current render. Only the link
/// changes hands; the engine keeps the picture's place and width and takes the height from the
/// render's proportions.
pub(super) fn update(
    target_node_id: &str,
    representation: &str,
    figure: &FigureFacts,
    rendered: &RenderedLink,
) -> Vec<Operation> {
    vec![Operation::UpdateFigureLink {
        link: FigureLink {
            figure_id: figure.id.clone(),
            figure_revision: figure.revision,
            target_node_id: target_node_id.to_owned(),
            representation: representation.to_owned(),
            render_hash: rendered.render_hash.clone(),
        },
    }]
}

/// Why the figure button cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Blocked {
    /// The document's engine does not take figure pictures.
    Unsupported,
    /// The workspace holds no figure to put in.
    NoFigures,
    /// A request for a figure picture is still under way.
    Busy,
}

fn blocked(supported: bool, figures: usize, busy: bool) -> Option<Blocked> {
    if !supported {
        Some(Blocked::Unsupported)
    } else if figures == 0 {
        Some(Blocked::NoFigures)
    } else if busy {
        Some(Blocked::Busy)
    } else {
        None
    }
}

/// The link of a picture of this document, if it has one.
pub(super) fn link_of<'a>(artifact: &'a Artifact, node_id: &str) -> Option<&'a FigureLink> {
    artifact
        .figure_links
        .iter()
        .find(|link| link.target_node_id == node_id)
}

/// The figure artifact a link points at, as the work list knows it now.
pub(super) fn linked_figure<'a>(
    link: &FigureLink,
    artifacts: &'a [ArtifactSummary],
) -> Option<&'a ArtifactSummary> {
    artifacts
        .iter()
        .find(|summary| summary.id == link.figure_id && summary.kind == ArtifactKind::Figure)
}

/// The figure's current revision when it is newer than the one the picture shows.
pub(super) fn newer_revision(link: &FigureLink, figure: &ArtifactSummary) -> Option<u64> {
    (figure.revision > link.figure_revision).then_some(figure.revision)
}

/// The figure's current revision and physical size, from the artifact the host returned.
pub(super) fn figure_facts(figure: &Artifact) -> Option<FigureFacts> {
    match &figure.body {
        ArtifactBody::Figure(document) if figure.kind == ArtifactKind::Figure && document.size.valid() => {
            Some(FigureFacts {
                id: figure.id.clone(),
                revision: figure.revision,
                size: document.size,
            })
        }
        _ => None,
    }
}

impl DesktopApp {
    /// A request to the host is under way (reading the figure, rendering it, or applying the change).
    fn link_in_progress(&self) -> bool {
        self.pending.values().any(|pending| {
            matches!(
                pending.action,
                PendingAction::FigureInfo { .. } | PendingAction::FigureRender { .. } | PendingAction::LinkApply
            )
        })
    }

    /// The element the user has selected: a picture or table, or the paragraph the caret is in.
    pub(super) fn selection_anchor(&self) -> Option<String> {
        self.context
            .as_ref()
            .and_then(|context| {
                let selection = &context.selection;
                selection
                    .node_ids
                    .first()
                    .cloned()
                    .or_else(|| selection.text_range.as_ref().map(|range| range.paragraph_id.clone()))
            })
            .or_else(|| self.selected_node.clone())
    }

    /// Why a figure cannot be put into the open document right now, if it cannot.
    pub(super) fn figure_insertion_blocked(&self) -> Option<Blocked> {
        blocked(
            self.feature_edit_supported(Feature::FigureLinks),
            self.artifacts.iter().filter(|summary| summary.kind == ArtifactKind::Figure).count(),
            self.link_in_progress(),
        )
    }

    /// The control that inserts a figure of this workspace into the open document: a labelled button
    /// with the list of figures to pick from. It is not drawn while the document's engine refuses
    /// figure pictures (§1 rule 6) or when the workspace has no figure.
    pub(super) fn draw_figure_insertion(&mut self, ui: &mut Ui) {
        if self.figure_insertion_blocked().is_some() {
            return;
        }
        let figures: Vec<(String, String)> = self
            .artifacts
            .iter()
            .filter(|summary| summary.kind == ArtifactKind::Figure)
            .map(|summary| (summary.id.clone(), summary.title.clone()))
            .collect();
        if figures.is_empty() {
            return;
        }
        let palette = crate::theme::palette(ui.ctx());
        let button = crate::widgets::labeled_button(ui, &palette, Some("ins-figure"), "논문 그림", false);
        egui::Popup::from_toggle_button_response(&button)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
            .show(|ui| {
                ui.set_min_width(200.0);
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for (id, title) in &figures {
                        if crate::widgets::menu_item_verbatim(ui, &palette, Some("kind-figure"), &super::truncate(title, 30), false).clicked() {
                            self.invoke_ui("add.figure_link", serde_json::json!({ "figure_id": id }));
                        }
                    }
                });
            });
    }

    pub(super) fn begin_figure_insert(&mut self, figure_id: &str) {
        let (Some(active), Some(artifact)) = (self.active.as_ref(), self.artifact.as_ref()) else {
            return;
        };
        let anchor = self.selection_anchor();
        let Some(placement) = placement(artifact, self.page_index, anchor.as_deref()) else {
            self.set_error("그림을 넣을 위치를 찾지 못했습니다".into());
            return;
        };
        let request = LinkRequest {
            parent_id: active.id.clone(),
            parent_revision: artifact.revision,
            figure_id: figure_id.to_owned(),
            intent: LinkIntent::Insert(placement),
        };
        self.read_figure_for_link(request);
    }

    pub(super) fn begin_figure_update(&mut self, node_id: &str) {
        let (Some(active), Some(artifact)) = (self.active.as_ref(), self.artifact.as_ref()) else {
            return;
        };
        let Some(link) = link_of(artifact, node_id) else {
            return;
        };
        let request = LinkRequest {
            parent_id: active.id.clone(),
            parent_revision: artifact.revision,
            figure_id: link.figure_id.clone(),
            intent: LinkIntent::Update {
                target_node_id: node_id.to_owned(),
                representation: link.representation.clone(),
            },
        };
        self.read_figure_for_link(request);
    }

    /// Step 1: the figure's current revision and size.
    fn read_figure_for_link(&mut self, request: LinkRequest) {
        if self.link_in_progress() {
            return;
        }
        self.call(
            "artifact.inspect",
            json!({ "artifact_id": request.figure_id }),
            PendingAction::FigureInfo { request },
        );
    }

    /// Step 2: the figure at that revision as a PNG.
    pub(super) fn render_figure_for_link(&mut self, request: LinkRequest, figure: FigureFacts) {
        let representation = match &request.intent {
            LinkIntent::Insert(_) => PNG.to_owned(),
            LinkIntent::Update { representation, .. } => representation.clone(),
        };
        self.call(
            "figure.render_link",
            json!({
                "figure_id": figure.id,
                "figure_revision": figure.revision,
                "representation": representation,
                "profile": { "dpi": LINK_DPI, "transparent": false },
            }),
            PendingAction::FigureRender { request, figure },
        );
    }

    /// Step 3: one change set on the document, against the revision the user saw.
    pub(super) fn apply_figure_link(
        &mut self,
        request: &LinkRequest,
        figure: &FigureFacts,
        rendered: &RenderedLink,
    ) {
        if self.active.as_ref().is_none_or(|active| active.id != request.parent_id) {
            return;
        }
        let (operations, selected) = match &request.intent {
            LinkIntent::Insert(placement) => {
                let (operations, id) = insertion(placement, figure, rendered);
                (operations, id)
            }
            LinkIntent::Update { target_node_id, representation } => (
                update(target_node_id, representation, figure, rendered),
                target_node_id.clone(),
            ),
        };
        if self.run_context.is_some() {
            if let Ok((expected_selection, expected_page, expected_selection_generation)) = self.selection_commit_guard() {
                let action = PendingAction::ApplyWithSelectionIfUnchanged {
                    node_ids: vec![selected], expected_selection, expected_page, expected_selection_generation,
                };
                if let Some(ticket) = self.apply_changes(operations, [Intent::Content].into_iter().collect(), request.parent_revision, action) {
                    self.ui.remember_agent_commit(ticket);
                }
            }
            return;
        }
        self.select_after_commit = Some(vec![selected]);
        self.select_after_commit_ticket = None;
        let _ = self.apply_changes(
            operations,
            [Intent::Content].into_iter().collect(),
            request.parent_revision,
            PendingAction::LinkApply,
        );
    }

    /// The part of the inspector for a picture that shows a figure: the figure's title opens it,
    /// and when the figure has a newer revision one button updates the picture to it. Nothing
    /// changes until that button is pressed.
    pub(super) fn draw_figure_link(&mut self, ui: &mut Ui) {
        let Some(node_id) = self.selected_node.clone() else {
            return;
        };
        let Some(artifact) = self.artifact.as_ref() else {
            return;
        };
        let Some(link) = link_of(artifact, &node_id).cloned() else {
            return;
        };
        let Some(figure) = linked_figure(&link, &self.artifacts).cloned() else {
            return;
        };
        let newer = newer_revision(&link, &figure);
        let supported = self.feature_edit_supported(Feature::FigureLinks);
        let reason = self
            .context
            .as_ref()
            .and_then(|context| context.capabilities.get(&Feature::FigureLinks))
            .map(|support| support.reason.clone())
            .unwrap_or_default();
        let updatable = supported && !self.link_in_progress();
        let palette = crate::theme::palette(ui.ctx());
        ui.horizontal_wrapped(|ui| {
            if crate::widgets::labeled_button_verbatim(ui, &palette, Some("figure-open"), &super::truncate(&figure.title, 22)).clicked() {
                self.activate(figure.clone());
            }
            if let Some(current) = newer {
                let label = format!("갱신 r{} → r{}", link.figure_revision, current);
                let mut update = ui.add_enabled_ui(updatable, |ui| crate::widgets::labeled_button(ui, &palette, Some("figure-update"), &label, false)).inner;
                if !supported && !reason.is_empty() {
                    update = update.on_disabled_hover_text(crate::i18n::tr(&reason));
                }
                if update.clicked() {
                    self.begin_figure_update(&node_id);
                }
            }
        });
    }

    /// The title of the figure a picture shows, for the outline of the document.
    pub(super) fn linked_picture_title(&self, node_id: &str) -> Option<String> {
        let link = link_of(self.artifact.as_ref()?, node_id)?;
        linked_figure(link, &self.artifacts).map(|figure| figure.title.clone())
    }
}

/// What to tell the user when the host refused a picture request. A document that cannot take the
/// picture says why in its own words (the engines write those sentences); other failures get the
/// usual message.
pub(super) fn refusal(error: &DltError) -> String {
    match error.code {
        ErrorCode::UnsupportedOperation | ErrorCode::InvalidArgument
            if !error.message.trim().is_empty() =>
        {
            error.message.clone()
        }
        _ => super::user_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::{
        Paragraph, ParagraphFormat, Section, Table, TableCell, TextRun, EMU_PER_MM,
    };

    fn mm(value: i64) -> i64 {
        value * EMU_PER_MM
    }

    fn figure_size() -> Size {
        Size { width: mm(170), height: mm(120) }
    }

    fn paragraph(id: &str) -> FlowBlock {
        FlowBlock::Paragraph(Paragraph {
            id: id.into(),
            runs: vec![TextRun::new(format!("{id}-run"), "x")],
            style_ref: None,
            format: ParagraphFormat::default(),
        })
    }

    fn section(id: &str, blocks: Vec<FlowBlock>) -> Section {
        Section { id: id.into(), setup: Default::default(), blocks }
    }

    fn table(id: &str, cell_paragraph: &str) -> FlowBlock {
        FlowBlock::Table(Table {
            id: id.into(),
            column_widths: vec![mm(50)],
            rows: vec![vec![TableCell {
                id: format!("{id}-cell"),
                blocks: vec![paragraph(cell_paragraph)],
                column_span: 1,
                row_span: 1,
            }]],
            style: Style::default(),
        })
    }

    fn document(sections: Vec<Section>) -> FlowDocument {
        FlowDocument { sections, styles: Default::default() }
    }

    fn flow(placement: Option<Placement>) -> (String, u32, i64) {
        match placement.expect("a placement") {
            Placement::Flow { section_id, index, content_width } => (section_id, index, content_width),
            other => panic!("not a flow placement: {other:?}"),
        }
    }

    #[test]
    fn a_picture_on_a_page_keeps_the_figures_size_when_it_fits_and_is_centred() {
        let slide = Size { width: 12_192_000, height: 6_858_000 };
        let bounds = page_bounds(slide, figure_size());
        assert_eq!((bounds.width, bounds.height), (mm(170), mm(120)));
        assert_eq!(bounds.x, (slide.width - bounds.width) / 2);
        assert_eq!(bounds.y, (slide.height - bounds.height) / 2);
    }

    #[test]
    fn a_picture_larger_than_the_page_is_shrunk_inside_the_margins_without_changing_its_proportions() {
        for (page, figure) in [
            (Size::a4(), Size { width: mm(400), height: mm(300) }),
            (Size { width: 12_192_000, height: 6_858_000 }, Size { width: mm(300), height: mm(300) }),
            (Size { width: mm(100), height: mm(100) }, Size { width: mm(10), height: mm(400) }),
        ] {
            let bounds = page_bounds(page, figure);
            let margin = page.width.min(page.height) / 10;
            assert!(bounds.x >= margin && bounds.y >= margin, "{bounds:?} on {page:?}");
            assert!(bounds.x + bounds.width <= page.width - margin + 1, "{bounds:?} on {page:?}");
            assert!(bounds.y + bounds.height <= page.height - margin + 1, "{bounds:?} on {page:?}");
            assert_eq!(bounds.height, proportional_height(bounds.width, figure));
        }
    }

    #[test]
    fn a_flow_picture_fills_the_text_area_but_never_exceeds_the_figures_physical_width() {
        let wide = flow_bounds(mm(170), Size { width: mm(300), height: mm(150) });
        assert_eq!((wide.width, wide.height), (mm(170), mm(85)));
        let small = flow_bounds(mm(170), Size { width: mm(85), height: mm(60) });
        assert_eq!((small.width, small.height), (mm(85), mm(60)));
        assert_eq!((small.x, small.y), (0, 0));
    }

    #[test]
    fn the_height_follows_the_proportions_rounded_half_up() {
        let figure = Size { width: 3, height: 2 };
        assert_eq!(proportional_height(10, figure), 7, "6.67 rounds up");
        assert_eq!(proportional_height(3, figure), 2);
        assert_eq!(proportional_height(1, Size { width: 2, height: 1 }), 1, "0.5 rounds up");
    }

    #[test]
    fn a_figure_with_a_zero_side_gets_a_one_emu_picture_side_instead_of_a_panic() {
        let page = Size::a4();
        for figure in [Size { width: 0, height: mm(50) }, Size { width: mm(50), height: 0 }, Size { width: 0, height: 0 }] {
            let on_page = page_bounds(page, figure);
            let in_flow = flow_bounds(mm(170), figure);
            for bounds in [on_page, in_flow] {
                assert!(bounds.width >= 1 && bounds.height >= 1, "{figure:?} gave {bounds:?}");
            }
        }
        assert_eq!(proportional_height(10, Size { width: 0, height: 5 }), 0, "no proportion without a figure width");
    }

    #[test]
    fn a_picture_goes_after_the_selected_block() {
        let doc = document(vec![section("s1", vec![paragraph("a"), paragraph("b"), paragraph("c")])]);
        let (section_id, index, width) = flow(flow_placement(&doc, Some("b")));
        assert_eq!((section_id.as_str(), index), ("s1", 2));
        assert_eq!(width, mm(170), "an A4 page with 20 mm margins");
        let (_, index, _) = flow(flow_placement(&doc, Some("b-run")));
        assert_eq!(index, 2, "a selected run counts as its paragraph");
    }

    #[test]
    fn a_selection_inside_a_table_puts_the_picture_after_the_table() {
        let doc = document(vec![section("s1", vec![paragraph("a"), table("t", "in-cell"), paragraph("z")])]);
        for anchor in ["t", "t-cell", "in-cell"] {
            let (_, index, _) = flow(flow_placement(&doc, Some(anchor)));
            assert_eq!(index, 2, "anchor {anchor}");
        }
    }

    #[test]
    fn without_a_selection_the_picture_goes_to_the_end_of_the_last_section() {
        let doc = document(vec![
            section("s1", vec![paragraph("a"), paragraph("break")]),
            section("s2", vec![paragraph("b"), paragraph("c")]),
        ]);
        let (section_id, index, _) = flow(flow_placement(&doc, None));
        assert_eq!((section_id.as_str(), index), ("s2", 2));
        let (section_id, index, _) = flow(flow_placement(&doc, Some("missing")));
        assert_eq!((section_id.as_str(), index), ("s2", 2), "an unknown selection counts as none");
    }

    #[test]
    fn a_section_that_another_follows_takes_the_picture_before_its_last_block() {
        let doc = document(vec![
            section("s1", vec![paragraph("a"), paragraph("break")]),
            section("s2", vec![paragraph("b")]),
        ]);
        let (section_id, index, _) = flow(flow_placement(&doc, Some("break")));
        assert_eq!((section_id.as_str(), index), ("s1", 1), "never after the section-break paragraph");
        let (_, index, _) = flow(flow_placement(&doc, Some("a")));
        assert_eq!(index, 1);
        let (section_id, index, _) = flow(flow_placement(&doc, Some("b")));
        assert_eq!((section_id.as_str(), index), ("s2", 1), "the last section may end with the picture");
        assert!(flow_placement(&document(Vec::new()), None).is_none());
    }

    fn render() -> RenderedLink {
        RenderedLink {
            asset: AssetRef { sha256: "a".repeat(64), mime: "image/png".into(), bytes: 1234 },
            render_hash: "a".repeat(64),
        }
    }

    fn facts() -> FigureFacts {
        FigureFacts { id: "figure-1".into(), revision: 3, size: figure_size() }
    }

    #[test]
    fn the_first_insertion_is_the_pictures_own_command_then_its_link_in_one_change_set() {
        let placement = Placement::Page { page_id: "page-1".into(), size: Size::a4() };
        let (operations, id) = insertion(&placement, &facts(), &render());
        assert_eq!(operations.len(), 2);
        let Operation::AddNode { page_id, parent_id, node } = &operations[0] else { panic!("the picture comes first") };
        assert_eq!((page_id.as_str(), parent_id.as_ref()), ("page-1", None));
        assert_eq!((node.id.as_str(), node.kind), (id.as_str(), NodeKind::Image));
        assert_eq!(node.asset.as_ref().map(|asset| asset.sha256.as_str()), Some("a".repeat(64).as_str()));
        assert_eq!(node.style, Style::default(), "a picture keeps its own appearance");
        let Operation::UpdateFigureLink { link } = &operations[1] else { panic!("the link comes second") };
        assert_eq!(link.target_node_id, id);
        assert_eq!((link.figure_id.as_str(), link.figure_revision), ("figure-1", 3));
        assert_eq!((link.representation.as_str(), link.render_hash.as_str()), ("png", "a".repeat(64).as_str()));

        let placement = Placement::Flow { section_id: "s1".into(), index: 4, content_width: mm(170) };
        let (operations, id) = insertion(&placement, &facts(), &render());
        let Operation::InsertFlowBlock { section_id, index, block: FlowBlock::Image(node) } = &operations[0] else {
            panic!("a flowing document takes the picture as a block")
        };
        assert_eq!((section_id.as_str(), *index, node.id.as_str()), ("s1", 4, id.as_str()));
        assert_eq!((node.bounds.width, node.bounds.height), (mm(170), mm(120)));
    }

    #[test]
    fn an_update_is_the_link_alone_for_the_same_picture() {
        let newer = FigureFacts { revision: 5, ..facts() };
        let operations = update("picture-1", "png", &newer, &render());
        assert_eq!(operations.len(), 1);
        let Operation::UpdateFigureLink { link } = &operations[0] else { panic!("only the link") };
        assert_eq!((link.target_node_id.as_str(), link.figure_revision, link.representation.as_str()), ("picture-1", 5, "png"));
    }

    #[test]
    fn the_figure_button_knows_why_it_cannot_be_used() {
        assert_eq!(blocked(true, 2, false), None);
        assert_eq!(blocked(false, 2, false), Some(Blocked::Unsupported));
        assert_eq!(blocked(false, 0, true), Some(Blocked::Unsupported), "the engine's refusal comes first");
        assert_eq!(blocked(true, 0, false), Some(Blocked::NoFigures));
        assert_eq!(blocked(true, 0, true), Some(Blocked::NoFigures));
        assert_eq!(blocked(true, 1, true), Some(Blocked::Busy));
    }

    #[test]
    fn only_a_newer_revision_offers_an_update() {
        let link = FigureLink {
            figure_id: "figure-1".into(),
            figure_revision: 3,
            target_node_id: "picture-1".into(),
            representation: "png".into(),
            render_hash: "a".repeat(64),
        };
        let summary = |revision| ArtifactSummary {
            id: "figure-1".into(),
            kind: ArtifactKind::Figure,
            title: "그림".into(),
            revision,
            engine_id: "figures".into(),
            dirty: false,
            modified_at_ms: None,
            active_jobs: vec![],
        };
        assert_eq!(newer_revision(&link, &summary(3)), None, "the picture shows the current revision");
        assert_eq!(newer_revision(&link, &summary(2)), None);
        assert_eq!(newer_revision(&link, &summary(5)), Some(5));
        assert!(linked_figure(&link, &[summary(5)]).is_some());
        let not_a_figure = ArtifactSummary { kind: ArtifactKind::Canvas, ..summary(5) };
        assert!(linked_figure(&link, &[not_a_figure]).is_none());
        assert!(linked_figure(&link, &[]).is_none());
    }
}
