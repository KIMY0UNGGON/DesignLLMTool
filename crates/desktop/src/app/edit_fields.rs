use super::*;
use super::{doc_tree::{find_node, find_paragraph}, state::RegionClick, toolbox::AddAt};

/// The size (EMU) of a node `add.text`, `add.rect`, `add.frame`, `add.oval` or `add.line` adds: a text box 100 × 16 mm, any
/// other 60 × 40 mm. The toolbox's drag preview (68g) draws the same size.
pub(super) fn new_node_size(kind: NodeKind) -> (i64, i64) {
    if kind == NodeKind::Text { (100 * EMU_PER_MM, 16 * EMU_PER_MM) } else { (60 * EMU_PER_MM, 40 * EMU_PER_MM) }
}

impl DesktopApp {
    /// The text a selected canvas node or figure element exposes for direct editing. Figure
    /// diagram nodes are shapes that carry a label, so they are editable as text too.
    pub(super) fn selection_text_target(&self, artifact: &Artifact, id: &str) -> Option<(TextTarget, String)> {
        if let ArtifactBody::Figure(figure) = &artifact.body {
            if let Some(panel) = figure.panels.iter().find(|panel| panel.id == id) {
                return Some((
                    TextTarget::PanelLabel {
                        panel_id: panel.id.clone(),
                        original: panel.label.clone(),
                        editable: self.feature_edit_supported(Feature::FigurePanels),
                    },
                    panel.label.clone(),
                ));
            }
        }
        let labeled_shapes = matches!(artifact.body, ArtifactBody::Figure(_));
        find_node(artifact, id)
            .filter(|node| node.kind == NodeKind::Text || labeled_shapes)
            .map(|node| {
                (
                    TextTarget::Node {
                        node_id: node.id.clone(),
                        original: node.text.clone(),
                        editable: self.feature_edit_supported(Feature::Text),
                    },
                    node.text.clone(),
                )
            })
    }

    pub(super) fn sync_paragraph_fields(&mut self) {
        let format = match (&self.artifact, &self.text_target) {
            (Some(artifact), Some(TextTarget::Paragraph { paragraph_id, .. })) => {
                find_paragraph(artifact, paragraph_id).map(|paragraph| paragraph.format.clone())
            }
            _ => None,
        };
        if let Some(format) = format {
            self.paragraph_align = format.align;
            self.paragraph_space_before_pt = format.space_before as f32 / EMU_PER_PT as f32;
            self.paragraph_space_after_pt = format.space_after as f32 / EMU_PER_PT as f32;
            self.paragraph_line_spacing_pt = format.line_spacing as f32 / EMU_PER_PT as f32;
            self.paragraph_indent_mm = format.indent as f32 / EMU_PER_MM as f32;
        }
    }

    pub(super) fn sync_style_fields(&mut self) {
        let node = self
            .artifact
            .as_ref()
            .and_then(|artifact| {
                self.selected_node
                    .as_deref()
                    .and_then(|id| find_node(artifact, id))
            })
            .cloned();
        let Some(node) = node else {
            let panel = self
                .artifact
                .as_ref()
                .and_then(|artifact| match &artifact.body {
                    ArtifactBody::Figure(figure) => self
                        .selected_node
                        .as_deref()
                        .and_then(|id| figure.panels.iter().find(|panel| panel.id == id)),
                    _ => None,
                })
                .cloned();
            if let Some(panel) = panel {
                self.geometry_mm = [
                    panel.bounds.x as f64 / EMU_PER_MM as f64,
                    panel.bounds.y as f64 / EMU_PER_MM as f64,
                    panel.bounds.width as f64 / EMU_PER_MM as f64,
                    panel.bounds.height as f64 / EMU_PER_MM as f64,
                ];
            }
            return;
        };
        self.style_fill = node.style.fill.clone();
        self.style_color = node.style.color.clone();
        self.style_font_size_pt = node.style.font_size as f32 / EMU_PER_PT as f32;
        self.geometry_mm = [
            node.bounds.x as f64 / EMU_PER_MM as f64,
            node.bounds.y as f64 / EMU_PER_MM as f64,
            node.bounds.width as f64 / EMU_PER_MM as f64,
            node.bounds.height as f64 / EMU_PER_MM as f64,
        ];
    }

