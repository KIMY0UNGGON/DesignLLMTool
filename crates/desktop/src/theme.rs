//! Design tokens. The source of truth is `design/tokens.json` (the lead designer's file); every value
//! here is that file's value, and `tests::every_token_matches_design_tokens_json` fails when one drifts.
//! Colours come in a light and a dark palette; the app fixes the light one unless the system is dark.
//! Sizes are logical points (1.0 scale).
//!
//! Accessors for the other screens: `palette(ctx)` (colours, plus `Palette::derived(..)` for the alpha
//! tokens), `font(Type::Body)` (the type scale), `shadow(ctx, Elevation::Float)` (two layers), and the
//! size constants below (`TOP_BAR_HEIGHT`, `LAYER_ROW`, `RADIUS_*`, `TOOLS_*`, …).
//!
//! The whole token file is mirrored here, also the tokens no screen draws yet (some are for the home and
//! the properties panel, some for P2), so the token test can hold every value to the file.
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::Arc;

use eframe::egui::{
    self, Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin,
    Shadow, Stroke, Style, TextStyle, Theme,
};

/// One colour set. Field names follow the token names in §5.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub board: Color32,
    pub home: Color32,
    pub surface: Color32,
    pub surface_2: Color32,
    pub surface_3: Color32,
    pub line: Color32,
    pub line_strong: Color32,
    pub ink: Color32,
    pub ink_2: Color32,
    pub ink_3: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub accent_ink: Color32,
    pub danger: Color32,
    pub danger_soft: Color32,
    pub warn: Color32,
    pub warn_soft: Color32,
    pub star: Color32,
    pub agents: [Color32; 4],
    /// Text and icons on an accent, agent or warning fill (white in light, dark ink in dark).
    pub on_accent: Color32,
    pub on_agent: Color32,
    pub on_warn: Color32,
    /// Input fields.
    pub field: Color32,
    /// The raised choice of a segmented control.
    pub raised: Color32,
    /// Selection handles.
    pub handle: Color32,
    /// Artboards and pages: their content keeps its own colours in both themes.
    pub artboard: Color32,
    /// The inner ring of a colour swatch.
    pub swatch_ring: Color32,
}

/// The alpha tokens of `color.derived`: a base colour at an opacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Derived {
    /// Home: the border of the chosen tile.
    TileOnBorder,
    /// Layers: a row under the selected one.
    RowDescendant,
    /// The selection's parent frame (dashed).
    ParentFrame,
    /// The ring of a refusal callout and its buttons.
    DangerRing,
    /// AI box: the border of an intent toggle that is on.
    ToggleOnBorder,
    /// Selected text.
    TextSelection,
    /// The ring of a design-warning marker, its callout and their buttons.
    WarnRing,
}

impl Derived {
    pub const ALL: [Derived; 7] =
        [Self::TileOnBorder, Self::RowDescendant, Self::ParentFrame, Self::DangerRing, Self::ToggleOnBorder, Self::TextSelection, Self::WarnRing];

    /// The token name in `design/tokens.json`.
    pub fn name(self) -> &'static str {
        match self {
            Self::TileOnBorder => "tile-on-border",
            Self::RowDescendant => "row-descendant",
            Self::ParentFrame => "parent-frame",
            Self::DangerRing => "danger-ring",
            Self::ToggleOnBorder => "toggle-on-border",
            Self::TextSelection => "text-selection",
            Self::WarnRing => "warn-ring",
        }
    }

    fn alpha(self) -> f32 {
        match self {
            Self::TileOnBorder => 0.55,
            Self::RowDescendant | Self::ParentFrame => 0.45,
            Self::DangerRing | Self::WarnRing => 0.24,
            Self::ToggleOnBorder => 0.35,
            Self::TextSelection => 0.25,
        }
    }
}

/// An agent colour at the opacity of one of the agent alpha tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentAlpha {
    /// The track circle under a running avatar ring.
    RingTrack,
    /// The rest of the current part of the AI progress rail.
    RailRest,
    /// The tint of a node an agent just changed.
    Arrival,
}

