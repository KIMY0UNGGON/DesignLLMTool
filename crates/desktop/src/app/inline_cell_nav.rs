//! Typing in a Canvas table the way a slide table works (68 item 11).
//!
//! - A click on a cell puts the caret in it; a drag that starts inside a cell opens it and selects text (the table
//!   selected or not). The table's border band (`TABLE_BORDER` points either side of its frame, drawn over the edge
//!   cells and under the selection handles) is the table itself: a click there selects the table, a drag moves it.
//! - In a cell's edit, Tab and Shift+Tab move to the next and previous cell, an arrow key on the first or last drawn row
//!   (or at the start or end of the text) moves to the cell beside it, Enter is a line break, and Escape ends the edit
//!   with the table selected. A click elsewhere ends the edit and is then handled as a click.
//! - With a table or a text-bearing node selected, Enter and F2 open its text (a table: the cell edited last, else its
//!   first cell). F2 that opens nothing stays `edit.rename`.
//!
//! A cell's edit ends through `finish_inline_text_edit`, which may wait for its last change or reservation to come back.
//! What follows is kept in `CellNav::follow` and done on the first frame with no edit and a current scene. A frame takes
//! one key that leaves the cell, and only as its first key: the keys before it are the cell's, the keys after it are held
//! (`held`) and given to what follows, in order. Keys held for a follow-up that is dropped are not given to the board;
//! if they held text, a notice says it was not entered.
//!
//! Each change of a cell waits for the page's render of it before the next one is sent
//! (`InlineCellReadiness::WaitingForRender`): the editor stays on the shown frame and keeps taking input, a hint shows once
//! the wait is noticeable, the render is asked for again after `RENDER_ASK_AGAIN`, and after `RENDER_GIVE_UP` (or a failed
//! render) the queued input is kept as the draft behind the conflict notice, which copies, retries or discards it.

use designllm_contracts::{ArtifactBody, ArtifactKind, Node, Selection, TextPosition};
use eframe::egui::{self, Context, Pos2, Vec2};

use super::{
    doc_tree::{find_canvas_table_cell, find_node, inline_text_target_node_ids},
    DesktopApp, Feature, InlineCellReadiness,
};

/// A follow-up older than this (seconds) is dropped.
const FOLLOW_EXPIRY: f64 = 5.0;
/// The wait for a cell's render after which the render is asked for again (seconds).
const RENDER_ASK_AGAIN: f64 = 1.5;
/// The wait for a cell's render after which the queued input is kept as a draft (seconds).
const RENDER_GIVE_UP: f64 = 5.0;
/// The wait after which the cell shows that it is waiting (seconds): a render that comes back quickly shows nothing.
const WAIT_HINT_AFTER: f64 = 0.3;
/// Half the width of a table's border band, in screen points.
const TABLE_BORDER: f32 = 4.0;
/// The most keys held for a follow-up.
const HELD_EVENTS: usize = 256;

/// The table typing state of the window.
#[derive(Default)]
pub(super) struct CellNav {
    follow: Option<Follow>,
    /// Keys typed while a follow-up waits; they go to what follows (`replay`).
    held: Vec<egui::Event>,
    /// Some keys held for a follow-up did not fit (`HELD_EVENTS`).
    overflowed: bool,
    /// Keys given back to the next frame's input, first.
    replay: Vec<egui::Event>,
    /// Tabs typed while the composition of the edit with this id was open: they follow its commit
    /// (`defer_tab_in_cell_composition`).
    composing_tabs: Option<(u64, Vec<egui::Event>)>,
    wait: Option<CellWait>,
    /// The table and cell edited last.
    last_edited: Option<(String, String)>,
    /// A drag that started inside a cell waits for its hit reply to open the cell and select from the press.
    pub(super) text_drag: bool,
    /// The table a border band drag moves.
    band_drag: Option<String>,
    /// The last hit test sent (page, point, scale, additive, edit text) and whether it was already sent again.
    last_hit: Option<(u32, Pos2, f32, bool, bool)>,
    hit_retried: bool,
    retrying_hit: bool,
    /// The last primary release (egui time, place): a press on the editor soon after it, near it, is its second click.
    last_release: Option<(f64, Pos2)>,
}

struct Follow {
    kind: CellEditFollow,
    /// egui time it was asked for.
    since: f64,
    /// The view's zoom and pan then: a click is in screen points and means nothing once the view moved.
    view: (f32, Vec2),
}

struct CellWait {
    session_id: u64,
    revision: u64,
    since: f64,
    asked_again: bool,
}

/// What a cell's edit is followed by once it has ended.
#[derive(Clone, Debug)]
pub(super) enum CellEditFollow {
    /// Edit another cell, the caret at `position` (`None`: at its end).
    Cell { artifact_id: String, page_index: u32, cell_id: String, position: Option<TextPosition> },
    /// Handle a board click again (`None`: the click was off every page).
    /// `additive`: Shift was held, so the click adds its cell to the selection (68 item 14).
    Click { artifact_id: String, at: Option<(u32, Pos2)>, pixels_per_point: f32, additive: bool },
    /// The click was sent again: the keys wait for its reply (it may open a cell).
    AwaitHit { artifact_id: String },
    /// Select the table.
    SelectTable { artifact_id: String, table_id: String },
}