    pub(super) fn is_figure_panel(&self, id: &str) -> bool {
        self.artifact.as_ref().is_some_and(|artifact| match &artifact.body {
            ArtifactBody::Figure(figure) => figure.panels.iter().any(|panel| panel.id == id),
            _ => false,
        })
    }

    /// Adds a node of `kind` to the page `at` names, its centre on `at` (EMU). Without `at` it goes to the current page's fixed
    /// place. A page that is not there is refused.
    pub(super) fn add_node(&mut self, kind: NodeKind, at: Option<AddAt>) -> Result<(), String> {
        let Some(artifact) = self.artifact.as_ref() else {
            return Ok(());
        };
        let page_index = at.map_or(self.page_index, |at| at.page_index);
        let page = match &artifact.body {
            ArtifactBody::Canvas(document) => document.pages.get(page_index as usize),
            ArtifactBody::Slides(document) => document.pages.get(page_index as usize),
            _ => None,
        };
        let Some(page) = page else {
            return Err("선택한 페이지를 편집할 수 없습니다".into());
        };
        let page_id = page.id.clone();
        let index = page.nodes.len() as i64;
        let is_text = kind == NodeKind::Text;
        let (width, height) = new_node_size(kind);
        let (x, y) = match at {
            Some(at) => (at.center.0 - width / 2, at.center.1 - height / 2),
            None => (20 * EMU_PER_MM, 20 * EMU_PER_MM + index * 18 * EMU_PER_MM),
        };
        let node = Node {
            id: new_id(),
            kind,
            bounds: designllm_contracts::Rect { x, y, width, height },
            // The placeholder of a new text box is the window's word, written in the window's language.
            text: if is_text { crate::i18n::tr("텍스트").into_owned() } else { String::new() },
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: Default::default(),
        };
        self.apply_operations(
            vec![Operation::AddNode {
                page_id,
                parent_id: None,
                node,
            }],
            [Intent::Content].into_iter().collect(),
        );
        Ok(())
    }

    pub(super) fn add_paragraph(&mut self) {
        let Some(artifact) = self.artifact.as_ref() else {
            return;
        };
        let ArtifactBody::Flow(document) = &artifact.body else {
            return;
        };
        let Some(section) = document.sections.first() else {
            self.set_error("이 문서에는 새 문단을 넣을 구역이 없습니다".into());
            return;
        };
        let block = FlowBlock::Paragraph(Paragraph {
            id: new_id(),
            runs: vec![TextRun::new(new_id(), String::new())],
            style_ref: None,
            format: ParagraphFormat::default(),
        });
        self.apply_operations(
            vec![Operation::InsertFlowBlock {
                section_id: section.id.clone(),
                index: section.blocks.len() as u32,
                block,
            }],
            [Intent::Content].into_iter().collect(),
        );
    }