impl AgentAlpha {
    pub fn name(self) -> &'static str {
        match self {
            Self::RingTrack => "ring-track",
            Self::RailRest => "rail-rest",
            Self::Arrival => "arrival",
        }
    }

    fn alpha(self) -> f32 {
        match self {
            Self::RingTrack => 0.18,
            Self::RailRest => 0.28,
            Self::Arrival => 0.12,
        }
    }
}

/// `color` with opacity `alpha` (not premultiplied in the token file).
pub fn with_alpha(color: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), (alpha.clamp(0.0, 1.0) * 255.0).round() as u8)
}

impl Palette {
    /// One of the derived alpha tokens.
    pub fn derived(&self, token: Derived) -> Color32 {
        let base = match token {
            Derived::TileOnBorder | Derived::ParentFrame | Derived::ToggleOnBorder | Derived::TextSelection => self.accent,
            Derived::RowDescendant => self.accent_soft,
            Derived::DangerRing => self.danger,
            Derived::WarnRing => self.warn,
        };
        with_alpha(base, token.alpha())
    }

    /// An agent colour at one of the agent alpha tokens.
    pub fn agent_alpha(&self, agent: Color32, token: AgentAlpha) -> Color32 {
        with_alpha(agent, token.alpha())
    }

    /// The colour of the agent that connected `index`-th (0-based); the fifth repeats the first.
    pub fn agent(&self, index: usize) -> Color32 {
        self.agents[index % self.agents.len()]
    }
}

pub const LIGHT: Palette = Palette {
    board: rgb(0xE9EBEF),
    home: rgb(0xF6F7F9),
    surface: rgb(0xFFFFFF),
    surface_2: rgb(0xF3F4F6),
    surface_3: rgb(0xE9ECF0),
    line: rgb(0xE2E5EA),
    line_strong: rgb(0xCDD2D9),
    ink: rgb(0x161B26),
    ink_2: rgb(0x4A5366),
    ink_3: rgb(0x687080),
    accent: rgb(0x2F64E0),
    accent_soft: rgb(0xE8EEFC),
    accent_ink: rgb(0x1F4BC0),
    danger: rgb(0xC9342B),
    danger_soft: rgb(0xFDECEA),
    warn: rgb(0x9A5B00),
    warn_soft: rgb(0xFFF3DC),
    star: rgb(0xC98A00),
    agents: [rgb(0x7A49D8), rgb(0x087A6C), rgb(0xC2327E), rgb(0x4B6B00)],
    on_accent: rgb(0xFFFFFF),
    on_agent: rgb(0xFFFFFF),
    on_warn: rgb(0xFFFFFF),
    field: rgb(0xFFFFFF),
    raised: rgb(0xFFFFFF),
    handle: rgb(0xFFFFFF),
    artboard: rgb(0xFFFFFF),
    swatch_ring: rgba(0x0A101C24),
};

