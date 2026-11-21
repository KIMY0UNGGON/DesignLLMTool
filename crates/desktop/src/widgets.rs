//! The controls of the new shell (GUI_DESIGN §15 step 3). Every add or design button shows an icon
//! and a label (rule L1); icon-only buttons are for the small floating groups and the top bar
//! (rules L2 and L4) and always carry a tooltip with their name and shortcut.

use std::ops::RangeInclusive;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Stroke,
    StrokeKind, TextEdit, Ui, Vec2,
};

use crate::{
    icons,
    theme::{self, Palette},
};

/// Agent state shown by an avatar ring (§5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Waiting,
    Running,
    Review,
    Done,
    Failed,
}

/// One part of a three-part progress rail (§10).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stage {
    Done,
    Current(f32),
    Pending,
}

fn text_width(ui: &Ui, text: &str, font: FontId, color: Color32) -> f32 {
    ui.painter().layout_no_wrap(text.to_owned(), font, color).size().x
}

/// A 32 px icon button with a 24 px icon. `active` shows the accent (an open panel, a chosen tool).
pub fn icon_button(ui: &mut Ui, palette: &Palette, icon: &str, tooltip: &str, active: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(theme::ICON_BUTTON), Sense::click());
    let corner = CornerRadius::same(theme::RADIUS_CONTROL);
    if active {
        ui.painter().rect_filled(rect, corner, palette.accent_soft);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, corner, palette.surface_2);
    }
    // tokens `component.icon-button`: ink-2, on = accent.
    let color = if active { palette.accent } else { palette.ink_2 };
    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(theme::ICON_IN_BUTTON));
    icons::paint(ui, icon_rect, icon, color);
    crate::ui_record::note(ui, tooltip, &response);
    response.on_hover_text(tooltip)
}

/// A 16 px icon and a label (tokens `component.label-button`: no fill and no border, `surface-2` on
/// hover or while its menu is open). `primary` is the one main action of a group (accent fill).
pub fn labeled_button(ui: &mut Ui, palette: &Palette, icon: Option<&str>, label: &str, primary: bool) -> Response {
    labeled_button_with(ui, palette, icon, label, primary, false, false)
}

/// `labeled_button` with the open state (its menu is showing) and an unsaved-changes dot after the label.
pub fn labeled_button_with(ui: &mut Ui, palette: &Palette, icon: Option<&str>, label: &str, primary: bool, open: bool, dot: bool) -> Response {
    let font = theme::font(if primary { theme::Type::Strong } else { theme::Type::Body });
    // A disabled button (`add_enabled_ui`) keeps its shape and takes `ink-3` with no hover; a disabled main button drops to `surface-2`.
    let enabled = ui.is_enabled();
    let ink = if !enabled { palette.ink_3 } else if primary { palette.on_accent } else { palette.ink };
    let label_width = text_width(ui, label, font.clone(), ink);
    let left = if icon.is_some() { 8.0 } else { 10.0 };
    let icon_width = if icon.is_some() { theme::ICON_SMALL + 6.0 } else { 0.0 };
    let dot_width = if dot { 4.0 + 6.0 } else { 0.0 };
    let width = left + icon_width + label_width + dot_width + 10.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, theme::CONTROL_HEIGHT), Sense::click());
    // The colours above and below are already the disabled look: the half opacity of a disabled ui is taken back while
    // the button paints (as `dialog_button` does), so that `ink-3` keeps its 4.5:1 on `surface` and `surface-2`.
    let opacity = undo_disabled_fade(ui);
    let pressed = enabled && response.is_pointer_button_down_on();
    let hovered = enabled && response.hovered();
    let fill = if !enabled {
        primary.then_some(palette.surface_2)
    } else if primary {
        Some(if pressed || hovered { palette.accent_ink } else { palette.accent })
    } else if pressed {
        Some(palette.surface_3)
    } else if open || hovered {
        Some(palette.surface_2)
    } else {
        None
    };
    let corner = CornerRadius::same(theme::RADIUS_CONTROL);
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, corner, fill);
    }
    let mut x = rect.left() + left;
    if let Some(icon) = icon {
        let icon_rect = Rect::from_min_size(
            Pos2::new(x, rect.center().y - theme::ICON_SMALL / 2.0),
            Vec2::splat(theme::ICON_SMALL),
        );
        icons::paint(ui, icon_rect, icon, if !enabled { palette.ink_3 } else if primary { palette.on_accent } else { palette.ink_2 });
        x += theme::ICON_SMALL + 6.0;
    }
    ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, label, font, ink);
    if dot {
        ui.painter().circle_filled(Pos2::new(x + label_width + 4.0 + 3.0, rect.center().y), 3.0, palette.accent);
    }
    ui.set_opacity(opacity);
    crate::ui_record::note(ui, label, &response);
    response
}

/// Takes back the half opacity egui gives a disabled ui (`Ui::disable`) for a control that paints its own disabled colours,
/// and returns the opacity to restore after it painted. An enabled ui is left as it is.
fn undo_disabled_fade(ui: &mut Ui) -> f32 {
    let opacity = ui.opacity();
    if !ui.is_enabled() {
        ui.set_opacity((opacity / ui.visuals().disabled_alpha().max(0.01)).min(1.0));
    }
    opacity
}

/// The zoom button of the view group: the value and a chevron, no border (tokens `component.zoom-button`).
pub fn zoom_button(ui: &mut Ui, palette: &Palette, value: &str, open: bool) -> Response {
    let font = theme::font(theme::Type::Body);
    let width = 10.0 + text_width(ui, value, font.clone(), palette.ink) + 2.0 + theme::ICON_SMALL + 4.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, theme::ICON_BUTTON), Sense::click());
    if open || response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
    }
    ui.painter().text(Pos2::new(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, value, font, palette.ink);
    let chevron = Rect::from_min_size(Pos2::new(rect.right() - 4.0 - theme::ICON_SMALL, rect.center().y - 8.0), Vec2::splat(theme::ICON_SMALL));
    icons::paint(ui, chevron, "chev-down", palette.ink_3);
    crate::ui_record::note(ui, value, &response);
    response
}

/// The 14 × 32 chevron half of a split tool button (tokens `component.split-button`).
pub fn split_chevron(ui: &mut Ui, palette: &Palette, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(theme::SPLIT_CHEVRON_WIDTH, theme::ICON_BUTTON), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_MINI), palette.surface_2);
    }
    icons::paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(theme::ICON_SMALL)), "chev-down", palette.ink_3);
    crate::ui_record::note(ui, tooltip, &response);
    response.on_hover_text(tooltip)
}

/// The 1 × 20 separator between groups of a floating bar, 4 on each side (tokens `layout.board.separator`).
pub fn group_separator(ui: &mut Ui, palette: &Palette) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(theme::SEPARATOR_MARGIN_X * 2.0 + 1.0, theme::ICON_BUTTON), Sense::hover());
    let line = Rect::from_center_size(rect.center(), egui::vec2(1.0, theme::SEPARATOR_HEIGHT));
    ui.painter().rect_filled(line, CornerRadius::ZERO, palette.line);
}

/// A 24 px icon button with a 16 px icon, for controls on a title row (tokens `component.mini-button`).
pub fn mini_button(ui: &mut Ui, palette: &Palette, icon: &str, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(theme::MINI_BUTTON), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_MINI), palette.surface_2);
    }
    icons::paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(theme::ICON_SMALL)), icon, palette.ink_2);
    crate::ui_record::note(ui, tooltip, &response);
    response.on_hover_text(tooltip)
}

/// A sidebar or panel tab (tokens `component.sidebar-tab`): 30 high, `surface-2` pill and bold when on,
/// and an agent dot after the label when asked.
pub fn tab(ui: &mut Ui, palette: &Palette, label: &str, on: bool, dot: Option<Color32>) -> Response {
    let font = theme::font(if on { theme::Type::Strong } else { theme::Type::Body });
    let ink = if on { palette.ink } else { palette.ink_2 };
    let label_width = text_width(ui, label, font.clone(), ink);
    let dot_width = if dot.is_some() { 6.0 + 6.0 } else { 0.0 };
    let width = theme::SIDEBAR_TAB_PADDING_X * 2.0 + label_width + dot_width;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, theme::SIDEBAR_TAB_HEIGHT), Sense::click());
    if on {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2.gamma_multiply(0.6));
    }
    let x = rect.left() + theme::SIDEBAR_TAB_PADDING_X;
    ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, label, font, ink);
    if let Some(color) = dot {
        ui.painter().circle_filled(Pos2::new(x + label_width + 6.0 + 3.0, rect.center().y), 3.0, color);
    }
    crate::ui_record::note(ui, label, &response);
    response
}

/// A small text button for callouts and headers (no border).
pub fn text_button(ui: &mut Ui, palette: &Palette, label: &str, color: Color32) -> Response {
    let font = theme::display_font(false, 12.0);
    let width = text_width(ui, label, font.clone(), color) + 12.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 28.0), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font, color);
    crate::ui_record::note(ui, label, &response);
    response
}

/// A menu row: optional 16 px icon, label, shortcut or value at the right (`ink_3`), and a check
/// at the far right for the chosen item (L5).
pub fn menu_item(
    ui: &mut Ui,
    palette: &Palette,
    icon: Option<&str>,
    label: &str,
    shortcut: &str,
    checked: bool,
) -> Response {
    menu_row(ui, palette, icon, label, shortcut, checked, theme::Type::Micro)
}

/// A menu row whose right text is a value (a size, a unit), set in `caption` 12 rather than the shortcut's `micro` 11
/// (tokens `menu.item.value`).
pub fn menu_item_value(ui: &mut Ui, palette: &Palette, icon: Option<&str>, label: &str, value: &str, checked: bool) -> Response {
    menu_row(ui, palette, icon, label, value, checked, theme::Type::Caption)
}

fn menu_row(ui: &mut Ui, palette: &Palette, icon: Option<&str>, label: &str, shortcut: &str, checked: bool, right: theme::Type) -> Response {
    // tokens `component.menu.item`: 30 high, radius 8, gap 8; the chosen item is accent-soft with accent-ink text.
    let width = ui.available_width().max(200.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, theme::MENU_ITEM_HEIGHT), Sense::click());
    let corner = CornerRadius::same(theme::RADIUS_MENU_ITEM);
    if checked {
        ui.painter().rect_filled(rect, corner, palette.accent_soft);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, corner, palette.surface_2);
    }
    let mut x = rect.left() + 8.0;
    if let Some(icon) = icon {
        let icon_rect = Rect::from_min_size(Pos2::new(x, rect.center().y - 8.0), Vec2::splat(theme::ICON_SMALL));
        icons::paint(ui, icon_rect, icon, if checked { palette.accent } else { palette.ink_2 });
        x += theme::ICON_SMALL + 8.0;
    }
    ui.painter().text(
        Pos2::new(x, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::font(theme::Type::Body),
        if checked { palette.accent_ink } else { palette.ink },
    );
    // Values and shortcuts end in one column for every row, before the check's place (home-presets).
    // home-presets: the value ends 37 inside the menu's right border (item edge + padding 4 + border 1 + 32).
    let right_edge = rect.right() - 32.0;
    if !shortcut.is_empty() {
        ui.painter().text(
            Pos2::new(right_edge, rect.center().y),
            Align2::RIGHT_CENTER,
            shortcut,
            theme::font(right),
            palette.ink_3,
        );
    }
    if checked {
        let check = Rect::from_min_size(Pos2::new(rect.right() - 24.0, rect.center().y - 8.0), Vec2::splat(theme::ICON_SMALL));
        icons::paint(ui, check, "check", palette.accent);
    }
    crate::ui_record::note(ui, label, &response);
    response
}

