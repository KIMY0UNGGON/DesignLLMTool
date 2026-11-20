use std::ops::Range;
use std::sync::Arc;

use eframe::egui::{
    text::{CCursor, CCursorRange},
    widgets::text_edit::TextEditState,
    Context, Event, Id, ImeEvent, Key, Modifiers, PointerButton, Pos2, RawInput, Rect,
};
use eframe::egui::widgets::text_edit::TextEditOutput;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone)]
pub struct EditorLayout {
    pub rect: Rect,
    pub galley_pos: Pos2,
    galley: Arc<eframe::egui::Galley>,
}

#[derive(Clone, Debug)]
pub struct TextInputEdit {
    pub request_id: String,
    pub range: Range<u32>,
    pub paragraph_index: u32,
    pub paragraph_range: Option<Range<u32>>,
    pub caret_after: u32,
    pub paragraph_after: u32,
    pub caret_after_in_paragraph: u32,
    pub text: String,
    pub ime_commit: bool,
    pub ime_generation: u64,
    pub after_text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyResult {
    Unhandled,
    Handled,
}

#[derive(Default)]
pub struct GraphemeAdapter {
    ime_active: bool,
    ime_generation: u64,
    ime_started: bool,
    ime_finished: bool,
    ime_cancelled: bool,
    ime_cancel_requested: bool,
    ime_base_text: Option<String>,
    ime_base_selection: Option<(usize, usize)>,
    ime_text_buffer: Vec<String>,
    input_edits: Vec<TextInputEdit>,
    drag_anchor: Option<usize>,
    layout: Option<EditorLayout>,
    selection: Option<(usize, usize)>,
    /// The last press the editor counted: its time, place and count in its run of clicks.
    last_press: Option<(f64, Pos2, u8)>,
    /// The drag in progress began with a press egui saw, so its release goes on to egui.
    release_to_egui: bool,
    /// The last primary release the window saw before this frame (time, place): the first click of a run may land before
    /// this editor is on screen.
    previous_click: Option<(f64, Pos2)>,
}

impl GraphemeAdapter {
    pub fn record_layout(&mut self, output: &TextEditOutput) {
        self.layout = Some(EditorLayout {
            rect: output.response.rect,
            galley_pos: output.galley_pos,
            galley: Arc::clone(&output.galley),
        });
        self.selection = output
            .cursor_range
            .map(|range| (range.primary.index, range.secondary.index));
    }

