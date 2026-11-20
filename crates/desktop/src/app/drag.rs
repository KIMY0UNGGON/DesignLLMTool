use super::*;
use super::doc_tree::find_node;

impl DesktopApp {
    /// `start_drag_at_scale` at display scale 1, for the tests that do not snap.
    #[cfg(test)]
    pub(super) fn start_drag(&mut self, node_id: String, page_index: u32, bounds: designllm_contracts::Rect, page_size: Size, page_rect: Rect, start: Pos2) {
        self.start_drag_at_scale(node_id, page_index, bounds, page_size, page_rect, start, 1.0);
    }

    /// Starts a drag of a node in a window of display scale `pixels_per_point`. The drop snaps within 8 physical pixels at
    /// that scale (`commit_drag_if_reserved`).
    pub(super) fn start_drag_at_scale(
        &mut self,
        node_id: String,
        page_index: u32,
        bounds: designllm_contracts::Rect,
        page_size: Size,
        page_rect: Rect,
        start: Pos2,
        pixels_per_point: f32,
    ) {
        let Some(active) = self.active.clone() else {
            return;
        };
        // In a figure only panels and diagram nodes move. A panel is placed in figure coordinates;
        // a diagram node is placed relative to its panel, so it starts from its own stored bounds.
        let (is_panel, bounds) = if active.kind == ArtifactKind::Figure {
            if self.is_figure_panel(&node_id) {
                (true, bounds)
            } else if let Some(node) = self
                .artifact
                .as_ref()
                .and_then(|artifact| find_node(artifact, &node_id))
            {
                (false, node.bounds)
            } else {
                return;
            }
        } else {
            (false, frame_bounds_in(self.artifact.as_ref().and_then(|artifact| find_node(artifact, &node_id)), bounds))
        };
        let feature = if is_panel {
            Feature::FigurePanels
        } else {
            Feature::Geometry
        };
        if !self.feature_edit_supported(feature) || self.drag.is_some() {
            return;
        }
        let Some(context) = self.context.clone() else {
            return;
        };
        let source_page_id = self.artifact.as_ref().and_then(|artifact| {
            if artifact.id != active.id || artifact.revision != context.revision || context.artifact_id != active.id {
                return None;
            }
            match &artifact.body {
                ArtifactBody::Canvas(document) => document.pages.get(page_index as usize).map(|page| page.id.clone()),
                ArtifactBody::Slides(document) => document.pages.get(page_index as usize).map(|page| page.id.clone()),
                _ => None,
            }
        });
        let source_page_frame = self
            .page_textures
            .get(&page_index)
            .filter(|texture| texture.revision == context.revision)
            .and_then(|texture| texture.frames.get(&node_id).copied());
        self.set_selected_ids(vec![node_id.clone()]);
        self.set_selection(Selection {
            node_ids: vec![node_id.clone()],
            text_range: None,
        });
        let token = self.next_drag_token;
        self.next_drag_token += 1;
        self.drag = Some(DragState {
            token,
            node_id: node_id.clone(),
            bounds,
            page_size,
            source_page_index: page_index,
            source_page_id,
            source_page_rect: page_rect,
            source_page_frame,
            start,
            delta: Vec2::ZERO,
            base_revision: context.revision,
            reservation_id: None,
            released: false,
            is_panel,
            pixels_per_point,
            zoom: self.view_ref().zoom,
        });
        let request = ReservationRequest {
            artifact_id: active.id.clone(),
            expected_revision: context.revision,
            view_id: context.view_id.clone(),
            owner: "desktop".into(),
            node_ids: vec![node_id],
        };
        match serde_json::to_value(request) {
            Ok(params) => {
                if self.call("targets.reserve", params, PendingAction::ReserveDrag { token }).is_none() {
                    if self.drag.as_ref().is_some_and(|drag| drag.token == token) {
                        self.drag = None;
                    }
                    self.set_error("드래그 대상 예약을 시작하지 못해 이동을 취소했습니다. 다시 시도하세요".into());
                }
            }
            Err(_) => {
                self.drag = None;
                self.set_error(String::from("이동 대상을 예약하지 못했습니다"));
            }
        }
    }

    pub(super) fn page_drop_target(&self, position: Pos2) -> Option<(u32, String, Size, Rect, u32)> {
        let active = self.active.as_ref()?;
        if !matches!(active.kind, ArtifactKind::Canvas | ArtifactKind::Slides) {
            return None;
        }
        let artifact = self.artifact.as_ref()?;
        let context = self.context.as_ref()?;
        if artifact.id != active.id || active.revision != artifact.revision || artifact.revision != context.revision || context.artifact_id != active.id {
            return None;
        }
        let pages = match &artifact.body {
            ArtifactBody::Canvas(document) => document.pages.as_slice(),
            ArtifactBody::Slides(document) => document.pages.as_slice(),
            _ => return None,
        };
        let sizes: Vec<Size> = pages.iter().map(|page| page.size).collect();
        let view = self.view_ref();
        let slots = crate::board::arrange(active.kind, &sizes, view.zoom);
        let board_rect = self.board_rect?;
        let origin = board_rect.min + view.pan;
        let (slot, page_rect) = slots.iter().find_map(|slot| {
            let rect = crate::board::to_screen(slot.world, origin, view.zoom);
            rect.contains(position).then_some((slot, rect))
        })?;
        let page = pages.get(slot.page_index as usize)?;
        Some((slot.page_index, page.id.clone(), page.size, page_rect, page.nodes.len() as u32))
    }