/// A segmented control: a row of options on a `surface-2` track, the chosen one raised. Returns
/// the index clicked this frame.
pub fn segmented(ui: &mut Ui, palette: &Palette, labels: &[&str], selected: usize) -> Option<usize> {
    let mut clicked = None;
    let font = theme::display_font(false, 12.0);
    let widths: Vec<f32> = labels
        .iter()
        .map(|label| text_width(ui, label, font.clone(), palette.ink) + 20.0)
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let (track, _) = ui.allocate_exact_size(egui::vec2(total, 28.0), Sense::hover());
    ui.painter().rect_filled(track, CornerRadius::same(theme::RADIUS_CHIP), palette.surface_2);
    let mut x = track.left() + 2.0;
    for (index, (label, width)) in labels.iter().zip(widths).enumerate() {
        let rect = Rect::from_min_size(Pos2::new(x, track.top() + 2.0), egui::vec2(width, 24.0));
        let response = ui.interact(rect, ui.id().with(("segment", index)), Sense::click());
        let ink = if index == selected {
            ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface);
            ui.painter().rect_stroke(
                rect,
                CornerRadius::same(theme::RADIUS_CONTROL),
                Stroke::new(1.0, palette.line),
                StrokeKind::Inside,
            );
            palette.ink
        } else {
            palette.ink_2
        };
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, *label, font.clone(), ink);
        if response.clicked() {
            clicked = Some(index);
        }
        x += width;
    }
    clicked
}

/// A segmented control of equal parts across `width` (tokens `component.segmented`: track `surface-2` radius 8
/// padding 2 gap 2, parts 28 high radius 6, the chosen one `raised` with a 1 px `line` ring). Returns the index clicked.
pub fn segmented_fill(ui: &mut Ui, palette: &Palette, labels: &[&str], selected: usize, width: f32) -> Option<usize> {
    let mut clicked = None;
    let font = theme::font(theme::Type::Caption);
    let (track, _) = ui.allocate_exact_size(egui::vec2(width, 32.0), Sense::hover());
    ui.painter().rect_filled(track, CornerRadius::same(theme::RADIUS_CHIP), palette.surface_2);
    let count = labels.len().max(1) as f32;
    let part = (track.width() - 4.0 - 2.0 * (count - 1.0)) / count;
    for (index, label) in labels.iter().enumerate() {
        let rect = Rect::from_min_size(Pos2::new(track.left() + 2.0 + index as f32 * (part + 2.0), track.top() + 2.0), egui::vec2(part, 28.0));
        let response = ui.interact(rect, ui.id().with(("segment-fill", *label, index)), Sense::click());
        let on = index == selected;
        if on {
            let corner = CornerRadius::same(theme::RADIUS_CONTROL);
            // tokens `shadow.raised`: a 1 px `line` ring and the small layer of the float shadow.
            let [small, _] = theme::shadow(ui.ctx(), theme::Elevation::Float);
            ui.painter().add(small.as_shape(rect, corner));
            ui.painter().rect_filled(rect, corner, palette.raised);
            ui.painter().rect_stroke(rect, corner, Stroke::new(1.0, palette.line), StrokeKind::Outside);
        } else if response.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_3);
        }
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, *label, font.clone(), if on { palette.ink } else { palette.ink_2 });
        if response.clicked() {
            clicked = Some(index);
        }
    }
    clicked
}

/// A labelled row of the export popover: a 52 px caption column and a segmented control (min height 36).
pub fn option_row(ui: &mut Ui, palette: &Palette, label: &str, options: &[&str], selected: usize) -> Option<usize> {
    let width = ui.available_width();
    let mut clicked = None;
    ui.allocate_ui_with_layout(egui::vec2(width, 36.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(52.0, 36.0), Sense::hover());
        ui.painter().text(Pos2::new(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, label, theme::font(theme::Type::Caption), palette.ink_2);
        clicked = segmented_fill(ui, palette, options, selected, width - 52.0);
    });
    clicked
}

/// A menu separator: 1 px `line` with 4 above and below.
pub fn menu_separator(ui: &mut Ui, palette: &Palette) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 9.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, palette.line));
}

/// The export popover's loss list (tokens `export-popover.loss`): `warn-soft`, one 24 px row per loss
/// with `state-warn`, no header.
pub fn loss_list(ui: &mut Ui, palette: &Palette, losses: &[designllm_contracts::Diagnostic]) {
    // One row per kind of loss, with its count when it repeats.
    let mut rows: Vec<(String, usize)> = Vec::new();
    for loss in losses {
        match rows.iter_mut().find(|(code, _)| *code == loss.code) {
            Some(row) => row.1 += 1,
            None => rows.push((loss.code.clone(), 1)),
        }
    }
    let width = ui.available_width();
    let height = 12.0 + rows.len() as f32 * 24.0 + rows.len().saturating_sub(1) as f32 * 2.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CHIP), palette.warn_soft);
    for (index, (code, count)) in rows.iter().enumerate() {
        let message = losses.iter().find(|loss| loss.code == *code).map(|loss| loss.message.clone()).unwrap_or_default();
        let top = rect.top() + 6.0 + index as f32 * 26.0;
        let row = Rect::from_min_size(Pos2::new(rect.left() + 10.0, top), egui::vec2(width - 20.0, 24.0));
        icons::paint(ui, Rect::from_min_size(Pos2::new(row.left(), row.center().y - 8.0), Vec2::splat(16.0)), "state-warn", palette.warn);
        let text = if *count > 1 { format!("{message} ({count})") } else { message };
        let shown = truncate_to_width(ui, &text, row.width() - 24.0, palette.ink);
        ui.painter().text(Pos2::new(row.left() + 24.0, row.center().y), Align2::LEFT_CENTER, shown, theme::font(theme::Type::Caption), palette.ink);
    }
}

/// The full-width main button of a popover (32 high): accent, or `warn` when the action accepts losses.
pub fn wide_primary(ui: &mut Ui, palette: &Palette, icon: Option<&str>, label: &str, warn: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), theme::CONTROL_HEIGHT), Sense::click());
    let (fill, ink) = if warn { (palette.warn, palette.on_warn) } else { (palette.accent, palette.on_accent) };
    let fill = if response.hovered() { fill.gamma_multiply(0.92) } else { fill };
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), fill);
    let font = theme::font(theme::Type::Strong);
    let label_width = text_width(ui, label, font.clone(), ink);
    let icon_width = if icon.is_some() { 16.0 + 6.0 } else { 0.0 };
    let mut x = rect.center().x - (icon_width + label_width) / 2.0;
    if let Some(icon) = icon {
        icons::paint(ui, Rect::from_min_size(Pos2::new(x, rect.center().y - 8.0), Vec2::splat(16.0)), icon, ink);
        x += icon_width;
    }
    ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, label, font, ink);
    crate::ui_record::note(ui, label, &response);
    response
}

/// A field-like button that shows a value and a chevron. The caller opens its menu from the
/// response with `egui::Popup::from_toggle_button_response`.
pub fn dropdown(ui: &mut Ui, palette: &Palette, value: &str, icon: Option<&str>, width: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, theme::FIELD_HEIGHT), Sense::click());
    // A disabled dropdown paints its own colours (`ink-3` text and icons, a `line` border on `surface`), so the half opacity
    // egui gives a disabled ui is taken back for the painting and restored after it.
    let enabled = ui.is_enabled();
    let opacity = undo_disabled_fade(ui);
    let fill = if enabled && response.hovered() { palette.surface_2 } else { palette.surface };
    let (outline, ink) = if enabled { (palette.line_strong, palette.ink) } else { (palette.line, palette.ink_3) };
    let corner = CornerRadius::same(theme::RADIUS_CONTROL);
    ui.painter().rect_filled(rect, corner, fill);
    ui.painter().rect_stroke(rect, corner, Stroke::new(1.0, outline), StrokeKind::Inside);
    let mut x = rect.left() + 8.0;
    if let Some(icon) = icon {
        let icon_rect = Rect::from_min_size(Pos2::new(x, rect.center().y - 8.0), Vec2::splat(theme::ICON_SMALL));
        icons::paint(ui, icon_rect, icon, if enabled { palette.ink_2 } else { palette.ink_3 });
        x += theme::ICON_SMALL + 6.0;
    }
    let chevron = Rect::from_min_size(
        Pos2::new(rect.right() - 22.0, rect.center().y - 8.0),
        Vec2::splat(theme::ICON_SMALL),
    );
    icons::paint(ui, chevron, "chev-down", palette.ink_3);
    let room = (rect.right() - 26.0 - x).max(0.0);
    let shown = truncate_to_width(ui, value, room, ink);
    ui.painter().text(
        Pos2::new(x, rect.center().y),
        Align2::LEFT_CENTER,
        shown,
        theme::display_font(false, 13.0),
        ink,
    );
    ui.set_opacity(opacity);
    crate::ui_record::note(ui, value, &response);
    response
}

/// Cuts `text` with an ellipsis so that it fits `width` points.
pub fn truncate_to_width(ui: &Ui, text: &str, width: f32, color: Color32) -> String {
    cut_to_width(ui, text, width, house_font(ui, theme::Type::Body), color)
}

/// `truncate_to_width` in `font`.
fn cut_to_width(ui: &Ui, text: &str, width: f32, font: FontId, color: Color32) -> String {
    if text_width(ui, text, font.clone(), color) <= width {
        return text.to_owned();
    }
    let mut shown: Vec<char> = text.chars().collect();
    while !shown.is_empty() {
        shown.pop();
        let candidate: String = shown.iter().collect::<String>() + "…";
        if text_width(ui, &candidate, font.clone(), color) <= width {
            return candidate;
        }
    }
    String::new()
}

/// Formats a number with at most `decimals` decimals and no trailing zeros.
pub fn format_number(value: f64, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

/// Reads a typed number; a comma is accepted as the decimal mark.
pub fn parse_number(text: &str) -> Option<f64> {
    let cleaned = text.trim().replace(',', ".");
    cleaned.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// A single-line text field in the house style (white, `line-strong` border).
pub fn text_field(ui: &mut Ui, palette: &Palette, text: &mut String, hint: &str, width: f32) -> Response {
    let response = ui.add_sized(
        egui::vec2(width, theme::FIELD_HEIGHT),
        TextEdit::singleline(text)
            .hint_text(egui::RichText::new(hint).color(palette.ink_3))
            .font(house_font(ui, theme::Type::Body))
            .margin(egui::Margin::symmetric(8, 4)),
    );
    crate::ui_record::note(ui, hint, &response);
    response
}

/// The options of a text edit made by `edit_line` or `edit_area` (task 68h). The defaults are a framed field in the property
/// panel's face (display 13, ink, margin 8 × 4, enabled, 28 high). `borderless` drops the frame and the margin for an editor
/// that is drawn inside a box of its own (a card, a canvas, a grid cell).
#[derive(Clone)]
pub struct Field<'a> {
    pub hint: &'a str,
    pub width: f32,
    pub font: Option<FontId>,
    pub color: Option<Color32>,
    pub id: Option<egui::Id>,
    pub margin: Option<egui::Margin>,
    pub borderless: bool,
    pub enabled: bool,
    /// Lines shown by `edit_area` (its height before it grows).
    pub rows: usize,
    /// The least height of `edit_area`.
    pub min_height: f32,
    pub align: egui::Align,
    /// Enter does not take the focus away from a single-line field (`return_key(None)`).
    pub enter_keeps_focus: bool,
    /// The label the field is recorded under for `ui.layout` (`ui_record`). None records nothing: a hint is not a label,
    /// since many fields show the same one (`여러 값`).
    pub record: Option<&'a str>,
    /// The height of an `edit_line`: framed fields default to the field height, borderless ones to their text's. A field
    /// in a grid cell or a list row takes the cell's height.
    pub height: Option<f32>,
}

impl Default for Field<'_> {
    fn default() -> Self {
        Self {
            hint: "",
            width: 0.0,
            font: None,
            color: None,
            id: None,
            margin: None,
            borderless: false,
            enabled: true,
            rows: 1,
            min_height: 0.0,
            align: egui::Align::Min,
            enter_keeps_focus: false,
            record: None,
            height: None,
        }
    }
}