    pub fn set_caret_at_grapheme(&mut self, ctx: &Context, editor_id: Id, text: &str, index: Option<u32>) {
        let grapheme_index = index.map_or_else(|| text.graphemes(true).count(), |index| index as usize);
        let char_index = text.graphemes(true).take(grapheme_index).map(|grapheme| grapheme.chars().count()).sum::<usize>();
        let mut state = TextEditState::load(ctx, editor_id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(char_index))));
        state.store(ctx, editor_id);
        self.selection = Some((char_index, char_index));
    }

    pub fn is_ime_active(&self) -> bool {
        self.ime_active
    }

    pub fn ime_generation(&self) -> u64 {
        self.ime_generation
    }

    pub fn take_ime_started(&mut self) -> bool {
        std::mem::take(&mut self.ime_started)
    }

    pub fn take_ime_cancelled(&mut self) -> bool {
        std::mem::take(&mut self.ime_cancelled)
    }

    pub fn take_ime_finished(&mut self) -> bool {
        std::mem::take(&mut self.ime_finished)
    }

    pub fn take_input_edits(&mut self) -> Vec<TextInputEdit> {
        std::mem::take(&mut self.input_edits)
    }

    /// Filter only the active document editor's navigation, deletion and pointer events.
    /// IME composition and unrelated widgets remain under egui/winit control.
    pub fn raw_input_hook(
        &mut self,
        ctx: &Context,
        input: &mut RawInput,
        editor_id: Id,
        text: &mut String,
    ) -> Option<Range<u32>> {
        self.input_edits.clear();
        self.ime_started = false;
        self.ime_finished = false;
        self.ime_cancelled = false;
        let has_focus = ctx.memory(|memory| memory.has_focus(editor_id));
        let mut state = TextEditState::load(ctx, editor_id).unwrap_or_default();
        let char_count = text.chars().count();
        let (mut primary, mut secondary) = state
            .cursor
            .char_range()
            .map(|range| (range.primary.index, range.secondary.index))
            .unwrap_or((char_count, char_count));
        let mut restored_ime_cursor = false;

        // egui applies each IME preedit to the text buffer for display. Restore the committed
        // editor buffer and selection before processing the next IME event, so an IME commit is
        // always mapped against the text that Core still has.
        if self.ime_active {
            if let Some(base_text) = self.ime_base_text.as_ref() {
                if text != base_text {
                    text.clone_from(base_text);
                }
            }
            if let Some((base_primary, base_secondary)) = self.ime_base_selection {
                primary = base_primary;
                secondary = base_secondary;
                state.cursor.set_char_range(Some(CCursorRange {
                    primary: CCursor::new(primary),
                    secondary: CCursor::new(secondary),
                    h_pos: None,
                }));
                restored_ime_cursor = true;
            }
        }

        // A press within egui's double-click delay and click distance of the last click is the next click of a run (the
        // first click of a run may have landed before this editor was on screen).
        let (delay, distance) = ctx.options(|options| (options.input_options.max_double_click_delay, options.input_options.max_click_dist));
        // The last click the window saw (`set_previous_click`), where and when it was released.
        let now = input.time.unwrap_or_default();
        let previous_click = self.previous_click.filter(|(time, _)| now - time < delay).map(|(_, at)| at);
        let press_context = PressContext { now, delay, distance, previous_click };
        let mut forwarded = Vec::with_capacity(input.events.len());
        let mut changed_cursor = false;
        let mut focus_requested = false;
        let mut ime_commits: Vec<String> = input.events.iter().filter_map(|event| match event {
            Event::Ime(ImeEvent::Commit(text)) => Some(text.clone()),
            _ => None,
        }).collect();
        // A preedit before the frame's last commit is that commit's and is not shown: the commit replaces it. Windows sends
        // a syllable's commit and the next syllable's first preedit in one message, so a preedit after the last commit is
        // the next composition's and is shown.
        let last_commit = input.events.iter().rposition(|event| matches!(event, Event::Ime(ImeEvent::Commit(_))));

        for (index, event) in std::mem::take(&mut input.events).into_iter().enumerate() {
            match &event {
                Event::Ime(ImeEvent::Enabled) => {
                    if !self.ime_active {
                        self.begin_composition(text, primary, secondary);
                    }
                    forwarded.push(event);
                }
                Event::Ime(ImeEvent::Disabled) => {
                    if self.ime_active {
                        self.ime_finished = true;
                        if self.ime_cancel_requested {
                            self.ime_cancelled = true;
                            self.ime_text_buffer.clear();
                        } else {
                            let inserted = std::mem::take(&mut self.ime_text_buffer).join("");
                            if !inserted.is_empty() {
                                self.apply_input_text(text, &inserted, &mut primary, &mut secondary, true);
                                changed_cursor = true;
                            }
                        }
                    }
                    self.ime_active = false;
                    self.ime_cancel_requested = false;
                    self.ime_base_text = None;
                    self.ime_base_selection = None;
                    forwarded.push(event);
                }
                Event::Ime(ImeEvent::Commit(inserted)) if has_focus || self.ime_active => {
                    let committed = if inserted.is_empty() { self.ime_text_buffer.join("") } else { inserted.clone() };
                    self.ime_text_buffer.clear();
                    self.ime_cancel_requested = false;
                    self.ime_finished = true;
                    self.ime_base_text = None;
                    self.ime_base_selection = None;
                    if !committed.is_empty() {
                        self.apply_input_text(text, &committed, &mut primary, &mut secondary, true);
                        changed_cursor = true;
                        forwarded.push(Event::Ime(ImeEvent::Commit(String::new())));
                    } else {
                        forwarded.push(event);
                    }
                    self.ime_active = false;
                }
                Event::Ime(ImeEvent::Commit(_)) => {
                    self.ime_text_buffer.clear();
                    self.ime_cancel_requested = false;
                    self.ime_base_text = None;
                    self.ime_base_selection = None;
                    forwarded.push(event);
                    self.ime_active = false;
                }
                // A preedit whose `Enabled` this editor did not see (an earlier frame's input taken before the editor saw
                // it) opens its composition here, so egui's preedit is undone before the commit is mapped.
                Event::Ime(ImeEvent::Preedit(preedit)) => {
                    if has_focus && !self.ime_active && !preedit.is_empty() {
                        self.begin_composition(text, primary, secondary);
                    }
                    if last_commit.is_none_or(|last| index > last) {
                        forwarded.push(event);
                    }
                }
                Event::Text(inserted) if has_focus && self.ime_active => {
                    if let Some(index) = ime_commits.iter().position(|commit| commit == inserted) {
                        ime_commits.remove(index);
                    } else {
                        self.ime_text_buffer.push(inserted.clone());
                    }
                }
                Event::Text(inserted) if has_focus && !inserted.is_empty() => {
                    if let Some(index) = ime_commits.iter().position(|commit| commit == inserted) {
                        ime_commits.remove(index);
                    } else {
                        self.apply_input_text(text, inserted, &mut primary, &mut secondary, false);
                        changed_cursor = true;
                    }
                }
                Event::Paste(inserted) if has_focus && !inserted.is_empty() => {
                    self.apply_input_text(text, inserted, &mut primary, &mut secondary, false);
                    changed_cursor = true;
                }
                Event::Cut if has_focus && primary != secondary => {
                    let start = floor_boundary(&grapheme_scalar_boundaries(text), primary.min(secondary));
                    let end = ceil_boundary(&grapheme_scalar_boundaries(text), primary.max(secondary));
                    let byte_start = scalar_to_byte(text, start);
                    let byte_end = scalar_to_byte(text, end);
                    ctx.copy_text(text[byte_start..byte_end].to_owned());
                    self.apply_input_text(text, "", &mut primary, &mut secondary, false);
                    changed_cursor = true;
                }
                Event::Key { key: Key::Enter, pressed: true, modifiers, .. }
                    if has_focus && !self.ime_active && !modifiers.ctrl && !modifiers.alt && !modifiers.mac_cmd =>
                {
                    self.apply_input_text(text, "\n", &mut primary, &mut secondary, false);
                    changed_cursor = true;
                }
                Event::Key { key: Key::Escape, pressed: true, .. } if has_focus && self.ime_active => {
                    self.ime_cancel_requested = true;
                    forwarded.push(event);
                }
                Event::Key { key, pressed: true, modifiers, .. }
                    if has_focus && (modifiers.ctrl || modifiers.mac_cmd)
                        && matches!(key, Key::Enter | Key::Z | Key::Y | Key::X) => {}
                Event::Key {
                    key,
                    modifiers,
                    pressed: true,
                    ..
                } if has_focus
                    && !self.ime_active
                    && self.handle_key(
                        text,
                        *key,
                        *modifiers,
                        &mut primary,
                        &mut secondary,
                    ) != KeyResult::Unhandled =>
                {
                    changed_cursor = true;
                }
                Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    ..
                } if !self.ime_active
                    && self.handle_pointer_press(*pos, &press_context, text, &mut primary, &mut secondary) =>
                {
                    changed_cursor = true;
                    focus_requested = true;
                }
                Event::PointerMoved(pos)
                    if !self.ime_active
                        && self.handle_pointer_move(*pos, text, &mut primary, &mut secondary) =>
                {
                    changed_cursor = true;
                }
                Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    ..
                } if !self.ime_active
                    && self.handle_pointer_release(*pos, text, &mut primary, &mut secondary) =>
                {
                    changed_cursor = true;
                    // egui saw the press of a drag started outside the editor (`begin_pointer_drag`): it sees the release too, or
                    // it would hold the button down.
                    if std::mem::take(&mut self.release_to_egui) {
                        forwarded.push(event.clone());
                    }
                }
                _ => forwarded.push(event),
            }
        }
        input.events = forwarded;

        if focus_requested {
            ctx.memory_mut(|memory| memory.request_focus(editor_id));
        }
        if changed_cursor || restored_ime_cursor {
            state.cursor.set_char_range(Some(CCursorRange {
                primary: CCursor::new(primary),
                secondary: CCursor::new(secondary),
                h_pos: None,
            }));
            state.store(ctx, editor_id);
        }

        self.selection = Some((primary, secondary));
        scalar_range_to_grapheme_range(text, primary.min(secondary)..primary.max(secondary))
    }

    /// Opens a composition: its generation is counted from here, and the text and caret it starts from are kept, so the
    /// preedit can be shown and then replaced by the commit.
    fn begin_composition(&mut self, text: &str, primary: usize, secondary: usize) {
        self.ime_generation = self.ime_generation.saturating_add(1);
        self.ime_started = true;
        self.ime_active = true;
        self.ime_base_text = Some(text.to_owned());
        self.ime_base_selection = Some((primary, secondary));
    }

    fn apply_input_text(
        &mut self,
        text: &mut String,
        inserted: &str,
        primary: &mut usize,
        secondary: &mut usize,
        ime_commit: bool,
    ) -> bool {
        let boundaries = grapheme_scalar_boundaries(text);
        *primary = nearest_boundary(&boundaries, *primary);
        *secondary = nearest_boundary(&boundaries, *secondary);
        let scalar_start = floor_boundary(&boundaries, (*primary).min(*secondary));
        let scalar_end = ceil_boundary(&boundaries, (*primary).max(*secondary));
        let Some(range) = scalar_range_to_grapheme_range(text, scalar_start..scalar_end) else {
            return false;
        };
        let normalized = inserted.replace("\r\n", "\n").replace('\r', "\n");
        if range.is_empty() && normalized.is_empty() {
            return false;
        }
        let (paragraph_index, paragraph_start) = paragraph_position(text, range.start as usize);
        let (end_paragraph, paragraph_end) = paragraph_position(text, range.end as usize);
        let Ok(paragraph_index) = u32::try_from(paragraph_index) else { return false };
        let paragraph_range = if paragraph_index as usize == end_paragraph {
            let (Ok(start), Ok(end)) = (u32::try_from(paragraph_start), u32::try_from(paragraph_end)) else { return false };
            Some(start..end)
        } else {
            None
        };
        let new_grapheme_count = text.graphemes(true).count()
            .saturating_sub((range.end - range.start) as usize)
            .saturating_add(normalized.graphemes(true).count());
        if new_grapheme_count > u32::MAX as usize { return false; }
        let start_byte = scalar_to_byte(text, scalar_start);
        let end_byte = scalar_to_byte(text, scalar_end);
        if &text[start_byte..end_byte] == normalized.as_str() {
            let scalar_caret = scalar_start + normalized.chars().count();
            *primary = scalar_caret;
            *secondary = scalar_caret;
            return false;
        }
        text.replace_range(start_byte..end_byte, &normalized);
        let caret_after = range.start as usize + normalized.graphemes(true).count();
        let (paragraph_after, caret_after_in_paragraph) = paragraph_position(text, caret_after);
        let scalar_caret = scalar_start + normalized.chars().count();
        let (Ok(caret_after), Ok(paragraph_after), Ok(caret_after_in_paragraph)) = (
            u32::try_from(caret_after), u32::try_from(paragraph_after), u32::try_from(caret_after_in_paragraph),
        ) else { return false };
        *primary = scalar_caret;
        *secondary = scalar_caret;
        let after_text = paragraph_range.is_none().then(|| text.clone());
        self.input_edits.push(TextInputEdit {
            request_id: String::new(),
            range,
            paragraph_index,
            paragraph_range,
            caret_after,
            paragraph_after,
            caret_after_in_paragraph,
            text: normalized,
            ime_commit,
            ime_generation: self.ime_generation,
            after_text,
        });
        true
    }

    fn handle_key(
        &mut self,
        text: &mut String,
        key: Key,
        modifiers: Modifiers,
        primary: &mut usize,
        secondary: &mut usize,
    ) -> KeyResult {
        if modifiers.ctrl || modifiers.alt || modifiers.mac_cmd {
            return KeyResult::Unhandled;
        }
        let boundaries = grapheme_scalar_boundaries(text);
        *primary = nearest_boundary(&boundaries, *primary);
        *secondary = nearest_boundary(&boundaries, *secondary);

        match key {
            Key::ArrowLeft | Key::ArrowRight => {
                let left = key == Key::ArrowLeft;
                if !modifiers.shift && *primary != *secondary {
                    let collapsed = if left {
                        floor_boundary(&boundaries, (*primary).min(*secondary))
                    } else {
                        ceil_boundary(&boundaries, (*primary).max(*secondary))
                    };
                    *primary = collapsed;
                    *secondary = collapsed;
                } else {
                    *primary = if left {
                        previous_boundary(&boundaries, *primary)
                    } else {
                        next_boundary(&boundaries, *primary)
                    };
                    if !modifiers.shift {
                        *secondary = *primary;
                    }
                }
                KeyResult::Handled
            }
            Key::Backspace | Key::Delete => {
                let (start, end) = if *primary != *secondary {
                    (
                        floor_boundary(&boundaries, (*primary).min(*secondary)),
                        ceil_boundary(&boundaries, (*primary).max(*secondary)),
                    )
                } else if key == Key::Backspace {
                    (previous_boundary(&boundaries, *primary), *primary)
                } else {
                    (*primary, next_boundary(&boundaries, *primary))
                };
                *primary = start;
                *secondary = end;
                if !self.apply_input_text(text, "", primary, secondary, false) {
                    *primary = start;
                    *secondary = start;
                }
                KeyResult::Handled
            }
            _ => KeyResult::Unhandled,
        }
    }

    fn pointer_cursor(&self, position: Pos2, text: &str) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let scalar = layout
            .galley
            .cursor_from_pos(position - layout.galley_pos)
            .index
            .min(text.chars().count());
        Some(nearest_boundary(&grapheme_scalar_boundaries(text), scalar))
    }

    fn handle_pointer_press(
        &mut self,
        position: Pos2,
        press: &PressContext,
        text: &str,
        primary: &mut usize,
        secondary: &mut usize,
    ) -> bool {
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        if self.drag_anchor.is_some() || !layout.rect.contains(position) {
            return false;
        }
        let Some(index) = self.pointer_cursor(position, text) else {
            return false;
        };
        // The count of this press in its run of clicks: the second selects the word under the pointer, the third its line
        // (no drag follows either).
        let near = |at: Pos2| at.distance(position) <= press.distance;
        let count = match self.last_press {
            Some((time, at, count)) if press.now - time < press.delay && near(at) => count.saturating_add(1),
            _ if press.previous_click.is_some_and(near) => 2,
            _ => 1,
        };
        self.last_press = Some((press.now, position, count));
        let range = match count {
            2 => word_around(text, index),
            3 => Some(line_around(text, index)),
            _ => None,
        };
        if let Some((start, end)) = range {
            *secondary = start;
            *primary = end;
            return true;
        }
        self.drag_anchor = Some(index);
        self.release_to_egui = false;
        *primary = index;
        *secondary = index;
        true
    }

    fn handle_pointer_move(
        &mut self,
        position: Pos2,
        text: &str,
        primary: &mut usize,
        secondary: &mut usize,
    ) -> bool {
        let Some(anchor) = self.drag_anchor else {
            return false;
        };
        let Some(index) = self.pointer_cursor(position, text) else {
            return false;
        };
        *primary = index;
        *secondary = anchor;
        true
    }

    fn handle_pointer_release(
        &mut self,
        position: Pos2,
        text: &str,
        primary: &mut usize,
        secondary: &mut usize,
    ) -> bool {
        // The release ends the drag even where the editor cannot place it (no layout yet).
        let Some(anchor) = self.drag_anchor.take() else {
            return false;
        };
        let Some(index) = self.pointer_cursor(position, text) else {
            return false;
        };
        *primary = index;
        *secondary = anchor;
        true
    }

    /// Starts a text drag at scalar `anchor` for a press that reached the board before this editor was on screen (a drag
    /// that starts on a table cell opens the cell, 68 item 11). The pointer moves and the release extend the selection.
    pub fn begin_pointer_drag(&mut self, anchor: usize) {
        self.drag_anchor = Some(anchor);
        self.release_to_egui = true;
    }

    /// The last primary release the window saw before this frame (egui time, place), for the count of the next press.
    pub fn set_previous_click(&mut self, click: Option<(f64, Pos2)>) {
        self.previous_click = click;
    }

    /// Selects the word of `text` around grapheme `index` (the second click of a double click that reached the board
    /// before this editor was on screen).
    pub fn select_word_at_grapheme(&mut self, ctx: &Context, editor_id: Id, text: &str, index: u32) {
        let boundaries = grapheme_scalar_boundaries(text);
        let scalar = boundaries.get(index as usize).copied().unwrap_or_else(|| text.chars().count());
        let Some((start, end)) = word_around(text, scalar) else { return };
        let mut state = TextEditState::load(ctx, editor_id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange { primary: CCursor::new(end), secondary: CCursor::new(start), h_pos: None }));
        state.store(ctx, editor_id);
        self.selection = Some((end, start));
    }

    /// Whether the caret at scalar `index` is on the first and on the last row of the editor's last layout (rows as
    /// drawn, wrapped lines included). `None` before the editor was laid out.
    pub fn caret_row_edges(&self, index: usize) -> Option<(bool, bool)> {
        let layout = self.layout.as_ref()?;
        let row = layout.galley.layout_from_cursor(CCursor::new(index)).row;
        Some((row == 0, row + 1 >= layout.galley.rows.len().max(1)))
    }
}

