//! The right-click menus of a Canvas or Slides node (its board region and its layer row) and of a Canvas page row (68 item
//! 16). The node menu is the toolbar's ⋯ menu; a right click selects the node first unless it is selected already, so each
//! item runs on what the menu was opened for. A table's nodes (the table and its cells) keep their own menu
//! (`table_merge`), from the board and from their layer rows alike.

use designllm_contracts::{ArtifactBody, ArtifactKind, Node, NodeKind};
use eframe::egui::{self, Response, Ui};
use serde_json::json;

use super::{doc_tree::find_node, DesktopApp};
use crate::{commands, theme, widgets};

/// The node menu's entries in order (`None`: a separator), for a node of `kind` with the given flags.
fn node_menu_entries(kind: ArtifactKind, many: bool, hidden: bool, locked: bool) -> Vec<Option<&'static str>> {
    let mut entries = Vec::new();
    if !many {
        entries.push(Some("edit.duplicate"));
    }
    entries.extend([Some("edit.copy"), Some("edit.paste"), Some("edit.copy_properties"), Some("edit.paste_properties"), None]);
    entries.extend([Some("edit.group"), Some("edit.ungroup"), Some("edit.stack"), None]);
    if !many {
        entries.extend([Some("edit.order_forward"), Some("edit.order_backward"), Some("edit.order_front"), Some("edit.order_back"), None]);
    }
    entries.extend(super::selection::layer_state_commands(kind, hidden, locked).into_iter().map(Some));
    entries.extend([None, Some("edit.delete")]);
    entries
}

/// Whether `id` is a table or one of a table's cells on the pages.
fn member_in(nodes: &[Node], id: &str) -> bool {
    nodes.iter().any(|node| {
        let own = node
            .ext
            .table
            .as_ref()
            .is_some_and(|table| node.id == id || table.rows.iter().any(|row| row.cell_ids.iter().any(|cell| cell == id)));
        own || member_in(&node.children, id)
    })
}

impl DesktopApp {
    /// Whether the node is a table or one of its cells (their right click belongs to the table menu).
    pub(super) fn is_table_member(&self, node_id: &str) -> bool {
        let Some(artifact) = self.artifact.as_ref() else { return false };
        let ArtifactBody::Canvas(document) = &artifact.body else { return false };
        document.pages.iter().any(|page| member_in(&page.nodes, node_id))
    }

    /// A right click on a node: the node is selected unless the selection holds it, and its page is current.
    fn select_for_menu(&mut self, page_index: Option<u32>, node_id: &str) {
        if let Some(page_index) = page_index {
            self.focus_page(page_index);
        }
        if !self.selected_nodes.iter().any(|id| id == node_id) {
            self.select_ids(vec![node_id.to_owned()]);
        }
    }

    /// The node menu's items for `node_id` (the selection when it holds the node): the toolbar's ⋯ menu.
    pub(super) fn draw_node_menu(&mut self, ui: &mut Ui, palette: &theme::Palette, node_id: &str) {
        let Some((group, hidden, locked)) = self
            .artifact
            .as_ref()
            .and_then(|artifact| find_node(artifact, node_id))
            .map(|node| (node.kind == NodeKind::Group, node.ext.hidden == Some(true), node.ext.locked == Some(true)))
        else {
            return;
        };
        let many = self.selected_nodes.len() > 1;
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        for entry in node_menu_entries(kind, many, hidden, locked) {
            let Some(command) = entry else {
                ui.separator();
                continue;
            };
            if !commands::available(command) || (command == "edit.ungroup" && !group) || (command == "edit.group" && !many && group) {
                continue;
            }
            let Some(spec) = commands::find(command) else { continue };
            if widgets::menu_item(ui, palette, None, spec.label, spec.shortcut, false).clicked() {
                self.invoke_ui(command, json!({}));
                ui.close();
            }
        }
    }

    /// Whether the open document's nodes take the node menu: Canvas and Slides. A figure's diagram boxes and connectors, an
    /// outline's headings and a project's files have menus of their own or none.
    fn node_menu_applies(&self) -> bool {
        self.active.as_ref().is_some_and(|item| matches!(item.kind, ArtifactKind::Canvas | ArtifactKind::Slides))
    }