/// A single-line text edit in the house style (`Field`): framed at `width` × `height` (the field height unless given), or
/// borderless at `width` (and `height` when given).
pub fn edit_line(ui: &mut Ui, palette: &Palette, text: &mut String, field: Field<'_>) -> Response {
    let font = field.font.clone().unwrap_or_else(|| theme::display_font(false, 13.0));
    let mut edit = TextEdit::singleline(text)
        .font(font.clone())
        .text_color(field.color.unwrap_or(palette.ink))
        .horizontal_align(field.align)
        .hint_text(egui::RichText::new(field.hint).font(font).color(palette.ink_3))
        .margin(field_margin(&field, egui::Margin::symmetric(8, 4)));
    if let Some(id) = field.id {
        edit = edit.id(id);
    }
    if field.enter_keeps_focus {
        edit = edit.return_key(None);
    }
    if field.borderless {
        edit = edit.frame(false);
    }
    let size = match (field.borderless, field.height) {
        (_, Some(height)) => Some(egui::vec2(field.width, height)),
        (false, None) => Some(egui::vec2(field.width, theme::FIELD_HEIGHT)),
        (true, None) => None,
    };
    let response = ui
        .add_enabled_ui(field.enabled, |ui| match size {
            Some(size) => ui.add_sized(size, edit),
            None => ui.add(edit.desired_width(field.width)),
        })
        .inner;
    if let Some(label) = field.record {
        crate::ui_record::note(ui, label, &response);
    }
    response
}

/// A multi-line text edit in the house style (`Field`): framed at `width` with `rows` lines, or borderless.
pub fn edit_area(ui: &mut Ui, palette: &Palette, text: &mut String, field: Field<'_>) -> Response {
    let font = field.font.clone().unwrap_or_else(|| theme::display_font(false, 13.0));
    let mut edit = TextEdit::multiline(text)
        .font(font.clone())
        .text_color(field.color.unwrap_or(palette.ink))
        .horizontal_align(field.align)
        .hint_text(egui::RichText::new(field.hint).font(font).color(palette.ink_3))
        .desired_rows(field.rows.max(1))
        .desired_width(field.width)
        .margin(field_margin(&field, egui::Margin::symmetric(8, 4)));
    if field.min_height > 0.0 {
        edit = edit.min_size(egui::vec2(field.width, field.min_height));
    }
    if let Some(id) = field.id {
        edit = edit.id(id);
    }
    if field.borderless {
        edit = edit.frame(false);
    }
    let response = ui.add_enabled_ui(field.enabled, |ui| ui.add(edit)).inner;
    if let Some(label) = field.record {
        crate::ui_record::note(ui, label, &response);
    }
    response
}

fn field_margin(field: &Field<'_>, framed: egui::Margin) -> egui::Margin {
    field.margin.unwrap_or(if field.borderless { egui::Margin::ZERO } else { framed })
}

const NUMBER_FIELD_WIDTH: f32 = 36.0;

/// A whole number in a dense row (task 68h: in place of a drag value): `prefix` (caption `ink-2`, may be empty), a minus
/// button, a field the number is typed into, and a plus button. The buttons take Tab focus and say what they do
/// (`1 줄이기`, `1 늘리기`); a typed number counts once it parses, and leaving the field shows the value again. The value is
/// kept inside `range`. Returns true when this frame changed it.
pub fn number_stepper(ui: &mut Ui, palette: &Palette, value: &mut u32, range: RangeInclusive<u32>, prefix: &str) -> bool {
    let (low, high) = (*range.start(), *range.end());
    let before = *value;
    *value = (*value).clamp(low, high);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        if !prefix.trim().is_empty() {
            ui.label(egui::RichText::new(prefix.trim()).font(theme::font(theme::Type::Caption)).color(palette.ink_2));
            ui.add_space(4.0);
        }
        // `unique_id`: the rows of a panel share their stable id, so two steppers would share a field.
        let id = ui.unique_id().with("number-stepper");
        let draft_id = id.with("draft");
        if step_button(ui, palette, false, "1 줄이기").clicked() {
            *value = value.saturating_sub(1).max(low);
            ui.data_mut(|data| data.remove::<String>(draft_id));
        }
        let mut text = ui.data(|data| data.get_temp::<String>(draft_id)).unwrap_or_else(|| value.to_string());
        let response = edit_line(ui, palette, &mut text, Field {
            width: NUMBER_FIELD_WIDTH,
            height: Some(theme::MINI_BUTTON),
            margin: Some(egui::Margin::symmetric(4, 2)),
            align: egui::Align::Center,
            id: Some(id),
            ..Default::default()
        });
        if response.changed() {
            if let Some(typed) = parse_whole_number(&text) {
                *value = typed.clamp(low, high);
            }
        }
        if response.has_focus() {
            ui.data_mut(|data| data.insert_temp(draft_id, text));
        } else {
            ui.data_mut(|data| data.remove::<String>(draft_id));
        }
        if step_button(ui, palette, true, "1 늘리기").clicked() {
            *value = value.saturating_add(1).min(high);
            ui.data_mut(|data| data.remove::<String>(draft_id));
        }
    });
    *value != before
}

/// A typed whole number; a number too large for `u32` is read as the largest one (the caller clamps it).
fn parse_whole_number(text: &str) -> Option<u32> {
    let digits = text.trim();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(digits.parse::<u32>().unwrap_or(u32::MAX))
}

/// A 24 px outlined minus or plus button of a `number_stepper`, recorded and shown with its `tooltip`.
fn step_button(ui: &mut Ui, palette: &Palette, plus: bool, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(theme::MINI_BUTTON), Sense::click());
    let opacity = undo_disabled_fade(ui);
    let ink = paint_compact_frame(ui, palette, rect, &response);
    let stroke = Stroke::new(1.5, ink);
    let centre = rect.center();
    ui.painter().line_segment([centre - egui::vec2(4.5, 0.0), centre + egui::vec2(4.5, 0.0)], stroke);
    if plus {
        ui.painter().line_segment([centre - egui::vec2(0.0, 4.5), centre + egui::vec2(0.0, 4.5)], stroke);
    }
    ui.set_opacity(opacity);
    let enabled = ui.is_enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tooltip));
    crate::ui_record::note(ui, tooltip, &response);
    if enabled { response.on_hover_text(tooltip) } else { response.on_disabled_hover_text(tooltip) }
}

/// A compact outlined button for a dense row (task 68h): 24 high, `ink-2` label, `surface-2` on hover.
pub fn compact_button(ui: &mut Ui, palette: &Palette, label: &str) -> Response {
    let font = theme::font(theme::Type::Caption);
    let width = text_width(ui, label, font.clone(), palette.ink) + 16.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 24.0), Sense::click());
    let opacity = undo_disabled_fade(ui);
    let ink = paint_compact_frame(ui, palette, rect, &response);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font, ink);
    ui.set_opacity(opacity);
    let enabled = ui.is_enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    crate::ui_record::note(ui, label, &response);
    response
}

/// The frame of a compact button: `line-strong` outline, `surface-2` on hover, the 2 px `accent` focus ring outside (§5.4).
/// Returns the colour of its label: `ink-2`, or `ink-3` when disabled.
fn paint_compact_frame(ui: &Ui, palette: &Palette, rect: Rect, response: &Response) -> Color32 {
    let enabled = ui.is_enabled();
    let corner = CornerRadius::same(theme::RADIUS_CONTROL);
    if enabled && response.hovered() {
        ui.painter().rect_filled(rect, corner, palette.surface_2);
    }
    ui.painter().rect_stroke(rect, corner, Stroke::new(1.0, if enabled { palette.line_strong } else { palette.line }), StrokeKind::Inside);
    if response.has_focus() {
        ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(theme::RADIUS_CONTROL + 2), Stroke::new(2.0, palette.accent), StrokeKind::Outside);
    }
    if enabled { palette.ink_2 } else { palette.ink_3 }
}

/// The borderless multi-line editor that a canvas text is edited in (task 68h): it fills `size` where the text is, and returns
/// its output, from which the IME layout is recorded. `interactive` false keeps the text shown and takes no input.
pub fn inline_edit(
    ui: &mut Ui,
    text: &mut String,
    id: egui::Id,
    font: FontId,
    color: Color32,
    align: egui::Align,
    size: Vec2,
    interactive: bool,
) -> egui::text_edit::TextEditOutput {
    TextEdit::multiline(text)
        .id(id)
        .font(font)
        .text_color(color)
        .horizontal_align(align)
        .frame(false)
        .margin(egui::Margin::ZERO)
        .desired_width(size.x)
        .desired_rows(1)
        .min_size(size)
        .interactive(interactive)
        .show(ui)
}

/// Parses `#RRGGBB` (the `#` is optional).
pub fn parse_hex(text: &str) -> Option<Color32> {
    let digits = text.trim().trim_start_matches('#');
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    Some(Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8))
}

/// `#RRGGBB` in upper case.
pub fn normalise_hex(text: &str) -> String {
    format!("#{}", text.trim().trim_start_matches('#').to_uppercase())
}

/// An agent avatar: a disc with the agent's initial and a state ring (§5.5). `size` is the disc
/// diameter (24 in the top bar and the AI tab).
pub fn avatar(ui: &mut Ui, palette: &Palette, letter: &str, color: Color32, state: AgentState, size: f32) -> Response {
    // tokens `component.avatar`: box 32, circle 24, ring radius 14.5 and 2 wide (scaled for smaller avatars).
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size + 8.0), Sense::click());
    let center = rect.center();
    let radius = size / 2.0;
    let ring_radius = radius * 14.5 / 12.0;
    ui.painter().circle_filled(center, radius, color);
    ui.painter().text(center, Align2::CENTER_CENTER, letter, theme::font(theme::Type::Avatar), palette.on_agent);
    let ring = Stroke::new(if size >= 24.0 { 2.0 } else { 1.5 }, color);
    let arc = |start: f32, sweep: f32| -> Vec<Pos2> {
        (0..=24)
            .map(|step| center + Vec2::angled(start + sweep * step as f32 / 24.0) * ring_radius)
            .collect()
    };
    let tau = std::f32::consts::TAU;
    match state {
        AgentState::Waiting => {
            // Dashed, standing still: eight short arcs around the disc.
            for index in 0..8 {
                let start = index as f32 / 8.0 * tau;
                ui.painter().add(egui::Shape::line(arc(start, tau / 16.0), ring));
            }
        }
        AgentState::Running => {
            // A faint track and one arc (26 of the 92 around) that turns once per second.
            let time = ui.input(|input| input.time) as f32;
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            ui.painter().circle_stroke(center, ring_radius, Stroke::new(ring.width, palette.agent_alpha(color, theme::AgentAlpha::RingTrack)));
            ui.painter().add(egui::Shape::line(arc(time * tau, tau * 26.0 / 92.0), ring));
        }
        AgentState::Review => {
            ui.painter().circle_stroke(center, ring_radius, Stroke::new(ring.width, palette.agent_alpha(color, theme::AgentAlpha::RingTrack)));
            ui.painter().add(egui::Shape::line(arc(-80f32.to_radians(), tau * 26.0 / 92.0), ring));
        }
        AgentState::Done => {}
        AgentState::Failed => {
            let dot = center + Vec2::new(radius * 0.7, radius * 0.7);
            ui.painter().circle_filled(dot, 4.0, palette.danger);
            ui.painter().circle_stroke(dot, 4.0, Stroke::new(1.0, palette.surface));
        }
    }
    response
}