    /// `interact_with_region_passing` for a region whose node takes its own presses.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn interact_with_region(
        &mut self,
        ui: &mut Ui,
        key: Id,
        page_index: u32,
        page_rect: Rect,
        node_id: &str,
        bounds: designllm_contracts::Rect,
        page_size: Size,
        rect: Rect,
    ) -> Option<RegionClick> {
        self.interact_with_region_passing(ui, key, page_index, page_rect, node_id, bounds, page_size, rect, false)
    }

    /// Outline, hover, drag and click handling of one clickable region of the preview. Returns the
    /// pointer position when the region was clicked (and not dragged). A region that passes presses on
    /// (`multi::passes_press`: locked, or a container not selected) clicks but leaves its drag to the board (the
    /// marquee). While the board pans (Space or the hand tool) a region takes no press at all.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn interact_with_region_passing(
        &mut self,
        ui: &mut Ui,
        key: Id,
        page_index: u32,
        page_rect: Rect,
        node_id: &str,
        bounds: designllm_contracts::Rect,
        page_size: Size,
        rect: Rect,
        pass_through: bool,
    ) -> Option<RegionClick> {
        let sense = if self.board_pan_mode(ui.ctx()) {
            Sense::hover()
        } else if pass_through {
            Sense::click()
        } else {
            Sense::click_and_drag()
        };
        let response = ui.interact(rect, key, sense);
        super::board::note_board_press(ui.ctx(), &response);
        // The right click menu of the node (68 item 16); a table's nodes keep the table menu.
        self.node_region_menu(ui.ctx(), &response, page_index, node_id);
        // The outline of a node is drawn from its frame (selection.rs). A text-bearing shape has its text
        // lines as regions too; those regions must not draw outlines of their own.

        if response.drag_started_by(egui::PointerButton::Primary) {
            // The drag starts where the press began. `interact_pointer_pos` is where egui recognised the drag (after the
            // click distance is passed), so the pointer's first move would be lost from the node's movement.
            let screen = ui.input(|input| input.pointer.press_origin()).or(response.interact_pointer_pos()).unwrap_or(rect.center());
            // A drag on another page makes it current, as a click there does.
            self.focus_page(page_index);
            if self.drags_cell_text(node_id, page_index) {
                // Inside a table cell a drag selects its text, the table selected or not (68 item 11); the table's border
                // band moves it (`draw_table_border_bands`).
                self.start_cell_text_drag(page_index, screen, ui.ctx().pixels_per_point());
            } else {
                let target = self.drag_target(node_id);
                self.start_drag_at_scale(target, page_index, bounds, page_size, page_rect, screen, ui.ctx().pixels_per_point());
            }
        }
        let dragging_here = |app: &Self| app.drag.as_ref().is_some_and(|drag| drag.node_id == app.drag_target(node_id));
        if response.dragged_by(egui::PointerButton::Primary) && dragging_here(self) {
            if let Some(drag) = self.drag.as_mut() {
                let current = response.interact_pointer_pos().unwrap_or(drag.start);
                drag.delta = current - drag.start;
            }
        }
        if response.drag_stopped_by(egui::PointerButton::Primary) && dragging_here(self) {
            let current = response.interact_pointer_pos().unwrap_or(rect.center());
            if let Some(drag) = self.drag.as_mut() {
                drag.delta = current - drag.start;
                drag.released = true;
            }
            self.commit_drag_if_reserved();
            None
        } else if response.clicked() || response.double_clicked() {
            response.interact_pointer_pos().map(|position| RegionClick {
                position,
                double_clicked: super::board::board_double_click(ui.ctx(), position, response.double_clicked()),
            })
        } else {
            None
        }
    }

    /// What a drag that starts on `node_id` moves: the selected container that holds it (a selected group moves as a
    /// whole, whichever part of it is pressed), else the node itself.
    pub(super) fn drag_target(&self, node_id: &str) -> String {
        let holder = self.artifact.as_ref().and_then(|artifact| {
            self.selected_nodes.iter().find(|id| {
                id.as_str() != node_id
                    && find_node(artifact, id).is_some_and(|node| {
                        super::multi::is_container(node) && super::doc_tree::find_node_in_node(node, node_id).is_some()
                    })
            })
        });
        holder.map_or_else(|| node_id.to_owned(), String::clone)
    }

    /// Whether a primary press pans the board instead of acting on what is under it: Space held, or the hand tool.
    pub(super) fn board_pan_mode(&self, ctx: &egui::Context) -> bool {
        self.view_ref().tool == super::board::Tool::Hand || ctx.input(|input| input.key_down(egui::Key::Space))
    }
}
