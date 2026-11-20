//! D17.6: the next or previous Canvas layer whose name or text holds the layers search query (`layer.find.next`,
//! `layer.find.previous`), across every page in layer order. The window and an agent's `ui.command.run` run the same arm.

use serde_json::json;

use super::DesktopApp;

impl DesktopApp {
    /// Selects the next (`forward`) or previous match after the selected node, wrapping at the ends; with nothing selected,
    /// the first or the last match. A match on another page opens that page, and a match outside the board is brought to its
    /// centre at the current zoom. An empty query or no match changes neither the selection nor the page. While a text is
    /// being edited on the board, nothing changes and the command is refused: the edit is finished first.
    pub(super) fn move_canvas_find_match(&mut self, forward: bool) -> Result<(), String> {
        let query = self.layer_find_query.trim().to_lowercase();
        if query.is_empty() {
            return Ok(());
        }
        let rows = self.layer_search_rows(&query);
        let matched: Vec<usize> = rows.iter().enumerate().filter(|(_, row)| row.2).map(|(index, _)| index).collect();
        let (Some(&first), Some(&last)) = (matched.first(), matched.last()) else {
            return Ok(());
        };
        if self.inline_text_edit.is_some() {
            return Err("텍스트 편집을 마친 뒤 다음·이전 찾기를 사용하세요".into());
        }
        let current = self.selected_nodes.first().and_then(|id| rows.iter().position(|row| &row.0 == id));
        let target = match current {
            Some(at) if forward => matched.iter().copied().find(|&index| index > at).unwrap_or(first),
            Some(at) => matched.iter().copied().rev().find(|&index| index < at).unwrap_or(last),
            None if forward => first,
            None => last,
        };
        let (node_id, page, _) = &rows[target];
        // Both page indices: the view's, and the one the selection reports (a reopened document starts the latter at 0).
        if *page != self.view_ref().page_index || *page != self.page_index {
            self.invoke("nav.page", json!({ "page_index": page }))?;
        }
        self.select_node(node_id.clone());
        self.canvas_find_reveal = Some(node_id.clone());
        self.reveal_pending_canvas_find();
        Ok(())
    }

    /// Once the board is fitted, reveal the match using its stored bounds until its page has a current layout.
    /// A hidden match has no drawn frame, so its stored bounds are final.
    pub(super) fn reveal_pending_canvas_find(&mut self) {
        let Some(node_id) = self.canvas_find_reveal.as_ref() else { return };
        if self.selected_nodes.first() != Some(node_id) {
            self.canvas_find_reveal = None;
            return;
        }
        let view = self.view_ref();
        let (Some(board_rect), true) = (self.board_rect, view.fitted) else { return };
        let zoom = view.zoom;
        let origin = board_rect.min + view.pan;
        let Some((_, frame)) = self.node_frames(origin, zoom).into_iter().find(|(id, _)| id == node_id) else {
            self.canvas_find_reveal = None;
            return;
        };
        let page_drawn = self.page_textures.get(&view.page_index).zip(self.active.as_ref()).is_some_and(|(texture, active)| {
            texture.artifact_id == active.id && texture.revision == active.revision
        });
        if page_drawn || self.selected_on_page().is_some_and(|(node, _, _)| node.ext.hidden == Some(true)) {
            self.canvas_find_reveal = None;
        }
        if board_rect.contains_rect(frame) {
            return;
        }
        let content = eframe::egui::Rect::from_min_size(((frame.min - origin) / zoom).to_pos2(), frame.size() / zoom);
        let centred = crate::board::centred_origin(content, board_rect, zoom);
        self.view().pan = centred - board_rect.min;
    }
}