/// Three segments: done (agent colour), current (partly filled), pending (`surface-3`).
pub fn progress_rail(ui: &mut Ui, palette: &Palette, stages: [Stage; 3], color: Color32) {
    let width = ui.available_width().max(60.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 4.0), Sense::hover());
    let gap = 3.0;
    let segment = (rect.width() - gap * 2.0) / 3.0;
    for (index, stage) in stages.iter().enumerate() {
        let left = rect.left() + index as f32 * (segment + gap);
        let track = Rect::from_min_size(Pos2::new(left, rect.top()), egui::vec2(segment, 4.0));
        ui.painter().rect_filled(track, CornerRadius::same(2), palette.surface_3);
        let filled = match stage {
            Stage::Done => Some(track),
            Stage::Current(fraction) => Some(Rect::from_min_size(
                track.min,
                egui::vec2(segment * fraction.clamp(0.0, 1.0), 4.0),
            )),
            Stage::Pending => None,
        };
        if let Some(filled) = filled {
            ui.painter().rect_filled(filled, CornerRadius::same(2), color);
        }
    }
}

/// A tinted explanation box for a notice: one message and up to two buttons. Returns the index of the button clicked.
pub fn callout(ui: &mut Ui, palette: &Palette, text: &str, buttons: &[&str], warning: bool) -> Option<usize> {
    let mut clicked = None;
    egui::Frame::new()
        .fill(if warning { palette.warn_soft } else { palette.danger_soft })
        .corner_radius(CornerRadius::same(theme::RADIUS_CHIP))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_max_width(280.0);
            ui.label(egui::RichText::new(text).font(theme::display_font(false, 12.0)).color(palette.ink));
            if !buttons.is_empty() {
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for (index, label) in buttons.iter().enumerate() {
                        if labeled_button(ui, palette, None, label, false).clicked() {
                            clicked = Some(index);
                        }
                    }
                });
            }
        });
    clicked
}


/// Where a dialog sits and which keys it answers (GUI_DESIGN §5.1, §5.3; task 68d).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DialogPlace {
    /// Centred on the window over a backdrop that takes the clicks behind it. Enter, Esc and a press on the backdrop answer it.
    Modal,
    /// Centred on the window with no backdrop (a form over the board). Enter and Esc answer it while no modal is open.
    Screen,
    /// A notice hung from the top middle of this rectangle (the board), under the notices drawn before it in the same pass, so
    /// that two notices never overlap. It takes no keys: the board under it keeps them.
    BoardTop(Rect),
}

/// The look of a dialog button (GUI_DESIGN §5.1): `Primary` is the one default action (accent), `Plain` a neutral choice,
/// `Danger` a choice that throws work away (`danger` text), `Warn` accepts a loss (`warn` fill, as the export popover).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogButton {
    Plain,
    Primary,
    Danger,
    Warn,
}

/// One button of a dialog's button row (`dialog_buttons`).
#[derive(Clone, Copy, Debug)]
pub struct DialogChoice<'a> {
    pub label: &'a str,
    pub kind: DialogButton,
    pub enabled: bool,
    /// Only a person may press it (task 69): it is reported to the UI-test layer as human only.
    pub human_only: bool,
}

impl<'a> DialogChoice<'a> {
    pub fn new(label: &'a str, kind: DialogButton) -> Self {
        Self { label, kind, enabled: true, human_only: false }
    }

    pub fn enabled(self, enabled: bool) -> Self {
        Self { enabled, ..self }
    }

    pub fn human_only(self) -> Self {
        Self { human_only: true, ..self }
    }
}

/// What a dialog gave back in this frame: the contents' value and the keys it took. The caller maps `enter` to its main
/// button (not when that button accepts a loss or throws work away) and `dismissed` to its safe choice (cancel, keep
/// working), each only while that button is shown and enabled.
#[derive(Debug)]
pub struct DialogResponse<R> {
    pub inner: R,
    /// Enter, with no menu open and no widget of the dialog holding the keyboard (a focused button takes its own Enter).
    pub enter: bool,
    /// Esc with no menu open, or (a modal) a press on the backdrop.
    pub dismissed: bool,
}

/// Dialog width: the widest floating panel the design allows (`theme::PANEL_MAX`, 420; §4 panels 280–420).
pub const DIALOG_WIDTH: f32 = theme::PANEL_MAX;
/// A notice's width on the board: that of the export popover, the other box that floats on the board (`theme::EXPORT_WIDTH`).
pub const NOTICE_WIDTH: f32 = theme::EXPORT_WIDTH;
/// A form row of a dialog: the properties panel's row (§5.3: width 320, label column 76, row 32 with a 28 field).
pub const DIALOG_FORM_WIDTH: f32 = theme::PANEL_WIDTH;
/// Padding inside the frame, and a notice's distance from the board's top: spacing step 16 (§5.3).
const DIALOG_PADDING: f32 = 16.0;
/// Between the title and the body, between buttons and button lines, between stacked notices: spacing step 8 (§5.3).
const DIALOG_GAP: f32 = 8.0;
/// What a centred dialog leaves to the window's top and bottom before its body scrolls: spacing step 24 (§5.3).
const DIALOG_EDGE: f32 = 24.0;
/// The title's line (`title` 14/20, §5.2).
const DIALOG_TITLE_LINE: f32 = 20.0;
/// The backdrop of a modal: black at 0x66 (40 %) in both themes, the alpha of the dark theme's large float shadow layer
/// (`shadow.float`, dark: `0 4 14 #00000066`). Black, not `ink`: the dark theme's `ink` is light and would whiten the window.
const BACKDROP_ALPHA: u8 = 0x66;
/// A button's padding on each side: spacing step 16 (§5.3).
const DIALOG_BUTTON_PADDING: f32 = 16.0;
/// No token gives a button's least width. 88 is a four-syllable label at 13 px (about 56) with the 16 padding on each side, so
/// that two-syllable choices (취소, 복원, 거절) are as wide as the four-syllable ones next to them.
const DIALOG_BUTTON_MIN_WIDTH: f32 = 88.0;

/// The house face of a role, or egui's proportional face while the app's fonts are not installed (a bare test context).
fn house_font(ui: &Ui, role: theme::Type) -> FontId {
    let font = theme::font(role);
    if ui.fonts(|fonts| fonts.families().contains(&font.family)) {
        font
    } else {
        FontId::new(font.size, egui::FontFamily::Proportional)
    }
}

/// A confirmation, recovery or notice window in the house style (GUI_DESIGN §5.1, §5.3): `surface`, a 1 px `line` border,
/// radius 10, the menu shadow, the title in the title face (14/20 Bold) and the contents centred under it. There is no window
/// bar and no close cross: the choices are buttons (`dialog_buttons`). A body taller than the window (the board, for a
/// notice) scrolls. A dialog whose choices end its contents takes `dialog_with_choices`, which keeps them in view.
pub fn dialog<R>(
    ctx: &egui::Context,
    palette: &Palette,
    id: egui::Id,
    title: &str,
    place: DialogPlace,
    contents: impl FnOnce(&mut Ui) -> R,
) -> DialogResponse<R> {
    dialog_with_footer(ctx, palette, id, title, place, contents, |_, inner| inner)
}

/// `dialog` with its button row under the scrolling body, always in view however long the body is: `contents` draws the body
/// and returns its value and the choices. Returns that value and, per choice, whether it was pressed in this frame.
pub fn dialog_with_choices<'c, B>(
    ctx: &egui::Context,
    palette: &Palette,
    id: egui::Id,
    title: &str,
    place: DialogPlace,
    contents: impl FnOnce(&mut Ui) -> (B, Vec<DialogChoice<'c>>),
) -> DialogResponse<(B, Vec<bool>)> {
    dialog_with_footer(ctx, palette, id, title, place, contents, |ui, (inner, choices)| {
        let pressed = dialog_buttons(ui, palette, &choices).iter().map(Response::clicked).collect();
        (inner, pressed)
    })
}

fn dialog_with_footer<B, R>(
    ctx: &egui::Context,
    palette: &Palette,
    id: egui::Id,
    title: &str,
    place: DialogPlace,
    contents: impl FnOnce(&mut Ui) -> B,
    footer: impl FnOnce(&mut Ui, B) -> R,
) -> DialogResponse<R> {
    let width = if matches!(place, DialogPlace::BoardTop(_)) { NOTICE_WIDTH } else { DIALOG_WIDTH };
    let frame = egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.line))
        .corner_radius(CornerRadius::same(theme::RADIUS_FLOAT))
        .inner_margin(egui::Margin::same(DIALOG_PADDING as i8))
        // A Frame takes one shadow layer: the larger one of the menu's two.
        .shadow(theme::shadow(ctx, theme::Elevation::Menu)[1]);
    // Notices stack down the board: each starts 8 under the one drawn before it in this pass.
    let stack_id = egui::Id::new("dialog-board-notices");
    let pass = ctx.cumulative_pass_nr();
    let (top, room) = match place {
        DialogPlace::BoardTop(board) => {
            let top = ctx
                .data(|data| data.get_temp::<(u64, f32)>(stack_id))
                .filter(|(stacked, _)| *stacked == pass)
                .map_or(board.top() + DIALOG_PADDING, |(_, next)| next);
            (top, board.bottom() - DIALOG_PADDING - top)
        }
        _ => (0.0, ctx.content_rect().height() - 2.0 * DIALOG_EDGE),
    };
    let title_room = if title.is_empty() { 0.0 } else { DIALOG_TITLE_LINE + DIALOG_GAP };
    // The button row is measured as it is drawn; the body scrolls in what the last frame's row left.
    let footer_id = id.with("dialog-footer-height");
    let footer_room = ctx.data(|data| data.get_temp::<f32>(footer_id)).unwrap_or(0.0);
    // The room less the padding and the border on both sides and the item spacing above and below the body.
    let spacing = ctx.style().spacing.item_spacing.y;
    let body_room = (room - 2.0 * (DIALOG_PADDING + 1.0 + spacing) - title_room - footer_room).max(theme::CONTROL_HEIGHT);
    let body = move |ui: &mut Ui| {
        // The frame is `width` wide: the contents, the padding and the 1 px border on each side.
        ui.set_width(width - 2.0 * (DIALOG_PADDING + 1.0));
        // An area offers its contents the size it had last frame: without this the body could not grow past it (a form whose
        // next step is taller would keep the last step's height and scroll).
        ui.set_max_height(title_room + body_room + footer_room);
        ui.vertical_centered(|ui| {
            if !title.is_empty() {
                ui.label(egui::RichText::new(title).font(house_font(ui, theme::Type::Title)).color(palette.ink));
                ui.add_space(DIALOG_GAP);
            }
            let inner = egui::ScrollArea::vertical()
                .id_salt("dialog-body")
                .max_height(body_room)
                .auto_shrink([false, true])
                .show(ui, |ui| ui.vertical_centered(contents).inner)
                .inner;
            let before = ui.cursor().min.y;
            let inner = footer(ui, inner);
            let measured = ui.cursor().min.y - before;
            ui.ctx().data_mut(|data| data.insert_temp(footer_id, measured.max(0.0)));
            inner
        })
        .inner
    };
    // The frame a dialog first shows in takes no keys: the Enter or Esc that opened it is not its answer.
    let shown_id = id.with("dialog-shown-frame");
    let frame_nr = ctx.cumulative_frame_nr();
    let fresh = ctx.data(|data| data.get_temp::<u64>(shown_id)).is_none_or(|last| last + 1 < frame_nr);
    ctx.data_mut(|data| data.insert_temp(shown_id, frame_nr));
    // Keys go to one dialog only, the last opened (the top one), never while an IME composes, and never to a menu's Esc.
    let keys_free = !fresh && !egui::Popup::is_any_open(ctx) && !ime_in_use(ctx);
    match place {
        DialogPlace::Modal => {
            let on_top = top_of_dialogs(ctx, id);
            let shown = egui::Modal::new(id)
                .backdrop_color(Color32::from_black_alpha(BACKDROP_ALPHA))
                .frame(frame)
                .show(ctx, body);
            let layer = shown.response.layer_id;
            let focus_inside = ctx
                .memory(|memory| memory.focused())
                .and_then(|focused| ctx.read_response(focused))
                .is_some_and(|focused| focused.layer_id == layer);
            let live = keys_free && on_top && shown.is_top_modal;
            let enter = live && !focus_inside && take_dialog_key(ctx, egui::Key::Enter);
            // What egui's `ModalResponse::should_close` answers (Esc on the top modal with no menu open, or the backdrop
            // pressed), with Esc read as `take_dialog_key` reads it.
            let dismissed = (live && take_dialog_key(ctx, egui::Key::Escape)) || (!fresh && shown.backdrop_response.clicked());
            DialogResponse { inner: shown.inner, enter, dismissed }
        }
        DialogPlace::Screen => {
            let on_top = top_of_dialogs(ctx, id);
            let area = egui::Area::new(id)
                .order(egui::Order::Foreground)
                .movable(false)
                .fade_in(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0]);
            let shown = area.show(ctx, |ui| frame.show(ui, body).inner);
            let live = keys_free && on_top && ctx.memory(|memory| memory.top_modal_layer()).is_none();
            let enter = live && ctx.memory(|memory| memory.focused()).is_none() && take_dialog_key(ctx, egui::Key::Enter);
            let dismissed = live && take_dialog_key(ctx, egui::Key::Escape);
            DialogResponse { inner: shown.inner, enter, dismissed }
        }
        DialogPlace::BoardTop(board) => {
            let area = egui::Area::new(id)
                .order(egui::Order::Foreground)
                .movable(false)
                .fade_in(false)
                .pivot(Align2::CENTER_TOP)
                .fixed_pos(Pos2::new(board.center().x, top));
            let shown = area.show(ctx, |ui| frame.show(ui, body).inner);
            ctx.data_mut(|data| data.insert_temp(stack_id, (pass, shown.response.rect.bottom() + DIALOG_GAP)));
            DialogResponse { inner: shown.inner, enter: false, dismissed: false }
        }
    }
}