/// Where a key moves the edit to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CellStep {
    Next,
    Previous,
    Left,
    Right,
    Up,
    Down,
}

fn is_keyboard(event: &egui::Event) -> bool {
    matches!(event, egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Ime(_) | egui::Event::Cut | egui::Event::Copy
        | egui::Event::Key { .. })
}

fn is_text(event: &egui::Event) -> bool {
    matches!(event, egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Ime(egui::ImeEvent::Commit(_)))
}

/// The editable cell a step from `cell_id` lands on: covered (merged) and locked cells are passed over. `None` past the
/// table's edge or when `cell_id` is not a cell of a table on `page_index`.
pub(super) fn neighbour_cell(artifact: &designllm_contracts::Artifact, page_index: u32, cell_id: &str, step: CellStep) -> Option<String> {
    let ArtifactBody::Canvas(document) = &artifact.body else { return None };
    let page = document.pages.get(page_index as usize)?;
    let (table_node, _, _, _, _) = find_canvas_table_cell(&page.nodes, cell_id, None, false, false)?;
    let table = table_node.ext.table.as_ref()?;
    let columns = table.rows.first()?.cell_ids.len();
    if columns == 0 || table.rows.iter().any(|row| row.cell_ids.len() != columns) {
        return None;
    }
    let (row, column) = table.rows.iter().enumerate().find_map(|(row, table_row)| {
        table_row.cell_ids.iter().position(|id| id == cell_id).map(|column| (row, column))
    })?;
    let rows = table.rows.len() as i64;
    let (mut row, mut column) = (row as i64, column as i64);
    loop {
        match step {
            CellStep::Next | CellStep::Previous => {
                let linear = row * columns as i64 + column + if step == CellStep::Next { 1 } else { -1 };
                if linear < 0 || linear >= rows * columns as i64 {
                    return None;
                }
                row = linear / columns as i64;
                column = linear % columns as i64;
            }
            CellStep::Left => column -= 1,
            CellStep::Right => column += 1,
            CellStep::Up => row -= 1,
            CellStep::Down => row += 1,
        }
        if row < 0 || row >= rows || column < 0 || column >= columns as i64 {
            return None;
        }
        let id = &table.rows[row as usize].cell_ids[column as usize];
        if editable_cell(artifact, page_index, id) {
            return Some(id.clone());
        }
    }
}

fn editable_cell(artifact: &designllm_contracts::Artifact, page_index: u32, id: &str) -> bool {
    inline_text_target_node_ids(artifact, ArtifactKind::Canvas, page_index, id).is_some_and(|ids| ids.len() > 1)
}

/// The cell a table's text opens in: `last` when it is still an editable cell of the table, else the first editable cell.
fn table_entry_cell(artifact: &designllm_contracts::Artifact, page_index: u32, table_id: &str, last: Option<&(String, String)>) -> Option<String> {
    let table = find_node(artifact, table_id)?.ext.table.as_ref()?;
    let cells = || table.rows.iter().flat_map(|row| row.cell_ids.iter());
    last.filter(|(table, cell)| table == table_id && cells().any(|id| id == cell) && editable_cell(artifact, page_index, cell))
        .map(|(_, cell)| cell.clone())
        .or_else(|| cells().find(|id| editable_cell(artifact, page_index, id)).cloned())
}

/// The tables of a page whose border band is drawn: not locked, not inside a component instance (an instance moves as
/// the instance does), with at least one cell.
fn band_tables<'a>(nodes: &'a [Node], found: &mut Vec<&'a Node>) {
    for node in nodes {
        if node.ext.instance.is_some() || node.ext.locked == Some(true) {
            continue;
        }
        if let Some(table) = node.ext.table.as_ref() {
            if table.rows.first().is_some_and(|row| !row.cell_ids.is_empty()) {
                found.push(node);
            }
            continue;
        }
        band_tables(&node.children, found);
    }
}

/// The four strips of a table's border band around `frame` (on screen).
fn band_strips(frame: egui::Rect) -> [egui::Rect; 4] {
    let (outer, inner) = (frame.expand(TABLE_BORDER), frame.shrink(TABLE_BORDER));
    [
        egui::Rect::from_min_max(outer.min, egui::pos2(outer.max.x, inner.min.y)),
        egui::Rect::from_min_max(egui::pos2(outer.min.x, inner.max.y), outer.max),
        egui::Rect::from_min_max(egui::pos2(outer.min.x, inner.min.y), egui::pos2(inner.min.x, inner.max.y)),
        egui::Rect::from_min_max(egui::pos2(inner.max.x, inner.min.y), egui::pos2(outer.max.x, inner.max.y)),
    ]
}