pub const DARK: Palette = Palette {
    board: rgb(0x17191E),
    home: rgb(0x1B1D22),
    surface: rgb(0x22252C),
    surface_2: rgb(0x2B2F37),
    surface_3: rgb(0x343943),
    line: rgb(0x343842),
    line_strong: rgb(0x474D59),
    ink: rgb(0xE9ECF1),
    ink_2: rgb(0xA9B0BD),
    ink_3: rgb(0x9098A7),
    accent: rgb(0x5B8CFF),
    accent_soft: rgb(0x24345C),
    accent_ink: rgb(0xA9C2FF),
    danger: rgb(0xFF6B5E),
    danger_soft: rgb(0x3A1F1E),
    warn: rgb(0xE6A23C),
    warn_soft: rgb(0x3A2C12),
    star: rgb(0xE6B422),
    agents: [rgb(0xA78BFA), rgb(0x2DD4BF), rgb(0xF472B6), rgb(0xA3C94A)],
    on_accent: rgb(0x17191E),
    on_agent: rgb(0x17191E),
    on_warn: rgb(0x17191E),
    field: rgb(0x22252C),
    raised: rgb(0x343943),
    handle: rgb(0xFFFFFF),
    artboard: rgb(0xFFFFFF),
    swatch_ring: rgba(0xFFFFFF29),
};

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `0xRRGGBBAA` with a straight (not premultiplied) alpha.
const fn rgba(hex: u32) -> Color32 {
    Color32::from_rgba_unmultiplied_const((hex >> 24) as u8, (hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The palette in effect for this frame.
pub fn palette(ctx: &Context) -> Palette {
    if ctx.theme() == Theme::Dark {
        DARK
    } else {
        LIGHT
    }
}

// Sizes (logical points, §4, §5.3, §5.6).
pub const TOP_BAR_HEIGHT: f32 = 44.0;
pub const SIDEBAR_WIDTH: f32 = 272.0;
pub const SIDEBAR_MIN: f32 = 240.0;
pub const SIDEBAR_MAX: f32 = 420.0;
pub const PANEL_WIDTH: f32 = 320.0;
pub const PANEL_MIN: f32 = 280.0;
pub const PANEL_MAX: f32 = 420.0;
/// Height of icon+label buttons (touch-friendly minimum is 28).
pub const CONTROL_HEIGHT: f32 = 32.0;
pub const FIELD_HEIGHT: f32 = 28.0;
pub const PROPERTY_ROW: f32 = 32.0;
pub const LABEL_COLUMN: f32 = 76.0;
pub const LAYER_ROW: f32 = 28.0;
pub const ICON_BUTTON: f32 = 32.0;
pub const ICON_IN_BUTTON: f32 = 24.0;
pub const ICON_SMALL: f32 = 16.0;

/// Colours the figure forms and the region outline still use (light values). New code uses `Palette`.
pub const INK: Color32 = LIGHT.ink;
pub const MUTED: Color32 = LIGHT.ink_3;
pub const TEAL: Color32 = LIGHT.agents[1];
pub const RED: Color32 = LIGHT.danger;

pub const RADIUS_SMALL: u8 = 4;
pub const RADIUS_MINI: u8 = 5;
pub const RADIUS_CONTROL: u8 = 6;
pub const RADIUS_HOME_TAB: u8 = 7;
pub const RADIUS_CHIP: u8 = 8;
pub const RADIUS_MENU_ITEM: u8 = 8;
pub const RADIUS_FLOAT: u8 = 10;
pub const RADIUS_MENU: u8 = 10;
pub const RADIUS_TOOLBAR: u8 = 12;
pub const RADIUS_BOX: u8 = 14;

// Editor frame (tokens `layout`).
pub const SIDEBAR_TABS_HEIGHT: f32 = 40.0;
pub const SIDEBAR_TAB_HEIGHT: f32 = 30.0;
pub const SIDEBAR_TAB_PADDING_X: f32 = 10.0;
pub const TREE_PADDING: f32 = 6.0;
pub const PANEL_TABS_HEIGHT: f32 = 44.0;
pub const SECTION_HEADER: f32 = 34.0;
pub const SECTION_HEADER_SHORT: f32 = 30.0;
/// Floating groups on the board.
pub const FLOAT_INSET: f32 = 12.0;
pub const VIEW_GROUP_HEIGHT: f32 = 36.0;
pub const VIEW_GROUP_PADDING: f32 = 1.0;
pub const TOOLS_HEIGHT: f32 = 42.0;
pub const TOOLS_PADDING: f32 = 4.0;
pub const TOOLS_BOTTOM: f32 = 16.0;
pub const GROUP_GAP: f32 = 2.0;
pub const SEPARATOR_HEIGHT: f32 = 20.0;
pub const SEPARATOR_MARGIN_X: f32 = 4.0;
pub const SPLIT_CHEVRON_WIDTH: f32 = 14.0;
pub const MINI_BUTTON: f32 = 24.0;
pub const ARTBOARD_TITLE_HEIGHT: f32 = 20.0;
pub const ARTBOARD_TITLE_GAP: f32 = 4.0;
pub const MENU_ITEM_HEIGHT: f32 = 30.0;
pub const MENU_PADDING: f32 = 4.0;
pub const EXPORT_WIDTH: f32 = 320.0;

/// Hairline in physical pixels: one device pixel expressed in logical points.
pub fn hairline(ctx: &Context) -> f32 {
    1.0 / ctx.pixels_per_point().max(1.0)
}

/// The type scale (tokens `type`): size, line height and weight of each role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    AvatarSmall,
    Micro,
    Tag,
    Avatar,
    Caption,
    CaptionStrong,
    BoardTitle,
    Body,
    ChipValue,
    Long,
    Strong,
    Title,
    Prompt,
}

impl Type {
    pub const ALL: [Type; 13] = [
        Self::AvatarSmall, Self::Micro, Self::Tag, Self::Avatar, Self::Caption, Self::CaptionStrong, Self::BoardTitle,
        Self::Body, Self::ChipValue, Self::Long, Self::Strong, Self::Title, Self::Prompt,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::AvatarSmall => "avatar-sm",
            Self::Micro => "micro",
            Self::Tag => "tag",
            Self::Avatar => "avatar",
            Self::Caption => "caption",
            Self::CaptionStrong => "caption-strong",
            Self::BoardTitle => "board-title",
            Self::Body => "body",
            Self::ChipValue => "chip-value",
            Self::Long => "long",
            Self::Strong => "strong",
            Self::Title => "title",
            Self::Prompt => "prompt",
        }
    }

    /// (size, line height, bold).
    pub fn metrics(self) -> (f32, f32, bool) {
        match self {
            Self::AvatarSmall => (10.0, 10.0, true),
            Self::Micro => (11.0, 14.0, false),
            Self::Tag => (11.0, 18.0, true),
            Self::Avatar => (11.0, 11.0, true),
            Self::Caption => (12.0, 16.0, false),
            Self::CaptionStrong => (12.0, 16.0, true),
            Self::BoardTitle => (12.0, 20.0, false),
            Self::Body => (13.0, 18.0, false),
            Self::ChipValue => (13.0, 17.0, false),
            Self::Long => (13.0, 20.0, false),
            Self::Strong => (13.0, 18.0, true),
            Self::Title => (14.0, 20.0, true),
            Self::Prompt => (15.0, 22.0, false),
        }
    }
}

/// The font of a type role.
pub fn font(role: Type) -> FontId {
    let (size, _, bold) = role.metrics();
    display_font(bold, size)
}

/// Shadow sets of `design/tokens.json` (`shadow`): two layers each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    Float,
    Menu,
    RequestBox,
    Artboard,
}