/// The dialogs that take keys (modal and centred), each with the frame it opened in and the last frame it was drawn in.
#[derive(Clone, Default)]
struct DialogStack(Vec<(egui::Id, u64, u64)>);

/// Records `id` as drawn in this frame and answers whether it is the top one: of the dialogs drawn in this frame or the last,
/// the one opened last (egui raises a newly shown area above the others). One that closed last frame still counts for a frame,
/// so that the key that closed it does not answer the one under it as well.
fn top_of_dialogs(ctx: &egui::Context, id: egui::Id) -> bool {
    let key = egui::Id::new("dialog-stack");
    let frame = ctx.cumulative_frame_nr();
    let mut stack = ctx.data(|data| data.get_temp::<DialogStack>(key)).unwrap_or_default();
    stack.0.retain(|(_, _, last)| last + 1 >= frame);
    match stack.0.iter_mut().find(|(shown, _, _)| *shown == id) {
        Some(entry) => entry.2 = frame,
        None => stack.0.push((id, frame, frame)),
    }
    let top = stack.0.iter().max_by_key(|(_, opened, _)| *opened).map(|(shown, _, _)| *shown);
    ctx.data_mut(|data| data.insert_temp(key, stack));
    top == Some(id)
}

/// Whether an IME composes: an IME event in this frame (a commit's Enter, a cancel's Esc), or a composition left open by the
/// frames before (kept from frame to frame).
fn ime_in_use(ctx: &egui::Context) -> bool {
    let key = egui::Id::new("dialog-ime");
    let composing = ctx.data(|data| data.get_temp::<bool>(key)).unwrap_or(false);
    let events = ctx.input(|input| {
        input.events.iter().filter_map(|event| match event { egui::Event::Ime(ime) => Some(ime.clone()), _ => None }).collect::<Vec<_>>()
    });
    // The events set the state absolutely (the last one wins), so a second dialog in the same frame reads the same answer.
    let composing = events.iter().fold(composing, |composing, ime| match ime {
        egui::ImeEvent::Preedit(text) => !text.is_empty(),
        egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => false,
        egui::ImeEvent::Enabled => composing,
    });
    ctx.data_mut(|data| data.insert_temp(key, composing));
    composing || !events.is_empty()
}

/// Takes the press event of Enter or Esc (no modifier, not a repeat of a held key) for the dialog drawn now. The board and the
/// window's shortcuts leave these keys alone while a dialog that takes keys is open (`DesktopApp::key_dialog_open`), so the
/// event is still in the frame's input.
fn take_dialog_key(ctx: &egui::Context, key: egui::Key) -> bool {
    ctx.input_mut(|input| {
        let index = input.events.iter().position(|event| {
            matches!(event, egui::Event::Key { key: pressed_key, pressed: true, repeat: false, modifiers, .. }
                if *pressed_key == key && modifiers.is_none())
        });
        index.map(|index| input.events.remove(index)).is_some()
    })
}

fn dialog_button_font(ui: &Ui, kind: DialogButton) -> FontId {
    house_font(ui, if kind == DialogButton::Primary { theme::Type::Strong } else { theme::Type::Body })
}

/// The width `dialog_button` takes for `label`: the label and 16 on each side, at least 88. A row that must not wrap is sized with it.
pub fn dialog_button_width(ui: &Ui, label: &str, kind: DialogButton) -> f32 {
    (text_width(ui, label, dialog_button_font(ui, kind), Color32::WHITE) + 2.0 * DIALOG_BUTTON_PADDING).max(DIALOG_BUTTON_MIN_WIDTH)
}

/// The buttons of a dialog (GUI_DESIGN §5.1): 8 under what precedes them, 8 apart, each line centred, and wrapped onto a new
/// line (8 below) when the dialog's width is used up. Each button is reported to the UI-test layer, human only where its
/// choice says so. Returns the buttons' responses in the order of `choices`.
pub fn dialog_buttons(ui: &mut Ui, palette: &Palette, choices: &[DialogChoice<'_>]) -> Vec<Response> {
    let widths: Vec<f32> = choices.iter().map(|choice| dialog_button_width(ui, choice.label, choice.kind)).collect();
    let room = ui.available_width();
    let mut lines: Vec<std::ops::Range<usize>> = Vec::new();
    let (mut start, mut used) = (0, 0.0);
    for (index, width) in widths.iter().enumerate() {
        if index > start && used + DIALOG_GAP + width > room {
            lines.push(start..index);
            (start, used) = (index, *width);
        } else {
            used = if index == start { *width } else { used + DIALOG_GAP + width };
        }
    }
    if start < choices.len() {
        lines.push(start..choices.len());
    }
    ui.add_space(DIALOG_GAP);
    let mut responses = Vec::with_capacity(choices.len());
    for (number, line) in lines.into_iter().enumerate() {
        if number > 0 {
            ui.add_space(DIALOG_GAP);
        }
        let total = widths[line.clone()].iter().sum::<f32>() + DIALOG_GAP * (line.len() - 1) as f32;
        ui.allocate_ui_with_layout(
            egui::vec2(total.min(room), theme::CONTROL_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = DIALOG_GAP;
                for choice in &choices[line] {
                    let response = dialog_button_shape(ui, palette, choice.label, choice.kind, choice.enabled);
                    if choice.human_only {
                        crate::ui_record::note_human_only(ui, choice.label, &response);
                    } else {
                        crate::ui_record::note(ui, choice.label, &response);
                    }
                    responses.push(response);
                }
            },
        );
    }
    responses
}

/// One dialog button: 32 high, radius 6, 16 padding, at least 88 wide. `Primary` takes the bold label. A disabled main button
/// (`Primary`, `Warn`) drops its fill to `surface-2` with `ink-3` text (4.5:1, §5.1), so that it reads as off beside the
/// outlined ones; a disabled outlined button keeps `surface` with `ink-3` text and a `line` border. The keyboard focus is
/// the 2 px `accent` ring 2 px outside (§5.4). It is reported to the UI-test layer under its label.
pub fn dialog_button(ui: &mut Ui, palette: &Palette, label: &str, kind: DialogButton, enabled: bool) -> Response {
    dialog_button_recorded(ui, palette, label, label, kind, enabled)
}

/// `dialog_button` reported under `record` instead of its label (task 68h-c): for a button whose words another control on
/// screen shares, such as a human-only `적용`, so that the report names each control once.
pub fn dialog_button_recorded(ui: &mut Ui, palette: &Palette, label: &str, record: &str, kind: DialogButton, enabled: bool) -> Response {
    let response = dialog_button_shape(ui, palette, label, kind, enabled);
    crate::ui_record::note(ui, record, &response);
    response
}

/// `dialog_button` without the report: `dialog_buttons` reports each choice itself (human only where it says so).
fn dialog_button_shape(ui: &mut Ui, palette: &Palette, label: &str, kind: DialogButton, enabled: bool) -> Response {
    let font = dialog_button_font(ui, kind);
    let width = dialog_button_width(ui, label, kind);
    // The colours below are the disabled look: painted without the half opacity egui gives a disabled ui, so that `ink-3`
    // keeps its 4.5:1 on `surface-2`. `add_enabled` places it in this ui (not in a scope), so that a wrapping row wraps it.
    let painter = ui.painter().clone();
    ui.add_enabled(enabled, |ui: &mut Ui| {
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, theme::CONTROL_HEIGHT), Sense::click());
        let hover = enabled && response.hovered();
        let (fill, ink, outline) = match (kind, enabled) {
            (DialogButton::Primary, true) => (if hover { palette.accent_ink } else { palette.accent }, palette.on_accent, None),
            (DialogButton::Warn, true) => (if hover { palette.warn.gamma_multiply(0.92) } else { palette.warn }, palette.on_warn, None),
            (DialogButton::Primary | DialogButton::Warn, false) => (palette.surface_2, palette.ink_3, None),
            (DialogButton::Danger, true) => (if hover { palette.danger_soft } else { palette.surface }, palette.danger, Some(palette.line_strong)),
            (DialogButton::Plain, true) => (if hover { palette.surface_2 } else { palette.surface }, palette.ink, Some(palette.line_strong)),
            (DialogButton::Danger | DialogButton::Plain, false) => (palette.surface, palette.ink_3, Some(palette.line)),
        };
        let corner = CornerRadius::same(theme::RADIUS_CONTROL);
        painter.rect_filled(rect, corner, fill);
        if let Some(outline) = outline {
            painter.rect_stroke(rect, corner, Stroke::new(1.0, outline), StrokeKind::Inside);
        }
        painter.text(rect.center(), Align2::CENTER_CENTER, label, font, ink);
        if response.has_focus() {
            painter.rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(theme::RADIUS_CONTROL + 2),
                Stroke::new(2.0, palette.accent),
                StrokeKind::Outside,
            );
        }
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
        response
    })
}

/// One row of a dialog form, centred in the dialog: a `DIALOG_FORM_WIDTH` block, 32 high, with `label` in the 76 column
/// (caption, `ink-2`, cut to fit) and `field` after it, given the width left. With an empty label the field has the block.
pub fn dialog_row<R>(ui: &mut Ui, palette: &Palette, label: &str, field: impl FnOnce(&mut Ui, f32) -> R) -> R {
    let width = DIALOG_FORM_WIDTH.min(ui.available_width());
    ui.allocate_ui_with_layout(
        egui::vec2(width, theme::PROPERTY_ROW),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(egui::vec2(width, theme::PROPERTY_ROW));
            ui.spacing_mut().item_spacing.x = DIALOG_GAP;
            let mut room = width;
            if !label.is_empty() {
                let column = theme::LABEL_COLUMN - DIALOG_GAP;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(column, theme::PROPERTY_ROW), Sense::hover());
                let shown = truncate_to_width(ui, label, column, palette.ink_2);
                ui.painter().text(Pos2::new(rect.left(), rect.center().y), Align2::LEFT_CENTER, shown, house_font(ui, theme::Type::Caption), palette.ink_2);
                room -= theme::LABEL_COLUMN;
            }
            field(ui, room)
        },
    )
    .inner
}

