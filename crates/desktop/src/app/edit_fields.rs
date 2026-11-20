use super::*;
use super::{doc_tree::{find_node, find_paragraph}, state::RegionClick};

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

    pub(super) fn add_node(&mut self, kind: NodeKind) {
        let Some(artifact) = self.artifact.as_ref() else {
            return;
        };
        let page = match &artifact.body {
            ArtifactBody::Canvas(document) => document.pages.get(self.page_index as usize),
            ArtifactBody::Slides(document) => document.pages.get(self.page_index as usize),
            _ => None,
        };
        let Some(page) = page else {
            self.set_error("선택한 페이지를 편집할 수 없습니다".into());
            return;
        };
        let page_id = page.id.clone();
        let index = page.nodes.len() as i64;
        let is_text = kind == NodeKind::Text;
        let node = Node {
            id: new_id(),
            kind,
            bounds: designllm_contracts::Rect {
                x: 20 * EMU_PER_MM,
                y: 20 * EMU_PER_MM + index * 18 * EMU_PER_MM,
                width: if is_text { 100 * EMU_PER_MM } else { 60 * EMU_PER_MM },
                height: if is_text { 16 * EMU_PER_MM } else { 40 * EMU_PER_MM },
            },
            text: if is_text { "텍스트".into() } else { String::new() },
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

    /// Outline, hover, drag and click handling of one clickable region of the preview. Returns the
    /// pointer position when the region was clicked (and not dragged).
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
        let response = ui.interact(rect, key, Sense::click_and_drag());
        // The outline of a node is drawn from its frame (selection.rs). A text-bearing shape has its text
        // lines as regions too; those regions must not draw outlines of their own.

        if response.drag_started() {
            // The drag starts where the press began. `interact_pointer_pos` is where egui recognised the drag (after the
            // click distance is passed), so the pointer's first move would be lost from the node's movement.
            let screen = ui.input(|input| input.pointer.press_origin()).or(response.interact_pointer_pos()).unwrap_or(rect.center());
            self.start_drag_at_scale(node_id.to_owned(), page_index, bounds, page_size, page_rect, screen, ui.ctx().pixels_per_point());
        }
        if response.dragged() {
            if let Some(drag) = self.drag.as_mut() {
                if drag.node_id == node_id {
                    let current = response.interact_pointer_pos().unwrap_or(drag.start);
                    drag.delta = current - drag.start;
                }
            }
        }
        if response.drag_stopped() && self.drag.as_ref().is_some_and(|drag| drag.node_id == node_id) {
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
                double_clicked: response.double_clicked(),
            })
        } else {
            None
        }
    }
}