impl Elevation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Float => "float",
            Self::Menu => "menu",
            Self::RequestBox => "request-box",
            Self::Artboard => "artboard",
        }
    }

    /// (x, y, blur, spread, colour) of both layers in a theme.
    pub fn layers(self, dark: bool) -> [(i8, i8, u8, u8, Color32); 2] {
        match (self, dark) {
            (Self::Float, false) => [(0, 1, 2, 0, rgba(0x1016220F)), (0, 4, 14, 0, rgba(0x10162214))],
            (Self::Menu, false) => [(0, 2, 4, 0, rgba(0x1016220F)), (0, 10, 28, 0, rgba(0x10162224))],
            (Self::RequestBox, false) => [(0, 1, 2, 0, rgba(0x1016220D)), (0, 8, 28, 0, rgba(0x10162214))],
            (Self::Artboard, false) => [(0, 0, 0, 1, rgba(0x141A260F)), (0, 1, 3, 0, rgba(0x141A2614))],
            (Self::Float, true) => [(0, 1, 2, 0, rgba(0x0000004D)), (0, 4, 14, 0, rgba(0x00000066))],
            (Self::Menu, true) => [(0, 2, 4, 0, rgba(0x0000004D)), (0, 10, 28, 0, rgba(0x00000080))],
            (Self::RequestBox, true) => [(0, 1, 2, 0, rgba(0x00000040)), (0, 8, 28, 0, rgba(0x00000066))],
            (Self::Artboard, true) => [(0, 0, 0, 1, rgba(0x00000066)), (0, 1, 3, 0, rgba(0x00000066))],
        }
    }
}

/// Both layers of a shadow for the theme in effect, as egui shadows.
pub fn shadow(ctx: &Context, elevation: Elevation) -> [Shadow; 2] {
    elevation
        .layers(ctx.theme() == Theme::Dark)
        .map(|(x, y, blur, spread, color)| Shadow { offset: [x, y], blur, spread, color })
}