    /// The context menu of a board region of a node (`response`). A table's nodes are left to the table menu.
    pub(super) fn node_region_menu(&mut self, ctx: &egui::Context, response: &Response, page_index: u32, node_id: &str) {
        let opening = response.secondary_clicked();
        if !opening && !egui::Popup::is_id_open(ctx, egui::Popup::default_response_id(response)) {
            return;
        }
        if !self.node_menu_applies() || self.is_table_member(node_id) {
            return;
        }
        if opening {
            self.select_for_menu(Some(page_index), node_id);
        }
        self.show_node_menu(ctx, response, egui::Popup::default_response_id(response), node_id);
    }

    /// The context menu of the layer row of a Canvas or Slides node on page `page_index`, at any depth. `popup_id` is the
    /// row's own (its node), so the menu stays with its node when the rows move. A table and its cells open the board's
    /// table menu, with its selection rules.
    pub(super) fn node_row_menu(&mut self, ctx: &egui::Context, response: &Response, popup_id: egui::Id, node_id: &str, page_index: u32) {
        let opening = response.secondary_clicked();
        if !opening && !egui::Popup::is_id_open(ctx, popup_id) {
            return;
        }
        if !self.node_menu_applies() {
            return;
        }
        if self.is_table_member(node_id) {
            if opening {
                self.open_table_menu_for(node_id, page_index);
            }
            return;
        }
        if opening {
            self.select_for_menu(Some(page_index), node_id);
        }
        self.show_node_menu(ctx, response, popup_id, node_id);
    }

    fn show_node_menu(&mut self, ctx: &egui::Context, response: &Response, popup_id: egui::Id, node_id: &str) {
        let palette = theme::palette(ctx);
        egui::Popup::context_menu(response).id(popup_id).show(|ui| {
            ui.set_width(240.0);
            self.draw_node_menu(ui, &palette, node_id);
        });
    }

    /// The context menu of a page row (the page at `page_index` when it opened): add a page, remove this page. The page is
    /// kept by its id while the menu is open, so a page list that changes meanwhile removes that page or none.
    pub(super) fn page_row_menu(&mut self, ctx: &egui::Context, response: &Response, popup_id: egui::Id, page_index: u32) {
        let opening = response.secondary_clicked();
        if !opening && !egui::Popup::is_id_open(ctx, popup_id) {
            return;
        }
        let page_key = popup_id.with("page-id");
        if opening {
            self.focus_page(page_index);
            let page_id = self.canvas_page_ids().and_then(|ids| ids.get(page_index as usize).cloned());
            ctx.data_mut(|data| match page_id {
                Some(page_id) => data.insert_temp(page_key, page_id),
                None => data.remove::<String>(page_key),
            });
        }
        let palette = theme::palette(ctx);
        egui::Popup::context_menu(response).id(popup_id).show(|ui| {
            ui.set_width(240.0);
            for command in ["add.page", "page.remove"] {
                if !commands::available(command) {
                    continue;
                }
                let Some(spec) = commands::find(command) else { continue };
                if !widgets::menu_item(ui, &palette, None, spec.label, "", false).clicked() {
                    continue;
                }
                ui.close();
                if command == "add.page" {
                    self.invoke_ui(command, json!({}));
                    continue;
                }
                let page_id: Option<String> = ctx.data(|data| data.get_temp(page_key));
                match page_id.and_then(|page_id| self.canvas_page_ids()?.iter().position(|id| *id == page_id)) {
                    Some(index) => self.invoke_ui(command, json!({ "page_index": index })),
                    None => self.set_error("메뉴를 연 페이지를 찾지 못했습니다".into()),
                }
            }
        });
    }

    fn canvas_page_ids(&self) -> Option<Vec<String>> {
        let artifact = self.artifact.as_ref()?;
        matches!(artifact.body, ArtifactBody::Canvas(_)).then(|| super::sidebar::page_ids_for_body(&artifact.body)).flatten()
    }
}