/// A `dropdown` whose menu lists `options` as menu rows, the current one checked. Returns the index picked this frame.
pub fn dropdown_menu(ui: &mut Ui, palette: &Palette, options: &[&str], current: Option<usize>, width: f32) -> Option<usize> {
    let shown = current.and_then(|index| options.get(index)).copied().unwrap_or_default();
    select_menu(ui, palette, shown, options, current, width)
}

/// A selection list: a `dropdown` that shows `shown` and whose menu lists `options`, the `current` one checked (task 68h: the
/// place of a combo box). Returns the index picked this frame.
pub fn select_menu(ui: &mut Ui, palette: &Palette, shown: &str, options: &[&str], current: Option<usize>, width: f32) -> Option<usize> {
    let button = dropdown(ui, palette, shown, None, width);
    let mut picked = None;
    egui::Popup::from_toggle_button_response(&button)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(width.max(200.0));
            for (index, label) in options.iter().enumerate() {
                if menu_item(ui, palette, None, label, "", Some(index) == current).clicked() {
                    picked = Some(index);
                    ui.close();
                }
            }
        });
    picked
}

/// A check box in the house style: a 16 px box (radius 4, `line-strong` border; on: `accent` with an `on-accent` check)
/// and the label after it 8 apart, body `ink`, wrapped to the width. Rows are at least 28 high (a field's height). Pressing
/// the row toggles `checked`; the focus ring is the 2 px `accent` ring 2 px outside the box (§5.4).
pub fn checkbox(ui: &mut Ui, palette: &Palette, checked: &mut bool, label: &str) -> Response {
    let width = ui.available_width().min(DIALOG_FORM_WIDTH);
    let enabled = ui.is_enabled();
    // A disabled check box paints `ink-3` and its own `line` and `surface-3`: the half opacity of a disabled ui is taken back.
    let opacity = undo_disabled_fade(ui);
    let ink = if enabled { palette.ink } else { palette.ink_3 };
    let text_left = theme::ICON_SMALL + DIALOG_GAP;
    let galley = ui.painter().layout(label.to_owned(), house_font(ui, theme::Type::Body), ink, (width - text_left).max(1.0));
    let height = (galley.size().y + 8.0).max(theme::FIELD_HEIGHT);
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    if response.clicked() {
        *checked = !*checked;
        response.mark_changed();
    }
    let square = Rect::from_min_size(Pos2::new(rect.left(), rect.top() + (theme::FIELD_HEIGHT - theme::ICON_SMALL) / 2.0), Vec2::splat(theme::ICON_SMALL));
    let corner = CornerRadius::same(theme::RADIUS_SMALL);
    if *checked {
        ui.painter().rect_filled(square, corner, if enabled { palette.accent } else { palette.surface_3 });
        icons::paint(ui, square, "check", if enabled { palette.on_accent } else { palette.ink_3 });
    } else {
        ui.painter().rect_filled(square, corner, if response.hovered() && enabled { palette.surface_2 } else { palette.surface });
        ui.painter().rect_stroke(square, corner, Stroke::new(1.0, if enabled { palette.line_strong } else { palette.line }), StrokeKind::Inside);
    }
    let text_top = rect.top() + ((theme::FIELD_HEIGHT - galley.size().y) / 2.0).max(4.0);
    ui.painter().galley(Pos2::new(rect.left() + text_left, text_top), galley, ink);
    if response.has_focus() {
        ui.painter().rect_stroke(square.expand(2.0), CornerRadius::same(theme::RADIUS_SMALL + 2), Stroke::new(2.0, palette.accent), StrokeKind::Outside);
    }
    ui.set_opacity(opacity);
    let state = *checked;
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, state, label));
    crate::ui_record::note(ui, label, &response);
    response
}

/// A 1 px `line` separator across the available width.
pub fn divider(ui: &mut Ui, palette: &Palette) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, palette.line);
}

/// A single-line field that takes all the width its button leaves, with the button at the right
/// edge. Returns true when the button is clicked, or Enter is pressed in the field, while `enabled`.
/// `focus` puts the keyboard cursor into the field.
pub fn field_with_button(
    ui: &mut Ui,
    text: &mut String,
    hint: &str,
    action: &str,
    enabled: bool,
    focus: bool,
) -> bool {
    let palette = theme::palette(ui.ctx());
    let mut submitted = false;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if dialog_button(ui, &palette, action, DialogButton::Plain, enabled).clicked() {
                submitted = true;
            }
            let width = ui.available_width().max(48.0);
            let field = edit_line(ui, &palette, text, Field { hint, record: Some(hint), width, ..Field::default() });
            if focus {
                field.request_focus();
            }
            if enabled && field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                submitted = true;
            }
        });
    });
    submitted
}

/// The least width of a `table_cell`: an empty value stays a target.
pub const CELL_MIN_WIDTH: f32 = 48.0;
/// The most width of a `table_cell`; a longer value is cut with an ellipsis.
pub const CELL_MAX_WIDTH: f32 = 120.0;
const CELL_PADDING: f32 = 6.0;

/// The header of a table column: its text 6 in from the left, as the values under it are (`table_cell`).
pub fn table_head(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(CELL_PADDING);
        ui.label(text);
    });
}