/// What a press needs from its frame to count its place in a run of clicks.
struct PressContext {
    now: f64,
    delay: f64,
    distance: f32,
    /// Where the last click was released, when it is within the double-click delay.
    previous_click: Option<Pos2>,
}

/// The word (Unicode word bounds) at scalar index `index` of `text`, as a scalar range: the word `index` is in, else the
/// word that ends at `index` (a press on the right half of a word's last letter). `None` on white space.
fn word_around(text: &str, index: usize) -> Option<(usize, usize)> {
    let mut start = 0;
    let mut ending_here = None;
    for word in text.split_word_bounds() {
        let end = start + word.chars().count();
        let is_word = !word.trim().is_empty();
        if index < end {
            return if is_word { Some((start, end)) } else { ending_here };
        }
        ending_here = (is_word && index == end).then_some((start, end));
        start = end;
    }
    ending_here
}

/// The line (between line breaks) at scalar index `index` of `text`, as a scalar range without its line break.
fn line_around(text: &str, index: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let index = index.min(chars.len());
    let start = chars[..index].iter().rposition(|c| *c == '\n').map_or(0, |at| at + 1);
    let end = chars[index..].iter().position(|c| *c == '\n').map_or(chars.len(), |at| index + at);
    (start, end)
}

pub fn scalar_range_to_grapheme_range(text: &str, range: Range<usize>) -> Option<Range<u32>> {
    if range.start > range.end {
        return None;
    }
    let boundaries = grapheme_scalar_boundaries(text);
    let start = u32::try_from(boundaries.binary_search(&range.start).ok()?).ok()?;
    let end = u32::try_from(boundaries.binary_search(&range.end).ok()?).ok()?;
    Some(start..end)
}