/// Paints both layers of `elevation` under `rect` (a Frame takes only one shadow).
pub fn paint_shadow(painter: &egui::Painter, rect: egui::Rect, corner: CornerRadius, elevation: Elevation) {
    for layer in shadow(painter.ctx(), elevation) {
        painter.add(layer.as_shape(rect, corner));
    }
}

/// The shadow of a floating group (both layers).
pub fn float_shadow() -> Vec<Shadow> {
    Elevation::Float.layers(false).map(|(x, y, blur, spread, color)| Shadow { offset: [x, y], blur, spread, color }).to_vec()
}

pub fn install(ctx: &Context) {
    let regular = "pretendard-regular";
    let bold = "pretendard-bold";
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        regular.into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../../assets/fonts/Pretendard-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        bold.into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../../assets/fonts/Pretendard-Bold.ttf"
        ))),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, regular.into());
    fonts.families.insert(
        FontFamily::Name("Pretendard Regular".into()),
        vec![regular.into()],
    );
    fonts.families.insert(
        FontFamily::Name("Pretendard Bold".into()),
        vec![bold.into()],
    );
    ctx.set_fonts(fonts);

    // `Context::set_style` writes the style of the theme that is active right now, which before
    // the first frame is the fallback (dark) one. Fix the theme and set both slots so that the
    // palette applies whatever the system reports.
    ctx.set_theme(Theme::Light);
    ctx.set_style_of(Theme::Light, Arc::new(build_style(&LIGHT, false)));
    ctx.set_style_of(Theme::Dark, Arc::new(build_style(&DARK, true)));
}

fn build_style(palette: &Palette, dark: bool) -> Style {
    let mut style = Style::default();
    style.visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let visuals = &mut style.visuals;
    visuals.override_text_color = Some(palette.ink);
    visuals.panel_fill = palette.surface;
    visuals.window_fill = palette.surface;
    visuals.window_stroke = Stroke::new(1.0, palette.line);
    visuals.window_corner_radius = CornerRadius::same(RADIUS_FLOAT);
    // egui takes one shadow per window or popup: the larger layer of `menu`.
    let [_, menu] = Elevation::Menu.layers(dark);
    let menu_shadow = Shadow { offset: [menu.0, menu.1], blur: menu.2, spread: menu.3, color: menu.4 };
    visuals.window_shadow = menu_shadow;
    visuals.popup_shadow = menu_shadow;
    visuals.menu_corner_radius = CornerRadius::same(RADIUS_MENU);
    // Text fields are white with a visible border (`line-strong`), so they read as fields.
    visuals.extreme_bg_color = palette.field;
    visuals.text_cursor.stroke = Stroke::new(1.5, palette.ink);
    visuals.selection.bg_fill = palette.derived(Derived::TextSelection);
    visuals.selection.stroke = Stroke::new(1.0, palette.accent);
    visuals.hyperlink_color = palette.accent;
    visuals.faint_bg_color = palette.surface_2;
    let radius = CornerRadius::same(RADIUS_CONTROL);
    let widgets = &mut visuals.widgets;
    for (state, fill, stroke) in [
        (&mut widgets.noninteractive, palette.surface, palette.line),
        (&mut widgets.inactive, palette.surface, palette.line_strong),
        (&mut widgets.hovered, palette.surface_2, palette.accent),
        (&mut widgets.active, palette.surface_3, palette.accent),
        (&mut widgets.open, palette.surface_2, palette.line_strong),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0, stroke);
        state.corner_radius = radius;
        state.fg_stroke = Stroke::new(1.0, palette.ink);
        state.expansion = 0.0;
    }
    style.spacing.item_spacing = egui::vec2(6.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.window_margin = Margin::same(8);
    style.spacing.menu_margin = Margin::same(4);
    style.spacing.interact_size.y = FIELD_HEIGHT;
    style.spacing.scroll.bar_width = 8.0;
    for (text_style, size, family) in [
        (TextStyle::Body, 13.0, FontFamily::Proportional),
        (TextStyle::Button, 13.0, FontFamily::Proportional),
        (TextStyle::Small, 12.0, FontFamily::Proportional),
        (TextStyle::Heading, 14.0, FontFamily::Name("Pretendard Bold".into())),
        (TextStyle::Monospace, 12.0, FontFamily::Monospace),
    ] {
        style.text_styles.insert(text_style, FontId::new(size, family));
    }
    style
}