/// One cell of a data table (task 68h): the value in the body face that a label takes, 6 in from each side, the cell between
/// `CELL_MIN_WIDTH` and `CELL_MAX_WIDTH` wide and a field high. A value cut to fit shows the whole value as its tooltip. An
/// `editable` cell takes clicks and the keyboard (Tab, then Enter or Space), shows `surface-2` on hover, the 2 px `accent`
/// focus ring when it has the focus and the pointing-hand cursor. It is reported to the UI-test layer as `r{row}c{column}: value`
/// (`at`, counted from 1), since the value alone repeats across a table; the others only show the value.
pub fn table_cell(ui: &mut Ui, palette: &Palette, value: &str, color: Color32, editable: bool, at: (usize, usize)) -> Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let full = text_width(ui, value, font.clone(), color);
    let width = (full + 2.0 * CELL_PADDING).clamp(CELL_MIN_WIDTH, CELL_MAX_WIDTH);
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(width, theme::FIELD_HEIGHT), if editable { Sense::click() } else { Sense::hover() });
    if editable && response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
    }
    if editable && response.has_focus() {
        ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(theme::RADIUS_CONTROL + 2), Stroke::new(2.0, palette.accent), StrokeKind::Outside);
    }
    let room = width - 2.0 * CELL_PADDING;
    let cut = full > room;
    let shown = if cut { cut_to_width(ui, value, room, font.clone(), color) } else { value.to_owned() };
    ui.painter().text(Pos2::new(rect.left() + CELL_PADDING, rect.center().y), Align2::LEFT_CENTER, shown, font, color);
    if cut {
        response = response.on_hover_text(value);
    }
    if editable {
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
        let shown = value.to_owned();
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, shown.as_str()));
        crate::ui_record::note(ui, &format!("r{}c{}: {value}", at.0, at.1), &response);
        return response;
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_shown_without_trailing_zeros() {
        assert_eq!(format_number(16.0, 0), "16");
        assert_eq!(format_number(12.5, 2), "12.5");
        assert_eq!(format_number(0.10, 2), "0.1");
        assert_eq!(format_number(74.333, 1), "74.3");
    }

    #[test]
    fn a_typed_number_accepts_a_comma_and_rejects_text_and_infinity() {
        assert_eq!(parse_number(" 12,5 "), Some(12.5));
        assert_eq!(parse_number("-3"), Some(-3.0));
        assert_eq!(parse_number("abc"), None);
        assert_eq!(parse_number("inf"), None);
    }

    #[test]
    fn hex_colours_parse_with_or_without_the_hash_and_normalise_to_upper_case() {
        assert_eq!(parse_hex("#1e7f4f"), Some(Color32::from_rgb(0x1e, 0x7f, 0x4f)));
        assert_eq!(parse_hex("1E7F4F"), Some(Color32::from_rgb(0x1e, 0x7f, 0x4f)));
        assert_eq!(parse_hex("#12345"), None);
        assert_eq!(parse_hex("#GG0000"), None);
        assert_eq!(normalise_hex("#1e7f4f"), "#1E7F4F");
    }

    fn screen_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            events,
            ..Default::default()
        }
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    /// A press and its release in one frame (egui counts a press of a key still down as a repeat).
    fn tap(key: egui::Key) -> Vec<egui::Event> {
        let release = egui::Event::Key { key, physical_key: Some(key), pressed: false, repeat: false, modifiers: egui::Modifiers::NONE };
        vec![self::key(key), release]
    }

    /// What one frame of a test dialog gave back, where egui placed it, and the frame's output.
    struct Shown {
        response: DialogResponse<Vec<Rect>>,
        rect: Rect,
    }

    /// Runs one frame of one dialog and returns its response (the buttons' rectangles) and its area.
    fn shown(
        ctx: &egui::Context,
        place: DialogPlace,
        choices: &[DialogChoice<'_>],
        events: Vec<egui::Event>,
    ) -> (Shown, egui::FullOutput) {
        let mut last = None;
        let id = egui::Id::new(("test-dialog", format!("{place:?}")));
        let output = ctx.run(screen_input(events), |ctx| {
            let palette = theme::palette(ctx);
            last = Some(dialog(ctx, &palette, id, "제목", place, |ui| {
                ui.label("본문");
                dialog_buttons(ui, &palette, choices).iter().map(|response| response.rect).collect::<Vec<_>>()
            }));
        });
        let rect = ctx.memory(|memory| memory.area_rect(id)).expect("the dialog's area");
        (Shown { response: last.expect("the dialog was drawn"), rect }, output)
    }

    #[test]
    fn a_dialog_is_centred_on_the_screen_with_its_buttons_centred_under_the_text_in_both_forms() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let choices = [DialogChoice::new("계속 작업", DialogButton::Primary), DialogChoice::new("버리기", DialogButton::Danger)];
        for place in [DialogPlace::Screen, DialogPlace::Modal] {
            for _ in 0..2 {
                shown(&ctx, place, &choices, Vec::new());
            }
            let (Shown { response, rect }, _) = shown(&ctx, place, &choices, Vec::new());
            assert!((rect.center().x - 400.0).abs() < 1.0 && (rect.center().y - 300.0).abs() < 1.0, "{place:?}: {rect:?}");
            assert!((rect.width() - DIALOG_WIDTH).abs() < 1.0, "{place:?}: the dialog is {} wide", rect.width());
            let row = response.inner[0].union(response.inner[1]);
            assert!((row.center().x - 400.0).abs() < 1.0, "{place:?}: the buttons sit in the middle ({row:?})");
            assert!((response.inner[0].top() - response.inner[1].top()).abs() < 0.5, "one line");
        }
    }

    #[test]
    fn a_button_row_wider_than_the_dialog_wraps_and_each_line_stays_centred() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let labels = ["예약 해제 다시 시도", "초안 복사", "같은 기준으로 다시 시도", "보드 초안을 버리고 예약 만료를 기다리며 닫기"];
        let choices: Vec<DialogChoice<'_>> = labels.iter().map(|label| DialogChoice::new(label, DialogButton::Plain)).collect();
        for _ in 0..2 {
            shown(&ctx, DialogPlace::Screen, &choices, Vec::new());
        }
        let (Shown { response, rect }, _) = shown(&ctx, DialogPlace::Screen, &choices, Vec::new());
        let inside = rect.shrink(DIALOG_PADDING - 0.5);
        let mut lines: Vec<Rect> = Vec::new();
        for rect in &response.inner {
            assert!(inside.contains_rect(*rect), "{rect:?} stays inside {inside:?}");
            match lines.iter_mut().find(|line| (line.top() - rect.top()).abs() < 0.5) {
                Some(line) => *line = line.union(*rect),
                None => lines.push(*rect),
            }
        }
        assert!(lines.len() >= 2, "the row wraps: {lines:?}");
        for line in &lines {
            assert!((line.center().x - rect.center().x).abs() < 1.0, "each line is centred: {line:?} in {rect:?}");
        }
        for pair in lines.windows(2) {
            assert!(pair[1].top() >= pair[0].bottom() + DIALOG_GAP - 0.5, "lines do not overlap: {pair:?}");
        }
    }

    #[test]
    fn notices_on_a_board_hang_from_its_top_middle_and_stack_without_overlapping() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let board = Rect::from_min_max(Pos2::new(272.0, 44.0), Pos2::new(780.0, 600.0));
        let mut rects = Vec::new();
        for _ in 0..3 {
            rects.clear();
            let _ = ctx.run(screen_input(Vec::new()), |ctx| {
                let palette = theme::palette(ctx);
                for name in ["first", "second"] {
                    dialog(ctx, &palette, egui::Id::new(name), name, DialogPlace::BoardTop(board), |ui| {
                        ui.label("본문");
                        dialog_buttons(ui, &palette, &[DialogChoice::new("초안 버리기", DialogButton::Danger)]);
                    });
                }
            });
            for name in ["first", "second"] {
                rects.push(ctx.memory(|memory| memory.area_rect(egui::Id::new(name))).expect("the notice's area"));
            }
        }
        assert!((rects[0].top() - (board.top() + DIALOG_PADDING)).abs() < 1.0, "the first hangs 16 under the board's top: {rects:?}");
        assert!(rects[1].top() >= rects[0].bottom() + DIALOG_GAP - 0.5, "the second stacks under it: {rects:?}");
        for rect in &rects {
            assert!((rect.center().x - board.center().x).abs() < 1.0, "centred on the board: {rect:?}");
            assert!((rect.width() - NOTICE_WIDTH).abs() < 1.0, "{rect:?}");
        }
    }

    #[test]
    fn enter_and_escape_answer_a_dialog_but_not_in_its_first_frame_and_never_a_notice() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let choices = [DialogChoice::new("취소", DialogButton::Plain)];
        for place in [DialogPlace::Screen, DialogPlace::Modal] {
            let (first, _) = shown(&ctx, place, &choices, [tap(egui::Key::Enter), tap(egui::Key::Escape)].concat());
            assert!(!first.response.enter && !first.response.dismissed, "{place:?}: the frame it opens in takes no keys");
            let (enter, _) = shown(&ctx, place, &choices, tap(egui::Key::Enter));
            assert!(enter.response.enter && !enter.response.dismissed, "{place:?}: Enter");
            let (escape, _) = shown(&ctx, place, &choices, tap(egui::Key::Escape));
            assert!(escape.response.dismissed && !escape.response.enter, "{place:?}: Esc");
        }
        let board = DialogPlace::BoardTop(Rect::from_min_size(Pos2::new(0.0, 40.0), Vec2::new(800.0, 560.0)));
        shown(&ctx, board, &choices, Vec::new());
        let (notice, _) = shown(&ctx, board, &choices, [tap(egui::Key::Enter), tap(egui::Key::Escape)].concat());
        assert!(!notice.response.enter && !notice.response.dismissed, "a notice leaves the keys to the board");
    }

    #[test]
    fn a_disabled_main_button_drops_its_fill_to_surface_2_and_its_label_to_ink_3() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let palette = theme::palette(&ctx);
        let choices = [
            DialogChoice::new("다시 시도", DialogButton::Primary).enabled(false),
            DialogChoice::new("이 손실을 허용하고 내보내기", DialogButton::Warn).enabled(false),
        ];
        shown(&ctx, DialogPlace::Screen, &choices, Vec::new());
        let (Shown { response, .. }, output) = shown(&ctx, DialogPlace::Screen, &choices, Vec::new());
        fn walk(shape: &egui::epaint::Shape, out: &mut Vec<egui::epaint::Shape>) {
            match shape {
                egui::epaint::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, out)),
                other => out.push(other.clone()),
            }
        }
        let mut shapes = Vec::new();
        output.shapes.iter().for_each(|clipped| walk(&clipped.shape, &mut shapes));
        for rect in &response.inner {
            let fill = shapes.iter().find_map(|shape| match shape {
                egui::epaint::Shape::Rect(drawn) if drawn.rect == *rect && drawn.fill != Color32::TRANSPARENT => Some(drawn.fill),
                _ => None,
            });
            assert_eq!(fill, Some(palette.surface_2), "{rect:?}: a disabled main button is filled with surface-2");
        }
        let labels: Vec<Color32> = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::epaint::Shape::Text(text) if ["다시 시도", "이 손실을 허용하고 내보내기"].contains(&text.galley.text()) => Some(text.fallback_color),
                _ => None,
            })
            .collect();
        assert_eq!(labels, [palette.ink_3, palette.ink_3], "their labels are ink-3, not faded further");
    }

    /// Runs one frame of `add` in a central panel of an 800 × 600 screen and returns the controls it recorded and its shapes.
    fn recorded(ctx: &egui::Context, events: Vec<egui::Event>, mut add: impl FnMut(&mut Ui, &Palette)) -> (Vec<crate::ui_record::Control>, Vec<egui::epaint::Shape>) {
        crate::ui_record::begin();
        let output = ctx.run(screen_input(events), |ctx| {
            let palette = theme::palette(ctx);
            egui::CentralPanel::default().show(ctx, |ui| add(ui, &palette));
        });
        fn walk(shape: &egui::epaint::Shape, out: &mut Vec<egui::epaint::Shape>) {
            match shape {
                egui::epaint::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, out)),
                other => out.push(other.clone()),
            }
        }
        let mut shapes = Vec::new();
        output.shapes.iter().for_each(|clipped| walk(&clipped.shape, &mut shapes));
        (crate::ui_record::finish(), shapes)
    }

    #[test]
    fn a_field_is_recorded_only_under_the_label_it_is_given_never_under_its_hint() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let (controls, _) = recorded(&ctx, Vec::new(), |ui, palette| {
            let mut text = String::new();
            edit_line(ui, palette, &mut text, Field { hint: "여러 값", width: 80.0, ..Default::default() });
            edit_line(ui, palette, &mut text, Field { hint: "", width: 80.0, ..Default::default() });
            edit_area(ui, palette, &mut text, Field { hint: "여러 값", width: 80.0, ..Default::default() });
            edit_area(ui, palette, &mut text, Field { hint: "무엇을 만들까요?", record: Some("무엇을 만들까요?"), width: 200.0, ..Default::default() });
            edit_line(ui, palette, &mut text, Field { record: Some("이름"), width: 80.0, ..Default::default() });
        });
        let labels: Vec<&str> = controls.iter().map(|control| control.label.as_str()).collect();
        assert_eq!(labels, ["무엇을 만들까요?", "이름"], "a hint alone records nothing; an empty hint neither");
    }

    #[test]
    fn a_field_takes_the_height_it_is_given_and_the_field_height_otherwise() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut rects = Vec::new();
        for _ in 0..2 {
            rects.clear();
            let _ = recorded(&ctx, Vec::new(), |ui, palette| {
                let mut text = "12".to_owned();
                rects.push(edit_line(ui, palette, &mut text, Field { width: 120.0, ..Default::default() }).rect);
                let cell = Rect::from_min_size(Pos2::new(300.0, 300.0), Vec2::new(148.0, 26.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(cell));
                rects.push(edit_line(&mut child, palette, &mut text, Field { width: cell.width(), height: Some(cell.height()), margin: Some(egui::Margin::symmetric(7, 4)), ..Default::default() }).rect);
                let row = Rect::from_min_size(Pos2::new(300.0, 400.0), Vec2::new(180.0, 24.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(row));
                rects.push(edit_line(&mut child, palette, &mut text, Field { width: row.width(), height: Some(row.height()), margin: Some(egui::Margin::symmetric(4, 2)), ..Default::default() }).rect);
            });
        }
        assert!((rects[0].height() - theme::FIELD_HEIGHT).abs() < 0.5, "a framed field is the field height: {:?}", rects[0]);
        assert_eq!(rects[1], Rect::from_min_size(Pos2::new(300.0, 300.0), Vec2::new(148.0, 26.0)), "a cell editor is its cell");
        assert_eq!(rects[2], Rect::from_min_size(Pos2::new(300.0, 400.0), Vec2::new(180.0, 24.0)), "a row editor is its row's field");
    }

    /// The labels of the text shapes a frame drew.
    fn drawn_texts(shapes: &[egui::epaint::Shape]) -> Vec<String> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_disabled_checkbox_and_dropdown_paint_their_own_colours_and_leave_the_opacity_they_found() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut checked = false;
        let mut seen = Vec::new();
        let (_, shapes) = recorded(&ctx, Vec::new(), |ui, palette| {
            ui.add_enabled_ui(false, |ui| {
                let before = ui.opacity();
                checkbox(ui, palette, &mut checked, "자동");
                seen.push((before, ui.opacity()));
                let before = ui.opacity();
                dropdown(ui, palette, "단위", None, 120.0);
                seen.push((before, ui.opacity()));
            });
        });
        assert!(seen[0].0 < 1.0, "a disabled ui is faded: {seen:?}");
        assert!(seen.iter().all(|(before, after)| before == after), "each control gives the opacity back: {seen:?}");
        let palette = theme::palette(&ctx);
        let colours: Vec<Color32> = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::epaint::Shape::Text(text) if ["자동", "단위"].contains(&text.galley.text()) => Some(text.fallback_color),
                _ => None,
            })
            .collect();
        assert_eq!(colours, [palette.ink_3, palette.ink_3], "both labels are ink-3, not faded further");
    }

    #[test]
    fn a_stepper_shows_the_value_again_on_blur_after_a_text_that_is_not_a_number_or_an_empty_field() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let select_all = egui::Event::Key { key: egui::Key::A, physical_key: Some(egui::Key::A), pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND };
        let mut value = 3;
        let (controls, _) = stepper_frame(&ctx, &mut value, Vec::new());
        let minus = control(&controls, "1 줄이기").rect;
        let plus = control(&controls, "1 늘리기").rect;
        let field = Pos2::new((minus.right() + plus.left()) / 2.0, minus.center().y);
        let blur = Pos2::new(700.0, 550.0);
        for events in pointer_click(field) {
            stepper_frame(&ctx, &mut value, events);
        }
        stepper_frame(&ctx, &mut value, vec![select_all.clone(), egui::Event::Text("x".into())]);
        assert_eq!(value, 3, "text that is not a number leaves the value");
        for events in pointer_click(blur) {
            stepper_frame(&ctx, &mut value, events);
        }
        let (_, shapes) = recorded(&ctx, Vec::new(), |ui, palette| {
            number_stepper(ui, palette, &mut value, 0..=5, "열");
        });
        let texts = drawn_texts(&shapes);
        assert!(texts.iter().any(|text| text == "3") && !texts.iter().any(|text| text == "x"), "the value is shown again: {texts:?}");

        // An empty field changes nothing either, and blur shows the value again.
        for events in pointer_click(field) {
            stepper_frame(&ctx, &mut value, events);
        }
        stepper_frame(&ctx, &mut value, vec![select_all, key(egui::Key::Backspace)]);
        assert_eq!(value, 3, "an empty field leaves the value");
        for events in pointer_click(blur) {
            stepper_frame(&ctx, &mut value, events);
        }
        let (_, shapes) = recorded(&ctx, Vec::new(), |ui, palette| {
            number_stepper(ui, palette, &mut value, 0..=5, "열");
        });
        assert!(drawn_texts(&shapes).iter().any(|text| text == "3"), "the value is shown again after an empty field");
    }

    #[test]
    fn a_table_cell_is_clicked_and_activated_from_the_keyboard_and_reports_its_row_and_column() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let clicked = std::cell::Cell::new(0);
        let frame = |ctx: &egui::Context, events: Vec<egui::Event>| {
            let (controls, _) = recorded(ctx, events, |ui, palette| {
                if table_cell(ui, palette, "0.10", palette.ink, true, (2, 3)).clicked() {
                    clicked.set(clicked.get() + 1);
                }
            });
            controls
        };
        let controls = frame(&ctx, Vec::new());
        let at = controls.iter().find(|control| control.label == "r2c3: 0.10").expect("reported as r2c3: 0.10").rect;
        assert!(controls.iter().all(|control| control.label != "0.10"), "not reported under the bare value");
        for events in pointer_click(at.center()) {
            frame(&ctx, events);
        }
        assert_eq!(clicked.get(), 1, "a click opens the cell");
        frame(&ctx, tap(egui::Key::Tab));
        frame(&ctx, Vec::new());
        frame(&ctx, tap(egui::Key::Enter));
        assert_eq!(clicked.get(), 2, "Enter on the focused cell opens it");
    }

    fn pointer_click(at: Pos2) -> [Vec<egui::Event>; 3] {
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        [vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)]]
    }

    /// One frame of a stepper on `value` (range 0..=5). Returns its controls, what the call returned, and the field's place.
    fn stepper_frame(ctx: &egui::Context, value: &mut u32, events: Vec<egui::Event>) -> (Vec<crate::ui_record::Control>, bool) {
        let mut changed = false;
        let (controls, _) = recorded(ctx, events, |ui, palette| {
            changed = number_stepper(ui, palette, value, 0..=5, "열");
        });
        (controls, changed)
    }

    fn control<'a>(controls: &'a [crate::ui_record::Control], label: &str) -> &'a crate::ui_record::Control {
        controls.iter().find(|control| control.label == label).unwrap_or_else(|| panic!("{label} is drawn: {controls:?}"))
    }

    #[test]
    fn the_stepper_keeps_its_range_with_the_buttons_and_a_typed_number() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut value = 9;
        let (controls, changed) = stepper_frame(&ctx, &mut value, Vec::new());
        assert_eq!((value, changed), (5, true), "a value above the range is brought into it");
        let minus = control(&controls, "1 줄이기").rect;
        let plus = control(&controls, "1 늘리기").rect;
        assert!(minus.right() < plus.left() && (minus.height() - 24.0).abs() < 0.5, "{minus:?} {plus:?}");

        let click = |ctx: &egui::Context, value: &mut u32, at: Pos2| {
            let mut changed = false;
            for events in pointer_click(at) {
                changed |= stepper_frame(ctx, value, events).1;
            }
            changed
        };
        assert!(!click(&ctx, &mut value, plus.center()), "+ at the top of the range changes nothing");
        assert_eq!(value, 5);
        assert!(click(&ctx, &mut value, minus.center()));
        assert_eq!(value, 4, "- takes one off");
        value = 0;
        stepper_frame(&ctx, &mut value, Vec::new());
        assert!(!click(&ctx, &mut value, minus.center()), "- at the bottom of the range changes nothing");
        assert_eq!(value, 0);

        // The field between the buttons takes a typed number; one outside the range is clamped, text is ignored.
        let field = Pos2::new((minus.right() + plus.left()) / 2.0, minus.center().y);
        click(&ctx, &mut value, field);
        let select_all = egui::Event::Key { key: egui::Key::A, physical_key: Some(egui::Key::A), pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND };
        stepper_frame(&ctx, &mut value, vec![select_all.clone(), egui::Event::Text("3".into())]);
        assert_eq!(value, 3, "a typed number is the value");
        stepper_frame(&ctx, &mut value, vec![select_all.clone(), egui::Event::Text("x".into())]);
        assert_eq!(value, 3, "text that is not a number leaves the value");
        stepper_frame(&ctx, &mut value, vec![select_all, egui::Event::Text("42".into())]);
        assert_eq!(value, 5, "a number above the range is clamped");
        assert!(click(&ctx, &mut value, minus.center()), "- after typing");
        assert_eq!(value, 4);
        assert_eq!(parse_whole_number(" 7 "), Some(7));
        assert_eq!(parse_whole_number("-1"), None);
        assert_eq!(parse_whole_number("99999999999"), Some(u32::MAX), "a number past u32 is the largest one, clamped by the caller");
    }

    #[test]
    fn tab_reaches_the_stepper_buttons_and_its_field() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut value = 2;
        let (controls, _) = stepper_frame(&ctx, &mut value, Vec::new());
        let minus = control(&controls, "1 줄이기").id;
        let plus = control(&controls, "1 늘리기").id;
        let mut order = Vec::new();
        for _ in 0..3 {
            stepper_frame(&ctx, &mut value, tap(egui::Key::Tab));
            stepper_frame(&ctx, &mut value, Vec::new());
            order.push(ctx.memory(|memory| memory.focused()));
        }
        assert_eq!(order[0], Some(minus), "Tab focuses the minus button first: {order:?}");
        assert!(order[1].is_some() && order[1] != Some(minus) && order[1] != Some(plus), "then the field: {order:?}");
        assert_eq!(order[2], Some(plus), "then the plus button: {order:?}");
        // Space presses the focused button.
        let (_, changed) = stepper_frame(&ctx, &mut value, tap(egui::Key::Space));
        assert!(changed && value == 3, "Space on + adds one: {value}");
    }

    #[test]
    fn a_disabled_house_button_paints_ink_3_without_the_disabled_fade() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let palette = theme::palette(&ctx);
        let mut main = Rect::NOTHING;
        let (_, shapes) = recorded(&ctx, Vec::new(), |ui, palette| {
            ui.add_enabled_ui(false, |ui| {
                labeled_button(ui, palette, None, "토큰 저장", false);
                main = labeled_button(ui, palette, None, "연결 적용", true).rect;
                compact_button(ui, palette, "빼기");
            });
        });
        let labels: Vec<(String, Color32)> = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::epaint::Shape::Text(text) => Some((text.galley.text().to_owned(), text.fallback_color)),
                _ => None,
            })
            .collect();
        for label in ["토큰 저장", "연결 적용", "빼기"] {
            let colour = labels.iter().find(|(text, _)| text == label).map(|(_, colour)| *colour);
            assert_eq!(colour, Some(palette.ink_3), "{label}: ink-3 at full strength, {labels:?}");
        }
        let fills: Vec<Color32> = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::epaint::Shape::Rect(rect) if rect.rect == main && rect.fill != Color32::TRANSPARENT => Some(rect.fill),
                _ => None,
            })
            .collect();
        assert_eq!(fills, [palette.surface_2], "the disabled main button is surface-2 at full strength");
    }

    /// Runs one frame with `add` in a column `width` wide and returns what `add` gave back and the controls it reported.
    fn in_column<R>(ctx: &egui::Context, width: f32, add: impl FnOnce(&mut Ui, &Palette) -> R) -> (R, Vec<crate::ui_record::Control>) {
        let mut add = Some(add);
        let mut result = None;
        crate::ui_record::begin();
        let _ = ctx.run(screen_input(Vec::new()), |ctx| {
            let palette = theme::palette(ctx);
            egui::CentralPanel::default().show(ctx, |ui| {
                let column = Rect::from_min_size(ui.max_rect().min, Vec2::new(width, 400.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(column).layout(egui::Layout::top_down(egui::Align::Min)));
                if let Some(add) = add.take() {
                    result = Some(add(&mut child, &palette));
                }
            });
        });
        (result.expect("the frame was drawn"), crate::ui_record::finish())
    }

    fn house_context() -> egui::Context {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        ctx
    }

    #[test]
    fn a_select_menu_shows_a_value_that_is_not_one_of_its_options_and_picks_nothing_by_itself() {
        let ctx = house_context();
        let (picked, controls) = in_column(&ctx, 300.0, |ui, palette| select_menu(ui, palette, "지워진 근거", &["근거 없음", "근거 1"], None, 160.0));
        assert_eq!(picked, None);
        assert_eq!(controls.iter().map(|control| control.label.as_str()).collect::<Vec<_>>(), ["지워진 근거"], "the box shows the value it was given");
        let (_, controls) = in_column(&ctx, 300.0, |ui, palette| dropdown_menu(ui, palette, &["근거 없음", "근거 1"], Some(1), 160.0));
        assert_eq!(controls[0].label, "근거 1", "dropdown_menu shows the current option");
        assert_eq!(controls[0].rect.width(), 160.0);
    }

    #[test]
    fn edit_line_takes_the_id_it_is_given() {
        let ctx = house_context();
        let id = egui::Id::new(("dataset-cell", "ds-1", "r1", "c1"));
        let mut text = "1".to_owned();
        let (response, controls) = in_column(&ctx, 300.0, |ui, palette| edit_line(ui, palette, &mut text, Field { id: Some(id), width: 120.0, ..Field::default() }));
        assert_eq!(response.id, id, "the caret and the IME find the field by this id");
        assert!(controls.is_empty(), "a field without a hint is not reported under an empty label");
    }

    #[test]
    fn a_table_cell_keeps_between_its_least_and_most_width_and_only_an_editable_cell_is_reported() {
        let ctx = house_context();
        let long = "세 번 반복해 잰 대조군의 평균값과 표준편차";
        let (widths, controls) = in_column(&ctx, 400.0, |ui, palette| {
            [("", true), ("12.5", true), (long, true), (long, false)]
                .iter()
                .enumerate()
                .map(|(column, (value, editable))| table_cell(ui, palette, value, palette.ink, *editable, (1, column + 1)).rect.width())
                .collect::<Vec<_>>()
        });
        assert_eq!(widths[0], CELL_MIN_WIDTH, "an empty cell keeps a target");
        assert_eq!(widths[1], CELL_MIN_WIDTH, "a short value takes the least width");
        assert_eq!(widths[2], CELL_MAX_WIDTH, "a long value is cut at the most width");
        assert_eq!(widths[3], CELL_MAX_WIDTH, "a read-only cell has the same width");
        let labels: Vec<&str> = controls.iter().map(|control| control.label.as_str()).collect();
        assert_eq!(labels, ["r1c1: ", "r1c2: 12.5", "r1c3: 세 번 반복해 잰 대조군의 평균값과 표준편차"], "only the editable cells are controls, reported as r<row>c<column>: value");
    }

    #[test]
    fn a_dialog_button_is_reported_once_and_wraps_in_a_wrapping_row() {
        let ctx = house_context();
        let (rects, controls) = in_column(&ctx, 200.0, |ui, palette| {
            ui.horizontal_wrapped(|ui| ["다시 넣기", "복사", "버리기"].map(|label| dialog_button(ui, palette, label, DialogButton::Plain, label != "버리기").rect)).inner
        });
        assert_eq!(controls.iter().map(|control| control.label.as_str()).collect::<Vec<_>>(), ["다시 넣기", "복사", "버리기"]);
        assert!(!controls[2].enabled, "a disabled button is reported disabled");
        let left = rects[0].left();
        assert!(rects.iter().all(|rect| rect.right() <= left + 200.0 + 0.5), "{rects:?} run past the row");
        assert!(rects[2].top() > rects[0].top(), "the third button goes to the next line");
        let (_, controls) = in_column(&ctx, 400.0, |ui, palette| dialog_buttons(ui, palette, &[DialogChoice::new("확인", DialogButton::Primary)]));
        assert_eq!(controls.len(), 1, "dialog_buttons reports each choice once");
    }

    #[test]
    fn a_field_with_a_button_is_the_house_field_and_button_inside_its_row() {
        let ctx = house_context();
        let mut text = String::new();
        let (_, controls) = in_column(&ctx, 260.0, |ui, _| field_with_button(ui, &mut text, "새 값", "적용", true, false));
        let labels: Vec<&str> = controls.iter().map(|control| control.label.as_str()).collect();
        assert_eq!(labels, ["적용", "새 값"]);
        assert_eq!(controls[0].rect.height(), theme::CONTROL_HEIGHT, "the button is a dialog button");
        assert_eq!(controls[1].rect.height(), theme::FIELD_HEIGHT, "the field is a house field");
        assert!(controls.iter().all(|control| control.rect.right() <= controls[0].rect.right() + 0.5));
    }
}