fn grapheme_scalar_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries = vec![0];
    let mut scalar = 0;
    for grapheme in text.graphemes(true) {
        scalar += grapheme.chars().count();
        boundaries.push(scalar);
    }
    boundaries
}

fn floor_boundary(boundaries: &[usize], index: usize) -> usize {
    boundaries
        .iter()
        .copied()
        .take_while(|&boundary| boundary <= index)
        .last()
        .unwrap_or(0)
}

fn ceil_boundary(boundaries: &[usize], index: usize) -> usize {
    boundaries
        .iter()
        .copied()
        .find(|&boundary| boundary >= index)
        .unwrap_or_else(|| *boundaries.last().expect("zero boundary exists"))
}

fn nearest_boundary(boundaries: &[usize], index: usize) -> usize {
    let index = index.min(*boundaries.last().expect("zero boundary exists"));
    let before = floor_boundary(boundaries, index);
    let after = ceil_boundary(boundaries, index);
    if index - before <= after - index {
        before
    } else {
        after
    }
}

fn previous_boundary(boundaries: &[usize], index: usize) -> usize {
    boundaries
        .iter()
        .copied()
        .take_while(|&boundary| boundary < index)
        .last()
        .unwrap_or(0)
}

fn next_boundary(boundaries: &[usize], index: usize) -> usize {
    boundaries
        .iter()
        .copied()
        .find(|&boundary| boundary > index)
        .unwrap_or_else(|| *boundaries.last().expect("zero boundary exists"))
}