/// A font for the given weight. Only two weights are bundled (Regular, Bold); hierarchy comes
/// from size and colour (§5.2).
pub fn display_font(bold: bool, size: f32) -> FontId {
    let family = if bold {
        FontFamily::Name("Pretendard Bold".into())
    } else {
        FontFamily::Name("Pretendard Regular".into())
    };
    FontId::new(size, family)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{RawInput, Visuals};

    fn style_in_effect(ctx: &Context, system: Option<Theme>) -> Arc<Style> {
        let input = RawInput {
            system_theme: system,
            ..Default::default()
        };
        let _ = ctx.run(input, |_| {});
        ctx.style()
    }

    #[test]
    fn the_light_palette_applies_whichever_theme_the_system_reports() {
        for system in [None, Some(Theme::Light), Some(Theme::Dark)] {
            let ctx = Context::default();
            install(&ctx);
            let style = style_in_effect(&ctx, system);
            assert_eq!(style.visuals.selection.bg_fill, LIGHT.derived(Derived::TextSelection), "system theme {system:?}");
            assert_eq!(style.visuals.panel_fill, LIGHT.surface);
            assert_eq!(style.visuals.extreme_bg_color, LIGHT.surface);
            assert_eq!(style.visuals.override_text_color, Some(LIGHT.ink));
            assert_eq!(style.spacing.button_padding, egui::vec2(10.0, 4.0));
        }
    }

    #[test]
    fn setting_only_the_active_style_is_lost_when_the_system_reports_the_other_theme() {
        // Why `install` sets both slots: this is what happened before.
        let ctx = Context::default();
        let mut style = Style::default();
        style.visuals = Visuals::light();
        style.visuals.selection.bg_fill = LIGHT.derived(Derived::TextSelection);
        ctx.set_style(Arc::new(style));
        let in_effect = style_in_effect(&ctx, Some(Theme::Light));
        assert_ne!(in_effect.visuals.selection.bg_fill, LIGHT.derived(Derived::TextSelection));
    }

    #[test]
    fn dark_palette_is_used_only_when_the_theme_is_dark() {
        let ctx = Context::default();
        install(&ctx);
        let _ = ctx.run(RawInput::default(), |_| {});
        assert_eq!(palette(&ctx), LIGHT);
        ctx.set_theme(Theme::Dark);
        let _ = ctx.run(RawInput::default(), |_| {});
        assert_eq!(palette(&ctx), DARK);
    }

    #[test]
    fn text_colours_reach_the_contrast_the_design_promises() {
        // §5.1: every text colour is at least 4.5:1 on white; the accent is 4.5:1 too.
        fn luminance(c: Color32) -> f64 {
            let channel = |v: u8| {
                let v = f64::from(v) / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
        }
        fn ratio(a: Color32, b: Color32) -> f64 {
            let (la, lb) = (luminance(a), luminance(b));
            (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
        }
        for ink in [LIGHT.ink, LIGHT.ink_2, LIGHT.ink_3, LIGHT.accent, LIGHT.accent_ink, LIGHT.danger, LIGHT.warn] {
            assert!(ratio(ink, LIGHT.surface) >= 4.5, "{ink:?} on surface");
        }
        // Dark: the tokens' reconciled values (ink-3 on surface-2, text on accent and agent fills).
        assert!(ratio(DARK.ink_3, DARK.surface_2) >= 4.5);
        assert!(ratio(DARK.on_accent, DARK.accent) >= 4.5);
        for agent in DARK.agents {
            assert!(ratio(DARK.on_agent, agent) >= 4.5, "{agent:?}");
        }
    }

    const TOKENS: &str = include_str!("../../../design/tokens.json");

    fn hex(value: &serde_json::Value) -> Color32 {
        let text = value.as_str().unwrap_or_else(|| panic!("a colour, got {value}"));
        let digits = text.trim_start_matches('#');
        let n = u32::from_str_radix(digits, 16).expect("hex");
        match digits.len() {
            6 => rgb(n),
            8 => rgba(n),
            _ => panic!("{text} is not #RRGGBB or #RRGGBBAA"),
        }
    }

    fn colours(palette: &Palette) -> Vec<(&'static str, Color32)> {
        vec![
            ("board", palette.board), ("home", palette.home), ("surface", palette.surface), ("surface-2", palette.surface_2),
            ("surface-3", palette.surface_3), ("line", palette.line), ("line-strong", palette.line_strong), ("ink", palette.ink),
            ("ink-2", palette.ink_2), ("ink-3", palette.ink_3), ("accent", palette.accent), ("accent-soft", palette.accent_soft),
            ("accent-ink", palette.accent_ink), ("danger", palette.danger), ("danger-soft", palette.danger_soft), ("warn", palette.warn),
            ("warn-soft", palette.warn_soft), ("star", palette.star), ("agent-1", palette.agents[0]), ("agent-2", palette.agents[1]),
            ("agent-3", palette.agents[2]), ("agent-4", palette.agents[3]), ("on-accent", palette.on_accent), ("on-agent", palette.on_agent),
            ("on-warn", palette.on_warn), ("field", palette.field), ("raised", palette.raised), ("handle", palette.handle),
            ("artboard", palette.artboard), ("swatch-ring", palette.swatch_ring),
        ]
    }

    #[test]
    fn every_token_matches_design_tokens_json() {
        let tokens: serde_json::Value = serde_json::from_str(TOKENS).expect("design/tokens.json is JSON");
        for (theme, palette) in [("light", &LIGHT), ("dark", &DARK)] {
            let file = tokens["color"][theme].as_object().expect("a palette");
            let ours = colours(palette);
            assert_eq!(file.len(), ours.len(), "{theme}: the file and theme.rs name the same colours");
            for (name, colour) in ours {
                assert_eq!(hex(&file[name]), colour, "{theme} {name}");
            }
        }
        for token in Derived::ALL {
            let entry = &tokens["color"]["derived"][token.name()];
            assert_eq!(entry["alpha"].as_f64().map(|a| a as f32), Some(token.alpha()), "{}", token.name());
            let base = entry["base"].as_str().expect("a base");
            let (_, colour) = colours(&LIGHT).into_iter().find(|(name, _)| *name == base).expect("a known base");
            assert_eq!(LIGHT.derived(token), with_alpha(colour, token.alpha()), "{}", token.name());
        }
        for token in [AgentAlpha::RingTrack, AgentAlpha::RailRest, AgentAlpha::Arrival] {
            let entry = &tokens["color"]["derived"][token.name()];
            assert_eq!(entry["base"], "agent");
            assert_eq!(entry["alpha"].as_f64().map(|a| a as f32), Some(token.alpha()), "{}", token.name());
        }
        for role in Type::ALL {
            let entry = &tokens["type"][role.name()];
            let (size, line, bold) = role.metrics();
            assert_eq!(entry["size"].as_f64(), Some(size as f64), "{}", role.name());
            assert_eq!(entry["line"].as_f64(), Some(line as f64), "{}", role.name());
            assert_eq!(entry["weight"].as_u64() == Some(700), bold, "{}", role.name());
        }
        assert_eq!(tokens["type"].as_object().expect("type").len(), Type::ALL.len());
        for (dark, theme) in [(false, "light"), (true, "dark")] {
            for elevation in [Elevation::Float, Elevation::Menu, Elevation::RequestBox, Elevation::Artboard] {
                let file = tokens["shadow"][theme][elevation.name()].as_array().expect("two layers");
                for (layer, (x, y, blur, spread, colour)) in file.iter().zip(elevation.layers(dark)) {
                    let got = (layer["x"].as_i64(), layer["y"].as_i64(), layer["blur"].as_u64(), layer["spread"].as_u64());
                    assert_eq!(got, (Some(x as i64), Some(y as i64), Some(blur as u64), Some(spread as u64)), "{theme} {}", elevation.name());
                    assert_eq!(hex(&layer["color"]), colour, "{theme} {}", elevation.name());
                }
            }
        }
        let radius = &tokens["radius"];
        for (name, value) in [
            ("small", RADIUS_SMALL), ("mini", RADIUS_MINI), ("control", RADIUS_CONTROL), ("home-tab", RADIUS_HOME_TAB), ("chip-home", RADIUS_CHIP),
            ("menu-item", RADIUS_MENU_ITEM), ("float", RADIUS_FLOAT), ("menu", RADIUS_MENU), ("toolbar", RADIUS_TOOLBAR), ("request-box", RADIUS_BOX),
        ] {
            assert_eq!(radius[name].as_u64(), Some(value as u64), "radius {name}");
        }
        let layout = &tokens["layout"];
        let component = &tokens["component"];
        for (path, value) in [
            (&layout["top-bar"]["height"], TOP_BAR_HEIGHT),
            (&layout["sidebar"]["width"], SIDEBAR_WIDTH),
            (&layout["sidebar"]["min"], SIDEBAR_MIN),
            (&layout["sidebar"]["max"], SIDEBAR_MAX),
            (&layout["sidebar"]["tabs"]["height"], SIDEBAR_TABS_HEIGHT),
            (&layout["sidebar"]["tree"]["padding"], TREE_PADDING),
            (&layout["panel"]["width"], PANEL_WIDTH),
            (&layout["panel"]["min"], PANEL_MIN),
            (&layout["panel"]["max"], PANEL_MAX),
            (&layout["panel"]["tabs"]["height"], PANEL_TABS_HEIGHT),
            (&layout["panel"]["section-header"]["height"], SECTION_HEADER),
            (&layout["panel"]["section-header"]["height-short"], SECTION_HEADER_SHORT),
            (&layout["panel"]["row"], PROPERTY_ROW),
            (&layout["panel"]["field"], FIELD_HEIGHT),
            (&layout["panel"]["label-column"], LABEL_COLUMN),
            (&layout["board"]["view-group"]["right"], FLOAT_INSET),
            (&layout["board"]["view-group"]["height"], VIEW_GROUP_HEIGHT),
            (&layout["board"]["view-group"]["padding"], VIEW_GROUP_PADDING),
            (&layout["board"]["view-group"]["gap"], GROUP_GAP),
            (&layout["board"]["tools"]["height"], TOOLS_HEIGHT),
            (&layout["board"]["tools"]["padding"], TOOLS_PADDING),
            (&layout["board"]["tools"]["bottom"], TOOLS_BOTTOM),
            (&layout["board"]["separator"]["height"], SEPARATOR_HEIGHT),
            (&layout["board"]["separator"]["margin-x"], SEPARATOR_MARGIN_X),
            (&layout["board"]["artboard-title"]["height"], ARTBOARD_TITLE_HEIGHT),
            (&layout["board"]["artboard-title"]["gap-to-artboard"], ARTBOARD_TITLE_GAP),
            (&component["icon-button"]["size"], ICON_BUTTON),
            (&component["icon-button"]["icon"], ICON_IN_BUTTON),
            (&component["label-button"]["height"], CONTROL_HEIGHT),
            (&component["label-button"]["icon"], ICON_SMALL),
            (&component["sidebar-tab"]["height"], SIDEBAR_TAB_HEIGHT),
            (&component["sidebar-tab"]["padding-x"], SIDEBAR_TAB_PADDING_X),
            (&component["layer-row"]["height"], LAYER_ROW),
            (&component["mini-button"]["size"], MINI_BUTTON),
            (&component["split-button"]["chevron"]["width"], SPLIT_CHEVRON_WIDTH),
            (&component["menu"]["item"]["height"], MENU_ITEM_HEIGHT),
            (&component["menu"]["padding"], MENU_PADDING),
            (&component["export-popover"]["width"], EXPORT_WIDTH),
        ] {
            assert_eq!(path.as_f64(), Some(value as f64), "{path} vs {value}");
        }
    }
}