impl CellNav {
    /// A hit test sent (`hit_test_page`): kept so that a reply refused as stale can be sent once more.
    pub(super) fn note_hit(&mut self, page_index: u32, position: Pos2, pixels_per_point: f32, additive: bool, edit_text: bool) {
        self.last_hit = Some((page_index, position, pixels_per_point, additive, edit_text));
        self.hit_retried = std::mem::take(&mut self.retrying_hit);
    }
}

impl DesktopApp {
    fn view_now(&self) -> (f32, Vec2) {
        let view = self.view_ref();
        (view.zoom, view.pan)
    }

    fn set_cell_follow(&mut self, ctx: &Context, kind: CellEditFollow) {
        let since = ctx.input(|input| input.time);
        let view = self.view_now();
        self.cell_nav.follow = Some(Follow { kind, since, view });
    }

    fn hold_keys(&mut self, keys: impl IntoIterator<Item = egui::Event>) {
        for key in keys {
            if self.cell_nav.held.len() < HELD_EVENTS {
                self.cell_nav.held.push(key);
            } else {
                self.cell_nav.overflowed = true;
            }
        }
    }

    /// Runs first on every frame's input (`raw_input_hook`). Notes the last primary release for the editor's click count;
    /// holds the keyboard while a follow-up waits; else gives back the keys held for the next frame, first.
    pub(super) fn route_cell_follow_input(&mut self, input: &mut egui::RawInput) {
        self.text_adapter.set_previous_click(self.cell_nav.last_release);
        if let (Some(time), Some(at)) = (input.time, input.events.iter().rev().find_map(|event| match event {
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, .. } => Some(*pos),
            _ => None,
        })) {
            self.cell_nav.last_release = Some((time, at));
        }
        if self.cell_nav.follow.is_some() {
            let (keys, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut input.events).into_iter().partition(is_keyboard);
            input.events = rest;
            self.hold_keys(keys);
        } else if !self.cell_nav.replay.is_empty() {
            let mut events = std::mem::take(&mut self.cell_nav.replay);
            events.append(&mut input.events);
            input.events = events;
        }
    }

    /// The key of a table cell's edit that leaves the cell, taken out of `input` before the editor sees it. A frame takes
    /// one such key, and only as its first key: a leaving key after other keys waits for the next frame with the keys
    /// after it (the keys before it are this cell's). The keys after a taken key are held for the next cell. Nothing is
    /// taken while a composition is open, the edit shows a conflict, or the edit is not of a table cell. Returns whether
    /// the edit is ending.
    pub(super) fn take_inline_cell_navigation(&mut self, ctx: &Context, input: &mut egui::RawInput) -> bool {
        let Some(session) = self.inline_text_edit.as_ref() else { return false };
        if session.reservation_node_ids.len() <= 1 || session.composing || session.conflict.is_some() || self.text_adapter.is_ime_active() {
            return false;
        }
        let (artifact_id, page_index, cell_id, table_id) =
            (session.artifact_id.clone(), session.page_index, session.node_id.clone(), session.reservation_node_ids[1].clone());
        let leaving = |event: &egui::Event| matches!(event, egui::Event::Key { key: egui::Key::Tab | egui::Key::Escape
            | egui::Key::ArrowLeft | egui::Key::ArrowRight | egui::Key::ArrowUp | egui::Key::ArrowDown, pressed: true, modifiers, .. }
            if !modifiers.ctrl && !modifiers.alt && !modifiers.mac_cmd);
        let Some(first_key) = input.events.iter().position(is_keyboard) else { return false };
        // A leaving key after other keys: it and what follows it wait for the next frame.
        if let Some(later) = input.events.iter().enumerate().skip(first_key + 1).find(|(_, event)| leaving(event)).map(|(index, _)| index) {
            let tail = input.events.split_off(later);
            let (keys, rest): (Vec<_>, Vec<_>) = tail.into_iter().partition(is_keyboard);
            input.events.extend(rest);
            self.cell_nav.replay.extend(keys);
        }
        if !leaving(&input.events[first_key]) {
            return false;
        }
        let egui::Event::Key { key, modifiers, .. } = input.events[first_key].clone() else { return false };
        let step = match key {
            egui::Key::Tab => Some(if modifiers.shift { CellStep::Previous } else { CellStep::Next }),
            egui::Key::Escape => None,
            arrow => {
                if modifiers.shift {
                    return false;
                }
                // Where the caret is: at the start or end of the text, on the first or last drawn row.
                let Some(index) = egui::TextEdit::load_state(ctx, self.text_editor_id)
                    .and_then(|state| state.cursor.char_range())
                    .filter(|range| range.primary.index == range.secondary.index)
                    .map(|range| range.primary.index)
                else { return false };
                let (first_row, last_row) = self.text_adapter.caret_row_edges(index).unwrap_or_else(|| {
                    let split = self.text_draft.char_indices().nth(index).map_or(self.text_draft.len(), |(byte, _)| byte);
                    (!self.text_draft[..split].contains('\n'), !self.text_draft[split..].contains('\n'))
                });
                let (at_edge, step) = match arrow {
                    egui::Key::ArrowLeft => (index == 0, CellStep::Left),
                    egui::Key::ArrowRight => (index >= self.text_draft.chars().count(), CellStep::Right),
                    egui::Key::ArrowUp => (first_row, CellStep::Up),
                    _ => (last_row, CellStep::Down),
                };
                if !at_edge {
                    return false;
                }
                Some(step)
            }
        };
        let target = step.and_then(|step| self.artifact.as_ref().and_then(|artifact| neighbour_cell(artifact, page_index, &cell_id, step)));
        // An arrow at the table's edge stays the editor's (the caret does not move).
        if matches!(step, Some(CellStep::Left | CellStep::Right | CellStep::Up | CellStep::Down)) && target.is_none() {
            return false;
        }
        let rest = input.events.split_off(first_key + 1);
        input.events.pop();
        let (keys, pointer): (Vec<_>, Vec<_>) = rest.into_iter().partition(is_keyboard);
        input.events.extend(pointer);
        let kind = match (step, target) {
            (None, _) => {
                self.cell_nav.last_edited = Some((table_id.clone(), cell_id));
                CellEditFollow::SelectTable { artifact_id, table_id }
            }
            (Some(step), Some(target)) => {
                let position = matches!(step, CellStep::Right | CellStep::Down)
                    .then(|| TextPosition { paragraph_id: target.clone(), grapheme_index: 0, trailing: false });
                CellEditFollow::Cell { artifact_id, page_index, cell_id: target, position }
            }
            // Tab at the table's edge does nothing and keeps the focus in the cell; the keys after it come next frame.
            (Some(_), None) => {
                self.cell_nav.replay.splice(0..0, keys);
                return false;
            }
        };
        self.set_cell_follow(ctx, kind);
        self.hold_keys(keys);
        self.finish_inline_text_edit();
        true
    }

    /// A Tab in a table cell's edit while its composition is open (one frame [Preedit, Tab], 68p 2) can leave the cell only
    /// once the composition is committed (`take_inline_cell_navigation` takes nothing while one is open), and given to the
    /// editor it is lost (egui takes it as a focus move). It is taken out of `input` and put back after the events that
    /// close the composition, in this frame or a later one, where `split_after_closed_cell_composition` hands it to the
    /// cell navigation. The Tabs kept are this edit's only: another edit drops them (`release_cell_composition_tabs`).
    pub(super) fn defer_tab_in_cell_composition(&mut self, input: &mut egui::RawInput) {
        let Some(session) = self.inline_text_edit.as_ref().filter(|session| session.reservation_node_ids.len() > 1) else { return };
        let session_id = session.id;
        let mut open = session.composing || self.text_adapter.is_ime_active();
        if !open {
            return;
        }
        let mut tabs = self.cell_nav.composing_tabs.take().filter(|(id, _)| *id == session_id).map(|(_, tabs)| tabs).unwrap_or_default();
        let is_tab = |event: &egui::Event| matches!(event, egui::Event::Key { key: egui::Key::Tab, pressed: true, modifiers, .. }
            if !modifiers.ctrl && !modifiers.alt && !modifiers.mac_cmd);
        let mut closed = false;
        let mut events = Vec::with_capacity(input.events.len() + tabs.len());
        for event in std::mem::take(&mut input.events) {
            // The Tabs go after the commit and the `Disabled` around it, before anything else.
            if closed && !open && !matches!(event, egui::Event::Ime(egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled)) {
                events.append(&mut tabs);
            }
            match &event {
                egui::Event::Ime(egui::ImeEvent::Enabled) => open = true,
                egui::Event::Ime(egui::ImeEvent::Preedit(preedit)) => open |= !preedit.is_empty(),
                egui::Event::Ime(egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled) => {
                    open = false;
                    closed = true;
                }
                _ if open && is_tab(&event) => {
                    tabs.push(event);
                    continue;
                }
                _ => {}
            }
            events.push(event);
        }
        if !open {
            events.append(&mut tabs);
        } else if !tabs.is_empty() {
            self.cell_nav.composing_tabs = Some((session_id, tabs));
        }
        input.events = events;
    }

    /// The Tabs `defer_tab_in_cell_composition` kept, when their composition was closed without its events reaching it (the
    /// editor's IME state reset): they go first in this frame's input of the same edit, before its cell navigation, never
    /// into a later frame (where the edit may have ended or moved on). Another edit, or none, drops them.
    pub(super) fn release_cell_composition_tabs(&mut self, input: &mut egui::RawInput) {
        let Some((session_id, _)) = self.cell_nav.composing_tabs.as_ref() else { return };
        let Some(session) = self.inline_text_edit.as_ref().filter(|session| session.id == *session_id) else {
            self.cell_nav.composing_tabs = None;
            return;
        };
        if session.composing || self.text_adapter.is_ime_active() {
            return;
        }
        if let Some((_, tabs)) = self.cell_nav.composing_tabs.take() {
            input.events.splice(0..0, tabs);
        }
    }

    /// Whether `position` (screen) is on the border band of `table_id` on page `page_index` drawn at `page_rect`.
    fn on_table_band(&self, table_id: &str, page_index: u32, page_rect: egui::Rect, position: Pos2) -> bool {
        let Some(artifact) = self.artifact.as_ref() else { return false };
        let Some(node) = find_node(artifact, table_id) else { return false };
        let Some(page) = self.page_textures.get(&page_index) else { return false };
        let frame = page.frames.get(table_id).copied().unwrap_or(node.bounds);
        let screen = super::page_to_screen(frame, page.page.size, page_rect);
        band_strips(screen).iter().any(|strip| strip.contains(position))
    }

    /// A board click while a table cell is edited (`at`: the page, the point and the page's screen rectangle). On the
    /// table's border band the edit ends with the table selected; off the cell the edit ends, then the click is handled as
    /// a click. A click on the cell itself reaches the board only in the editor's first frame, before the editor takes the
    /// pointer: it leaves the edit as it is.
    pub(super) fn click_ends_inline_cell_edit(&mut self, ctx: &Context, at: Option<(u32, Pos2, egui::Rect)>, pixels_per_point: f32) {
        let Some(session) = self.inline_text_edit.as_ref().filter(|session| session.reservation_node_ids.len() > 1) else { return };
        if session.composing || self.text_adapter.is_ime_active() {
            return;
        }
        let (artifact_id, table_id, cell_id, session_page) =
            (session.artifact_id.clone(), session.reservation_node_ids[1].clone(), session.node_id.clone(), session.page_index);
        let on_band = at.is_some_and(|(page_index, position, page_rect)| {
            page_index == session_page && self.on_table_band(&table_id, page_index, page_rect, position)
        });
        let on_cell = at.is_some_and(|(page_index, position, page_rect)| {
            page_index == session_page && self.page_textures.get(&page_index).is_some_and(|page| {
                page.frames.get(&cell_id)
                    .is_some_and(|frame| super::page_to_screen(*frame, page.page.size, page_rect).contains(position))
            })
        });
        if on_band {
            if let Some((page_index, ..)) = at {
                self.focus_page(page_index);
            }
            self.cell_nav.last_edited = Some((table_id.clone(), cell_id));
            self.set_cell_follow(ctx, CellEditFollow::SelectTable { artifact_id, table_id });
        } else if on_cell {
            return;
        } else {
            if let Some((page_index, ..)) = at {
                self.focus_page(page_index);
            }
            let additive = super::multi::additive_modifier(ctx);
            self.set_cell_follow(ctx, CellEditFollow::Click { artifact_id, at: at.map(|(page, position, _)| (page, position)), pixels_per_point, additive });
        }
        self.finish_inline_text_edit();
    }

    /// A hit reply that comes back while a text is edited (a click sent before the edit opened). A double click on the
    /// edited cell selects the word there; on another editable cell of a table cell's edit the edit moves there; anything
    /// else leaves the edit and the selection as they are, so a late reply cannot pull the selection from under the edit.
    pub(super) fn hit_while_editing(&mut self, ctx: &Context, page_index: u32, node_id: &str, position: Option<TextPosition>, double: bool) {
        let Some(session) = self.inline_text_edit.as_ref() else { return };
        if session.composing || session.conflict.is_some() {
            return;
        }
        if session.node_id == node_id {
            if let Some(position) = position.filter(|position| double && position.paragraph_id == node_id) {
                self.text_adapter.select_word_at_grapheme(ctx, self.text_editor_id, &self.text_draft, position.grapheme_index);
            }
            return;
        }
        if session.reservation_node_ids.len() <= 1 || !self.artifact.as_ref().is_some_and(|artifact| editable_cell(artifact, page_index, node_id)) {
            return;
        }
        let artifact_id = session.artifact_id.clone();
        let position = position.filter(|position| position.paragraph_id == node_id);
        self.set_cell_follow(ctx, CellEditFollow::Cell { artifact_id, page_index, cell_id: node_id.to_owned(), position });
        self.finish_inline_text_edit();
    }

    /// A hit reply refused because the scene moved on since the click: the click is sent once more against the current
    /// scene. Returns false when it was sent again already.
    pub(super) fn retry_stale_hit(&mut self) -> bool {
        if self.cell_nav.hit_retried {
            return false;
        }
        let Some((page_index, position, pixels_per_point, additive, edit_text)) = self.cell_nav.last_hit else { return false };
        self.cell_nav.retrying_hit = true;
        self.hit_test_page(page_index, position, pixels_per_point, additive, edit_text);
        true
    }

    /// A hit reply was handled (or refused): keys held for it go on.
    pub(super) fn hit_answered(&mut self) {
        if self.cell_nav.follow.as_ref().is_some_and(|follow| matches!(follow.kind, CellEditFollow::AwaitHit { .. })) {
            self.cell_nav.follow = None;
            self.release_held_keys();
        }
    }

    /// Opens the text of the one selected node for Enter or F2: a table's cell edited last (else its first cell), or the
    /// node's own text. `None` when the selection has no text to open.
    pub(super) fn begin_text_edit_of_selection(&mut self, ctx: &Context) -> Option<bool> {
        if self.inline_text_edit.is_some() || self.selected_nodes.len() != 1 {
            return None;
        }
        let (Some(active), Some(artifact)) = (self.active.as_ref(), self.artifact.as_ref()) else { return None };
        let node_id = self.selected_nodes[0].clone();
        let page_index = super::board::node_page_index(artifact, &node_id)?;
        let target = if find_node(artifact, &node_id).is_some_and(|node| node.ext.table.is_some()) && active.kind == ArtifactKind::Canvas {
            table_entry_cell(artifact, page_index, &node_id, self.cell_nav.last_edited.as_ref())?
        } else if inline_text_target_node_ids(artifact, active.kind, page_index, &node_id).is_some() {
            node_id
        } else {
            return None;
        };
        Some(self.begin_inline_text_edit(ctx, &target, page_index, None))
    }

    /// F2 before the registry's shortcuts (`handle_shortcuts`): it is taken only when it opened a text; otherwise it stays
    /// `edit.rename`.
    pub(super) fn take_f2_for_text_edit(&mut self, ctx: &Context) {
        if !self.screen_is_editor() || ctx.wants_keyboard_input()
            || !ctx.input(|input| input.key_pressed(egui::Key::F2) && input.modifiers.is_none())
        {
            return;
        }
        if self.begin_text_edit_of_selection(ctx) == Some(true) {
            ctx.input_mut(|input| { input.consume_key(egui::Modifiers::NONE, egui::Key::F2); });
        }
    }

    /// Each frame of the board: the render watch of a cell's edit, then a waiting follow-up.
    pub(super) fn tick_inline_cells(&mut self, ctx: &Context) {
        let now = ctx.input(|input| input.time);
        self.watch_cell_render(now);
        self.resume_cell_edit_follow(ctx, now);
    }

    fn watch_cell_render(&mut self, now: f64) {
        if let Some(session) = self.inline_text_edit.as_ref().filter(|session| session.reservation_node_ids.len() > 1) {
            if self.cell_nav.last_edited.as_ref().is_none_or(|(table, cell)| *table != session.reservation_node_ids[1] || *cell != session.node_id) {
                self.cell_nav.last_edited = Some((session.reservation_node_ids[1].clone(), session.node_id.clone()));
            }
        }
        let waiting = self.inline_text_edit.as_ref()
            .filter(|session| session.reservation_node_ids.len() > 1 && session.conflict.is_none())
            .filter(|session| self.inline_table_cell_readiness(session) == InlineCellReadiness::WaitingForRender)
            .map(|session| (session.id, session.page_index));
        let Some((session_id, page_index)) = waiting else {
            self.cell_nav.wait = None;
            return;
        };
        let revision = self.active.as_ref().map_or(0, |active| active.revision);
        if self.cell_nav.wait.as_ref().is_none_or(|wait| wait.session_id != session_id || wait.revision != revision) {
            self.cell_nav.wait = Some(CellWait { session_id, revision, since: now, asked_again: false });
        }
        // The page is asked for even when it is off screen (`draw_page` asks only for the pages it draws); `request_page`
        // sends one request per resolution and revision.
        let dpi = self.page_textures.get(&page_index).map_or(self.board_dpi, |page| page.dpi);
        self.request_page(page_index, dpi);
        let Some(wait) = self.cell_nav.wait.as_mut() else { return };
        let waited = now - wait.since;
        if waited >= RENDER_GIVE_UP {
            self.cell_nav.wait = None;
            self.keep_cell_input_as_draft("표 셀 배치를 5초 동안 받지 못했습니다. 입력은 초안으로 보관했습니다");
        } else if waited >= RENDER_ASK_AGAIN && !wait.asked_again {
            wait.asked_again = true;
            self.requested_pages.remove(&page_index);
            self.request_page(page_index, dpi);
        }
    }

    /// A render of the edited cell's page failed: the queued input is kept as the draft.
    pub(super) fn inline_cell_render_failed(&mut self, page_index: u32) {
        if self.inline_text_edit.as_ref().is_some_and(|session| {
            session.page_index == page_index && session.reservation_node_ids.len() > 1 && session.conflict.is_none()
        }) {
            self.cell_nav.wait = None;
            self.keep_cell_input_as_draft("표 셀을 다시 그리지 못했습니다. 입력은 초안으로 보관했습니다");
        }
    }

    fn keep_cell_input_as_draft(&mut self, message: &str) {
        let Some(session) = self.inline_text_edit.as_mut() else { return };
        session.layout_pending = false;
        session.conflict = Some(message.into());
        // The conflict notice retries the queued input, copies the draft or discards it.
        session.retry_allowed = !session.queue.is_empty();
        session.retry_expected = false;
        self.set_error(message.into());
    }

    /// Whether the edited cell has waited long enough for its render to show that it waits.
    pub(super) fn cell_wait_shown(&self, now: f64) -> bool {
        self.cell_nav.wait.as_ref().is_some_and(|wait| now - wait.since >= WAIT_HINT_AFTER)
    }

    /// The small "waiting" mark at the bottom right of the edited cell (`rect`, on screen).
    pub(super) fn paint_cell_wait_hint(&self, ui: &egui::Ui, rect: egui::Rect) {
        let palette = crate::theme::palette(ui.ctx());
        ui.painter().text(
            rect.right_bottom() + Vec2::new(-2.0, -1.0),
            egui::Align2::RIGHT_BOTTOM,
            "반영 중…",
            egui::FontId::proportional(10.0),
            palette.ink_2,
        );
    }

    /// Does what follows a cell's edit once the edit has ended and the board shows the current revision. Dropped when
    /// another document (or none) is shown, after `FOLLOW_EXPIRY`, and for a click once the view has moved.
    fn resume_cell_edit_follow(&mut self, ctx: &Context, now: f64) {
        let Some(follow) = self.cell_nav.follow.as_ref() else { return };
        let (artifact_id, page_index) = match &follow.kind {
            CellEditFollow::Cell { artifact_id, page_index, .. } => (artifact_id, Some(*page_index)),
            CellEditFollow::Click { artifact_id, at, .. } => (artifact_id, at.map(|(page, _)| page)),
            CellEditFollow::AwaitHit { artifact_id } | CellEditFollow::SelectTable { artifact_id, .. } => (artifact_id, None),
        };
        let stale = now - follow.since > FOLLOW_EXPIRY
            || self.active.as_ref().is_none_or(|active| active.id != *artifact_id)
            || (matches!(follow.kind, CellEditFollow::Click { at: Some(_), .. }) && follow.view != self.view_now());
        if stale {
            self.drop_cell_follow();
            return;
        }
        if self.inline_text_edit.is_some() || matches!(follow.kind, CellEditFollow::AwaitHit { .. }) {
            return;
        }
        let (Some(active), Some(artifact), Some(context)) = (self.active.as_ref(), self.artifact.as_ref(), self.context.as_ref()) else { return };
        let synced = artifact.id == active.id && context.artifact_id == active.id
            && artifact.revision == active.revision && context.revision == active.revision;
        let scene_current = page_index.is_none_or(|page_index| self.page_textures.get(&page_index).is_some_and(|page| {
            page.artifact_id == active.id && page.revision == active.revision && !page.snapshot_id.is_empty()
        }));
        if !synced || !scene_current {
            return;
        }
        let Some(follow) = self.cell_nav.follow.take() else { return };
        match follow.kind {
            CellEditFollow::Cell { page_index, cell_id, position, .. } => {
                if self.begin_inline_text_edit(ctx, &cell_id, page_index, position) {
                    self.release_held_keys();
                } else {
                    // The keys typed for that cell go nowhere else.
                    self.drop_cell_follow();
                }
            }
            CellEditFollow::Click { artifact_id, at: Some((page_index, position)), pixels_per_point, additive } => {
                self.hit_test_page(page_index, position, pixels_per_point, additive, false);
                // The reply may open a cell: the keys wait for it.
                self.cell_nav.follow = Some(Follow { kind: CellEditFollow::AwaitHit { artifact_id }, since: now, view: follow.view });
            }
            CellEditFollow::Click { at: None, .. } => {
                self.select_clear();
                self.release_held_keys();
            }
            CellEditFollow::SelectTable { table_id, .. } => {
                self.select_table(table_id, false);
                self.release_held_keys();
            }
            CellEditFollow::AwaitHit { .. } => {}
        }
    }

    fn release_held_keys(&mut self) {
        let held = std::mem::take(&mut self.cell_nav.held);
        self.cell_nav.replay.extend(held);
        if std::mem::take(&mut self.cell_nav.overflowed) {
            self.set_error("입력이 너무 많아 일부 키를 받지 못했습니다".into());
        }
    }

    /// A follow-up that can no longer be done: the keys held for it are dropped, never given to the board's shortcuts.
    fn drop_cell_follow(&mut self) {
        self.cell_nav.follow = None;
        let held = std::mem::take(&mut self.cell_nav.held);
        self.cell_nav.overflowed = false;
        if held.iter().any(is_text) {
            self.set_error("셀을 열지 못해 그사이 입력한 글자는 넣지 않았습니다".into());
        }
    }

    fn select_table(&mut self, table_id: String, additive: bool) {
        let ids = if additive { super::multi::toggle_id(&self.selected_nodes, &table_id) } else { vec![table_id] };
        self.set_selected_ids(ids.clone());
        self.sync_style_fields();
        self.set_selection(Selection { node_ids: ids, text_range: None });
    }

    /// Whether a drag that starts on the region of `node_id` selects cell text: an editable cell of a table outside a
    /// component instance, where text can be edited now.
    pub(super) fn drags_cell_text(&self, node_id: &str, page_index: u32) -> bool {
        self.run_context.is_none() && self.feature_edit_supported(Feature::Text)
            && self.artifact.as_ref().is_some_and(|artifact| {
                artifact.kind == ArtifactKind::Canvas
                    && inline_text_target_node_ids(artifact, ArtifactKind::Canvas, page_index, node_id).is_some_and(|ids| ids.len() == 2)
            })
    }

    /// A drag that starts inside a cell: the cell opens at the press (its hit reply) and the drag selects from there.
    pub(super) fn start_cell_text_drag(&mut self, page_index: u32, position: Pos2, pixels_per_point: f32) {
        self.cell_nav.text_drag = true;
        self.hit_test_page(page_index, position, pixels_per_point, false, true);
    }

    /// After a cell opened for a drag: the editor's drag starts at the caret, if the button is still down.
    pub(super) fn seed_cell_text_drag(&mut self, ctx: &Context) {
        if !std::mem::take(&mut self.cell_nav.text_drag) || self.inline_text_edit.is_none() {
            return;
        }
        if !ctx.input(|input| input.pointer.primary_down()) {
            return;
        }
        if let Some(caret) = egui::TextEdit::load_state(ctx, self.text_editor_id).and_then(|state| state.cursor.char_range()) {
            self.text_adapter.begin_pointer_drag(caret.primary.index);
        }
    }

    /// The border bands of the tables of a page (on screen at `page_rect`), over the node regions and under the selection
    /// handles (interacted after the regions, before the handles): a click selects the table (Shift adds it), a drag moves
    /// it. While the board pans they take no press.
    pub(super) fn draw_table_border_bands(&mut self, ctx: &Context, ui: &mut egui::Ui, page_index: u32, page: &designllm_contracts::RenderPage, page_rect: egui::Rect) {
        let tables: Vec<(String, designllm_contracts::Rect)> = {
            let Some(artifact) = self.artifact.as_ref().filter(|artifact| artifact.kind == ArtifactKind::Canvas) else { return };
            let ArtifactBody::Canvas(document) = &artifact.body else { return };
            let Some(nodes) = document.pages.get(page_index as usize).map(|page| &page.nodes) else { return };
            let mut found = Vec::new();
            band_tables(nodes, &mut found);
            let frames = self.page_textures.get(&page_index).map(|texture| &texture.frames);
            found.into_iter()
                .map(|node| (node.id.clone(), frames.and_then(|frames| frames.get(&node.id)).copied().unwrap_or(node.bounds)))
                .collect()
        };
        let sense = if self.board_pan_mode(ctx) { egui::Sense::hover() } else { egui::Sense::click_and_drag() };
        let pixels_per_point = ctx.pixels_per_point();
        for (table_id, frame) in tables {
            let screen = super::page_to_screen(frame, page.size, page_rect);
            for (side, strip) in band_strips(screen).into_iter().enumerate() {
                let response = ui.interact(strip, egui::Id::new(("table-border-band", page_index, table_id.as_str(), side)), sense);
                super::board::note_board_press(ctx, &response);
                if response.hovered() {
                    ctx.set_cursor_icon(egui::CursorIcon::Move);
                }
                if response.drag_started_by(egui::PointerButton::Primary) {
                    let press = ui.input(|input| input.pointer.press_origin()).or(response.interact_pointer_pos()).unwrap_or(strip.center());
                    self.focus_page(page_index);
                    self.cell_nav.band_drag = Some(table_id.clone());
                    self.start_drag_at_scale(table_id.clone(), page_index, frame, page.size, page_rect, press, pixels_per_point);
                }
                let dragging = self.cell_nav.band_drag.as_deref() == Some(table_id.as_str())
                    && self.drag.as_ref().is_some_and(|drag| drag.node_id == table_id);
                if response.dragged_by(egui::PointerButton::Primary) && dragging {
                    if let Some(drag) = self.drag.as_mut() {
                        let current = response.interact_pointer_pos().unwrap_or(drag.start);
                        drag.delta = current - drag.start;
                    }
                }
                if response.drag_stopped_by(egui::PointerButton::Primary) && dragging {
                    let current = response.interact_pointer_pos().unwrap_or(strip.center());
                    if let Some(drag) = self.drag.as_mut() {
                        drag.delta = current - drag.start;
                        drag.released = true;
                    }
                    self.cell_nav.band_drag = None;
                    self.commit_drag_if_reserved();
                } else if response.clicked() {
                    self.region_clicked = true;
                    self.focus_page(page_index);
                    self.select_table(table_id.clone(), super::multi::additive_modifier(ctx));
                }
            }
        }
    }
}