#[cfg(test)]
fn delete_scalar_range(text: &mut String, range: Range<usize>) {
    let start = text
        .char_indices()
        .nth(range.start)
        .map_or(text.len(), |(byte, _)| byte);
    let end = text
        .char_indices()
        .nth(range.end)
        .map_or(text.len(), |(byte, _)| byte);
    text.replace_range(start..end, "");
}

fn scalar_to_byte(text: &str, scalar: usize) -> usize {
    text.char_indices().nth(scalar).map_or(text.len(), |(byte, _)| byte)
}

fn paragraph_position(text: &str, global: usize) -> (usize, usize) {
    let mut paragraph = 0;
    let mut offset = 0;
    for grapheme in text.graphemes(true).take(global) {
        if grapheme == "\n" {
            paragraph += 1;
            offset = 0;
        } else {
            offset += 1;
        }
    }
    (paragraph, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::TextEdit;

    #[test]
    fn a_double_press_selects_the_word_it_is_in_or_ends_and_a_triple_press_its_line() {
        assert_eq!(word_around("abc def", 1), Some((0, 3)));
        // The right half of the word's last letter places the caret after it: still that word.
        assert_eq!(word_around("abc def", 3), Some((0, 3)));
        assert_eq!(word_around("abc def", 7), Some((4, 7)));
        assert_eq!(word_around("abc  def", 4), None, "white space selects nothing");
        assert_eq!(word_around("가나 다", 1), Some((0, 2)));
        assert_eq!(line_around("ab\ncd\nef", 4), (3, 5));
        assert_eq!(line_around("ab\ncd", 0), (0, 2));
        assert_eq!(line_around("ab\ncd", 5), (3, 5));
    }

    fn test_input(events: Vec<Event>) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                eframe::egui::Vec2::new(700.0, 400.0),
            )),
            events,
            ..Default::default()
        }
    }

    fn draw_text(
        ctx: &Context,
        id: Id,
        text: &mut String,
        input: RawInput,
    ) -> TextEditOutput {
        let mut output = None;
        let _ = ctx.run(input, |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                output = Some(TextEdit::multiline(text).id(id).show(ui));
            });
        });
        output.expect("text edit rendered")
    }

    fn set_cursor(ctx: &Context, id: Id, index: usize) {
        let mut state = TextEditState::load(ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(CCursorRange::one(CCursor::new(index))));
        state.store(ctx, id);
    }

    fn key_event(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn raw_input_hook_keeps_keyboard_caret_on_grapheme_boundaries() {
        let ctx = Context::default();
        let id = Id::new("document-text-editor");
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut text = "한🧑‍🔬e\u{301}".to_owned();
        let output = draw_text(&ctx, id, &mut text, test_input(Vec::new()));
        set_cursor(&ctx, id, text.chars().count());

        let mut adapter = GraphemeAdapter::default();
        adapter.record_layout(&output);
        let mut input = test_input(vec![key_event(Key::ArrowLeft, Modifiers::default())]);
        assert_eq!(
            adapter.raw_input_hook(&ctx, &mut input, id, &mut text),
            Some(2..2)
        );
        assert!(input.events.is_empty());

        let output = draw_text(&ctx, id, &mut text, input);
        assert_eq!(
            output.cursor_range.map(|range| range.as_sorted_char_range()),
            Some(6..6)
        );
    }

    #[test]
    fn raw_input_hook_keeps_pointer_drag_selection_on_grapheme_boundaries() {
        let ctx = Context::default();
        let id = Id::new("document-text-editor");
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut text = "한🧑‍🔬e\u{301}".to_owned();
        let output = draw_text(&ctx, id, &mut text, test_input(Vec::new()));
        set_cursor(&ctx, id, text.chars().count());

        let mut adapter = GraphemeAdapter::default();
        adapter.record_layout(&output);
        let start = output.galley_pos + output.galley.pos_from_cursor(CCursor::new(6)).center().to_vec2();
        let end = output.galley_pos + output.galley.pos_from_cursor(CCursor::new(8)).center().to_vec2();
        let mut input = test_input(vec![
            Event::PointerButton {
                pos: start,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::default(),
            },
            Event::PointerMoved(end),
            Event::PointerButton {
                pos: end,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::default(),
            },
        ]);
        assert_eq!(
            adapter.raw_input_hook(&ctx, &mut input, id, &mut text),
            Some(2..3)
        );
        assert!(input.events.is_empty());

        let output = draw_text(&ctx, id, &mut text, input);
        assert_eq!(
            output.cursor_range.map(|range| range.as_sorted_char_range()),
            Some(6..8)
        );
    }

    #[test]
    fn ime_preedit_updates_are_rebased_and_commit_once_at_the_original_caret() {
        let ctx = Context::default();
        let id = Id::new("ime-rebased-editor");
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut text = String::from("A");
        let output = draw_text(&ctx, id, &mut text, test_input(Vec::new()));
        set_cursor(&ctx, id, 1);
        let mut adapter = GraphemeAdapter::default();
        adapter.record_layout(&output);

        let mut first_preedit = test_input(vec![
            Event::Ime(ImeEvent::Enabled),
            Event::Ime(ImeEvent::Preedit("ㅎ".into())),
        ]);
        adapter.raw_input_hook(&ctx, &mut first_preedit, id, &mut text);
        draw_text(&ctx, id, &mut text, first_preedit);
        assert_eq!(text, "Aㅎ");

        let mut updated_preedit = test_input(vec![Event::Ime(ImeEvent::Preedit("하".into()))]);
        adapter.raw_input_hook(&ctx, &mut updated_preedit, id, &mut text);
        draw_text(&ctx, id, &mut text, updated_preedit);
        assert_eq!(text, "A하", "a new preedit replaces the previous transient preedit");

        let mut commit = test_input(vec![
            Event::Ime(ImeEvent::Commit("한".into())),
            Event::Text("한".into()),
        ]);
        adapter.raw_input_hook(&ctx, &mut commit, id, &mut text);
        let edits = adapter.take_input_edits();
        draw_text(&ctx, id, &mut text, commit);
        assert_eq!(text, "A한");
        assert_eq!(edits.len(), 1, "the paired Text event does not duplicate the IME commit");
        assert_eq!(edits[0].range, 1..1, "the operation uses the original committed caret");
        assert_eq!(edits[0].text, "한");
    }

    /// 68c: Windows sends a syllable's commit and the next syllable's start in one frame (egui events of winit 0.30.13 and
    /// egui-winit 0.33.3). The commit carries the generation of the composition that made it, the next composition gets
    /// the next generation, and its preedit is shown.
    #[test]
    fn a_commit_and_the_next_composition_in_one_frame_keep_their_own_generations() {
        let ctx = Context::default();
        let id = Id::new("ime-windows-order-editor");
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut text = String::new();
        let output = draw_text(&ctx, id, &mut text, test_input(Vec::new()));
        set_cursor(&ctx, id, 0);
        let mut adapter = GraphemeAdapter::default();
        adapter.record_layout(&output);
        let ime = Event::Ime;

        let mut frames = vec![
            vec![ime(ImeEvent::Enabled), ime(ImeEvent::Preedit("ㅎ".into()))],
            vec![ime(ImeEvent::Preedit("한".into()))],
            vec![ime(ImeEvent::Disabled), ime(ImeEvent::Commit("한".into())), ime(ImeEvent::Disabled), ime(ImeEvent::Enabled), ime(ImeEvent::Preedit("ㄱ".into()))],
            vec![ime(ImeEvent::Preedit("글".into()))],
            vec![ime(ImeEvent::Disabled), ime(ImeEvent::Commit("글".into())), ime(ImeEvent::Disabled), ime(ImeEvent::Disabled)],
        ].into_iter();
        let mut play = |adapter: &mut GraphemeAdapter, text: &mut String| {
            let mut input = test_input(frames.next().expect("a frame"));
            adapter.raw_input_hook(&ctx, &mut input, id, text);
            let edits = adapter.take_input_edits();
            draw_text(&ctx, id, text, input);
            edits
        };
        assert!(play(&mut adapter, &mut text).is_empty());
        assert!(play(&mut adapter, &mut text).is_empty());
        assert_eq!((adapter.ime_generation(), text.as_str()), (1, "한"));

        let edits = play(&mut adapter, &mut text);
        assert_eq!(edits.len(), 1);
        assert_eq!((edits[0].text.as_str(), edits[0].range.clone(), edits[0].ime_generation), ("한", 0..0, 1), "the commit is the first composition's");
        assert!(adapter.is_ime_active() && adapter.take_ime_started(), "the next composition is open");
        assert_eq!(adapter.ime_generation(), 2);
        assert_eq!(text, "한ㄱ", "the next syllable's first preedit is shown after the commit");

        assert!(play(&mut adapter, &mut text).is_empty());
        assert_eq!(text, "한글");
        let edits = play(&mut adapter, &mut text);
        assert_eq!(edits.len(), 1);
        assert_eq!((edits[0].text.as_str(), edits[0].range.clone(), edits[0].ime_generation), ("글", 1..1, 2), "the second commit is the second composition's");
        assert!(!adapter.is_ime_active());
        assert_eq!(text, "한글", "each syllable once");
    }

    #[test]
    fn ime_preedit_replaces_the_original_selection_and_escape_discards_only_preedit() {
        let ctx = Context::default();
        let id = Id::new("ime-replacement-editor");
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut text = String::from("AB");
        let output = draw_text(&ctx, id, &mut text, test_input(Vec::new()));
        let mut state = TextEditState::load(&ctx, id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange {
            primary: CCursor::new(1), secondary: CCursor::new(2), h_pos: None,
        }));
        state.store(&ctx, id);
        let mut adapter = GraphemeAdapter::default();
        adapter.record_layout(&output);

        let mut preedit = test_input(vec![
            Event::Ime(ImeEvent::Enabled),
            Event::Ime(ImeEvent::Preedit("ㅎ".into())),
        ]);
        adapter.raw_input_hook(&ctx, &mut preedit, id, &mut text);
        draw_text(&ctx, id, &mut text, preedit);
        assert_eq!(text, "Aㅎ");

        let mut commit = test_input(vec![Event::Ime(ImeEvent::Commit("하".into()))]);
        adapter.raw_input_hook(&ctx, &mut commit, id, &mut text);
        let edits = adapter.take_input_edits();
        draw_text(&ctx, id, &mut text, commit);
        assert_eq!(text, "A하");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range, 1..2, "the saved selection, not the preedit selection, is replaced");

        let mut second_text = String::from("AB");
        let output = draw_text(&ctx, id, &mut second_text, test_input(Vec::new()));
        set_cursor(&ctx, id, 2);
        let mut cancelled = GraphemeAdapter::default();
        cancelled.record_layout(&output);
        let mut preedit = test_input(vec![
            Event::Ime(ImeEvent::Enabled),
            Event::Ime(ImeEvent::Preedit("ㅎ".into())),
        ]);
        cancelled.raw_input_hook(&ctx, &mut preedit, id, &mut second_text);
        draw_text(&ctx, id, &mut second_text, preedit);
        assert_eq!(second_text, "ABㅎ");
        let mut escape = test_input(vec![
            key_event(Key::Escape, Modifiers::default()),
            Event::Ime(ImeEvent::Disabled),
        ]);
        cancelled.raw_input_hook(&ctx, &mut escape, id, &mut second_text);
        let edits = cancelled.take_input_edits();
        draw_text(&ctx, id, &mut second_text, escape);
        assert_eq!(second_text, "AB");
        assert!(edits.is_empty(), "cancelling preedit does not emit a text change");
    }
    #[test]
    fn scalar_ranges_map_only_at_unicode_16_grapheme_boundaries() {
        let text = "한🧑‍🔬e\u{301}";
        assert_eq!(scalar_range_to_grapheme_range(text, 0..3), Some(0..1));
        assert_eq!(scalar_range_to_grapheme_range(text, 3..6), Some(1..2));
        assert_eq!(scalar_range_to_grapheme_range(text, 6..8), Some(2..3));
        assert_eq!(scalar_range_to_grapheme_range(text, 7..8), None);
        assert_eq!(unicode_segmentation::UNICODE_VERSION, (16, 0, 0));
    }

    #[test]
    fn deleting_from_scalar_offsets_removes_entire_validated_cluster() {
        let mut text = "한🧑‍🔬e\u{301}".to_owned();
        delete_scalar_range(&mut text, 6..8);
        assert_eq!(text, "한🧑‍🔬");
    }
}
