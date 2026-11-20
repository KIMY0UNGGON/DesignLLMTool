//! The controls of the new shell (GUI_DESIGN §15 step 3). Every add or design button shows an icon
//! and a label (rule L1); icon-only buttons are for the small floating groups and the top bar
//! (rules L2 and L4) and always carry a tooltip with their name and shortcut.

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
    let ink = if primary { palette.on_accent } else { palette.ink };
    let label_width = text_width(ui, label, font.clone(), ink);
    let left = if icon.is_some() { 8.0 } else { 10.0 };
    let icon_width = if icon.is_some() { theme::ICON_SMALL + 6.0 } else { 0.0 };
    let dot_width = if dot { 4.0 + 6.0 } else { 0.0 };
    let width = left + icon_width + label_width + dot_width + 10.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, theme::CONTROL_HEIGHT), Sense::click());
    let pressed = response.is_pointer_button_down_on();
    let fill = if primary {
        Some(if pressed || response.hovered() { palette.accent_ink } else { palette.accent })
    } else if pressed {
        Some(palette.surface_3)
    } else if open || response.hovered() {
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
        icons::paint(ui, icon_rect, icon, if primary { palette.on_accent } else { palette.ink_2 });
        x += theme::ICON_SMALL + 6.0;
    }
    ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, label, font, ink);
    if dot {
        ui.painter().circle_filled(Pos2::new(x + label_width + 4.0 + 3.0, rect.center().y), 3.0, palette.accent);
    }
    response
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
    response
}

/// The 14 × 32 chevron half of a split tool button (tokens `component.split-button`).
pub fn split_chevron(ui: &mut Ui, palette: &Palette, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(theme::SPLIT_CHEVRON_WIDTH, theme::ICON_BUTTON), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_MINI), palette.surface_2);
    }
    icons::paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(theme::ICON_SMALL)), "chev-down", palette.ink_3);
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
    response
}

/// A field-like button that shows a value and a chevron. The caller opens its menu from the
/// response with `egui::Popup::from_toggle_button_response`.
pub fn dropdown(ui: &mut Ui, palette: &Palette, value: &str, icon: Option<&str>, width: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, theme::FIELD_HEIGHT), Sense::click());
    let fill = if response.hovered() { palette.surface_2 } else { palette.surface };
    let corner = CornerRadius::same(theme::RADIUS_CONTROL);
    ui.painter().rect_filled(rect, corner, fill);
    ui.painter().rect_stroke(rect, corner, Stroke::new(1.0, palette.line_strong), StrokeKind::Inside);
    let mut x = rect.left() + 8.0;
    if let Some(icon) = icon {
        let icon_rect = Rect::from_min_size(Pos2::new(x, rect.center().y - 8.0), Vec2::splat(theme::ICON_SMALL));
        icons::paint(ui, icon_rect, icon, palette.ink_2);
        x += theme::ICON_SMALL + 6.0;
    }
    let chevron = Rect::from_min_size(
        Pos2::new(rect.right() - 22.0, rect.center().y - 8.0),
        Vec2::splat(theme::ICON_SMALL),
    );
    icons::paint(ui, chevron, "chev-down", palette.ink_3);
    let room = (rect.right() - 26.0 - x).max(0.0);
    let shown = truncate_to_width(ui, value, room, palette.ink);
    ui.painter().text(
        Pos2::new(x, rect.center().y),
        Align2::LEFT_CENTER,
        shown,
        theme::display_font(false, 13.0),
        palette.ink,
    );
    response
}

/// Cuts `text` with an ellipsis so that it fits `width` points.
pub fn truncate_to_width(ui: &Ui, text: &str, width: f32, color: Color32) -> String {
    let font = theme::display_font(false, 13.0);
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
    ui.add_sized(
        egui::vec2(width, theme::FIELD_HEIGHT),
        TextEdit::singleline(text)
            .hint_text(egui::RichText::new(hint).color(palette.ink_3))
            .font(theme::display_font(false, 13.0))
            .margin(egui::Margin::symmetric(8, 4)),
    )
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
    let mut submitted = false;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add_enabled(enabled, egui::Button::new(action)).clicked() {
                submitted = true;
            }
            let width = ui.available_width().max(48.0);
            let field = ui.add(TextEdit::singleline(text).hint_text(hint).desired_width(width));
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
}