    pub(super) fn commit_drag_if_reserved(&mut self) {
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        if !drag.released || drag.reservation_id.is_none() {
            return;
        }
        let drag = self.drag.take().unwrap();
        let drop_position = drag.start + drag.delta;
        let page_kind = self.active.as_ref().is_some_and(|active| matches!(active.kind, ArtifactKind::Canvas | ArtifactKind::Slides));
        let target = self.page_drop_target(drop_position);
        let mut operations = Vec::new();

        if page_kind && target.is_none() {
            self.set_error("페이지 안에 놓아 주세요".into());
        } else if let Some((_, target_page_id, target_size, target_rect, target_node_count)) = target
            .filter(|(page_index, ..)| *page_index != drag.source_page_index)
        {
            let source_page_matches = self.artifact.as_ref().is_some_and(|artifact| match &artifact.body {
                ArtifactBody::Canvas(document) => document
                    .pages
                    .get(drag.source_page_index as usize)
                    .is_some_and(|page| Some(page.id.as_str()) == drag.source_page_id.as_deref()),
                ArtifactBody::Slides(document) => document
                    .pages
                    .get(drag.source_page_index as usize)
                    .is_some_and(|page| Some(page.id.as_str()) == drag.source_page_id.as_deref()),
                _ => false,
            });
            if !source_page_matches {
                self.set_error("페이지 상태가 바뀌어 이동하지 않았습니다. 최신 상태를 다시 읽어 주세요".into());
            } else if let Some(source_frame) = drag.source_page_frame {
                let source_point = page_point_to_emu(drag.start, drag.source_page_rect, drag.page_size);
                let target_point = page_point_to_emu(drop_position, target_rect, target_size);
                if let (Some((source_x, source_y)), Some((target_x, target_y))) = (source_point, target_point) {
                    let grab_x = source_x.saturating_sub(source_frame.x);
                    let grab_y = source_y.saturating_sub(source_frame.y);
                    let bounds = designllm_contracts::Rect {
                        x: target_x.saturating_sub(grab_x),
                        y: target_y.saturating_sub(grab_y),
                        ..source_frame
                    };
                    // The node lands on the target page as a page child, so its frame is the page frame. The drop point is read
                    // on the target page as drawn now, so the snap window takes the zoom of now.
                    let zoom = self.view_ref().zoom;
                    let bounds = self.snap_free_move(&target_page_id, (0, 0), bounds, zoom, drag.pixels_per_point);
                    operations.push(Operation::MoveNode {
                        node_id: drag.node_id.clone(),
                        page_id: target_page_id,
                        parent_id: None,
                        index: target_node_count,
                    });
                    operations.push(Operation::SetGeometry { node_id: drag.node_id, bounds });
                } else {
                    self.set_error("이동할 페이지 좌표를 확인하지 못했습니다".into());
                }
            } else {
                self.set_error("이동할 개체의 페이지 위치를 확인하지 못했습니다".into());
            }
        } else if drag.source_page_rect.width() <= 0.0 || drag.source_page_rect.height() <= 0.0 {
            self.set_error("현재 페이지의 크기를 확인하지 못했습니다".into());
        } else {
            let dx = (drag.delta.x as f64 / f64::from(drag.source_page_rect.width()) * drag.page_size.width as f64).round() as i64;
            let dy = (drag.delta.y as f64 / f64::from(drag.source_page_rect.height()) * drag.page_size.height as f64).round() as i64;
            let bounds = designllm_contracts::Rect { x: drag.bounds.x.saturating_add(dx), y: drag.bounds.y.saturating_add(dy), ..drag.bounds };
            // A child of a stack or grid is placed by the engine: the drop changes its index, not its stored frame.
            let operation = if drag.is_panel {
                Some(Operation::ArrangePanels { bounds: [(drag.node_id.clone(), bounds)].into_iter().collect() })
            } else {
                match self.drag_drop(&drag.node_id, dx, dy) {
                    multi::DragDrop::Free => {
                        // The frame is relative to the node's parent, so the snap runs from the parent's place on the page. The move
                        // was measured on the page as drawn when the drag started, so the snap window takes that zoom.
                        let bounds = match drag.source_page_id.as_deref() {
                            Some(page_id) => match self.parent_frame_origin(page_id, &drag.node_id) {
                                Some(parent) => self.snap_free_move(page_id, parent, bounds, drag.zoom, drag.pixels_per_point),
                                None => bounds,
                            },
                            None => bounds,
                        };
                        Some(Operation::SetGeometry { node_id: drag.node_id, bounds })
                    }
                    multi::DragDrop::Unchanged => None,
                    multi::DragDrop::Reorder(operation) => Some(operation),
                }
            };
            if let Some(operation) = operation {
                operations.push(operation);
            }
        }

        if !operations.is_empty() {
            let intents = kind_commands::intents_of(&operations);
            self.apply_operations_at(operations, intents, drag.base_revision);
        }
        if let Some(reservation_id) = drag.reservation_id {
            self.call(
                "targets.release",
                json!({ "reservation_id": reservation_id }),
                PendingAction::ReleaseDrag,
            );
        }
    }
}
