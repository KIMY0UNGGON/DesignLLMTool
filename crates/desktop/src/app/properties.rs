//! The properties panel (GUI_DESIGN §9). Sections come in the order of §9.1 and show only what the
//! selection has and what the engine accepts (§1 rule 6: `Unsupported` hides a control). A value
//! is applied when it is committed (Enter, focus leaves, a menu choice), and every commit is one command,
//! so one commit is one change set and one undo step. The path section is the one exception: its data and
//! view box are edited as text and applied together with the 적용 button.

use designllm_contracts::{
    new_id, ArtifactBody, ArtifactKind, BindTarget, CanvasDocument, DesignSystemSummary, Feature, Intent, Interaction, Justify, Node,
    NodeKind, Operation, PaddingSide, PathSpec, Size, Support, TokenDef, TokenMode, TokenSource, TokenValues,
    CAPABILITY_EXTENSION_DESIGN_TOKENS, CAPABILITY_EXTENSION_LISTS, EMU_PER_MM, EMU_PER_PT,
};
use eframe::egui::{self, vec2, Align2, Color32, CornerRadius, Id, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use serde_json::json;

use super::DesktopApp;
use crate::{icons, theme, widgets};

const TABLE_SPANS_UI_ENABLED: bool = true;

#[derive(Debug)]
pub(super) struct TokenPanelState {
    token_id: String,
    name: String,
    kind: String,
    light: String,
    dark: String,
    original_values: Option<TokenValues>,
    original_light: String,
    original_dark: String,
    baseline_revision: Option<u64>,
    baseline_existed: bool,
    baseline_token_id: Option<String>,
    baseline_name: String,
    baseline_values: Option<TokenValues>,
    selected_token: Option<String>,
    pub(super) design_systems: Vec<DesignSystemSummary>,
    pub(super) design_systems_loaded: bool,
    pub(super) design_systems_error: bool,
    pub(super) binding_source: TokenSource,
    pub(super) selected_shared_token: Option<String>,
    pub(super) shared_tokens: std::collections::BTreeMap<String, TokenDef>,
    pub(super) shared_source_id: Option<String>,
    pub(super) shared_source_revision: Option<u64>,
    pub(super) shared_source_generation: u64,
    pub(super) shared_source_loading: bool,
    pub(super) shared_source_loaded: bool,
    pub(super) shared_source_error: bool,
    target: String,
    side: String,
    min_width_css_px: String,
}

#[derive(Debug, Clone)]
pub(super) enum TokenChangeBaseline {
    Advance,
    Put { token_id: String, name: String, values: TokenValues },
    Remove,
}

/// Owned fields needed by property controls. It deliberately stores no Node or child vector.
#[derive(Debug)]
pub(super) struct NodeProperties {
    pub(super) id: String,
    pub(super) kind: NodeKind,
    pub(super) bounds: designllm_contracts::Rect,
    pub(super) has_children: bool,
    pub(super) style: designllm_contracts::Style,
    pub(super) layout: Option<LayoutProperties>,
    /// The node's whole layout: a breakpoint change sends it back whole, so no field is lost.
    pub(super) full_layout: designllm_contracts::LayoutSpec,
    pub(super) list: Option<designllm_contracts::TextListKind>,
    pub(super) image_fit: Option<designllm_contracts::ImageFit>,
    pub(super) image_crop_pixel_rect: Option<designllm_contracts::PixelRect>,
    pub(super) alt: Option<String>,
    pub(super) locked: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct LayoutProperties {
    pub(super) mode: String,
    pub(super) width_mode: String,
    pub(super) height_mode: String,
    pub(super) gap: i64,
    pub(super) padding: i64,
    pub(super) align: String,
    pub(super) justify: Option<Justify>,
    pub(super) padding_sides: Option<[i64; 4]>,
    pub(super) grid_columns: Option<u32>,
    pub(super) row_gap: Option<i64>,
    pub(super) align_self: Option<designllm_contracts::AlignSelf>,
}

impl From<&designllm_contracts::LayoutSpec> for LayoutProperties {
    fn from(layout: &designllm_contracts::LayoutSpec) -> Self {
        Self {
            mode: layout.mode.clone(),
            width_mode: layout.width_mode.clone(),
            height_mode: layout.height_mode.clone(),
            gap: layout.gap,
            padding: layout.padding,
            align: layout.align.clone(),
            justify: layout.justify,
            padding_sides: layout.padding_sides,
            grid_columns: layout.grid_columns,
            row_gap: layout.row_gap,
            align_self: layout.align_self,
        }
    }
}

impl NodeProperties {
    fn from_node(node: &Node) -> Self {
        Self {
            id: node.id.clone(),
            kind: node.kind,
            bounds: node.bounds,
            has_children: !node.children.is_empty(),
            style: node.style.clone(),
            layout: node.layout.as_ref().map(LayoutProperties::from),
            full_layout: node.layout.clone().unwrap_or_default(),
            list: node.ext.list,
            image_fit: node.ext.image_fit,
            image_crop_pixel_rect: node.ext.image_crop_pixel_rect,
            alt: node.ext.alt.clone(),
            locked: node.ext.locked,
        }
    }
}

impl Default for TokenPanelState {
    fn default() -> Self {
        Self {
            token_id: String::new(),
            name: String::new(),
            kind: String::from("color"),
            light: String::from("#20252D"),
            dark: String::from("#F7F8FA"),
            original_values: None,
            original_light: String::new(),
            original_dark: String::new(),
            baseline_revision: None,
            baseline_existed: false,
            baseline_token_id: None,
            baseline_name: String::new(),
            baseline_values: None,
            selected_token: None,
            design_systems: Vec::new(),
            design_systems_loaded: false,
            design_systems_error: false,
            binding_source: TokenSource::Local,
            selected_shared_token: None,
            shared_tokens: Default::default(),
            shared_source_id: None,
            shared_source_revision: None,
            shared_source_generation: 0,
            shared_source_loading: false,
            shared_source_loaded: false,
            shared_source_error: false,
            target: String::from("style_color"),
            side: String::from("left"),
            min_width_css_px: String::from("600"),
        }
    }
}

impl TokenPanelState {
    pub(super) fn set_design_systems(&mut self, systems: Vec<DesignSystemSummary>) {
        self.design_systems = systems;
        self.design_systems_loaded = true;
        self.design_systems_error = false;
    }

    pub(super) fn begin_shared_source_load(&mut self, artifact_id: &str, revision: u64) -> u64 {
        self.shared_source_generation = self.shared_source_generation.saturating_add(1);
        self.shared_source_id = Some(artifact_id.to_owned());
        self.shared_source_revision = Some(revision);
        self.shared_tokens.clear();
        self.selected_shared_token = None;
        self.shared_source_loading = true;
        self.shared_source_loaded = false;
        self.shared_source_error = false;
        self.shared_source_generation
    }

    pub(super) fn clear_shared_source(&mut self) {
        self.shared_source_generation = self.shared_source_generation.saturating_add(1);
        self.shared_source_id = None;
        self.shared_source_revision = None;
        self.shared_tokens.clear();
        self.selected_shared_token = None;
        self.shared_source_loading = false;
        self.shared_source_loaded = false;
        self.shared_source_error = false;
        self.binding_source = TokenSource::Local;
    }

    pub(super) fn install_shared_source(
        &mut self,
        generation: u64,
        artifact_id: &str,
        revision: u64,
        tokens: std::collections::BTreeMap<String, TokenDef>,
    ) -> bool {
        if self.shared_source_generation != generation
            || self.shared_source_id.as_deref() != Some(artifact_id)
            || self.shared_source_revision != Some(revision)
        {
            return false;
        }
        self.shared_tokens = tokens;
        self.shared_source_loading = false;
        self.shared_source_loaded = true;
        self.shared_source_error = false;
        if self.selected_shared_token.as_ref().is_none_or(|id| !self.shared_tokens.contains_key(id)) {
            self.selected_shared_token = self.shared_tokens.keys().next().cloned();
        }
        true
    }

    pub(super) fn fail_shared_source(&mut self, generation: u64) {
        if self.shared_source_generation == generation {
            self.shared_source_loading = false;
            self.shared_source_loaded = false;
            self.shared_source_error = true;
            self.shared_tokens.clear();
            self.selected_shared_token = None;
            self.binding_source = TokenSource::Local;
        }
    }

    pub(super) fn discard_local_draft(&mut self, document: &CanvasDocument, revision: u64) {
        if let Some(token_id) = self.baseline_token_id.clone() {
            if let Some(token) = document.tokens.get(&token_id) {
                self.load(&token_id, token, revision);
                return;
            }
        }
        self.begin_new(revision);
    }

    fn begin_new(&mut self, revision: u64) {
        self.token_id.clear();
        self.name.clear();
        self.kind = String::from("color");
        self.light = String::from("#20252D");
        self.dark = String::from("#F7F8FA");
        self.original_values = None;
        self.original_light.clear();
        self.original_dark.clear();
        self.baseline_revision = Some(revision);
        self.baseline_existed = false;
        self.baseline_token_id = None;
        self.baseline_name.clear();
        self.baseline_values = None;
        self.selected_token = None;
    }

    fn refresh_new_revision(&mut self, revision: u64) {
        self.baseline_revision = Some(revision);
        self.baseline_existed = false;
        self.baseline_token_id = None;
        self.baseline_name.clear();
        self.baseline_values = None;
        self.original_values = None;
        self.original_light.clear();
        self.original_dark.clear();
    }

    pub(super) fn load(&mut self, token_id: &str, token: &TokenDef, revision: u64) {
        self.token_id = token_id.to_owned();
        self.name.clone_from(&token.name);
        match &token.values {
            TokenValues::Color { light, dark } => {
                self.kind = String::from("color");
                self.light.clone_from(light);
                self.dark.clone_from(dark);
            }
            TokenValues::FontFamily { light, dark } => {
                self.kind = String::from("font_family");
                self.light.clone_from(light);
                self.dark.clone_from(dark);
            }
            TokenValues::FontSize { light, dark } => {
                self.kind = String::from("font_size");
                self.light = emu_as_px(*light);
                self.dark = emu_as_px(*dark);
            }
            TokenValues::Spacing { light, dark } => {
                self.kind = String::from("spacing");
                self.light = emu_as_px(*light);
                self.dark = emu_as_px(*dark);
            }
            TokenValues::Radius { light, dark } => {
                self.kind = String::from("radius");
                self.light = emu_as_px(*light);
                self.dark = emu_as_px(*dark);
            }
        }
        self.original_values = Some(token.values.clone());
        self.original_light.clone_from(&self.light);
        self.original_dark.clone_from(&self.dark);
        self.baseline_revision = Some(revision);
        self.baseline_existed = true;
        self.baseline_token_id = Some(token_id.to_owned());
        self.baseline_name.clone_from(&token.name);
        self.baseline_values = Some(token.values.clone());
        self.selected_token = Some(token_id.to_owned());
    }

    pub(super) fn is_stale(&self, document: &CanvasDocument, revision: u64) -> bool {
        if self.baseline_revision.is_some_and(|baseline| baseline != revision) {
            return true;
        }
        if self.baseline_existed {
            let Some(token_id) = self.baseline_token_id.as_deref() else { return true };
            let Some(token) = document.tokens.get(token_id) else { return true };
            token.name != self.baseline_name || Some(&token.values) != self.baseline_values.as_ref()
        } else {
            !self.token_id.trim().is_empty() && document.tokens.contains_key(&self.token_id)
        }
    }

    pub(super) fn reload(&mut self, document: &CanvasDocument, revision: u64) {
        if let Some(token_id) = self.selected_token.clone() {
            if let Some(token) = document.tokens.get(&token_id) {
                self.load(&token_id, token, revision);
            } else {
                self.begin_new(revision);
            }
            return;
        }
        if let Some(token) = document.tokens.get(&self.token_id) {
            let token_id = self.token_id.clone();
            self.load(&token_id, token, revision);
        } else {
            self.refresh_new_revision(revision);
        }
    }

    pub(super) fn has_local_draft(&self) -> bool {
        if self.baseline_existed {
            self.baseline_token_id.as_deref() != Some(self.token_id.as_str())
                || self.name != self.baseline_name
                || token_values(self).as_ref() != self.baseline_values.as_ref()
        } else {
            !self.token_id.trim().is_empty()
                || !self.name.trim().is_empty()
                || self.kind != "color"
                || self.light != "#20252D"
                || self.dark != "#F7F8FA"
        }
    }

    pub(super) fn reconcile_if_clean(&mut self, document: &CanvasDocument, revision: u64) {
        if !self.is_stale(document, revision) || self.has_local_draft() {
            return;
        }
        if self.baseline_existed {
            let Some(token_id) = self.baseline_token_id.clone() else {
                self.begin_new(revision);
                return;
            };
            if let Some(token) = document.tokens.get(&token_id) {
                self.load(&token_id, token, revision);
            } else {
                self.begin_new(revision);
            }
        } else if let Some(token) = document.tokens.get(&self.token_id) {
            let token_id = self.token_id.clone();
            self.load(&token_id, token, revision);
        } else {
            self.refresh_new_revision(revision);
        }
    }

    pub(super) fn commit_local_change(&mut self, revision: u64, baseline: TokenChangeBaseline) {
        match baseline {
            TokenChangeBaseline::Advance => self.baseline_revision = Some(revision),
            TokenChangeBaseline::Put { token_id, name, values } => {
                self.token_id.clone_from(&token_id);
                self.name.clone_from(&name);
                self.selected_token = Some(token_id.clone());
                self.baseline_revision = Some(revision);
                self.baseline_existed = true;
                self.baseline_token_id = Some(token_id);
                self.baseline_name = name;
                self.original_values = Some(values.clone());
                self.baseline_values = Some(values);
                self.original_light.clone_from(&self.light);
                self.original_dark.clone_from(&self.dark);
            }
            TokenChangeBaseline::Remove => self.begin_new(revision),
        }
    }

    #[cfg(test)]
    pub(super) fn set_name_for_test(&mut self, name: &str) {
        self.name = name.to_owned();
    }

    #[cfg(test)]
    pub(super) fn name_for_test(&self) -> &str {
        &self.name
    }
}

/// EMU per CSS px (`EMU_PER_CSS_PX`), for the panel's px arithmetic.
pub(super) const EMU_PER_PX: f64 = designllm_contracts::EMU_PER_CSS_PX as f64;

fn emu_as_px(value: i64) -> String {
    widgets::format_number(value as f64 / EMU_PER_PX, 6)
}

/// The display unit of a document kind: CSS px for canvases, slides and web pages (1 px = 9525
/// EMU); mm for documents and figures (§8).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Unit {
    Px,
    Mm,
}

impl Unit {
    pub fn emu(self) -> f64 {
        match self {
            Self::Px => EMU_PER_PX,
            Self::Mm => EMU_PER_MM as f64,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Px => "px",
            Self::Mm => "mm",
        }
    }

    pub fn decimals(self) -> usize {
        match self {
            Self::Px => 0,
            Self::Mm => 1,
        }
    }
}

/// Whether a fill or stroke colour paints anything: the canvas engine's no-paint colour is `transparent`, the others' `none`.
pub(super) fn paints(colour: &str) -> bool {
    !matches!(colour, "" | "none" | "transparent")
}

pub(super) fn unit_for(kind: ArtifactKind) -> Unit {
    match kind {
        ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp | ArtifactKind::Figure => Unit::Mm,
        _ => Unit::Px,
    }
}

/// Whether the engine accepts writes for a feature (`Supported` and `Approximate` are editable;
/// `PreserveOnly` shows the value and a lock; `Unsupported` hides the control).
pub(super) fn feature_shown(support: Option<Support>) -> bool {
    matches!(support, Some(Support::Supported | Support::Approximate | Support::PreserveOnly))
}

fn feature_editable(support: Option<Support>) -> bool {
    matches!(support, Some(Support::Supported | Support::Approximate))
}

#[derive(Clone)]
struct InteractionTarget {
    id: String,
    label: String,
    page_label: String,
    variants: Vec<String>,
}

const INTERACTION_ACTIONS: &[(&str, &str, &str)] = &[
    ("navigate_page", "layer-page", "페이지 이동"),
    ("toggle_visibility", "eye", "표시 전환"),
    ("set_variant", "more", "상태 바꾸기"),
    ("back", "chev-left", "뒤로"),
];

fn collect_interaction_targets(nodes: &[Node], page_label: &str, out: &mut Vec<InteractionTarget>) {
    for node in nodes {
        out.push(InteractionTarget {
            id: node.id.clone(),
            label: super::sidebar::node_label(node),
            page_label: page_label.to_owned(),
            variants: node.variants.keys().cloned().collect(),
        });
        collect_interaction_targets(&node.children, page_label, out);
    }
}

fn interaction_kind(interaction: &Interaction) -> Option<&'static str> {
    match interaction {
        Interaction::NavigatePage { .. } => Some("navigate_page"),
        Interaction::ToggleVisibility { .. } => Some("toggle_visibility"),
        Interaction::SetVariant { .. } => Some("set_variant"),
        Interaction::Back => Some("back"),
        Interaction::OpenUrl { .. } => None,
    }
}

fn interaction_command_args(index: usize, interaction: &Interaction) -> serde_json::Value {
    match interaction {
        Interaction::NavigatePage { page_id } => json!({ "index": index, "action": "navigate_page", "target_id": page_id }),
        Interaction::ToggleVisibility { node_id } => json!({ "index": index, "action": "toggle_visibility", "target_id": node_id }),
        Interaction::SetVariant { node_id, variant } => json!({ "index": index, "action": "set_variant", "target_id": node_id, "variant": variant }),
        Interaction::OpenUrl { url } => json!({ "index": index, "action": "open_url", "url": url }),
        Interaction::Back => json!({ "index": index, "action": "back" }),
    }
}

fn default_interaction(
    kind: &str,
    source_node: &str,
    pages: &[(String, String)],
    targets: &[InteractionTarget],
) -> Option<Interaction> {
    match kind {
        "navigate_page" => Some(Interaction::NavigatePage { page_id: pages.first()?.0.clone() }),
        "toggle_visibility" => Some(Interaction::ToggleVisibility { node_id: source_node.to_owned() }),
        "set_variant" => {
            let target = targets.iter().find(|target| !target.variants.is_empty())?;
            Some(Interaction::SetVariant { node_id: target.id.clone(), variant: target.variants[0].clone() })
        }
        "back" => Some(Interaction::Back),
        _ => None,
    }
}

fn interaction_target_menu(
    ui: &mut Ui,
    palette: &theme::Palette,
    options: &[(String, String)],
    current: &str,
) -> Option<String> {
    let label = options.iter().find(|(id, _)| id == current).map_or("선택", |(_, label)| label.as_str());
    let width = ui.available_width();
    let button = widgets::dropdown(ui, palette, label, None, width);
    let mut selected = None;
    egui::Popup::from_toggle_button_response(&button)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(width.max(200.0));
            for (id, label) in options {
                if widgets::menu_item(ui, palette, None, label, "", id == current).clicked() {
                    selected = Some(id.clone());
                    ui.close();
                }
            }
        });
    selected
}

fn interaction_action_row(
    ui: &mut Ui,
    palette: &theme::Palette,
    items: &[(&str, &str, &str)],
    current: &str,
) -> (Option<String>, bool) {
    let (label, icon) = items.iter().find(|(kind, _, _)| *kind == current).map_or(("동작", "more"), |(_, icon, label)| (*label, *icon));
    let (picked, remove) = row(ui, palette, Some("동작"), |ui| {
        let width = (ui.available_width() - 36.0).max(40.0);
        let button = widgets::dropdown(ui, palette, label, Some(icon), width);
        let mut picked = None;
        egui::Popup::from_toggle_button_response(&button)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_width(width.max(200.0));
                for (kind, icon, label) in items {
                    if widgets::menu_item(ui, palette, Some(icon), label, "", *kind == current).clicked() {
                        picked = Some((*kind).to_owned());
                        ui.close();
                    }
                }
            });
        (picked, remove_button(ui, palette, "동작 제거"))
    });
    (picked, remove)
}

fn interaction_add_menu(ui: &mut Ui, palette: &theme::Palette, items: &[(&str, &str, &str)]) -> Option<String> {
    row(ui, palette, None, |ui| {
        let width = ui.available_width();
        let button = widgets::dropdown(ui, palette, "추가", Some("add"), width);
        let mut picked = None;
        egui::Popup::from_toggle_button_response(&button)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_width(width.max(200.0));
                for (kind, icon, label) in items {
                    if widgets::menu_item(ui, palette, Some(icon), label, "", false).clicked() {
                        picked = Some((*kind).to_owned());
                        ui.close();
                    }
                }
            });
        picked
    })
}

fn interaction_url_row(
    ui: &mut Ui,
    palette: &theme::Palette,
    artifact_id: &str,
    revision: u64,
    selection_key: &str,
    index: usize,
    current: Option<&str>,
    mixed: bool,
    has_url: bool,
) -> (Option<String>, bool) {
    let value = if mixed { "" } else { current.unwrap_or_default() };
    let state_key = format!("{artifact_id}\u{1F}{selection_key}");
    let id = Id::new(("canvas-open-url", selection_key.to_owned(), index));
    row(ui, palette, Some("URL"), |ui| {
        let width = ui.available_width() - 24.0 - 8.0 - if has_url { 36.0 } else { 0.0 };
        let hint = if mixed { "여러 값" } else { "https://…" };
        let (draft, enter) = text_line_draft(ui, palette, id, &state_key, revision, width, value, hint);
        let changed = draft != value;
        let apply_clicked = ui.add_enabled_ui(changed, |ui| widgets::mini_button(ui, palette, "check", "적용")).inner.clicked();
        let apply = if (apply_clicked || enter) && changed { Some(draft) } else { None };
        let remove = has_url && remove_button(ui, palette, "링크 제거");
        (apply, remove)
    })
}

impl DesktopApp {
    /// Whether the open document's engine lets the user write `feature` at all.
    pub(super) fn feature_support(&self, feature: Feature) -> Option<Support> {
        self.context.as_ref().and_then(|context| context.capabilities.get(&feature)).map(|item| item.edit)
    }

    pub(super) fn canvas_list_support(&self) -> Option<(Support, Support)> {
        self.context
            .as_ref()
            .and_then(|context| context.capability_extensions.get(CAPABILITY_EXTENSION_LISTS))
            .map(|support| (support.read, support.edit))
    }

    pub(super) fn canvas_token_support(&self) -> Option<(Support, Support)> {
        self.context
            .as_ref()
            .and_then(|context| context.capability_extensions.get(CAPABILITY_EXTENSION_DESIGN_TOKENS))
            .map(|support| (support.read, support.edit))
    }

    pub(super) fn canvas_token_form_stale(&self) -> bool {
        let Some(artifact) = self.artifact.as_ref() else { return false };
        let ArtifactBody::Canvas(document) = &artifact.body else { return false };
        !self.canvas_token_snapshot_current()
            || (self.token_panel.is_stale(document, artifact.revision) && self.token_panel.has_local_draft())
    }

    pub(super) fn canvas_token_snapshot_current(&self) -> bool {
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else { return false };
        active.id == artifact.id
            && artifact.id == context.artifact_id
            && active.revision == artifact.revision
            && artifact.revision == context.revision
    }

    /// The selected node, cloned out of the artifact so that the panel can change the document.
    pub(super) fn selected_node_copy(&self) -> Option<Node> {
        let id = self.selected_node.as_deref()?;
        let artifact = self.artifact.as_ref()?;
        super::doc_tree::find_node(artifact, id).cloned()
    }

    pub(super) fn selected_node_properties(&self) -> Option<NodeProperties> {
        let id = self.selected_node.as_deref()?;
        let artifact = self.artifact.as_ref()?;
        super::doc_tree::find_node(artifact, id).map(NodeProperties::from_node)
    }

    pub(super) fn selected_nodes_properties(&self) -> Vec<NodeProperties> {
        let Some(artifact) = self.artifact.as_ref() else { return Vec::new() };
        self.selected_nodes
            .iter()
            .filter_map(|id| super::doc_tree::find_node(artifact, id).map(NodeProperties::from_node))
            .collect()
    }

    /// The panel body: the sections of the selection, or the document's own sections. Sections sit 12 px from the
    /// panel edges with a 1 px `line` between them, rows are 32 px with a 76 px label column (§5.3, mockup `.sec`).
    pub(super) fn draw_properties(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        let node = self.selected_node_properties();
        let selected_table = self.selected_canvas_table();
        let mut token_action = None;
        let mut table_action = None;
        let mut repeat_action = None;
        let mut table_cell_action = None;
        let span_offer = self.panel_span_offer();
        let mut discard_crop_draft = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 12, top: 0, bottom: 12 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let mut first = true;
                if let Some(editor) = self.image_crop_editor.as_ref().filter(|editor| {
                    !self.active.as_ref().is_some_and(|active| active.id == editor.intent.artifact_id)
                        || node.as_ref().is_none_or(|node| node.id != editor.intent.node_id)
                }) {
                    section_head(ui, palette, &mut first, "이미지 자르기 초안", None);
                    ui.label("다른 이미지 작업이 선택되어 있습니다. 초안은 보존되어 있습니다.");
                    if let Some(copy) = image_crop_copy_payload(editor) {
                        if widgets::labeled_button(ui, palette, None, "자르기 초안 복사", false).clicked() { ui.ctx().copy_text(copy); }
                    }
                    if ui.add_enabled_ui(editor.stage != super::ImageCropStage::Applying, |ui| widgets::labeled_button(ui, palette, None, "초안 버리기", false)).inner.clicked() {
                        discard_crop_draft = true;
                    }
                    ui.add_space(8.0);
                }
                let token_editable = self.canvas_token_support().is_some_and(|(_, edit)| feature_editable(Some(edit)));
                if token_editable {
                    if let (Some(active), Some(artifact)) = (self.active.as_ref(), self.artifact.as_ref()) {
                        if let ArtifactBody::Canvas(document) = &artifact.body {
                            token_action = draw_local_token_section(
                                ui,
                                palette,
                                active.id.as_str(),
                                artifact.revision,
                                self.canvas_token_snapshot_current(),
                                document,
                                artifact.design_system.as_ref(),
                                artifact.publishes_design_system,
                                &mut self.token_panel,
                                &self.selected_nodes,
                                &mut first,
                            );
                        }
                    }
                }
                match node {
                    Some(node) => {
                        // No gap after the (usually empty) figure-link block, so the first section starts at the
                        // panel top + 4 (`.sec.first`).
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.scope(|ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            self.draw_figure_link(ui);
                        });
                        self.node_sections(ui, palette, &node, &mut first);
                        if let Some((table_id, header, rows, columns, column_widths, has_spans)) = selected_table {
                            table_action = self.draw_canvas_table_controls(ui, palette, &table_id, header, rows, columns, column_widths, has_spans, &mut first);
                        }
                        repeat_action = self.draw_repeat_controls(ui, palette, &mut first);
                    }
                    None => {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        self.document_sections(ui, palette, &mut first);
                    }
                }
                table_cell_action = self.draw_canvas_table_merge_control(ui, palette, &mut first, span_offer);
                if let Some(figure) = self.figure_document() {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    self.draw_figure_details(ui, &figure);
                    self.draw_figure_output_profile_section(ui, palette, &figure, &mut first);
                    // A selected panel's `내보내기 ›` is the panel's last row (below the data rows).
                    let panel_selected = self.selected_node.as_deref().is_some_and(|id| figure.panels.iter().any(|panel| panel.id == id));
                    if panel_selected {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let mut last = false;
                        self.export_row(ui, palette, &mut last, "selection");
                    }
                }
            });
        });
        if let Some((command, args)) = token_action {
            self.invoke_ui(command, args);
        }
        if let Some((command, args)) = table_action {
            self.invoke_ui(command, args);
        }
        if let Some((command, args)) = repeat_action {
            self.invoke_ui(command, args);
        }
        if let Some(action) = table_cell_action {
            self.run_span_action(action);
        }
        if discard_crop_draft { self.discard_canvas_image_crop(); }
    }

    fn selected_canvas_table(&self) -> Option<(String, bool, usize, usize, Option<Vec<u32>>, bool)> {
        let node = self.selected_canvas_table_node()?;
        let table = node.ext.table.as_ref()?;
        Some((node.id.clone(), table.header_row_id.is_some(), table.rows.len(), table.rows.first().map_or(0, |row| row.cell_ids.len()), table.column_widths.clone(), !table.cell_spans.is_empty()))
    }

    pub(super) fn selected_canvas_table_node(&self) -> Option<&Node> {
        if self.selected_nodes.len() > 1 { return None; }
        let id = self.selected_node.as_deref().or_else(|| self.selected_nodes.first().map(String::as_str))?;
        let artifact = self.artifact.as_ref()?;
        let ArtifactBody::Canvas(document) = &artifact.body else { return None };
        let page = document.pages.get(self.view_ref().page_index as usize)?;
        let node = page.nodes.iter().find_map(|root| super::doc_tree::find_node_in_node(root, id))?;
        node.ext.table.as_ref().map(|_| node)
    }

    fn draw_canvas_image_crop_controls(&mut self, ui: &mut Ui, palette: &theme::Palette, node: &NodeProperties) {
        let Some(active_id) = self.active.as_ref().map(|active| active.id.as_str()) else { return };
        let editor = self.image_crop_editor.as_ref().filter(|editor| {
            editor.intent.artifact_id == active_id && editor.intent.node_id == node.id
        }).cloned();
        let mut start = false;
        let mut changed_fields = None;
        let mut apply_crop = None;
        let mut reset = false;
        let mut rebase = false;
        let mut retry: Option<Option<designllm_contracts::PixelRect>> = None;
        let mut discard = false;
        if let Some(editor) = editor {
            let current = self.image_crop_target_is_current(&editor.intent);
            let editable = editor.stage == super::ImageCropStage::Editing && current && !editor.outcome_uncertain;
            ui.small(match node.image_crop_pixel_rect {
                Some(rect) => format!("현재 자르기: X {} · Y {} · 폭 {} · 높이 {} px", rect.x, rect.y, rect.width, rect.height),
                None => "현재 자르기: 전체 이미지".into(),
            });
            if let Some(message) = editor.message.as_deref() {
                ui.colored_label(palette.warn, message);
            }
            if !current && editor.message.is_none() {
                let reserved = self.reservations_for.as_deref() == Some(editor.intent.artifact_id.as_str())
                    && self.has_reservation_for_nodes(std::slice::from_ref(&editor.intent.node_id));
                ui.colored_label(palette.warn, if reserved {
                    "다른 작업이 이 이미지를 사용하는 중입니다. 끝난 뒤 최신 상태를 확인하세요"
                } else {
                    "revision, 선택, 잠금 또는 이미지 원본을 확인할 수 없습니다. 초안을 적용하지 않았습니다"
                });
            }
            match editor.stage {
                super::ImageCropStage::Loading => {
                    ui.small("원본 픽셀 크기를 확인하고 있습니다");
                    if widgets::labeled_button(ui, palette, None, "자르기 확인 취소", false).clicked() { discard = true; }
                }
                super::ImageCropStage::Applying => {
                    ui.small("이미지 자르기를 적용하고 있습니다. 결과가 확인될 때까지 다른 변경을 보류합니다");
                }
                super::ImageCropStage::Editing => {
                    if let (Some((image_width, image_height)), Some(mut fields)) = (editor.dimensions, editor.draft.clone()) {
                        ui.small(format!("원본 크기: {image_width} × {image_height} px"));
                        let mut changed = false;
                        // Two fields and their labels share a row: each field takes half of what the labels leave, 40 to 64
                        // wide, with a 4 px margin so that five digits fit.
                        let crop_field_width = |ui: &Ui, labels: [&str; 2]| {
                            let font = egui::TextStyle::Small.resolve(ui.style());
                            let taken: f32 = labels.iter().map(|label| ui.painter().layout_no_wrap((*label).to_owned(), font.clone(), palette.ink).size().x).sum();
                            ((ui.available_width() - taken - 3.0 * ui.spacing().item_spacing.x) / 2.0).clamp(40.0, 64.0)
                        };
                        row(ui, palette, Some("원점 (px)"), |ui| {
                            ui.horizontal(|ui| {
                                let width = crop_field_width(ui, ["X", "Y"]);
                                for (label, index) in [("X", 0usize), ("Y", 1usize)] {
                                    ui.small(label);
                                    changed |= widgets::edit_line(ui, palette, &mut fields[index], widgets::Field {
                                        width,
                                        margin: Some(egui::Margin::symmetric(4, 4)),
                                        id: Some(Id::new(("image-crop", node.id.as_str(), label))),
                                        enabled: editable,
                                        ..Default::default()
                                    }).changed();
                                }
                            });
                        });
                        row(ui, palette, Some("크기 (px)"), |ui| {
                            ui.horizontal(|ui| {
                                let width = crop_field_width(ui, ["폭", "높이"]);
                                for (label, index) in [("폭", 2usize), ("높이", 3usize)] {
                                    ui.small(label);
                                    changed |= widgets::edit_line(ui, palette, &mut fields[index], widgets::Field {
                                        width,
                                        margin: Some(egui::Margin::symmetric(4, 4)),
                                        id: Some(Id::new(("image-crop", node.id.as_str(), label))),
                                        enabled: editable,
                                        ..Default::default()
                                    }).changed();
                                }
                            });
                        });
                        let parsed = parse_image_crop_fields(&fields);
                        let valid = parsed.is_some_and(|rect| super::state::pixel_rect_within(rect, (image_width, image_height)));
                        if !valid && editable {
                            ui.colored_label(palette.warn, "X/Y는 0 이상, 폭/높이는 1 이상이어야 하며 자르기 범위는 원본 안에 있어야 합니다");
                        }
                        let payload = image_crop_copy_payload_fields(&fields, image_width, image_height);
                        ui.horizontal(|ui| {
                            if widgets::labeled_button(ui, palette, None, "초안 복사", false).clicked() { ui.ctx().copy_text(payload); }
                            if ui.add_enabled_ui(editable && node.image_crop_pixel_rect.is_some(), |ui| widgets::labeled_button(ui, palette, None, "전체 이미지로 초기화", false)).inner.clicked() {
                                reset = true;
                            }
                            if ui.add_enabled_ui(editable && valid, |ui| widgets::labeled_button(ui, palette, None, "적용", true)).inner.clicked() {
                                apply_crop = parsed;
                            }
                        });
                        if editor.requested_crop.is_some() && !editor.outcome_uncertain && current {
                            if widgets::labeled_button(ui, palette, None, "보존한 요청 다시 적용", false).clicked() { retry = editor.requested_crop; }
                        }
                        if changed { changed_fields = Some(fields); }
                    } else {
                        ui.small("원본 픽셀 크기를 확인하지 못했습니다");
                        ui.horizontal(|ui| {
                            if ui.add_enabled_ui(current, |ui| widgets::labeled_button(ui, palette, None, "크기 다시 확인", false)).inner.clicked() { start = true; }
                            if widgets::labeled_button(ui, palette, None, "초안 버리기", false).clicked() { discard = true; }
                        });
                    }
                    if !current || editor.outcome_uncertain {
                        ui.horizontal(|ui| {
                            if widgets::labeled_button(ui, palette, None, "최신 이미지 상태 확인", false).clicked() { rebase = true; }
                            if widgets::labeled_button(ui, palette, None, "초안 버리기", false).clicked() { discard = true; }
                        });
                    } else if editor.draft.is_some() {
                        if widgets::labeled_button(ui, palette, None, "초안 버리기", false).clicked() { discard = true; }
                    }
                }
            }
        } else if self.image_crop_editor.is_none() {
            let available = self.canvas_image_crop_available(&node.id);
            let response = row(ui, palette, Some("자르기"), |ui| {
                let button = ui.add_enabled_ui(available, |ui| widgets::labeled_button(ui, palette, None, "원본 픽셀 범위 편집", false)).inner;
                let response = if available { button } else {
                    button.on_disabled_hover_text("잠금, 다른 작업의 예약, 선택 또는 이미지 원본을 확인한 뒤 시작할 수 있습니다")
                };
                response.clicked()
            });
            start = response;
        }
        if let Some(fields) = changed_fields { self.update_canvas_image_crop_draft(fields); }
        if let Some(rect) = apply_crop {
            if let Err(message) = self.apply_canvas_image_crop(Some(rect)) { self.set_error(message); }
        }
        if reset {
            if let Err(message) = self.apply_canvas_image_crop(None) { self.set_error(message); }
        }
        if let Some(crop) = retry {
            if let Err(message) = self.apply_canvas_image_crop(crop) { self.set_error(message); }
        }
        if rebase {
            if let Err(message) = self.rebase_canvas_image_crop() { self.set_error(message); }
        }
        if discard { self.discard_canvas_image_crop(); }
        if start {
            if self.image_crop_editor.as_ref().is_some_and(|editor| editor.dimensions.is_none()) {
                self.retry_canvas_image_crop_lookup();
            } else if let Err(message) = self.begin_canvas_image_crop(&node.id) {
                self.set_error(message);
            }
        }
    }

    fn draw_canvas_table_controls(
        &self,
        ui: &mut Ui,
        palette: &theme::Palette,
        table_id: &str,
        header: bool,
        rows: usize,
        columns: usize,
        column_widths: Option<Vec<u32>>,
        has_spans: bool,
        first: &mut bool,
    ) -> Option<(&'static str, serde_json::Value)> {
        if !self.feature_edit_supported(Feature::Tables) { return None; }
        let header_label = if header { "머리글 해제" } else { "첫 행 머리글" };
        if !has_spans && section_head(ui, palette, first, "표", Some(("check", header_label))) {
            return Some(("canvas.table.set_header", json!({ "table_id": table_id, "enabled": !header })));
        }
        if has_spans {
            section_head(ui, palette, first, "표", None);
            ui.small("병합을 해제한 뒤 행·열을 편집할 수 있습니다");
        }
        ui.small(format!("{rows}행 × {columns}열"));
        let mut action = None;
        let current_widths = column_widths.clone().unwrap_or_else(|| vec![1; columns]);
        let mut invalid_width = false;
        ui.horizontal_wrapped(|ui| {
            ui.small("열 비율");
            for (index, weight) in current_widths.iter().enumerate() {
                let prefix = format!("{}:", index + 1);
                let value = field(
                    ui, palette, Id::new(("canvas-table-width", table_id, index)), 54.0,
                    &prefix, Some(f64::from(*weight)), "", 0, true,
                );
                if let Some(value) = value {
                    if value.is_finite() && value.fract() == 0.0 && (1.0..=f64::from(u32::MAX)).contains(&value) {
                        let mut next = current_widths.clone();
                        next[index] = value as u32;
                        action = Some(("canvas.table.set_column_widths", json!({ "table_id": table_id, "column_widths": next })));
                    } else {
                        invalid_width = true;
                    }
                }
            }
            if column_widths.is_some() && widgets::mini_button(ui, palette, "check", "균등").clicked() {
                action = Some(("canvas.table.set_column_widths", json!({ "table_id": table_id, "column_widths": null })));
            }
        });
        if invalid_width {
            ui.colored_label(palette.danger, "1 이상의 정수를 입력하세요");
        }
        if action.is_some() {
            return action;
        }
        ui.horizontal(|ui| {
            if ui.add_enabled_ui(!has_spans, |ui| widgets::mini_button(ui, palette, "add", "행 추가")).inner.clicked() {
                action = Some(("canvas.table.add_row", json!({ "table_id": table_id })));
            }
            if ui.add_enabled_ui(!has_spans && rows > 1, |ui| widgets::mini_button(ui, palette, "minus", "행 삭제")).inner.clicked() {
                action = Some(("canvas.table.remove_row", json!({ "table_id": table_id })));
            }
            if ui.add_enabled_ui(!has_spans, |ui| widgets::mini_button(ui, palette, "add", "열 추가")).inner.clicked() {
                action = Some(("canvas.table.add_column", json!({ "table_id": table_id })));
            }
            if ui.add_enabled_ui(!has_spans && columns > 1, |ui| widgets::mini_button(ui, palette, "minus", "열 삭제")).inner.clicked() {
                action = Some(("canvas.table.remove_column", json!({ "table_id": table_id })));
            }
        });
        action
    }

    fn selected_canvas_table_cells(&self) -> Option<(&Node, designllm_contracts::CanvasTable, Vec<String>)> {
        let mut selected = self.selected_nodes.clone();
        if let Some(primary) = &self.selected_node {
            if !selected.contains(primary) { selected.insert(0, primary.clone()); }
        }
        if selected.is_empty() || selected.iter().collect::<std::collections::BTreeSet<_>>().len() != selected.len() {
            return None;
        }
        let artifact = self.artifact.as_ref()?;
        let ArtifactBody::Canvas(document) = &artifact.body else { return None };
        let page = document.pages.get(self.view_ref().page_index as usize)?;
        fn find_table<'a>(nodes: &'a [Node], selected: &[String], inside_instance: bool) -> Option<&'a Node> {
            for node in nodes {
                let inside_instance = inside_instance || node.ext.instance.is_some();
                if !inside_instance && node.ext.table.as_ref().is_some_and(|table| {
                    selected.iter().all(|id| table.rows.iter().any(|row| row.cell_ids.contains(id)))
                }) {
                    return Some(node);
                }
                if let Some(found) = find_table(&node.children, selected, inside_instance) {
                    return Some(found);
                }
            }
            None
        }
        let table_node = find_table(&page.nodes, &selected, false)?;
        Some((table_node, table_node.ext.table.as_ref()?.clone(), selected))
    }

    /// Whether a node of the selection (`node` and the rest of `group`) is a cell of a locked Canvas table. The canvas engine
    /// refuses a style change to such a cell as it does for a locked node (`ensure_table_owner_unlocked`).
    fn in_locked_canvas_table(&self, node: &NodeProperties, group: &[NodeProperties]) -> bool {
        let Some(ArtifactBody::Canvas(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
            return false;
        };
        std::iter::once(node).chain(group).any(|cell| {
            document.pages.iter().any(|page| {
                super::doc_tree::find_canvas_table_cell(&page.nodes, &cell.id, None, false, false)
                    .is_some_and(|(table, ..)| table.ext.locked == Some(true))
            })
        })
    }

    /// The edit the selected cells ask for: the unmerge of a merged cell (its anchor, with or without cells it covers), else a
    /// merge of the rectangle they form. `Err` says why the selection is not an edit.
    pub(super) fn canvas_table_span_request(&self) -> Result<(String, designllm_contracts::CanvasTable, super::table_merge::SpanEdit), String> {
        if !self.canvas_table_context_current() || !self.feature_edit_supported(Feature::Tables) {
            return Err("표 편집 상태에서만 셀을 합칠 수 있습니다".into());
        }
        let (table_node, expected, selected) = self.selected_canvas_table_cells().ok_or("같은 표의 셀을 고르세요")?;
        let edit = self.span_request_for(&table_node.id, &selected)?;
        Ok((table_node.id.clone(), expected, edit))
    }

    /// The structure operation the selection's merge or unmerge makes; `None` when the selection is not one or the span
    /// rules refuse it.
    pub(super) fn canvas_table_span_operation(&self) -> Option<(bool, designllm_contracts::Operation)> {
        let (table_id, expected, edit) = self.canvas_table_span_request().ok()?;
        let unmerge = matches!(edit, super::table_merge::SpanEdit::Unmerge { .. });
        let table = super::table_merge::structure_after(&expected, &super::table_merge::cell_positions(&expected), edit).ok()?;
        Some((unmerge, designllm_contracts::Operation::SetCanvasTableStructure { table_id, expected, table }))
    }

    /// The command the panel's or the menu's merge (`unmerge` false) or unmerge asks for; `Err` says why it is refused (every
    /// check of the command itself, `span_edit_check`).
    pub(super) fn canvas_table_span_command(&self, unmerge: bool) -> Result<(&'static str, serde_json::Value), String> {
        if !self.canvas_table_context_current() || !self.feature_edit_supported(Feature::Tables) {
            return Err("표 편집 상태에서만 셀을 합칠 수 있습니다".into());
        }
        let (table_node, _, selected) = self.selected_canvas_table_cells().ok_or("같은 표의 셀을 고르세요")?;
        self.span_command_for(&table_node.id, &selected, unmerge)
    }

    /// The 셀 section: 병합 or 병합 해제 for the selected table's cells (`offer`, `panel_span_offer`). A click yields the
    /// command, or the reason it is refused.
    fn draw_canvas_table_merge_control(&self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool, offer: Option<bool>) -> Option<Result<(&'static str, serde_json::Value), String>> {
        if !TABLE_SPANS_UI_ENABLED { return None; }
        // Shown only for a selection that can merge or unmerge (an invalid one has no action, as before).
        let unmerge = offer?;
        let action = if unmerge { ("minus", "병합 해제") } else { ("add", "병합") };
        section_head(ui, palette, first, "셀", Some(action)).then(|| self.canvas_table_span_command(unmerge))
    }
}

const TOKEN_MODES: &[(&str, &str, &str)] = &[("light", "", "라이트"), ("dark", "", "다크")];
const TOKEN_SOURCES: &[(&str, &str, &str)] = &[("local", "", "로컬"), ("shared", "", "공유")];
const TOKEN_KINDS: &[(&str, &str, &str)] = &[
    ("color", "", "색"),
    ("font_family", "", "글꼴"),
    ("font_size", "", "글자 크기"),
    ("spacing", "", "간격"),
    ("radius", "", "모서리"),
];
const TOKEN_SIDES: &[(&str, &str, &str)] = &[
    ("top", "", "위"),
    ("right", "", "오른쪽"),
    ("bottom", "", "아래"),
    ("left", "", "왼쪽"),
];
const TOKEN_COLOR_TARGETS: &[(&str, &str, &str)] = &[
    ("style_color", "", "글자 색"),
    ("style_fill", "", "채우기"),
    ("style_stroke", "", "선 색"),
];
const TOKEN_FONT_TARGETS: &[(&str, &str, &str)] = &[("style_font_family", "", "글꼴")];
const TOKEN_SIZE_TARGETS: &[(&str, &str, &str)] = &[("style_font_size", "", "글자 크기")];
const TOKEN_RADIUS_TARGETS: &[(&str, &str, &str)] = &[("style_corner_radius", "", "모서리")];
const TOKEN_SPACING_TARGETS: &[(&str, &str, &str)] = &[
    ("layout_gap", "", "열 간격"),
    ("layout_row_gap", "", "행 간격"),
    ("layout_padding", "", "안쪽 여백"),
    ("layout_padding_side", "", "한 면 여백"),
    ("breakpoint_gap", "", "중단점 열 간격"),
    ("breakpoint_padding", "", "중단점 안쪽 여백"),
];

fn token_targets(kind: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    match kind {
        "color" => TOKEN_COLOR_TARGETS,
        "font_family" => TOKEN_FONT_TARGETS,
        "font_size" => TOKEN_SIZE_TARGETS,
        "radius" => TOKEN_RADIUS_TARGETS,
        "spacing" => TOKEN_SPACING_TARGETS,
        _ => &[],
    }
}

fn token_kind_key(values: &TokenValues) -> &'static str {
    match values {
        TokenValues::Color { .. } => "color",
        TokenValues::FontFamily { .. } => "font_family",
        TokenValues::FontSize { .. } => "font_size",
        TokenValues::Spacing { .. } => "spacing",
        TokenValues::Radius { .. } => "radius",
    }
}

fn token_display_values(values: &TokenValues) -> (String, String, String) {
    match values {
        TokenValues::Color { light, dark } => ("색".into(), light.clone(), dark.clone()),
        TokenValues::FontFamily { light, dark } => ("글꼴".into(), light.clone(), dark.clone()),
        TokenValues::FontSize { light, dark } => ("글자 크기".into(), format!("{} px", emu_as_px(*light)), format!("{} px", emu_as_px(*dark))),
        TokenValues::Spacing { light, dark } => ("간격".into(), format!("{} px", emu_as_px(*light)), format!("{} px", emu_as_px(*dark))),
        TokenValues::Radius { light, dark } => ("모서리".into(), format!("{} px", emu_as_px(*light)), format!("{} px", emu_as_px(*dark))),
    }
}

fn set_token_kind(state: &mut TokenPanelState, kind: &str) {
    if state.kind == kind {
        return;
    }
    state.kind = kind.to_owned();
    match kind {
        "color" => {
            state.light = String::from("#20252D");
            state.dark = String::from("#F7F8FA");
        }
        "font_family" => {
            state.light = String::from("sans-serif");
            state.dark = String::from("sans-serif");
        }
        "font_size" => {
            state.light = String::from("16");
            state.dark = String::from("16");
        }
        "spacing" => {
            state.light = String::from("8");
            state.dark = String::from("8");
        }
        "radius" => {
            state.light = String::from("0");
            state.dark = String::from("0");
        }
        _ => {}
    }
}

fn token_number_emu(value: &str, allow_zero: bool) -> Option<i64> {
    let px = widgets::parse_number(value)?;
    if !px.is_finite() || px < 0.0 || (!allow_zero && px == 0.0) {
        return None;
    }
    let emu = px * EMU_PER_PX;
    (emu <= i64::MAX as f64).then(|| emu.round() as i64)
}

fn token_values(state: &TokenPanelState) -> Option<TokenValues> {
    if state.light == state.original_light && state.dark == state.original_dark {
        if let Some(values) = &state.original_values {
            return Some(values.clone());
        }
    }
    match state.kind.as_str() {
        "color" if valid_hex_colour(&state.light) && valid_hex_colour(&state.dark) => Some(TokenValues::Color {
            light: state.light.clone(),
            dark: state.dark.clone(),
        }),
        "font_family" if !state.light.trim().is_empty() && !state.dark.trim().is_empty() => Some(TokenValues::FontFamily {
            light: state.light.clone(),
            dark: state.dark.clone(),
        }),
        "font_size" => Some(TokenValues::FontSize {
            light: token_number_emu(&state.light, false)?,
            dark: token_number_emu(&state.dark, false)?,
        }),
        "spacing" => Some(TokenValues::Spacing {
            light: token_number_emu(&state.light, true)?,
            dark: token_number_emu(&state.dark, true)?,
        }),
        "radius" => Some(TokenValues::Radius {
            light: token_number_emu(&state.light, true)?,
            dark: token_number_emu(&state.dark, true)?,
        }),
        _ => None,
    }
}

fn valid_hex_colour(value: &str) -> bool {
    value.len() == 7 && value.starts_with('#') && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn token_text_field(ui: &mut Ui, palette: &theme::Palette, id: Id, width: f32, value: &mut String, hint: &str, enabled: bool) {
    let hint = if value.is_empty() { hint } else { "" };
    let _ = widgets::edit_line(ui, palette, value, widgets::Field { hint, width: width.max(40.0), id: Some(id), enabled, ..Default::default() });
}

fn panel_bind_target(state: &TokenPanelState) -> Option<BindTarget> {
    Some(match state.target.as_str() {
        "style_color" => BindTarget::StyleColor,
        "style_fill" => BindTarget::StyleFill,
        "style_stroke" => BindTarget::StyleStroke,
        "style_font_family" => BindTarget::StyleFontFamily,
        "style_font_size" => BindTarget::StyleFontSize,
        "style_corner_radius" => BindTarget::StyleCornerRadius,
        "layout_gap" => BindTarget::LayoutGap,
        "layout_row_gap" => BindTarget::LayoutRowGap,
        "layout_padding" => BindTarget::LayoutPadding,
        "layout_padding_side" => BindTarget::LayoutPaddingSide {
            side: match state.side.as_str() {
                "top" => PaddingSide::Top,
                "right" => PaddingSide::Right,
                "bottom" => PaddingSide::Bottom,
                "left" => PaddingSide::Left,
                _ => return None,
            },
        },
        "breakpoint_gap" | "breakpoint_padding" => {
            let width = state.min_width_css_px.parse::<u32>().ok().filter(|width| *width > 0)?;
            if state.target == "breakpoint_gap" {
                BindTarget::BreakpointGap { min_width_css_px: width }
            } else {
                BindTarget::BreakpointPadding { min_width_css_px: width }
            }
        }
        _ => return None,
    })
}

fn draw_local_token_section(
    ui: &mut Ui,
    palette: &theme::Palette,
    artifact_id: &str,
    revision: u64,
    snapshot_current: bool,
    document: &CanvasDocument,
    design_system: Option<&designllm_contracts::DesignSystemRef>,
    publishes_design_system: bool,
    state: &mut TokenPanelState,
    selected_nodes: &[String],
    first: &mut bool,
) -> Option<(&'static str, serde_json::Value)> {
    use serde_json::json;

    section_head(ui, palette, first, "디자인 토큰", None);
    let mut action = None;
    if state.baseline_revision.is_none() {
        state.refresh_new_revision(revision);
    }
    state.reconcile_if_clean(document, revision);

    let mut local_draft_pending = state.has_local_draft();
    if local_draft_pending {
        ui.horizontal(|ui| {
            ui.small(egui::RichText::new("로컬 토큰 초안이 저장되지 않았습니다. 공유 설정을 바꾸기 전에 저장하거나 취소하세요").color(palette.danger));
            if widgets::labeled_button(ui, palette, Some("close"), "초안 취소", false).clicked() {
                state.discard_local_draft(document, revision);
                local_draft_pending = false;
            }
        });
    }
    let mut published = publishes_design_system;
    let publish_enabled = snapshot_current && design_system.is_none() && !local_draft_pending;
    let published_changed = row(ui, palette, Some("공유 게시"), |ui| {
        ui.add_enabled_ui(publish_enabled, |ui| widgets::checkbox(ui, palette, &mut published, "이 캔버스를 디자인 시스템으로 공개")).inner.changed()
    });
    if published_changed {
        action = Some(("canvas.design_system.publish", json!({ "publish": published })));
    }
    if design_system.is_some() && !publishes_design_system {
        ui.small(egui::RichText::new("공유 원본에 연결된 작업물은 게시할 수 없습니다. 먼저 연결을 해제하세요").color(palette.ink_3));
    }

    let current_source_name = design_system.map_or_else(
        || String::from("연결 안 됨"),
        |reference| {
            state
                .design_systems
                .iter()
                .find(|system| system.artifact_id == reference.artifact_id)
                .map(|system| format!("{} · r{}", system.title, reference.revision))
                .unwrap_or_else(|| format!("{} · r{}", reference.artifact_id, reference.revision))
        },
    );
    let source_picker_enabled = snapshot_current
        && !publishes_design_system
        && !local_draft_pending
        && (design_system.is_some() || state.design_systems_loaded);
    let next_source = row(ui, palette, Some("공유 원본"), |ui| {
        ui.add_enabled_ui(source_picker_enabled, |ui| {
            let width = ui.available_width();
            let button = widgets::dropdown(ui, palette, &current_source_name, None, width);
            let mut picked: Option<Option<(String, u64)>> = None;
            egui::Popup::from_toggle_button_response(&button)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_width(width.max(180.0));
                    if widgets::menu_item(ui, palette, None, "연결 안 함", "", design_system.is_none()).clicked() {
                        picked = Some(None);
                        ui.close();
                    }
                    for system in &state.design_systems {
                        if system.artifact_id == artifact_id {
                            continue;
                        }
                        let label = format!("{} · r{}", system.title, system.revision);
                        if widgets::menu_item(ui, palette, None, &label, "", design_system.is_some_and(|reference| reference.artifact_id == system.artifact_id)).clicked() {
                            picked = Some(Some((system.artifact_id.clone(), system.revision)));
                            ui.close();
                        }
                    }
                });
            picked
        }).inner
    });
    if let Some(next_source) = next_source {
        let (source_id, expected_system_revision) = match next_source {
            Some((source_id, source_revision)) => (Some(source_id), Some(source_revision)),
            None => (None, None),
        };
        let same_source = match (design_system, source_id.as_deref(), expected_system_revision) {
            (Some(reference), Some(source_id), Some(source_revision)) => {
                reference.artifact_id == source_id && reference.revision == source_revision
            }
            (None, None, None) => true,
            _ => false,
        };
        if !same_source && action.is_none() {
            let mut args = json!({ "design_system": source_id });
            if let Some(revision) = expected_system_revision {
                args["expected_system_revision"] = json!(revision);
            }
            action = Some((
                "canvas.design_system.set",
                args,
            ));
        }
    }
    if !state.design_systems_loaded {
        ui.small(egui::RichText::new("공유 디자인 시스템 목록을 읽는 중입니다").color(palette.ink_3));
    } else if state.design_systems_error {
        ui.small(egui::RichText::new("공유 디자인 시스템 목록을 읽지 못했습니다").color(palette.danger));
        if widgets::labeled_button(ui, palette, Some("refresh"), "다시 읽기", false).clicked() && action.is_none() {
            action = Some(("canvas.design_system.list", json!({})));
        }
    } else if widgets::labeled_button(ui, palette, Some("refresh"), "공유 목록 새로고침", false).clicked() && action.is_none() {
        action = Some(("canvas.design_system.list", json!({})));
    }

    let shared_snapshot_current = design_system.is_some_and(|reference| {
        state.shared_source_id.as_deref() == Some(reference.artifact_id.as_str())
            && state.shared_source_revision == Some(reference.revision)
            && !state.shared_source_loading
            && !state.shared_source_error
    });
    if let Some(next_source) = row(ui, palette, Some("연결 토큰 출처"), |ui| {
        if shared_snapshot_current {
            menu_field(ui, palette, TOKEN_SOURCES, if state.binding_source == TokenSource::Shared { "shared" } else { "local" })
        } else {
            ui.label("로컬");
            None
        }
    }) {
        state.binding_source = if next_source == "shared" { TokenSource::Shared } else { TokenSource::Local };
        if state.binding_source == TokenSource::Shared && state.selected_shared_token.is_none() {
            state.selected_shared_token = state.shared_tokens.keys().next().cloned();
        }
    }
    if design_system.is_some() && state.shared_source_loading {
        ui.small(egui::RichText::new("공유 토큰을 읽는 중입니다").color(palette.ink_3));
    } else if design_system.is_some() && state.shared_source_error {
        ui.small(egui::RichText::new("공유 토큰을 읽지 못했습니다. 목록을 다시 읽어 주세요").color(palette.danger));
        if widgets::labeled_button(ui, palette, Some("refresh"), "공유 원본 다시 읽기", false).clicked() && action.is_none() {
            action = Some(("canvas.design_system.list", json!({})));
        }
    }

    let current_name = state
        .selected_token
        .as_deref()
        .and_then(|id| document.tokens.get(id))
        .map_or_else(
            || if state.baseline_existed { "토큰이 삭제됨" } else { "새 토큰" },
            |token| token.name.as_str(),
        );
    let picked = row(ui, palette, Some("토큰"), |ui| {
        let width = ui.available_width();
        let button = widgets::dropdown(ui, palette, current_name, None, width);
        let mut picked: Option<Option<String>> = None;
        egui::Popup::from_toggle_button_response(&button)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_width(width.max(180.0));
                if widgets::menu_item(ui, palette, Some("add"), "새 토큰", "", state.selected_token.is_none()).clicked() {
                    picked = Some(None);
                    ui.close();
                }
                for (token_id, token) in &document.tokens {
                    let label = format!("{} · {}", token.name, token_id);
                    if widgets::menu_item(ui, palette, None, &label, "", state.selected_token.as_deref() == Some(token_id)).clicked() {
                        picked = Some(Some(token_id.clone()));
                        ui.close();
                    }
                }
            });
        picked
    });
    if let Some(picked) = picked {
        match picked {
            Some(token_id) => {
                if let Some(token) = document.tokens.get(&token_id) {
                    state.load(&token_id, token, revision);
                } else {
                    state.begin_new(revision);
                }
            }
            None => state.begin_new(revision),
        }
    }

    let mut stale = !snapshot_current || (state.is_stale(document, revision) && state.has_local_draft());
    let mode = document.token_mode.unwrap_or(TokenMode::Light);
    let current_mode = match mode {
        TokenMode::Light => "light",
        TokenMode::Dark => "dark",
    };
    if let Some(next) = row(ui, palette, Some("모드"), |ui| {
        ui.add_enabled_ui(!stale, |ui| menu_field(ui, palette, TOKEN_MODES, current_mode)).inner
    }) {
        action = Some(("canvas.token.mode", json!({ "mode": next })));
    }

    if stale {
        let message = if snapshot_current { "작업물이 바뀌었습니다. 저장 전에 최신 토큰을 불러오세요" } else { "최신 작업물 상태를 읽는 중입니다" };
        ui.small(egui::RichText::new(message).color(palette.danger));
        let id_conflict = state.selected_token.is_none() && document.tokens.contains_key(&state.token_id);
        let label = if state.selected_token.as_deref().is_some_and(|id| document.tokens.contains_key(id)) || id_conflict {
            "최신 토큰 불러오기"
        } else if state.baseline_existed {
            "새 토큰으로 시작"
        } else {
            "기준 새로고침"
        };
        if snapshot_current && widgets::labeled_button(ui, palette, Some("refresh"), label, false).clicked() {
            state.reload(document, revision);
            stale = state.is_stale(document, revision) && state.has_local_draft();
        }
    }

    let selected_definition = state.selected_token.as_deref().and_then(|id| document.tokens.get(id));
    let editing_existing = selected_definition.is_some();
    let form_id = |field: &str| Id::new(("canvas-token", artifact_id, field));
    row(ui, palette, Some("ID"), |ui| {
        let width = ui.available_width();
        token_text_field(ui, palette, form_id("id"), width, &mut state.token_id, "brand.ink", !editing_existing && !stale)
    });
    row(ui, palette, Some("이름"), |ui| {
        let width = ui.available_width();
        token_text_field(ui, palette, form_id("name"), width, &mut state.name, "브랜드 색", !stale)
    });
    if let Some(kind) = row(ui, palette, Some("종류"), |ui| {
        ui.add_enabled_ui(!editing_existing && !stale, |ui| menu_field(ui, palette, TOKEN_KINDS, &state.kind)).inner
    }) {
        set_token_kind(state, kind);
    }
    let unit = if matches!(state.kind.as_str(), "font_size" | "spacing" | "radius") { " px" } else { "" };
    let value_hint = if state.kind == "color" { "#20252D" } else { "값" };
    let light_label = format!("라이트{unit}");
    row(ui, palette, Some(&light_label), |ui| {
        let width = ui.available_width();
        token_text_field(ui, palette, form_id("light"), width, &mut state.light, value_hint, !stale)
    });
    let dark_label = format!("다크{unit}");
    row(ui, palette, Some(&dark_label), |ui| {
        let width = ui.available_width();
        token_text_field(ui, palette, form_id("dark"), width, &mut state.dark, value_hint, !stale)
    });

    let values = token_values(state);
    let token_id_conflict = !editing_existing && document.tokens.contains_key(&state.token_id);
    let form_valid = !state.token_id.trim().is_empty() && !state.name.trim().is_empty() && values.is_some();
    let dirty = selected_definition.is_some_and(|definition| {
        definition.name != state.name || values.as_ref().is_none_or(|values| values != &definition.values)
    });
    let existing_id = state.selected_token.as_deref().filter(|id| document.tokens.contains_key(*id)).map(str::to_owned);
    let buttons = row(ui, palette, None, |ui| {
        let can_save = form_valid && !stale && !token_id_conflict && (!editing_existing || dirty);
        let save = ui.add_enabled_ui(can_save, |ui| widgets::labeled_button(ui, palette, Some("check"), "토큰 저장", true)).inner;
        let remove = ui.add_enabled_ui(existing_id.is_some() && !dirty && !stale, |ui| widgets::labeled_button(ui, palette, Some("delete"), "삭제", false)).inner;
        (save.clicked(), remove.clicked())
    });
    if buttons.0 && action.is_none() {
        if let Some(values) = values {
            let token_id = state.token_id.trim().to_owned();
            state.selected_token = Some(token_id.clone());
            action = Some(("canvas.token.put", json!({ "token_id": token_id, "name": state.name.trim(), "values": values })));
        }
    }
    if buttons.1 && action.is_none() {
        if let Some(token_id) = existing_id.as_deref() {
            let token_id = token_id.to_owned();
            action = Some(("canvas.token.remove", json!({ "token_id": token_id })));
        }
    }
    if token_id_conflict && !stale {
        ui.small(egui::RichText::new("이미 있는 ID입니다. 토큰 목록에서 불러오세요").color(palette.ink_3));
    }
    if dirty {
        ui.small(egui::RichText::new("저장되지 않음").color(palette.ink_3));
    }

    let selected_shared = if state.binding_source == TokenSource::Shared && shared_snapshot_current {
        let current = state
            .selected_shared_token
            .as_deref()
            .and_then(|id| state.shared_tokens.get(id));
        let current_name = current.map_or("공유 토큰 선택", |token| token.name.as_str());
        let picked = row(ui, palette, Some("공유 토큰"), |ui| {
            let width = ui.available_width();
            let button = widgets::dropdown(ui, palette, current_name, None, width);
            let mut picked = None;
            egui::Popup::from_toggle_button_response(&button)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_width(width.max(180.0));
                    for (token_id, token) in &state.shared_tokens {
                        let label = format!("{} · {}", token.name, token_id);
                        if widgets::menu_item(ui, palette, None, &label, "", state.selected_shared_token.as_deref() == Some(token_id)).clicked() {
                            picked = Some(token_id.clone());
                            ui.close();
                        }
                    }
                });
            picked
        });
        if let Some(token_id) = picked {
            state.selected_shared_token = Some(token_id);
        }
        state.selected_shared_token.as_deref().and_then(|id| state.shared_tokens.get(id))
    } else {
        None
    };
    if state.binding_source == TokenSource::Shared && shared_snapshot_current {
        if let Some(token) = selected_shared {
            let (kind, light, dark) = token_display_values(&token.values);
            ui.small(egui::RichText::new(format!("{} · {}", kind, token.name)).color(palette.ink_3));
            row(ui, palette, Some("라이트 값"), |ui| { ui.label(light); });
            row(ui, palette, Some("다크 값"), |ui| { ui.label(dark); });
        } else {
            ui.small(egui::RichText::new("공유 원본에 토큰이 없습니다").color(palette.ink_3));
        }
    }

    if !selected_nodes.is_empty() {
        let binding_kind = if state.binding_source == TokenSource::Shared {
            selected_shared.map_or(state.kind.as_str(), |token| token_kind_key(&token.values))
        } else {
            state.kind.as_str()
        };
        let target_items = token_targets(binding_kind);
        if !target_items.iter().any(|item| item.0 == state.target) {
            state.target = target_items.first().map_or("", |item| item.0).to_owned();
        }
        if let Some(target) = row(ui, palette, Some("연결 대상"), |ui| {
            ui.add_enabled_ui(snapshot_current, |ui| menu_field(ui, palette, target_items, &state.target)).inner
        }) {
            state.target = target.to_owned();
        }
        if state.target == "layout_padding_side" {
            if let Some(side) = row(ui, palette, Some("면"), |ui| {
                ui.add_enabled_ui(snapshot_current, |ui| menu_field(ui, palette, TOKEN_SIDES, &state.side)).inner
            }) {
                state.side = side.to_owned();
            }
        } else if matches!(state.target.as_str(), "breakpoint_gap" | "breakpoint_padding") {
            let current = state.min_width_css_px.parse::<f64>().ok();
            if let Some(width) = row(ui, palette, Some("최소 너비"), |ui| {
                let width = ui.available_width();
                field(ui, palette, form_id("breakpoint"), width, "", current, "px", 0, snapshot_current)
            }) {
                if width.is_finite() && width >= 1.0 && width <= f64::from(u32::MAX) {
                    state.min_width_css_px = format!("{}", width.round() as u32);
                }
            }
        }

        let target = panel_bind_target(state);
        let bound_count = target.as_ref().map_or(0, |target| {
            document
                .bindings
                .iter()
                .filter(|binding| selected_nodes.iter().any(|node_id| node_id == &binding.node_id) && &binding.target == target)
                .count()
        });
        ui.small(egui::RichText::new(format!("{}개 선택 · {bound_count}개 연결", selected_nodes.len())).color(palette.ink_3));
        let binding_token_id = match state.binding_source {
            TokenSource::Local => existing_id.as_deref(),
            TokenSource::Shared => selected_shared.map(|_| state.selected_shared_token.as_deref()).flatten(),
        };
        let can_bind = action.is_none()
            && snapshot_current
            && !selected_nodes.is_empty()
            && binding_token_id.is_some()
            && (state.binding_source == TokenSource::Local || shared_snapshot_current)
            && (state.binding_source == TokenSource::Shared || !dirty)
            && target.is_some();
        let can_unbind = action.is_none() && snapshot_current && !selected_nodes.is_empty() && bound_count > 0 && target.is_some();
        let binding_buttons = row(ui, palette, None, |ui| {
            let bind = ui.add_enabled_ui(can_bind, |ui| widgets::labeled_button(ui, palette, Some("link"), "연결", true)).inner.clicked();
            let unbind = ui.add_enabled_ui(can_unbind, |ui| widgets::labeled_button(ui, palette, Some("close"), "연결 해제", false)).inner.clicked();
            (bind, unbind)
        });
        if (binding_buttons.0 || binding_buttons.1) && action.is_none() {
            if let Some(target) = target {
                let mut args = json!({ "target": state.target });
                match target {
                    BindTarget::LayoutPaddingSide { .. } => args["side"] = json!(state.side),
                    BindTarget::BreakpointGap { min_width_css_px } | BindTarget::BreakpointPadding { min_width_css_px } => {
                        args["min_width_css_px"] = json!(min_width_css_px);
                    }
                    _ => {}
                }
                if binding_buttons.0 {
                    if let Some(token_id) = binding_token_id {
                        args["token_id"] = json!(token_id);
                        args["source"] = json!(if state.binding_source == TokenSource::Shared { "shared" } else { "local" });
                        action = Some(("canvas.token.bind", args));
                    }
                } else {
                    action = Some(("canvas.token.unbind", args));
                }
            }
        }
    }
    ui.add_space(8.0);
    action
}

#[cfg(test)]
mod local_token_panel_tests {
    use super::*;

    #[test]
    fn an_unchanged_numeric_token_keeps_its_exact_emu_values() {
        let definition = TokenDef {
            name: String::from("Fine spacing"),
            values: TokenValues::Spacing { light: 1, dark: 42_863 },
        };
        let mut state = TokenPanelState::default();
        state.load("spacing.fine", &definition, 4);

        assert_ne!(state.light, "0", "sub-pixel values should remain visible in the form");
        assert_eq!(token_values(&state), Some(definition.values.clone()));
    }

    #[test]
    fn a_late_shared_source_reply_cannot_replace_a_newer_selection() {
        let mut state = TokenPanelState::default();
        let stale_generation = state.begin_shared_source_load("source-a", 3);
        let current_generation = state.begin_shared_source_load("source-b", 8);
        assert!(!state.install_shared_source(stale_generation, "source-a", 3, Default::default()));
        assert_eq!(state.shared_source_id.as_deref(), Some("source-b"));
        assert!(state.shared_tokens.is_empty());
        assert!(state.install_shared_source(current_generation, "source-b", 8, Default::default()));
        assert!(state.shared_source_loaded, "an empty but valid token map is still loaded");
        assert!(!state.shared_source_loading);
    }

    #[test]
    fn a_local_token_draft_is_detected_before_gui_shared_settings_change() {
        let mut state = TokenPanelState::default();
        state.begin_new(2);
        assert!(!state.has_local_draft());
        state.token_id = String::from("brand.new");
        state.name = String::from("Uncommitted");
        assert!(state.has_local_draft());
        state.discard_local_draft(&CanvasDocument::default(), 3);
        assert!(!state.has_local_draft(), "an explicit cancel clears the uncommitted definition draft");
    }

    #[test]
    fn baseline_detects_revision_changes_and_removed_or_recreated_token_ids() {
        let token = TokenDef {
            name: String::from("Brand ink"),
            values: TokenValues::Color { light: "#112233".into(), dark: "#445566".into() },
        };
        let mut document = CanvasDocument::default();
        document.tokens.insert("brand.ink".into(), token.clone());
        let mut state = TokenPanelState::default();
        state.load("brand.ink", &token, 7);
        assert!(!state.is_stale(&document, 7));
        assert!(state.is_stale(&document, 8), "an unchanged token at a newer revision is stale until explicitly reloaded");

        document.tokens.remove("brand.ink");
        assert!(state.is_stale(&document, 7), "an externally removed token cannot become a new-token draft");
        document.tokens.insert("brand.ink".into(), token);
        assert!(state.is_stale(&document, 8), "recreating the same id at a newer revision does not revive an old edit session");
    }

    #[test]
    fn numeric_token_inputs_convert_canvas_pixels_to_emu() {
        let mut state = TokenPanelState::default();
        state.kind = String::from("font_size");
        state.light = String::from("16");
        state.dark = String::from("20");

        assert_eq!(token_values(&state), Some(TokenValues::FontSize { light: 152_400, dark: 190_500 }));
    }
}

#[cfg(test)]
mod node_properties_projection_tests {
    use super::*;

    #[test]
    fn a_large_nested_node_projects_only_fields_the_panel_uses() {
        let leaves = (0..256)
            .map(|index| serde_json::json!({
                "id": format!("leaf-{index}"),
                "kind": "rectangle",
                "bounds": { "x": index * 1000, "y": 0, "width": 900, "height": 900 },
            }))
            .collect::<Vec<_>>();
        let tree: Node = serde_json::from_value(serde_json::json!({
            "id": "outer.frame", "kind": "rectangle", "style": { "fill": "#123456" },
            "bounds": { "x": 0, "y": 0, "width": 500000, "height": 500000 },
            "layout": { "mode": "grid", "grid_columns": 2 },
            "children": [{
                "id": "inner.group", "kind": "group",
                "bounds": { "x": 1000, "y": 1000, "width": 400000, "height": 400000 },
                "children": leaves,
            }],
        }))
        .expect("a large nested group parses");

        let outer = NodeProperties::from_node(&tree);
        let inner = NodeProperties::from_node(&tree.children[0]);
        assert_eq!(outer.id, "outer.frame");
        assert!(outer.has_children);
        assert_eq!(outer.style.fill, "#123456");
        assert_eq!(outer.layout.as_ref().map(|layout| layout.mode.as_str()), Some("grid"));
        assert!(inner.has_children);
        assert_eq!(inner.kind, NodeKind::Group);
        assert!(!format!("{outer:?}").contains("leaf-0"));
        assert!(!format!("{inner:?}").contains("leaf-255"), "projection does not own descendant nodes");
        assert_eq!(tree.children[0].children.len(), 256, "the model still owns all nested children");
    }
}

impl DesktopApp {
    /// Whether the parent of `id` lays its children out (a stack or a grid), so that `꽉 채우기` applies (§9.1).
    fn parent_lays_out(&self, id: &str) -> bool {
        fn parent_of<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
            nodes.iter().find_map(|node| {
                if node.children.iter().any(|child| child.id == id) {
                    Some(node)
                } else {
                    parent_of(&node.children, id)
                }
            })
        }
        let pages: Vec<&[Node]> = match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => document.pages.iter().map(|page| page.nodes.as_slice()).collect(),
            Some(ArtifactBody::Slides(document)) => document.pages.iter().map(|page| page.nodes.as_slice()).collect(),
            _ => Vec::new(),
        };
        pages
            .into_iter()
            .find_map(|nodes| parent_of(nodes, id))
            .and_then(|parent| parent.layout.as_ref())
            .is_some_and(|layout| matches!(layout.mode.as_str(), "row" | "column" | "grid"))
    }

    /// Sections for a selected node, in the order of §9.1 and the selection kinds of §9.2 (mockups `canvas-board`,
    /// `state-multiselect`). With several nodes selected a value they disagree on is an empty field showing `여러 값`;
    /// an edit there applies that one value to every selected node.
    fn node_sections(&mut self, ui: &mut Ui, palette: &theme::Palette, node: &NodeProperties, first: &mut bool) {
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        let unit = unit_for(kind);
        let geometry_shown = feature_shown(self.feature_support(Feature::Geometry));
        let geometry_editable = feature_editable(self.feature_support(Feature::Geometry));
        let many = self.selected_nodes.len() > 1;
        let component_instance_id = if !many && kind == ArtifactKind::Canvas {
            self.artifact.as_ref().and_then(|artifact| super::doc_tree::find_node(artifact, &node.id))
                .filter(|selected| selected.ext.instance.is_some()).map(|selected| selected.id.clone())
        } else { None };
        // Every selected node, nested ones too (`selected_siblings` only answers for nodes that share a parent).
        let group: Vec<NodeProperties> = if many {
            self.selected_nodes_properties()
        } else {
            Vec::new()
        };
        let id = |key: &str| Id::new(("node-field", node.id.as_str(), key.to_owned()));
        // §9.1: a child of a stack or a grid has no free position; 위치 holds only `자체 정렬 ▾`.
        // Every selected node must be such a child (a multi-selection of stack children shows only 자체 정렬 too).
        let laid_out_child = self.parent_lays_out(&node.id)
            && group.iter().all(|other| self.parent_lays_out(&other.id))
            && feature_editable(self.feature_support(Feature::GridLayout));
        if geometry_shown && laid_out_child {
            section_head(ui, palette, first, "위치", None);
            let current = match node.layout.as_ref().and_then(|layout| layout.align_self) {
                Some(designllm_contracts::AlignSelf::Start) => "start",
                Some(designllm_contracts::AlignSelf::Center) => "center",
                Some(designllm_contracts::AlignSelf::End) => "end",
                Some(designllm_contracts::AlignSelf::Stretch) => "stretch",
                _ => "auto",
            };
            let items = [
                ("auto", "align-v-center", "부모 따름"),
                ("start", "align-top", "시작"),
                ("center", "align-v-center", "가운데"),
                ("end", "align-bottom", "끝"),
                ("stretch", "items-stretch", "늘이기"),
            ];
            let align_of = |other: &NodeProperties| other.layout.as_ref().and_then(|layout| layout.align_self);
            let current = if agree(&group, node, align_of) { current } else { "" };
            if let Some(align) = row(ui, palette, Some("자체 정렬"), |ui| menu_field(ui, palette, &items, current)) {
                // Every selected node in one change set (one undo step).
                let ids: Vec<String> = if many { group.iter().map(|other| other.id.clone()).collect() } else { vec![node.id.clone()] };
                self.invoke_ui("props.align_self", json!({ "align": align, "node_ids": ids }));
            }
            ui.add_space(8.0);
        } else if geometry_shown {
            section_head(ui, palette, first, "위치", None);
            let aligns = [
                [("align-left", "왼쪽", "left"), ("align-h-center", "가운데", "h-center"), ("align-right", "오른쪽", "right")],
                [("align-top", "위쪽", "top"), ("align-v-center", "중간", "v-center"), ("align-bottom", "아래쪽", "bottom")],
            ];
            for (index, line) in aligns.iter().enumerate() {
                if index > 0 {
                    ui.add_space(2.0);
                }
                let items: Vec<(&str, &str)> = line.iter().map(|(icon, label, _)| (*icon, *label)).collect();
                if let Some(chosen) = acts(ui, palette, &items) {
                    self.invoke_ui("props.align", json!({ "edge": line[chosen].2 }));
                }
            }
            // Spreading needs a middle node: three or more (§7).
            if self.selected_nodes.len() >= 3 {
                ui.add_space(2.0);
                if let Some(chosen) = acts(ui, palette, &[("distribute-h", "가로 분배"), ("distribute-v", "세로 분배")]) {
                    self.invoke_ui("props.distribute", json!({ "axis": (["h", "v"][chosen]) }));
                }
            }
            ui.add_space(4.0);
            let x = agree(&group, node, |other| other.bounds.x).then_some(node.bounds.x as f64 / unit.emu());
            let y = agree(&group, node, |other| other.bounds.y).then_some(node.bounds.y as f64 / unit.emu());
            let (new_x, new_y) = row(ui, palette, None, |ui| {
                let width = (ui.available_width() - 8.0) / 2.0;
                let new_x = field(ui, palette, id("x"), width, "X", x, "", unit.decimals(), geometry_editable);
                let new_y = field(ui, palette, id("y"), width, "Y", y, "", unit.decimals(), geometry_editable);
                (new_x, new_y)
            });
            let current = (node.bounds.x as f64 / unit.emu(), node.bounds.y as f64 / unit.emu());
            if let Some(new) = new_x {
                self.invoke_ui("props.position", if many { json!({ "x": new }) } else { json!({ "x": new, "y": current.1 }) });
            }
            if let Some(new) = new_y {
                self.invoke_ui("props.position", if many { json!({ "y": new }) } else { json!({ "x": current.0, "y": new }) });
            }
            ui.add_space(8.0);
        }
        let frame = node.kind == NodeKind::Rectangle && (node.has_children || node.layout.is_some());
        let default_layout = LayoutProperties::default();
        let layout = node.layout.as_ref().unwrap_or(&default_layout);
        if geometry_shown {
            section_head(ui, palette, first, "크기", None);
            // The size modes live in the frame's own layout, so they are offered for one selected frame only.
            let modes_shown = !many && frame && feature_editable(self.feature_support(Feature::CanvasLayout));
            let parent_lays_out = self.parent_lays_out(&node.id);
            for (label, key, value, mode) in [
                ("너비", "w", node.bounds.width, layout.width_mode.as_str()),
                ("높이", "h", node.bounds.height, layout.height_mode.as_str()),
            ] {
                let mode = if mode.is_empty() { "fixed" } else { mode };
                let shown = agree(&group, node, |other| if key == "w" { other.bounds.width } else { other.bounds.height })
                    .then_some(value as f64 / unit.emu());
                let (new, choice) = row(ui, palette, Some(label), |ui| {
                    let width = if modes_shown { ui.available_width() - 8.0 - 104.0 } else { ui.available_width() };
                    let new = field(ui, palette, id(key), width, "", shown, unit.label(), unit.decimals(), geometry_editable);
                    let mut choice = None;
                    if modes_shown {
                        let button = widgets::dropdown(ui, palette, size_mode_label(mode), None, 104.0);
                        egui::Popup::from_toggle_button_response(&button)
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                            .show(|ui| {
                                ui.set_width(180.0);
                                for (value, name) in [("fixed", "고정"), ("hug", "콘텐츠 맞춤"), ("fill", "꽉 채우기")] {
                                    if value == "fill" && !parent_lays_out {
                                        continue;
                                    }
                                    if widgets::menu_item(ui, palette, Some(size_mode_icon(value)), name, "", mode == value).clicked() {
                                        choice = Some(value);
                                        ui.close();
                                    }
                                }
                            });
                    }
                    (new, choice)
                });
                if let Some(new) = new {
                    self.invoke_ui("props.size", if key == "w" { json!({ "width": new }) } else { json!({ "height": new }) });
                }
                if let Some(choice) = choice.filter(|choice| *choice != mode) {
                    self.invoke_ui("props.size_mode", if key == "w" { json!({ "width_mode": choice }) } else { json!({ "height_mode": choice }) });
                }
            }
            ui.add_space(8.0);
        }
        let style = &node.style;
        let effects_shown = feature_shown(self.feature_support(Feature::Effects));
        let effects_editable = feature_editable(self.feature_support(Feature::Effects));
        let shaped_kind = |other: &NodeProperties| !matches!(other.kind, NodeKind::Group | NodeKind::Line);
        let shaped = shaped_kind(node) && group.iter().all(shaped_kind);
        if effects_shown && shaped {
            section_head(ui, palette, first, "모양", None);
            let opacity = agree(&group, node, |other| other.style.opacity_milli.unwrap_or(1000))
                .then_some(f64::from(style.opacity_milli.unwrap_or(1000)) / 10.0);
            if let Some(new) = row(ui, palette, Some("불투명도"), |ui| {
                let width = ui.available_width();
                field(ui, palette, id("opacity"), width, "", opacity, "%", 0, effects_editable)
            }) {
                self.invoke_ui("props.opacity", json!({ "percent": new.clamp(0.0, 100.0) }));
            }
            let rounded = |other: &NodeProperties| matches!(other.kind, NodeKind::Rectangle | NodeKind::Image);
            if rounded(node) && group.iter().all(rounded) {
                let radius = agree(&group, node, |other| other.style.corner_radius.unwrap_or(0))
                    .then_some(style.corner_radius.unwrap_or(0) as f64 / EMU_PER_PX);
                if let Some(new) = row(ui, palette, Some("모서리"), |ui| {
                    let width = ui.available_width();
                    field(ui, palette, id("radius"), width, "", radius, "px", 0, effects_editable)
                }) {
                    self.invoke_ui("props.corner_radius", json!({ "value": new.max(0.0) }));
                }
            }
            ui.add_space(8.0);
        }
        let images = self.feature_support(Feature::Images);
        if kind == ArtifactKind::Canvas && !many && node.kind == NodeKind::Image && feature_shown(images) {
            let crop_edit_open = self.image_crop_edit_open(&node.id);
            let editable = feature_editable(images) && node.locked != Some(true)
                && !self.image_property_change_pending(&node.id) && !crop_edit_open;
            section_head(ui, palette, first, "이미지", None);
            let current_fit = node.image_fit.unwrap_or(designllm_contracts::ImageFit::Fill);
            let fit = match current_fit {
                designllm_contracts::ImageFit::Fill => "fill",
                designllm_contracts::ImageFit::Contain => "contain",
                designllm_contracts::ImageFit::Cover => "cover",
            };
            let fits = [("fill", "", "늘이기"), ("contain", "", "맞추기"), ("cover", "", "채우기")];
            let selected_fit = row(ui, palette, Some("맞춤"), |ui| {
                ui.add_enabled_ui(editable, |ui| menu_field(ui, palette, &fits, fit)).inner
            });
            if let Some(next_fit) = selected_fit.filter(|next| *next != fit) {
                self.invoke_ui("props.image_fit", json!({ "fit": next_fit }));
            }
            let alt = node.alt.as_deref().unwrap_or_default();
            if let Some(alt) = row(ui, palette, Some("대체 텍스트"), |ui| {
                let width = ui.available_width();
                text_line(ui, palette, id("image-alt"), width, alt, editable)
            }) {
                self.invoke_ui("props.image_alt", json!({ "alt": alt }));
            }
            let replacement_stage = self.pending_image_replacement.as_ref()
                .filter(|pending| pending.intent.node_id == node.id)
                .map(|pending| pending.stage);
            if let Some(stage) = replacement_stage {
                let cancel = row(ui, palette, Some("교체"), |ui| {
                    ui.horizontal(|ui| {
                        ui.label(match stage {
                            super::ImageReplacementStage::Importing => "이미지를 가져오는 중",
                            super::ImageReplacementStage::Applying => "교체를 적용하는 중",
                        });
                        ui.add_enabled_ui(stage == super::ImageReplacementStage::Importing, |ui| widgets::labeled_button(ui, palette, None, "취소", false)).inner.clicked()
                    }).inner
                });
                if cancel { self.cancel_canvas_image_replacement(); }
            } else if row(ui, palette, Some("원본"), |ui| {
                let enabled = editable && self.canvas_image_replacement_available(&node.id);
                let response = ui.add_enabled_ui(enabled, |ui| widgets::labeled_button(ui, palette, None, "이미지 교체", true)).inner;
                let response = if enabled { response } else {
                    response.on_disabled_hover_text("잠긴 이미지, 컴포넌트 원본·인스턴스와 그림 연결 이미지는 이 경로로 교체할 수 없습니다")
                };
                response.clicked()
            }) {
                if let Err(message) = self.begin_canvas_image_replacement() {
                    self.set_error(message);
                }
            }
            self.draw_canvas_image_crop_controls(ui, palette, node);
            ui.add_space(8.0);
        }
        if kind == ArtifactKind::Canvas && !many && node.kind == NodeKind::Video && feature_shown(images) {
            let editable = feature_editable(images) && node.locked != Some(true);
            section_head(ui, palette, first, "동영상", None);
            let alt = node.alt.as_deref().unwrap_or_default();
            if let Some(alt) = row(ui, palette, Some("대체 텍스트"), |ui| {
                let width = ui.available_width();
                text_line(ui, palette, id("video-alt"), width, alt, editable)
            }) {
                self.invoke_ui("props.image_alt", json!({ "alt": alt }));
            }
            ui.add_space(8.0);
        }
        if kind == ArtifactKind::Canvas && !many && node.kind == NodeKind::Path && feature_shown(self.feature_support(Feature::VectorPaths)) {
            self.path_section(ui, palette, node, first);
        }
        if !many && frame && feature_editable(self.feature_support(Feature::CanvasLayout)) {
            self.layout_section(ui, palette, node, &layout, first);
        }
        let all_text = node.kind == NodeKind::Text && group.iter().all(|other| other.kind == NodeKind::Text);
        if all_text && feature_shown(self.feature_support(Feature::TextStyle)) {
            self.text_section(ui, palette, node, &group, first);
        }
        let has_fill = |other: &NodeProperties| paints(&other.style.fill);
        // A multi-selection shows a style section only when every selected node has it (§9.2 "같은 값만").
        let fillable = |other: &NodeProperties| matches!(other.kind, NodeKind::Rectangle | NodeKind::Ellipse | NodeKind::Path);
        if geometry_shown && fillable(node) && group.iter().all(fillable) {
            // A locked node's fill is shown, not edited: the engine refuses any change to it, and to a cell of a locked table.
            let unlocked = all_unlocked(node, &group) && !self.in_locked_canvas_table(node, &group);
            if has_fill(node) || group.iter().any(has_fill) || style.gradient.is_some() {
                section_head(ui, palette, first, "채우기", None);
                let value = agree(&group, node, |other| other.style.fill.clone()).then(|| style.fill.clone());
                let gradient = style.gradient.clone().filter(|_| agree(&group, node, |other| other.style.gradient.clone()));
                let gradients = unlocked && feature_editable(self.feature_support(Feature::Effects)) && commands_available("props.gradient");
                // Smoke captures open the popover without a click (`DESIGNLLM_SMOKE_OPEN=fill_popover`).
                let force_open = gradients && self.smoke_open.as_deref() == Some("fill_popover");
                if force_open {
                    self.smoke_open = None;
                }
                let (new, removed, choice) = row(ui, palette, Some("색"), |ui| ui.add_enabled_ui(unlocked, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let width = ui.available_width() - 4.0 - 28.0;
                    let (new, field_rect, swatch) = match &gradient {
                        Some(gradient) => {
                            let (rect, swatch) = gradient_field(ui, palette, width, gradient);
                            (None, rect, swatch)
                        }
                        None => {
                            let (new, rect) = colour_field_at(ui, palette, id("fill"), width, value.as_deref());
                            let sample = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.center().y - 8.0), Vec2::splat(16.0));
                            let swatch = ui.interact(sample.expand(4.0), id("fill-swatch"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                            (new, rect, swatch)
                        }
                    };
                    let removed = remove_button(ui, palette, "채우기 없애기");
                    let choice = if gradients { fill_popover(palette, &swatch, field_rect, id("fill-popover"), value.as_deref(), gradient.as_ref(), force_open) } else { None };
                    (new, removed, choice)
                }).inner);
                if let Some(new) = new {
                    self.invoke_ui("props.fill", json!({ "color": new }));
                }
                if let Some(choice) = choice {
                    let (command, args) = fill_choice_command(&choice);
                    self.invoke_ui(command, args);
                }
                if removed {
                    self.invoke_ui("props.fill_remove", json!({}));
                }
                ui.add_space(8.0);
            } else if section_head(ui, palette, first, "채우기", unlocked.then_some(("add", "추가"))) {
                self.invoke_ui("props.fill", json!({ "color": "#FFFFFF" }));
            }
        }
        let has_stroke = |other: &NodeProperties| paints(&other.style.stroke) && other.style.stroke_width > 0;
        let strokable = |other: &NodeProperties| matches!(other.kind, NodeKind::Rectangle | NodeKind::Ellipse | NodeKind::Path | NodeKind::Line);
        if geometry_shown && strokable(node) && group.iter().all(strokable) {
            // A locked node's stroke is shown, not edited, as its fill is: the engine refuses the change.
            let unlocked = all_unlocked(node, &group) && !self.in_locked_canvas_table(node, &group);
            if has_stroke(node) || group.iter().any(has_stroke) {
                section_head(ui, palette, first, "테두리", None);
                let colour = agree(&group, node, |other| other.style.stroke.clone()).then(|| style.stroke.clone());
                let (per, label, decimals) = stroke_unit(unit);
                let width = agree(&group, node, |other| other.style.stroke_width).then_some(style.stroke_width as f64 / per);
                // tokens `component.stroke-rows` ①: colour field, width 72, remove.
                let (new_colour, new_width, removed) = row(ui, palette, Some("색"), |ui| ui.add_enabled_ui(unlocked, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let room = ui.available_width() - 4.0 - 72.0 - 4.0 - 28.0;
                    let new_colour = colour_field(ui, palette, id("stroke"), room, colour.as_deref());
                    let new_width = field(ui, palette, id("stroke-width"), 72.0, "", width, label, decimals, true);
                    (new_colour, new_width, remove_button(ui, palette, "테두리 없애기"))
                }).inner);
                if let Some(new) = new_colour {
                    self.invoke_ui("props.stroke", json!({ "color": new }));
                }
                if let Some(new) = new_width {
                    self.invoke_ui("props.stroke_width", json!({ "value": (new.max(0.0) * per).round() }));
                }
                if removed {
                    self.invoke_ui("props.stroke_remove", json!({}));
                }
                if feature_editable(self.feature_support(Feature::Effects)) && commands_available("props.stroke_style") {
                    // ② 모양, and ③ 끝·꺾임 for open paths only.
                    if let Some(dash) = ui.add_enabled_ui(unlocked, |ui| stroke_dash_row(ui, palette, &group, node)).inner {
                        self.invoke_ui("props.stroke_style", json!({ "dash": dash }));
                    }
                    let open = |other: &NodeProperties| matches!(other.kind, NodeKind::Line | NodeKind::Path);
                    if open(node) && group.iter().all(open) {
                        let cap = match style.stroke_cap {
                            Some(designllm_contracts::StrokeCap::Butt) => "butt",
                            Some(designllm_contracts::StrokeCap::Square) => "square",
                            _ => "round",
                        };
                        let join = match style.stroke_join {
                            Some(designllm_contracts::StrokeJoin::Miter) => "miter",
                            Some(designllm_contracts::StrokeJoin::Bevel) => "bevel",
                            _ => "round",
                        };
                        let (new_cap, new_join) = row(ui, palette, Some("끝·꺾임"), |ui| ui.add_enabled_ui(unlocked, |ui| {
                            let width = (ui.available_width() - 8.0) / 2.0;
                            let caps = [("round", "", "둥글게"), ("butt", "", "평평"), ("square", "", "네모")];
                            let joins = [("round", "", "둥글게"), ("miter", "", "뾰족"), ("bevel", "", "깎기")];
                            let new_cap = ui.allocate_ui(vec2(width, theme::FIELD_HEIGHT), |ui| menu_field(ui, palette, &caps, cap)).inner;
                            let new_join = ui.allocate_ui(vec2(width, theme::FIELD_HEIGHT), |ui| menu_field(ui, palette, &joins, join)).inner;
                            (new_cap, new_join)
                        }).inner);
                        if let Some(cap) = new_cap {
                            self.invoke_ui("props.stroke_style", json!({ "cap": cap }));
                        }
                        if let Some(join) = new_join {
                            self.invoke_ui("props.stroke_style", json!({ "join": join }));
                        }
                    }
                }
                ui.add_space(8.0);
            } else if section_head(ui, palette, first, "테두리", unlocked.then_some(("add", "추가"))) {
                self.invoke_ui("props.stroke", json!({ "color": "#566479" }));
            }
        }
        if effects_shown && shaped {
            if let Some(shadow) = style.shadows.first().cloned() {
                section_head(ui, palette, first, "효과", None);
                let (changed, removed) = row(ui, palette, Some("그림자"), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let width = ui.available_width() - 4.0 - 28.0;
                    (shadow_field(ui, palette, id("shadow"), width, &shadow), remove_button(ui, palette, "그림자 없애기"))
                });
                if let Some((x, y, blur)) = changed {
                    self.invoke_ui(
                        "props.shadow",
                        json!({ "x": x, "y": y, "blur": blur.max(0.0), "spread": shadow.spread as f64 / EMU_PER_PX, "color": shadow.color }),
                    );
                }
                if removed {
                    self.invoke_ui("props.shadow_remove", json!({}));
                }
                ui.add_space(8.0);
            } else if section_head(ui, palette, first, "효과", Some(("shadow", "그림자"))) {
                self.invoke_ui("props.shadow", json!({ "x": 0.0, "y": 4.0, "blur": 12.0, "spread": 0.0, "color": "#101622", "opacity": 24.0 }));
            }
        }
        // §9.1 `내보내기 ›`: this selection as PNG·SVG.
        if kind == ArtifactKind::Canvas && feature_editable(self.feature_support(Feature::Interactions)) {
            self.interaction_section(ui, palette, node, &group, many, first);
        }
        if let Some(instance_id) = component_instance_id.filter(|_| self.command_enabled_for_current_kind("component.detach")) {
            section_head(ui, palette, first, "컴포넌트", None);
            if row(ui, palette, Some("연결"), |ui| widgets::labeled_button(ui, palette, None, "원본에서 분리", false).clicked()) {
                self.invoke_ui("component.detach", json!({ "node_id": instance_id }));
            }
            ui.add_space(8.0);
        }
        self.export_row(ui, palette, first, "selection");
    }

    /// The path section of a Canvas Path node: the path data and the view box as text, applied with 적용 as one change.
    /// Text the parser refuses is shown here and sends nothing. The draft lives in the window's memory, so an engine
    /// refusal, which arrives later, leaves the typed text as it is.
    fn path_section(&mut self, ui: &mut Ui, palette: &theme::Palette, node: &NodeProperties, first: &mut bool) {
        section_head(ui, palette, first, "경로", None);
        let editable = feature_editable(self.feature_support(Feature::VectorPaths)) && node.locked != Some(true);
        let Some(artifact) = self.artifact.as_ref() else { return };
        let Some(stored) = super::doc_tree::find_node(artifact, &node.id).and_then(|found| found.ext.path.as_ref()) else { return };
        let revision = artifact.revision;
        let key = Id::new(("node-path-draft", node.id.as_str()));
        // Taken out of the memory for this frame and put back at its end, as the path form does: the text is moved, not cloned.
        let mut draft = ui.data_mut(|data| data.remove_temp::<NodePathDraft>(key)).unwrap_or_default();
        if draft.revision != Some(revision) {
            refresh_path_draft(&mut draft, revision, stored);
        }
        let dirty = draft.d != draft.base_d || draft.view_box != draft.base_view_box;
        let mut edited = false;
        ui.small("경로 데이터 (d)");
        let d_width = ui.available_width();
        edited |= widgets::edit_area(ui, palette, &mut draft.d, widgets::Field {
            width: d_width,
            rows: 3,
            id: Some(Id::new(("node-path-d", node.id.as_str()))),
            enabled: editable,
            ..Default::default()
        })
        .changed();
        row(ui, palette, Some("viewBox"), |ui| {
            let width = ui.available_width();
            edited |= path_text_field(ui, palette, Id::new(("node-path-view-box", node.id.as_str())), width, &mut draft.view_box, editable);
        });
        if edited {
            draft.error = None;
        }
        let mut apply = false;
        if let Some(message) = draft.error.as_deref() {
            ui.colored_label(palette.danger, message);
        }
        row(ui, palette, None, |ui| {
            ui.add_enabled_ui(editable && dirty, |ui| {
                apply = widgets::labeled_button(ui, palette, None, "적용", true).clicked();
            });
        });
        if apply {
            let args = json!({ "node_id": node.id, "d": draft.d, "view_box": draft.view_box });
            draft.error = self.props_path(&args).err();
        }
        ui.data_mut(|data| data.insert_temp(key, draft));
        ui.add_space(8.0);
    }

    /// The `props.path` command: parses the path data and the view box, then sends one content change that replaces the
    /// node's path. `expected` is the path the node has now. Text the parser refuses, a node that is not a path, and
    /// an unchanged path send nothing. `node_id` defaults to the one selected node.
    pub(super) fn props_path(&mut self, args: &serde_json::Value) -> Result<(), String> {
        let text = |key: &str| args.get(key).and_then(serde_json::Value::as_str).unwrap_or_default();
        let node_id = match args.get("node_id").and_then(serde_json::Value::as_str) {
            Some(id) => id.to_owned(),
            None if self.selected_nodes.len() > 1 => return Err("경로를 바꿀 노드를 하나만 선택하세요".into()),
            None => self.selected_node.clone().or_else(|| self.selected_nodes.first().cloned()).ok_or("경로를 바꿀 노드를 선택하세요")?,
        };
        let path = path_from_fields(text("d"), text("view_box"))?;
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let node = super::doc_tree::find_node(artifact, &node_id).ok_or("경로를 바꿀 노드를 찾지 못했습니다")?;
        if node.kind != NodeKind::Path {
            return Err("경로 데이터는 경로 노드에만 넣을 수 있습니다".into());
        }
        let expected: PathSpec = node.ext.path.clone().ok_or("경로 노드에 경로 데이터가 없습니다")?;
        if expected == path {
            return Err("바꿀 것이 없습니다".into());
        }
        self.change(vec![Operation::SetNodePath { node_id, expected, path }], &[Intent::Content])
    }

    fn interaction_section(
        &mut self,
        ui: &mut Ui,
        palette: &theme::Palette,
        node: &NodeProperties,
        group: &[NodeProperties],
        many: bool,
        first: &mut bool,
    ) {
        let (artifact_id, revision, pages, targets, selected) = {
            let Some(artifact) = self.artifact.as_ref() else { return };
            let ArtifactBody::Canvas(document) = &artifact.body else { return };
            let pages: Vec<(String, String)> = document
                .pages
                .iter()
                .enumerate()
                .map(|(index, page)| (page.id.clone(), page.name.clone().unwrap_or_else(|| format!("페이지 {}", index + 1))))
                .collect();
            let mut targets = Vec::new();
            for (index, page) in document.pages.iter().enumerate() {
                let page_label = page.name.clone().unwrap_or_else(|| format!("페이지 {}", index + 1));
                collect_interaction_targets(&page.nodes, &page_label, &mut targets);
            }
            let ids = if many && !group.is_empty() {
                group.iter().map(|node| node.id.as_str()).collect::<Vec<_>>()
            } else {
                vec![node.id.as_str()]
            };
            let selected = ids
                .iter()
                .filter_map(|id| super::doc_tree::find_node(artifact, id))
                .map(|node| (node.id.clone(), node.interactions.clone()))
                .collect::<Vec<_>>();
            (artifact.id.clone(), artifact.revision, pages, targets, selected)
        };
        let Some((source_id, source_interactions)) = selected.first() else { return };
        let selection_key = if many { self.selected_nodes.join("\u{1F}") } else { source_id.clone() };
        let urls: Vec<Option<&str>> = selected
            .iter()
            .map(|(_, interactions)| interactions.iter().find_map(|interaction| match interaction {
                Interaction::OpenUrl { url } => Some(url.as_str()),
                _ => None,
            }))
            .collect();
        let first_url = urls.first().copied().flatten();
        let mixed_url = urls.iter().any(|url| *url != first_url);
        let has_url = urls.iter().any(Option::is_some);
        section_head(ui, palette, first, "클릭 동작", None);
        if many {
            let (apply, remove) = interaction_url_row(
                ui, palette, &artifact_id, revision, &selection_key, 0,
                first_url, mixed_url, has_url,
            );
            if remove {
                self.invoke_ui("canvas.open_url", json!({ "url": "" }));
            } else if let Some(url) = apply {
                self.invoke_ui("canvas.open_url", json!({ "url": url }));
            }
            ui.label(egui::RichText::new("다른 클릭 동작은 한 요소씩 편집하세요").color(palette.ink_3));
            ui.add_space(8.0);
            return;
        }

        let source_id = source_id.clone();
        let interactions = source_interactions;
        let has_single_url = interactions.iter().any(|interaction| matches!(interaction, Interaction::OpenUrl { .. }));
        for (index, interaction) in interactions.iter().enumerate() {
            if let Interaction::OpenUrl { url } = interaction {
                let (apply, remove) = interaction_url_row(
                    ui, palette, &artifact_id, revision, &selection_key, index,
                    Some(url), false, true,
                );
                if remove {
                    self.invoke_ui("canvas.interaction.remove", json!({ "index": index }));
                    return;
                }
                if let Some(url) = apply {
                    self.invoke_ui("canvas.open_url", json!({ "url": url }));
                    return;
                }
                continue;
            }

            let Some(current_kind) = interaction_kind(interaction) else { continue };
            let has_variants = targets.iter().any(|target| !target.variants.is_empty());
            let action_items: Vec<_> = INTERACTION_ACTIONS
                .iter()
                .copied()
                .filter(|(kind, _, _)| *kind != "set_variant" || has_variants || current_kind == "set_variant")
                .collect();
            let (new_kind, remove) = interaction_action_row(ui, palette, &action_items, current_kind);
            if remove {
                self.invoke_ui("canvas.interaction.remove", json!({ "index": index }));
                return;
            }
            if let Some(new_kind) = new_kind.filter(|new_kind| *new_kind != current_kind) {
                if let Some(updated) = default_interaction(&new_kind, &source_id, &pages, &targets) {
                    self.invoke_ui("canvas.interaction.set", interaction_command_args(index, &updated));
                } else if new_kind == "set_variant" {
                    self.set_error("상태를 바꿀 수 있는 요소가 없습니다".into());
                }
                return;
            }

            match interaction {
                Interaction::NavigatePage { page_id } => {
                    if let Some(target) = row(ui, palette, Some("대상"), |ui| interaction_target_menu(ui, palette, &pages, page_id)) {
                        self.invoke_ui(
                            "canvas.interaction.set",
                            interaction_command_args(index, &Interaction::NavigatePage { page_id: target }),
                        );
                        return;
                    }
                }
                Interaction::ToggleVisibility { node_id } => {
                    let options: Vec<(String, String)> = targets
                        .iter()
                        .map(|target| (target.id.clone(), format!("{} · {}", target.page_label, target.label)))
                        .collect();
                    if let Some(target) = row(ui, palette, Some("대상"), |ui| interaction_target_menu(ui, palette, &options, node_id)) {
                        self.invoke_ui(
                            "canvas.interaction.set",
                            interaction_command_args(index, &Interaction::ToggleVisibility { node_id: target }),
                        );
                        return;
                    }
                }
                Interaction::SetVariant { node_id, variant } => {
                    let variant_targets: Vec<&InteractionTarget> = targets.iter().filter(|target| !target.variants.is_empty()).collect();
                    let options: Vec<(String, String)> = variant_targets
                        .iter()
                        .map(|target| (target.id.clone(), format!("{} · {}", target.page_label, target.label)))
                        .collect();
                    if let Some(target) = row(ui, palette, Some("대상"), |ui| interaction_target_menu(ui, palette, &options, node_id)) {
                        if let Some(variant) = variant_targets.iter().find(|candidate| candidate.id == target).and_then(|target| target.variants.first()) {
                            self.invoke_ui(
                                "canvas.interaction.set",
                                interaction_command_args(index, &Interaction::SetVariant { node_id: target, variant: variant.clone() }),
                            );
                        }
                        return;
                    }
                    let variants: Vec<(String, String)> = variant_targets
                        .iter()
                        .find(|target| target.id == *node_id)
                        .map(|target| target.variants.iter().map(|variant| (variant.clone(), variant.clone())).collect())
                        .unwrap_or_default();
                    if let Some(variant) = row(ui, palette, Some("상태"), |ui| interaction_target_menu(ui, palette, &variants, variant)) {
                        self.invoke_ui(
                            "canvas.interaction.set",
                            interaction_command_args(index, &Interaction::SetVariant { node_id: node_id.clone(), variant }),
                        );
                        return;
                    }
                }
                Interaction::Back | Interaction::OpenUrl { .. } => {}
            }
        }

        if !has_single_url {
            let (apply, _) = interaction_url_row(
                ui, palette, &artifact_id, revision, &selection_key, interactions.len(),
                None, false, false,
            );
            if let Some(url) = apply {
                self.invoke_ui("canvas.open_url", json!({ "url": url }));
                return;
            }
        }

        let has_variants = targets.iter().any(|target| !target.variants.is_empty());
        let action_items: Vec<_> = INTERACTION_ACTIONS
            .iter()
            .copied()
            .filter(|(kind, _, _)| *kind != "set_variant" || has_variants)
            .collect();
        if let Some(kind) = interaction_add_menu(ui, palette, &action_items) {
            if let Some(interaction) = default_interaction(&kind, &source_id, &pages, &targets) {
                self.invoke_ui(
                    "canvas.interaction.set",
                    interaction_command_args(interactions.len(), &interaction),
                );
            }
        }
        ui.add_space(8.0);
    }

    /// The layout of one selected frame: arrangement, and for a stack its direction, distribution and cross-axis
    /// alignment; gap and padding for a stack or a grid (mockup `canvas-board` 레이아웃).
    fn layout_section(
        &mut self,
        ui: &mut Ui,
        palette: &theme::Palette,
        node: &NodeProperties,
        layout: &LayoutProperties,
        first: &mut bool,
    ) {
        section_head(ui, palette, first, "레이아웃", None);
        let mode = layout.mode.as_str();
        let stack = matches!(mode, "row" | "column");
        let grid = mode == "grid";
        let arrangement = [!stack && !grid, stack, grid];
        if let Some(chosen) = row(ui, palette, Some("배치"), |ui| {
            seg(ui, palette, &[("layout-free", "자유"), ("layout-stack", "스택"), ("layout-grid", "그리드")], &arrangement)
        }) {
            if !arrangement[chosen] {
                match chosen {
                    0 => self.invoke_ui("props.layout_mode", json!({ "mode": "free" })),
                    1 => self.invoke_ui("props.layout_direction", json!({ "direction": "row" })),
                    _ => self.invoke_ui("props.layout_mode", json!({ "mode": "grid" })),
                }
            }
        }
        if stack {
            let column = mode == "column";
            if let Some(chosen) = row(ui, palette, Some("방향"), |ui| seg(ui, palette, &[("dir-row", "가로"), ("dir-col", "세로")], &[!column, column])) {
                if (chosen == 1) != column {
                    self.invoke_ui("props.layout_direction", json!({ "direction": (["row", "column"][chosen]) }));
                }
            }
            let justify = match layout.justify {
                Some(Justify::Center) => "center",
                Some(Justify::End) => "end",
                Some(Justify::SpaceBetween) => "between",
                _ => "start",
            };
            let justify_items = [
                ("start", "justify-start", "시작"),
                ("center", "justify-center", "가운데"),
                ("end", "justify-end", "끝"),
                ("between", "justify-between", "사이 띄우기"),
            ];
            if let Some(chosen) = row(ui, palette, Some("배분"), |ui| menu_field(ui, palette, &justify_items, justify)) {
                self.invoke_ui("props.layout_justify", json!({ "justify": chosen }));
            }
            let align = if layout.align.is_empty() { "start" } else { layout.align.as_str() };
            // The cross axis of a row is vertical, of a column horizontal.
            let align_items = if column {
                [("start", "align-left", "시작"), ("center", "align-h-center", "가운데"), ("end", "align-right", "끝"), ("stretch", "items-stretch", "늘이기")]
            } else {
                [("start", "align-top", "시작"), ("center", "align-v-center", "가운데"), ("end", "align-bottom", "끝"), ("stretch", "items-stretch", "늘이기")]
            };
            if let Some(chosen) = row(ui, palette, Some("정렬"), |ui| menu_field(ui, palette, &align_items, align)) {
                self.invoke_ui("props.layout_align", json!({ "align": chosen }));
            }
        }
        if grid && feature_editable(self.feature_support(Feature::GridLayout)) {
            let columns = f64::from(layout.grid_columns.unwrap_or(2));
            if let Some(new) = row(ui, palette, Some("열 수"), |ui| {
                let width = ui.available_width();
                field(ui, palette, Id::new(("node-field", node.id.as_str(), "grid-columns")), width, "", Some(columns), "", 0, true)
            }) {
                self.invoke_ui("props.grid", json!({ "columns": new.round().max(1.0) }));
            }
            let row_gap = layout.row_gap.unwrap_or(layout.gap) as f64 / EMU_PER_PX;
            if let Some(new) = row(ui, palette, Some("행 간격"), |ui| {
                let width = ui.available_width();
                field(ui, palette, Id::new(("node-field", node.id.as_str(), "row-gap")), width, "", Some(row_gap), "px", 0, true)
            }) {
                self.invoke_ui("props.grid", json!({ "row_gap": new.max(0.0) }));
            }
            self.breakpoint_rows(ui, palette, node);
        }
        if stack || grid {
            let gap = layout.gap as f64 / EMU_PER_PX;
            if let Some(new) = row(ui, palette, Some("간격"), |ui| {
                let width = ui.available_width();
                field(ui, palette, Id::new(("node-field", node.id.as_str(), "gap")), width, "", Some(gap), "px", 0, true)
            }) {
                self.invoke_ui("props.layout_gap", json!({ "value": new.max(0.0) }));
            }
            self.padding_rows(ui, palette, node, layout);
        }
        ui.add_space(8.0);
    }

    /// The breakpoints of a grid (D05.2). Each row holds a breakpoint's minimum width and its columns, and a `-` that removes
    /// it. A `+` below the rows adds a wider breakpoint. One frame makes at most one change, which sends the whole layout.
    fn breakpoint_rows(&mut self, ui: &mut Ui, palette: &theme::Palette, node: &NodeProperties) {
        // A locked node's breakpoints are shown, not edited: the engine refuses any change to it.
        ui.add_enabled_ui(node.locked != Some(true), |ui| self.breakpoint_rows_of(ui, palette, node));
    }

    fn breakpoint_rows_of(&mut self, ui: &mut Ui, palette: &theme::Palette, node: &NodeProperties) {
        let spec = node.full_layout.clone();
        let base_columns = spec.grid_columns.unwrap_or(2);
        let rows = spec.breakpoints.iter().map(|breakpoint| BreakpointRow::of(breakpoint, base_columns)).collect();
        let artifact_id = self.active.as_ref().map_or("", |active| active.id.as_str());
        let (field_ids, discarded_draft) = follow_focused_breakpoint(ui, artifact_id, node.id.as_str(), rows, breakpoint_field_ids(node.id.as_str(), &spec), node.locked == Some(true));
        if discarded_draft {
            let now = std::time::Instant::now();
            if !self.notices.current(now).is_some_and(|notice| notice.severity == crate::notice::Severity::Error) {
                self.notices.info("다른 변경으로 중단점 목록이 바뀌어 입력하던 값을 취소했습니다", now);
            }
        }
        let mut edit: Option<Result<designllm_contracts::LayoutSpec, &'static str>> = None;
        for (index, breakpoint) in spec.breakpoints.iter().enumerate() {
            let (width_id, columns_id) = field_ids[index];
            let width_value = f64::from(breakpoint.min_width_css_px);
            let columns_value = f64::from(breakpoint.grid_columns.unwrap_or(base_columns));
            let label = (index == 0).then_some("중단점");
            let (new_width, new_columns, remove) = row(ui, palette, label, |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let width = (ui.available_width() - 4.0 - 4.0 - 28.0) / 2.0;
                let new_width = field(ui, palette, width_id, width, "폭", Some(width_value), "px", 0, true);
                let new_columns = field(ui, palette, columns_id, width, "열", Some(columns_value), "", 0, true);
                let remove = widgets::mini_button(ui, palette, "minus", "중단점 삭제").clicked();
                (new_width, new_columns, remove)
            });
            if edit.is_some() {
                continue;
            }
            if let Some(new) = new_width {
                // The row's ids change with its width, so its fields give up the focus and the text they hold under the old ids. egui
                // keeps a field's text under `id.with("draft")` until the field is drawn without focus (`field_with_hint`) and never
                // clears it by itself; without this, that text would stay under ids a later breakpoint can take.
                ui.memory_mut(|memory| {
                    memory.surrender_focus(width_id);
                    memory.surrender_focus(columns_id);
                });
                ui.data_mut(|data| {
                    data.remove::<String>(width_id.with("draft"));
                    data.remove::<String>(columns_id.with("draft"));
                });
                edit = Some(breakpoint_whole(new).ok_or(BREAKPOINT_WIDTH_TOO_LARGE).and_then(|width| breakpoint_width_set(&spec, index, width)));
            } else if let Some(new) = new_columns {
                // A count past u32 is out of the 1 to 24 range as 0 is.
                edit = Some(breakpoint_columns_set(&spec, index, breakpoint_whole(new).unwrap_or(0)));
            } else if remove {
                edit = Some(breakpoint_removed(&spec, index));
            }
        }
        let add = row(ui, palette, None, |ui| widgets::mini_button(ui, palette, "add", "중단점 추가").clicked());
        if add && edit.is_none() {
            edit = Some(breakpoint_added(&spec));
        }
        if let Some(result) = edit {
            self.commit_layout(&node.id, result);
        }
    }

    /// Sends one breakpoint edit as a `SetLayout` of the whole layout. A refused edit is shown as an error.
    fn commit_layout(&mut self, node_id: &str, layout: Result<designllm_contracts::LayoutSpec, &'static str>) {
        match layout {
            Ok(layout) => {
                if let Err(message) = self.change(vec![layout_operation(node_id, layout)], &[designllm_contracts::Intent::Design]) {
                    self.set_error(message);
                }
            }
            Err(message) => self.set_error(message.to_string()),
        }
    }

    /// 안쪽 여백 (mockup `canvas-board`): `좌우 [ ]` · `위아래 [ ]` with a `padding-sides` button that opens the four
    /// sides one by one. Every edit sends all four sides with `props.padding_sides` (top, right, bottom, left, px); a
    /// pair whose two sides differ shows `여러 값`.
    fn padding_rows(&mut self, ui: &mut Ui, palette: &theme::Palette, node: &NodeProperties, layout: &LayoutProperties) {
        let px = |emu: i64| emu as f64 / EMU_PER_PX;
        let sides = layout.padding_sides.unwrap_or([layout.padding; 4]).map(px);
        let [top, right, bottom, left] = sides;
        let horizontal = ((left - right).abs() < 1e-9).then_some(left);
        let vertical = ((top - bottom).abs() < 1e-9).then_some(top);
        let open_id = Id::new(("padding-sides-open", node.id.as_str()));
        let mut open = ui.data(|data| data.get_temp::<bool>(open_id)).unwrap_or(false);
        let id = |key: &str| Id::new(("node-field", node.id.as_str(), key.to_owned()));
        let (new_h, new_v, toggled) = row(ui, palette, Some("안쪽 여백"), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let width = (ui.available_width() - 4.0 - 4.0 - 28.0) / 2.0;
            let new_h = field(ui, palette, id("padding-h"), width, "좌우", horizontal, "", 0, true);
            let new_v = field(ui, palette, id("padding-v"), width, "위아래", vertical, "", 0, true);
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
            let corner = CornerRadius::same(theme::RADIUS_CONTROL);
            if open {
                ui.painter().rect_filled(rect, corner, palette.accent_soft);
            } else if response.hovered() {
                ui.painter().rect_filled(rect, corner, palette.surface_2);
            }
            icons::paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(16.0)), "padding-sides", if open { palette.accent } else { palette.ink_2 });
            (new_h, new_v, response.on_hover_text("네 면 따로").clicked())
        });
        if toggled {
            open = !open;
            ui.data_mut(|data| data.insert_temp(open_id, open));
        }
        let mut next = sides;
        let mut changed = false;
        if let Some(value) = new_h {
            next[1] = value.max(0.0);
            next[3] = value.max(0.0);
            changed = true;
        }
        if let Some(value) = new_v {
            next[0] = value.max(0.0);
            next[2] = value.max(0.0);
            changed = true;
        }
        if open {
            for pair in [[(0, "위", "padding-top"), (2, "아래", "padding-bottom")], [(3, "왼쪽", "padding-left"), (1, "오른쪽", "padding-right")]] {
                let edits = row(ui, palette, Some(""), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let width = (ui.available_width() - 4.0 - 4.0 - 28.0) / 2.0;
                    pair.map(|(index, prefix, key)| (index, field(ui, palette, id(key), width, prefix, Some(sides[index]), "", 0, true)))
                });
                for (index, value) in edits {
                    if let Some(value) = value {
                        next[index] = value.max(0.0);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.invoke_ui("props.padding_sides", json!({ "top": next[0], "right": next[1], "bottom": next[2], "left": next[3] }));
        }
    }

    /// Text style of the selected text nodes: size, weight and slant toggles, colour, alignment (§9.1 텍스트).
    fn text_section(&mut self, ui: &mut Ui, palette: &theme::Palette, node: &NodeProperties, group: &[NodeProperties], first: &mut bool) {
        let editable = feature_editable(self.feature_support(Feature::TextStyle));
        let style = &node.style;
        let id = |key: &str| Id::new(("node-field", node.id.as_str(), key.to_owned()));
        section_head(ui, palette, first, "텍스트", None);
        let size = agree(group, node, |other| other.style.font_size).then_some(style.font_size as f64 / EMU_PER_PT as f64);
        if let Some(new) = row(ui, palette, Some("크기"), |ui| {
            let width = ui.available_width();
            field(ui, palette, id("font"), width, "", size, "pt", 1, editable)
        }) {
            self.invoke_ui("props.text_style", json!({ "font_size": new.max(1.0) }));
        }
        let (bold, italic) = (style.bold, style.italic);
        if let Some(chosen) = row(ui, palette, Some("모양"), |ui| seg(ui, palette, &[("bold", "굵게"), ("italic", "기울임")], &[bold, italic])) {
            if editable {
                self.invoke_ui("props.text_style", if chosen == 0 { json!({ "bold": !bold }) } else { json!({ "italic": !italic }) });
            }
        }
        let colour = agree(group, node, |other| other.style.color.clone()).then(|| style.color.clone());
        if let Some(new) = row(ui, palette, Some("색"), |ui| {
            let width = ui.available_width();
            colour_field(ui, palette, id("text-color"), width, colour.as_deref())
        }) {
            if editable {
                self.invoke_ui("props.text_style", json!({ "color": new }));
            }
        }
        // §9.1 텍스트: 줄 높이(배수) · 자간 · 세로 정렬 are the v1.2 text extras behind the Effects capability.
        if feature_editable(self.feature_support(Feature::Effects)) {
            let line = agree(group, node, |other| other.style.line_height_milli)
                .then(|| style.line_height_milli.map(|milli| f64::from(milli) / 1000.0))
                .flatten();
            let line_mixed = !agree(group, node, |other| other.style.line_height_milli);
            if let Some(new) = row(ui, palette, Some("줄 높이"), |ui| {
                let width = ui.available_width();
                // Empty means the font's own line height; a typed multiple sets it.
                field(ui, palette, id("line-height"), width, "", if line_mixed { None } else { Some(line.unwrap_or(1.2)) }, "배", 2, true)
            }) {
                self.invoke_ui("props.line_height", json!({ "value": new.max(0.1) }));
            }
            let spacing = agree(group, node, |other| other.style.letter_spacing.unwrap_or(0))
                .then_some(style.letter_spacing.unwrap_or(0) as f64 / EMU_PER_PX);
            if let Some(new) = row(ui, palette, Some("자간"), |ui| {
                let width = ui.available_width();
                field(ui, palette, id("letter-spacing"), width, "", spacing, "px", 1, true)
            }) {
                self.invoke_ui("props.letter_spacing", json!({ "value": new }));
            }
            let vertical = match style.vertical_align {
                Some(designllm_contracts::VerticalAlign::Middle) => 1,
                Some(designllm_contracts::VerticalAlign::Bottom) => 2,
                _ => 0,
            };
            let flags: Vec<bool> = (0..3).map(|index| index == vertical).collect();
            if let Some(chosen) = row(ui, palette, Some("세로 정렬"), |ui| {
                seg(ui, palette, &[("text-valign-top", "위"), ("text-valign-middle", "가운데"), ("text-valign-bottom", "아래")], &flags)
            }) {
                if chosen != vertical {
                    self.invoke_ui("props.vertical_align", json!({ "align": (["top", "middle", "bottom"][chosen]) }));
                }
            }
        }
        let aligns = ["left", "center", "right", "justify"];
        let current = aligns.iter().position(|align| *align == style.align.as_str()).unwrap_or(0);
        let chosen_flags: Vec<bool> = (0..4).map(|index| index == current).collect();
        if let Some(chosen) = row(ui, palette, None, |ui| {
            seg(
                ui,
                palette,
                &[("text-align-left", "왼쪽"), ("text-align-center", "가운데"), ("text-align-right", "오른쪽"), ("text-align-justify", "양쪽")],
                &chosen_flags,
            )
        }) {
            if chosen != current {
                self.invoke_ui("props.text_align", json!({ "align": aligns[chosen] }));
            }
        }
        let kind = self.active.as_ref().map(|active| active.kind);
        if kind == Some(ArtifactKind::Canvas) {
            if let Some((_, edit)) = self.canvas_list_support().filter(|(read, _)| feature_shown(Some(*read))) {
                let same = agree(group, node, |other| other.list);
                let current = if same {
                    match node.list {
                        None => "none",
                        Some(designllm_contracts::TextListKind::Bullet) => "bullet",
                        Some(designllm_contracts::TextListKind::Numbered) => "numbered",
                    }
                } else {
                    ""
                };
                let items = [
                    ("none", "", "없음"),
                    ("bullet", "list-bullet", "글머리표"),
                    ("numbered", "list-number", "번호"),
                ];
                let editable = feature_editable(Some(edit)) && commands_available("text.list");
                if let Some(list) = row(ui, palette, Some("목록"), |ui| {
                    ui.add_enabled_ui(editable, |ui| menu_field(ui, palette, &items, current)).inner
                }) {
                    self.invoke_ui("text.list", json!({ "list": list }));
                }
            }
        }
        ui.add_space(8.0);
    }
    /// Sections when nothing is selected: the page's own settings.
    fn document_sections(&mut self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool) {
        let Some(kind) = self.artifact.as_ref().map(|artifact| artifact.kind) else {
            return;
        };
        let page_index = self.view_ref().page_index;
        let size = self.page_sizes().get(page_index as usize).copied();
        if matches!(kind, ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp) {
            self.flow_sections(ui, palette, first);
            if self.export_format_supported("pdf")
                && row(ui, palette, None, |ui| {
                    widgets::labeled_button(ui, palette, Some("fmt-document"), "PDF로 인쇄…", false).clicked()
                })
            {
                self.export_format = "pdf".into();
                self.export_scope = "all".into();
                self.smoke_open = Some("export".into());
            }
            return;
        }
        if !matches!(kind, ArtifactKind::Canvas | ArtifactKind::Slides) {
            return;
        }
        let Some(size) = size else {
            return;
        };
        section_head(ui, palette, first, if kind == ArtifactKind::Slides { "슬라이드" } else { "페이지" }, None);
        if kind == ArtifactKind::Canvas {
            let component_state = self.artifact.as_ref().and_then(|artifact| match &artifact.body {
                ArtifactBody::Canvas(document) => document.pages.get(page_index as usize).map(|page| {
                    let sources: Vec<(String, String)> = document.pages.iter().enumerate()
                        .filter(|(_, candidate)| candidate.role == Some(designllm_contracts::PageRole::Component) && candidate.id != page.id)
                        .map(|(index, candidate)| (candidate.id.clone(), candidate.name.clone().unwrap_or_else(|| format!("컴포넌트 {}", index + 1))))
                        .collect();
                    (page.id.clone(), page.role == Some(designllm_contracts::PageRole::Component), sources)
                }),
                _ => None,
            });
            if let Some((page_id, is_component, sources)) = component_state {
                if self.command_enabled_for_current_kind("page.component") {
                    let mut enabled = is_component;
                    if row(ui, palette, Some("컴포넌트"), |ui| widgets::checkbox(ui, palette, &mut enabled, "페이지를 소스로 사용").changed()) {
                        self.invoke_ui("page.component", json!({ "page_index": page_index, "enabled": enabled }));
                    }
                }
                if !is_component && !sources.is_empty() && self.command_enabled_for_current_kind("component.instantiate") {
                    let source_key = Id::new(("component-source-page", page_id.as_str()));
                    let first_source = sources[0].0.clone();
                    let mut selected = ui.data(|data| data.get_temp::<String>(source_key)).filter(|id| sources.iter().any(|(source, _)| source == id)).unwrap_or(first_source);
                    let label = sources.iter().find(|(source, _)| source == &selected).map(|(_, name)| name.as_str()).unwrap_or("컴포넌트 선택");
                    let names: Vec<&str> = sources.iter().map(|(_, name)| name.as_str()).collect();
                    let current = sources.iter().position(|(source, _)| *source == selected);
                    row(ui, palette, Some("원본"), |ui| {
                        let width = ui.available_width();
                        if let Some(index) = widgets::select_menu(ui, palette, label, &names, current, width) {
                            selected = sources[index].0.clone();
                        }
                    });
                    ui.data_mut(|data| data.insert_temp(source_key, selected.clone()));
                    if row(ui, palette, Some(""), |ui| widgets::labeled_button(ui, palette, None, "컴포넌트 삽입", false).clicked()) {
                        self.invoke_ui("component.instantiate", json!({ "component_page_id": selected, "instance_id": new_id(), "page_index": page_index }));
                    }
                }
            }
        }
        let unit = unit_for(kind);
        let (width, height) = (size.width as f64 / unit.emu(), size.height as f64 / unit.emu());
        let custom_id = Id::new(("page-size-custom", page_index));
        let mut custom = ui.data(|data| data.get_temp::<bool>(custom_id)).unwrap_or(false);
        if kind == ArtifactKind::Slides {
            // Mockup `slides-board`: one 크기 row, `preset-slide` + ratio name + the size in `ink-3`.
            let ratio = if (size.width * 9 - size.height * 16).abs() <= size.width / 100 {
                "16:9"
            } else if (size.width * 3 - size.height * 4).abs() <= size.width / 100 {
                "4:3"
            } else {
                "사용자 크기"
            };
            let measure = format!("{} × {}", widgets::format_number(width, unit.decimals()), widgets::format_number(height, unit.decimals()));
            let choice = row(ui, palette, Some("크기"), |ui| {
                let room = ui.available_width();
                let (rect, response) = ui.allocate_exact_size(vec2(room, theme::FIELD_HEIGHT), Sense::click());
                let corner = CornerRadius::same(theme::RADIUS_CONTROL);
                ui.painter().rect(rect, corner, if response.hovered() { palette.surface_2 } else { palette.field }, Stroke::new(1.0, palette.line_strong), StrokeKind::Inside);
                icons::paint(ui, Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.center().y - 8.0), Vec2::splat(16.0)), "preset-slide", palette.ink_2);
                ui.painter().text(Pos2::new(rect.left() + 30.0, rect.center().y), Align2::LEFT_CENTER, ratio, theme::display_font(false, 13.0), palette.ink);
                ui.painter().text(Pos2::new(rect.right() - 30.0, rect.center().y), Align2::RIGHT_CENTER, &measure, theme::display_font(false, 13.0), palette.ink_3);
                icons::paint(ui, Rect::from_min_size(Pos2::new(rect.right() - 24.0, rect.center().y - 8.0), Vec2::splat(16.0)), "chev-down", palette.ink_3);
                let mut choice = None;
                egui::Popup::from_toggle_button_response(&response)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| {
                        ui.set_width(room.max(200.0));
                        if widgets::menu_item(ui, palette, Some("preset-slide"), "16:9", "", ratio == "16:9").clicked() {
                            choice = Some("16:9");
                            ui.close();
                        }
                        if widgets::menu_item(ui, palette, Some("preset-custom"), "사용자 크기", "", ratio == "사용자 크기").clicked() {
                            choice = Some("custom");
                            ui.close();
                        }
                    });
                choice
            });
            match choice {
                Some("16:9") if ratio != "16:9" => self.invoke_ui("props.page_size", json!({ "preset": "slide_16_9" })),
                Some("custom") => {
                    custom = !custom;
                    ui.data_mut(|data| data.insert_temp(custom_id, custom));
                }
                _ => {}
            }
        }
        if kind == ArtifactKind::Slides && !custom {
            ui.add_space(8.0);
            if kind == ArtifactKind::Slides && feature_shown(self.feature_support(Feature::Notes)) {
                self.notes_section(ui, palette, page_index, first);
            }
            self.slides_theme_section(ui, palette, first);
            self.export_row(ui, palette, first, "page");
            return;
        }
        // The size fields are keyed by the page's id, so text typed for one page never lands on another page that takes its index.
        let page_key = self
            .artifact
            .as_ref()
            .and_then(|artifact| match &artifact.body {
                ArtifactBody::Canvas(document) => document.pages.get(page_index as usize).map(|page| page.id.clone()),
                ArtifactBody::Slides(document) => document.pages.get(page_index as usize).map(|page| page.id.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if let Some(new) = row(ui, palette, Some("너비"), |ui| {
            let room = ui.available_width();
            field(ui, palette, page_field_id(&page_key, "w"), room, "", Some(width), unit.label(), unit.decimals(), true)
        }) {
            self.invoke_ui("props.page_size", json!({ "width": new, "height": height }));
        }
        if let Some(new) = row(ui, palette, Some("높이"), |ui| {
            let room = ui.available_width();
            field(ui, palette, page_field_id(&page_key, "h"), room, "", Some(height), unit.label(), unit.decimals(), true)
        }) {
            self.invoke_ui("props.page_size", json!({ "width": width, "height": new }));
        }
        if kind == ArtifactKind::Canvas {
            self.page_guides_section(ui, palette, page_index, first);
        }
        ui.add_space(8.0);
        if feature_shown(self.feature_support(Feature::Notes)) {
            self.notes_section(ui, palette, page_index, first);
        }
        self.export_row(ui, palette, first, "page");
    }

    /// `내보내기 ›` (mockups `canvas-board`, `slides-board`, `figure-board`): the last section, one 34 px row that opens
    /// the export popover with its range set to `scope` — this page (as the title row's ⇪), or the selection
    /// (`file.export_selection`, PNG·SVG).
    fn export_row(&mut self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool, scope: &str) {
        let command = if scope == "selection" { "file.export_selection" } else { "file.export" };
        if !commands_available(command) {
            return;
        }
        let height = section_height(ui);
        if !*first {
            let top = ui.cursor().top();
            let span = ui.max_rect();
            ui.painter().hline((span.left() - 12.0)..=(span.right() + 12.0), top + 0.5, Stroke::new(1.0, palette.line));
            ui.add_space(1.0);
        }
        *first = false;
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
        ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, "내보내기", theme::display_font(true, 13.0), palette.ink);
        icons::paint(ui, Rect::from_min_size(Pos2::new(rect.right() - 16.0, rect.center().y - 8.0), Vec2::splat(16.0)), "chev-right", palette.ink_3);
        if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.export_scope = scope.to_owned();
            self.smoke_open = Some(String::from("export"));
        }
    }

    /// Sections of a text document with nothing selected (§9.2 선택 없음): its named styles and `쪽 ›`.
    fn flow_sections(&mut self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool) {
        let Some(ArtifactBody::Flow(document)) = self.artifact.as_ref().map(|artifact| artifact.body.clone()) else {
            return;
        };
        let kind = self.active.as_ref().map_or(ArtifactKind::Docx, |item| item.kind);
        // The engine's style list (`flow.styles`) for this revision, asked for once; until it answers, or when the
        // engine has none, the model's styles stand in.
        if let Some((artifact_id, revision)) = self.active.as_ref().map(|item| (item.id.clone(), item.revision)) {
            let current = self.flow_styles.as_ref().is_some_and(|(id, seen, _)| *id == artifact_id && *seen == revision);
            if !current {
                self.flow_styles = Some((artifact_id.clone(), revision, None));
                self.call("flow.styles", json!({ "artifact_id": artifact_id }), super::PendingAction::FlowStyles { artifact_id, revision });
            }
        }
        let engine_styles = self.flow_styles.as_ref().and_then(|(_, _, styles)| styles.clone());
        let listed = doc_style_list(&document, kind, engine_styles.as_deref());
        if feature_shown(self.feature_support(Feature::NamedStyles)) && !listed.is_empty() {
            let editable = feature_editable(self.feature_support(Feature::NamedStyles));
            section_head(ui, palette, first, "스타일", None);
            let open_id = Id::new("doc-style-open");
            let mut open = ui.data(|data| data.get_temp::<String>(open_id));
            let mut divided = false;
            for entry in &listed {
                if entry.user && !divided {
                    divided = true;
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
                    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, palette.line));
                }
                let Some(style) = document.styles.get(&entry.id) else {
                    continue;
                };
                // tokens `component.doc-style-list.row`: a menu item (30, 13 `ink`).
                let is_open = open.as_deref() == Some(entry.id.as_str());
                if widgets::menu_item(ui, palette, None, &entry.label, "", is_open).clicked() {
                    open = if is_open { None } else { Some(entry.id.clone()) };
                    ui.data_mut(|data| match &open {
                        Some(id) => data.insert_temp(open_id, id.clone()),
                        None => data.remove::<String>(open_id),
                    });
                }
                if !is_open {
                    continue;
                }
                // The style's text fields open under its row; each change is `doc.named_style`.
                let name = entry.id.as_str();
                let size = style.font_size as f64 / EMU_PER_PT as f64;
                if let Some(family) = row(ui, palette, Some("글꼴"), |ui| {
                    let width = ui.available_width();
                    text_line(ui, palette, Id::new(("named-style", name, "font")), width, &style.font_family, editable)
                }) {
                    self.invoke_ui("doc.named_style", json!({ "name": name, "font_family": family }));
                }
                if let Some(new) = row(ui, palette, Some("크기"), |ui| {
                    let width = ui.available_width();
                    field(ui, palette, Id::new(("named-style", name, "size")), width, "", Some(size), "pt", 1, editable)
                }) {
                    self.invoke_ui("doc.named_style", json!({ "name": name, "font_size": new.max(1.0) }));
                }
                if let Some(chosen) = row(ui, palette, Some("모양"), |ui| seg(ui, palette, &[("bold", "굵게"), ("italic", "기울임")], &[style.bold, style.italic])) {
                    if editable {
                        self.invoke_ui("doc.named_style", if chosen == 0 { json!({ "name": name, "bold": !style.bold }) } else { json!({ "name": name, "italic": !style.italic }) });
                    }
                }
                if let Some(colour) = row(ui, palette, Some("색"), |ui| {
                    let width = ui.available_width();
                    colour_field(ui, palette, Id::new(("named-style", name, "color")), width, Some(&style.color))
                }) {
                    if editable {
                        self.invoke_ui("doc.named_style", json!({ "name": name, "color": colour }));
                    }
                }
            }
            ui.add_space(8.0);
        }
        let Some(first_section) = document.sections.first() else {
            return;
        };
        if !feature_shown(self.feature_support(Feature::PageSetup)) {
            return;
        }
        let editable = feature_editable(self.feature_support(Feature::PageSetup));
        let document_id = self.active.as_ref().map_or("", |item| item.id.as_str());
        // The section the page setup edits: the one the `구역` row picked, else the first.
        let choice_id = Id::new(("page-setup-section", document_id));
        let section = ui
            .data(|data| data.get_temp::<String>(choice_id))
            .and_then(|id| document.sections.iter().find(|candidate| candidate.id == id))
            .unwrap_or(first_section);
        let open_id = Id::new(("page-setup-open", document_id));
        let mut open = ui.data(|data| data.get_temp::<bool>(open_id)).unwrap_or(false);
        // `쪽 ›`: a section head that opens the page setup (§9.2 쪽).
        if !*first {
            let top = ui.cursor().top();
            let span = ui.max_rect();
            ui.painter().hline((span.left() - 12.0)..=(span.right() + 12.0), top + 0.5, Stroke::new(1.0, palette.line));
            ui.add_space(1.0);
        }
        *first = false;
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), section_height(ui)), Sense::click());
        ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, "쪽", theme::display_font(true, 13.0), palette.ink);
        icons::paint(ui, Rect::from_min_size(Pos2::new(rect.right() - 16.0, rect.center().y - 8.0), Vec2::splat(16.0)), if open { "chev-down" } else { "chev-right" }, palette.ink_3);
        if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            open = !open;
            ui.data_mut(|data| data.insert_temp(open_id, open));
        }
        if !open {
            return;
        }
        if document.sections.len() > 1 {
            // `구역`: the section whose page setup the rows below change. The group stays open when another section is picked.
            let items: Vec<(String, String)> = document
                .sections
                .iter()
                .enumerate()
                .map(|(index, candidate)| (candidate.id.clone(), format!("구역 {}", index + 1)))
                .collect();
            let current = items.iter().find(|(id, _)| *id == section.id).map_or_else(String::new, |(_, name)| name.clone());
            let mut picked = None;
            row(ui, palette, Some("구역"), |ui| {
                let width = ui.available_width();
                let button = widgets::dropdown(ui, palette, &current, None, width);
                egui::Popup::from_toggle_button_response(&button)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| {
                        ui.set_width(width.max(180.0));
                        for (id, name) in &items {
                            if widgets::menu_item(ui, palette, None, name, "", *id == section.id).clicked() {
                                picked = Some(id.clone());
                                ui.close();
                            }
                        }
                    });
            });
            if let Some(id) = picked {
                ui.data_mut(|data| data.insert_temp(choice_id, id));
            }
        }
        let setup = &section.setup;
        let mm = EMU_PER_MM as f64;
        let a4 = Size::a4();
        let paper = if (setup.size.width - a4.width).abs() < mm as i64 || (setup.size.height - a4.width).abs() < mm as i64 { 0 } else { 1 };
        if let Some(chosen) = row(ui, palette, Some("용지"), |ui| seg(ui, palette, &[("preset-paper", "A4"), ("preset-paper", "Letter")], &[paper == 0, paper == 1])) {
            if editable && chosen != paper {
                self.invoke_ui("doc.page_setup", json!({ "section_id": section.id, "paper": (["a4", "letter"][chosen]) }));
            }
        }
        let landscape = setup.size.width > setup.size.height;
        if let Some(chosen) = row(ui, palette, Some("방향"), |ui| seg(ui, palette, &[("orient-portrait", "세로"), ("orient-landscape", "가로")], &[!landscape, landscape])) {
            if editable && (chosen == 1) != landscape {
                self.invoke_ui("doc.page_setup", json!({ "section_id": section.id, "orientation": (["portrait", "landscape"][chosen]) }));
            }
        }
        for (label, pair) in [("여백", [("위", "margin_top", setup.margin_top), ("아래", "margin_bottom", setup.margin_bottom)]), ("", [("왼쪽", "margin_left", setup.margin_left), ("오른쪽", "margin_right", setup.margin_right)])] {
            let edits = row(ui, palette, Some(label), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let width = (ui.available_width() - 4.0) / 2.0;
                pair.map(|(prefix, key, value)| (key, field(ui, palette, Id::new(("page-setup", section.id.as_str(), key)), width, prefix, Some(value as f64 / mm), "mm", 1, editable)))
            });
            for (key, value) in edits {
                if let Some(value) = value {
                    let mut change = serde_json::Map::new();
                    change.insert("section_id".into(), json!(section.id));
                    change.insert(key.into(), json!(value.max(0.0)));
                    self.invoke_ui("doc.page_setup", serde_json::Value::Object(change));
                }
            }
        }
        ui.add_space(8.0);
    }

    /// `안내선` of a canvas page: the show and snap switches, the clear button, and for each axis a switch with its
    /// count and gutters. Every edit is one command. Its `expected` value is the setting this window shows, so a
    /// change made elsewhere is refused by the engine and nothing is overwritten.
    fn page_guides_section(&mut self, ui: &mut Ui, palette: &theme::Palette, page_index: u32, first: &mut bool) {
        let Some(stored) = self.artifact.as_ref().and_then(|artifact| match &artifact.body {
            ArtifactBody::Canvas(document) => document
                .pages
                .get(page_index as usize)
                .map(|page| document.layout_guides.get(&page.id).cloned()),
            _ => None,
        }) else {
            return;
        };
        let editable = self.ensure_interaction_revision_matches_context().is_ok();
        let view = self.view_ref();
        let px = |emu: i64| emu as f64 / EMU_PER_PX;
        let mut actions: Vec<(&str, serde_json::Value)> = Vec::new();
        section_head(ui, palette, first, "안내선", None);
        row(ui, palette, None, |ui| {
            if widgets::icon_button(ui, palette, "layout-grid", &super::chrome::tooltip_for("view.toggle_guides"), view.guides_visible).clicked() {
                actions.push(("view.toggle_guides", json!({})));
            }
            if widgets::icon_button(ui, palette, "align-h-center", &super::chrome::tooltip_for("view.toggle_snap"), view.snap_guides).clicked() {
                actions.push(("view.toggle_snap", json!({})));
            }
            if stored.is_some() && editable && commands_available("canvas.guides.clear") {
                let delete = widgets::icon_button(ui, palette, "delete", &super::chrome::tooltip_for("canvas.guides.clear"), false);
                if delete.clicked() {
                    actions.push(("canvas.guides.clear", json!({ "page_index": page_index })));
                }
            }
        });
        for (axis, label, current) in [
            ("columns", "열", stored.as_ref().and_then(|guides| guides.columns)),
            ("rows", "행", stored.as_ref().and_then(|guides| guides.rows)),
        ] {
            let mut on = current.is_some();
            let switched = row(ui, palette, Some(label), |ui| {
                // Only the box takes clicks, as the egui check box did: the rest of the row is not a target.
                ui.add_enabled_ui(editable, |ui| {
                    ui.set_max_width(theme::ICON_SMALL);
                    widgets::checkbox_recorded(ui, palette, &mut on, "", &format!("{label} 가이드"))
                })
                .inner
                .changed()
            });
            if switched {
                actions.push(if on {
                    ("canvas.guides.set", json!({ "page_index": page_index, "axis": axis, "count": 3, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 }))
                } else {
                    ("canvas.guides.clear", json!({ "page_index": page_index, "axis": axis }))
                });
            }
            let Some(spec) = current else {
                continue;
            };
            let mut next = spec;
            let mut edited = false;
            row(ui, palette, None, |ui| {
                let room = ui.available_width();
                if let Some(value) = field(ui, palette, Id::new(("guides-count", page_index, axis)), 88.0, "", Some(f64::from(spec.count)), "개", 0, editable) {
                    next.count = value.round().clamp(1.0, 24.0) as u32;
                    edited = true;
                }
                if let Some(value) = field(ui, palette, Id::new(("guides-spacing", page_index, axis)), (room - 96.0).max(40.0), "간격", Some(px(spec.spacing_emu)), "px", 0, editable) {
                    next.spacing_emu = (value * EMU_PER_PX).round() as i64;
                    edited = true;
                }
            });
            row(ui, palette, None, |ui| {
                let half = ((ui.available_width() - 8.0) / 2.0).max(40.0);
                if let Some(value) = field(ui, palette, Id::new(("guides-lead", page_index, axis)), half, "앞", Some(px(spec.leading_margin_emu)), "px", 0, editable) {
                    next.leading_margin_emu = (value * EMU_PER_PX).round() as i64;
                    edited = true;
                }
                if let Some(value) = field(ui, palette, Id::new(("guides-trail", page_index, axis)), half, "뒤", Some(px(spec.trailing_margin_emu)), "px", 0, editable) {
                    next.trailing_margin_emu = (value * EMU_PER_PX).round() as i64;
                    edited = true;
                }
            });
            if edited {
                actions.push(("canvas.guides.set", json!({
                    "page_index": page_index,
                    "axis": axis,
                    "count": next.count,
                    "spacing_emu": next.spacing_emu,
                    "leading_margin_emu": next.leading_margin_emu,
                    "trailing_margin_emu": next.trailing_margin_emu
                })));
            }
        }
        for (id, args) in actions {
            self.invoke_ui(id, args);
        }
    }

    /// Canvas page notes and Slides speaker notes use the same stored field and edit command.
    fn notes_section(&mut self, ui: &mut Ui, palette: &theme::Palette, page_index: u32, first: &mut bool) {
        let notes = match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => document.pages.get(page_index as usize).map(|page| page.notes.clone()),
            Some(ArtifactBody::Slides(document)) => document.pages.get(page_index as usize).map(|page| page.notes.clone()),
            _ => None,
        };
        let Some(notes) = notes else {
            return;
        };
        let editable = feature_editable(self.feature_support(Feature::Notes));
        section_head(ui, palette, first, "노트", None);
        let id = Id::new(("slide-notes", page_index));
        let draft_id = id.with("draft");
        let mut draft = ui.data(|data| data.get_temp::<String>(draft_id)).unwrap_or_else(|| notes.clone());
        let width = ui.available_width();
        let response = widgets::edit_area(ui, palette, &mut draft, widgets::Field {
            width,
            rows: 4,
            min_height: 96.0,
            id: Some(id),
            enabled: editable,
            margin: Some(egui::Margin::symmetric(8, 6)),
            ..Default::default()
        });
        if response.has_focus() {
            ui.data_mut(|data| data.insert_temp(draft_id, draft.clone()));
        } else {
            ui.data_mut(|data| data.remove::<String>(draft_id));
        }
        if response.lost_focus() && draft != notes {
            self.invoke_ui("page.notes", json!({ "notes": draft, "page_index": page_index }));
        }
        ui.add_space(8.0);
    }
}

#[cfg(test)]
#[path = "properties_print_tests.rs"]
mod print_flow_tests;

#[cfg(test)]
#[path = "properties_layout_tests.rs"]
mod layout_breakpoint_tests;

#[cfg(test)]
#[path = "properties_plot_tests.rs"]
mod properties_plot_tests;

#[cfg(test)]
#[path = "path_editor_tests.rs"]
mod path_editor_tests;

#[cfg(test)]
#[path = "properties_path_tests.rs"]
mod properties_path_tests;

impl DesktopApp {
    /// The selected figure panel (mockup `figure-board`): 패널 (라벨, 위치 X·Y, 너비, 높이 in mm), and for a chart
    /// panel 차트 (종류, X 열, Y 열) and 축 (X·Y 범위, Y 척도). Each value is applied when it is committed, as one
    /// change set. `supported` gates the panel rows (FigurePanels); the chart rows follow the Plot capability.
    pub(super) fn draw_geometry_fields(&mut self, ui: &mut Ui, key: &str, supported: bool, _label: &str) {
        let Some(panel_id) = self.selected_node.clone() else {
            return;
        };
        let Some(figure) = self.figure_document() else {
            return;
        };
        let Some(panel) = super::figure_tools::owning_panel(&figure, &panel_id).cloned() else {
            return;
        };
        let palette = theme::palette(ui.ctx());
        let mm = EMU_PER_MM as f64;
        let bounds = panel.bounds;
        let id = |name: &str| Id::new((key.to_owned(), panel.id.clone(), name.to_owned()));
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut first = true;
            section_head(ui, &palette, &mut first, "패널", None);
            if let Some(label) = row(ui, &palette, Some("라벨"), |ui| {
                let width = ui.available_width();
                text_line(ui, &palette, id("label"), width, &panel.label, supported)
            }) {
                self.invoke_ui("figure.panel_label", json!({ "panel_id": panel.id, "label": label }));
            }
            let mut changed = bounds;
            let mut committed = false;
            let (x, y) = row(ui, &palette, Some("위치"), |ui| {
                let width = (ui.available_width() - 8.0) / 2.0;
                let x = field(ui, &palette, id("x"), width, "X", Some(bounds.x as f64 / mm), "mm", 1, supported);
                let y = field(ui, &palette, id("y"), width, "Y", Some(bounds.y as f64 / mm), "mm", 1, supported);
                (x, y)
            });
            if let Some(x) = x {
                changed.x = (x * mm).round() as i64;
                committed = true;
            }
            if let Some(y) = y {
                changed.y = (y * mm).round() as i64;
                committed = true;
            }
            for (label, name, value) in [("너비", "w", bounds.width), ("높이", "h", bounds.height)] {
                if let Some(new) = row(ui, &palette, Some(label), |ui| {
                    let width = ui.available_width();
                    field(ui, &palette, id(name), width, "", Some(value as f64 / mm), "mm", 1, supported)
                }) {
                    let emu = (new.max(1.0) * mm).round() as i64;
                    if name == "w" {
                        changed.width = emu;
                    } else {
                        changed.height = emu;
                    }
                    committed = true;
                }
            }
            ui.add_space(8.0);
            if committed {
                let arranged = [(panel.id.clone(), changed)].into_iter().collect();
                self.apply_operations(
                    vec![designllm_contracts::Operation::ArrangePanels { bounds: arranged }],
                    [designllm_contracts::Intent::Design].into_iter().collect(),
                );
            }
            if let designllm_contracts::PanelBody::Plot(plot) = &panel.body {
                if feature_shown(self.feature_support(Feature::Plot)) {
                    self.plot_sections(ui, &palette, &figure, &panel.id, plot, &panel.style, &mut first);
                }
            } else {
                self.panel_style_section(ui, &palette, &panel.id, &panel.style, &mut first);
            }
            // `내보내기 ›` comes last, after the figure's data rows: `draw_properties` draws it.
        });
    }

    /// 차트 and 축 for a chart panel. Choices go to `figure.plot` (the data intent), titles to `figure.axis_label`.
    fn plot_sections(
        &mut self,
        ui: &mut Ui,
        palette: &theme::Palette,
        figure: &designllm_contracts::FigureDocument,
        panel_id: &str,
        plot: &designllm_contracts::PlotSpec,
        style: &designllm_contracts::Style,
        first: &mut bool,
    ) {
        let editable = feature_editable(self.feature_support(Feature::Plot));
        let columns: Vec<designllm_contracts::DataColumn> = figure
            .datasets
            .iter()
            .find(|dataset| dataset.id == plot.dataset_ref.dataset_id)
            .map(|dataset| dataset.columns.clone())
            .unwrap_or_default();
        let id = |name: &str| Id::new(("plot-field", panel_id.to_owned(), name.to_owned()));
        section_head(ui, palette, first, "차트", None);
        // Mockup `figure-board`: 종류 and Y 척도 are text-only fields.
        let kinds = [("line", "", "선"), ("bar", "", "막대"), ("scatter", "", "점")];
        if let Some(kind) = row(ui, palette, Some("종류"), |ui| menu_field(ui, palette, &kinds, plot.kind.as_str())) {
            if editable {
                self.invoke_ui("figure.plot", json!({ "panel_id": panel_id, "kind": kind }));
            }
        }
        let column_label = |column: &designllm_contracts::DataColumn| match column.unit.as_deref() {
            Some(unit) if !unit.is_empty() => format!("{}  {unit}", column.name),
            _ => column.name.clone(),
        };
        // A column of numbers (any numeric type the CSV reader names) shows `col-number`; text shows `col-category`.
        let numeric = |column: &designllm_contracts::DataColumn| !matches!(column.data_type.as_str(), "text" | "string" | "category");
        let icon_of = |column: &designllm_contracts::DataColumn| if numeric(column) { "col-number" } else { "col-category" };
        for (label, axis) in [("X 열", "x"), ("Y 열", "y")] {
            let current = if axis == "x" { Some(plot.x_column.as_str()) } else { plot.y_columns.first().map(String::as_str) };
            let shown = columns.iter().find(|column| Some(column.id.as_str()) == current);
            let picked = row(ui, palette, Some(label), |ui| {
                let width = ui.available_width();
                let button = widgets::dropdown(ui, palette, &shown.map(column_label).unwrap_or_default(), shown.map(icon_of), width);
                let mut picked = None;
                egui::Popup::from_toggle_button_response(&button)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| {
                        ui.set_width(width.max(200.0));
                        for column in &columns {
                            let chosen = Some(column.id.as_str()) == current;
                            if widgets::menu_item(ui, palette, Some(icon_of(column)), &column_label(column), "", chosen).clicked() {
                                if !chosen {
                                    picked = Some(column.id.clone());
                                }
                                ui.close();
                            }
                        }
                    });
                picked
            });
            if let Some(column) = picked.filter(|_| editable) {
                let change = if axis == "x" { json!({ "panel_id": panel_id, "x_column": column }) } else { json!({ "panel_id": panel_id, "y_columns": [column] }) };
                self.invoke_ui("figure.plot", change);
            }
        }
        ui.add_space(8.0);
        section_head(ui, palette, first, "축", None);
        // The engine reads the scale and unit of a numeric x column only; the y unit is always required.
        let x_numeric = columns.iter().any(|column| column.id == plot.x_column && column.data_type == "numeric");
        for (label, axis, spec) in [("X 범위", "x", &plot.x_axis), ("Y 범위", "y", &plot.y_axis)] {
            // The ends go out together (`range_change`). An end typed alone waits in the draft until the pair can be sent.
            let base = (spec.minimum.clone().unwrap_or_default(), spec.maximum.clone().unwrap_or_default());
            let key = Id::new(("plot-range", panel_id.to_owned(), axis.to_owned()));
            let draft = ui
                .data(|data| data.get_temp::<RangeDraft>(key))
                .filter(|draft| draft.base == base)
                .unwrap_or_else(|| RangeDraft { base: base.clone(), ..RangeDraft::default() });
            let shown_min = draft.min.clone().unwrap_or_else(|| base.0.clone());
            let shown_max = draft.max.clone().unwrap_or_else(|| base.1.clone());
            let (low, high) = row(ui, palette, Some(label), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let dash = text_width(ui, "–", theme::display_font(false, 12.0));
                let width = (ui.available_width() - dash - 8.0) / 2.0;
                let low = text_line_with_hint(ui, palette, id(&format!("{axis}-min")), width, &shown_min, "자동", editable);
                let (rect, _) = ui.allocate_exact_size(vec2(dash, theme::FIELD_HEIGHT), Sense::hover());
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, "–", theme::display_font(false, 12.0), palette.ink_3);
                let high = text_line_with_hint(ui, palette, id(&format!("{axis}-max")), width, &shown_max, "자동", editable);
                (low, high)
            });
            let (draft, change) = if editable && (low.is_some() || high.is_some()) {
                range_commit(panel_id, axis, draft, low, high)
            } else {
                (draft, None)
            };
            if let Some(change) = change {
                self.invoke_ui("figure.plot", change);
            }
            store_range_draft(ui, key, draft);
            if axis == "y" || x_numeric {
                self.axis_scale_unit_rows(ui, palette, panel_id, axis, spec, editable);
            }
        }
        ui.add_space(8.0);
        self.error_section(ui, palette, panel_id, plot, &columns, editable, first);
        self.panel_style_section(ui, palette, panel_id, style, first);
    }

    /// The scale and unit rows of one axis (`X 척도`, `X 단위`, `Y 척도`, `Y 단위`), through `figure.plot`.
    fn axis_scale_unit_rows(&mut self, ui: &mut Ui, palette: &theme::Palette, panel_id: &str, axis: &str, spec: &designllm_contracts::AxisSpec, editable: bool) {
        let (scale_label, unit_label) = if axis == "x" { ("X 척도", "X 단위") } else { ("Y 척도", "Y 단위") };
        let scales = [("linear", "", "선형"), ("log10", "", "로그")];
        let current = if spec.scale.is_empty() { "linear" } else { spec.scale.as_str() };
        if let Some(scale) = row(ui, palette, Some(scale_label), |ui| menu_field(ui, palette, &scales, current)) {
            if editable {
                self.invoke_ui("figure.plot", axis_field_request(panel_id, axis, "scale", json!(scale)));
            }
        }
        let unit_id = Id::new(("plot-field", panel_id.to_owned(), format!("{axis}-unit")));
        let current_unit = spec.unit.as_deref().unwrap_or_default();
        if let Some(unit) = row(ui, palette, Some(unit_label), |ui| {
            let width = ui.available_width();
            text_line_with_hint(ui, palette, unit_id, width, current_unit, "단위", editable)
        }) {
            if editable {
                self.invoke_ui("figure.plot", axis_field_request(panel_id, axis, "unit", json!(unit.trim())));
            }
        }
    }

    /// 오차 (mockup `figure-board`): the kind of error bar, the column(s) it reads, the sample count of SEM and the level
    /// of CI, through `figure.plot`. Each request carries the whole error its kind uses (`error_request`).
    #[allow(clippy::too_many_arguments)]
    fn error_section(
        &mut self,
        ui: &mut Ui,
        palette: &theme::Palette,
        panel_id: &str,
        plot: &designllm_contracts::PlotSpec,
        columns: &[designllm_contracts::DataColumn],
        editable: bool,
        first: &mut bool,
    ) {
        let kind = plot.error.as_ref().map_or("none", |spec| error_kind_name(spec.kind));
        let parts = ErrorParts::of(plot.error.as_ref());
        let value_columns = error_value_columns(columns, &plot.x_column, &plot.y_columns);
        section_head(ui, palette, first, "오차", None);
        let kinds = [("none", "", "없음"), ("sd", "", "표준편차"), ("sem", "", "표준오차"), ("ci", "", "신뢰구간"), ("range", "", "범위")];
        if let Some(new) = row(ui, palette, Some("종류"), |ui| menu_field(ui, palette, &kinds, kind)) {
            if editable {
                // A new kind keeps the columns it had; the columns it lacks come from the value columns.
                let spares: Vec<&str> = value_columns.iter().map(|column| column.id.as_str()).collect();
                let counts = count_columns(columns);
                let next = error_kind_change(&parts, new, &spares, counts.first().map(|column| column.id.as_str()));
                self.invoke_ui("figure.plot", error_request(panel_id, new, &next));
            }
        }
        let column_rows: Vec<(&str, &str, Option<&str>)> = match kind {
            "none" => Vec::new(),
            "range" => vec![("아래", "lower", parts.lower.as_deref()), ("위", "upper", parts.upper.as_deref())],
            _ => vec![("열", "column", parts.column.as_deref())],
        };
        for (label, part, current) in column_rows {
            // The two bounds of a range are two different columns: each menu leaves out the column of the other bound.
            let other = match part {
                "lower" => parts.upper.as_deref(),
                "upper" => parts.lower.as_deref(),
                _ => None,
            };
            let choices = value_columns.iter().copied().filter(|column| Some(column.id.as_str()) != other);
            if let Some(column) = row(ui, palette, Some(label), |ui| column_menu(ui, palette, choices, current)) {
                if editable {
                    let mut next = parts.clone();
                    let slot = match part {
                        "lower" => &mut next.lower,
                        "upper" => &mut next.upper,
                        _ => &mut next.column,
                    };
                    *slot = Some(column);
                    self.invoke_ui("figure.plot", error_request(panel_id, kind, &next));
                }
            }
        }
        if kind == "sem" {
            let counts = count_columns(columns);
            if let Some(column) = row(ui, palette, Some("n 열"), |ui| column_menu(ui, palette, &counts, parts.count.as_deref())) {
                if editable {
                    let mut next = parts.clone();
                    next.count = Some(column);
                    self.invoke_ui("figure.plot", error_request(panel_id, kind, &next));
                }
            }
        }
        if kind == "ci" {
            let level_id = Id::new(("plot-field", panel_id.to_owned(), "confidence".to_owned()));
            let level = row(ui, palette, Some("신뢰수준"), |ui| {
                let width = ui.available_width();
                text_line_with_hint(ui, palette, level_id, width, parts.level.as_deref().unwrap_or_default(), "0.95", editable)
            });
            if let Some(level) = level {
                if editable {
                    let mut next = parts.clone();
                    next.level = Some(level.trim().to_owned());
                    self.invoke_ui("figure.plot", error_request(panel_id, kind, &next));
                }
            }
        }
        ui.add_space(8.0);
    }

    /// 모양 of a figure panel (mockup `figure-board`): 글자 pt, 선 pt, 색, through `figure.style` on the panel.
    fn panel_style_section(&mut self, ui: &mut Ui, palette: &theme::Palette, panel_id: &str, style: &designllm_contracts::Style, first: &mut bool) {
        if !commands_available("figure.style") || !feature_editable(self.feature_support(Feature::FigurePanels)) {
            return;
        }
        let id = |name: &str| Id::new(("panel-style", panel_id.to_owned(), name.to_owned()));
        section_head(ui, palette, first, "모양", None);
        let size = style.font_size as f64 / EMU_PER_PT as f64;
        if let Some(new) = row(ui, palette, Some("글자"), |ui| {
            let width = ui.available_width();
            field(ui, palette, id("font"), width, "", (style.font_size > 0).then_some(size), "pt", 1, true)
        }) {
            self.invoke_ui("figure.style", json!({ "target_id": panel_id, "font_size": new.max(1.0) }));
        }
        let line = style.stroke_width as f64 / EMU_PER_PT as f64;
        if let Some(new) = row(ui, palette, Some("선"), |ui| {
            let width = ui.available_width();
            field(ui, palette, id("line"), width, "", Some(line), "pt", 1, true)
        }) {
            self.invoke_ui("figure.style", json!({ "target_id": panel_id, "stroke_width": new.max(0.0) }));
        }
        let colour = (!style.fill.is_empty() && style.fill != "none").then(|| style.fill.clone());
        if let Some(new) = row(ui, palette, Some("색"), |ui| {
            let width = ui.available_width();
            colour_field(ui, palette, id("colour"), width, colour.as_deref())
        }) {
            self.invoke_ui("figure.style", json!({ "target_id": panel_id, "fill": new }));
        }
        ui.add_space(8.0);
    }
}

/// A data column picker filling the row (the 열 rows of 차트 and 오차): `12` / tag icon + name + unit. Returns the
/// column picked when it changes.
fn column_menu<'a>(
    ui: &mut Ui,
    palette: &theme::Palette,
    columns: impl IntoIterator<Item = &'a designllm_contracts::DataColumn>,
    current: Option<&str>,
) -> Option<String> {
    let columns: Vec<&designllm_contracts::DataColumn> = columns.into_iter().collect();
    let label = |column: &designllm_contracts::DataColumn| match column.unit.as_deref() {
        Some(unit) if !unit.is_empty() => format!("{}  {unit}", column.name),
        _ => column.name.clone(),
    };
    let icon_of = |column: &designllm_contracts::DataColumn| if matches!(column.data_type.as_str(), "text" | "string" | "category") { "col-category" } else { "col-number" };
    let shown = columns.iter().copied().find(|column| Some(column.id.as_str()) == current);
    let width = ui.available_width();
    let button = widgets::dropdown(ui, palette, &shown.map(label).unwrap_or_default(), shown.map(icon_of), width);
    let mut picked = None;
    egui::Popup::from_toggle_button_response(&button)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(width.max(200.0));
            for column in columns.iter().copied() {
                let chosen = Some(column.id.as_str()) == current;
                if widgets::menu_item(ui, palette, Some(icon_of(column)), &label(column), "", chosen).clicked() {
                    if !chosen {
                        picked = Some(column.id.clone());
                    }
                    ui.close();
                }
            }
        });
    picked
}

/// The parts of a plot's error that the error section edits. `figure_plot` rebuilds the whole error from a request, so a
/// request carries every part its kind uses (`error_request`).
#[derive(Clone, Debug, Default, PartialEq)]
struct ErrorParts {
    column: Option<String>,
    lower: Option<String>,
    upper: Option<String>,
    count: Option<String>,
    level: Option<String>,
}

impl ErrorParts {
    fn of(error: Option<&designllm_contracts::ErrorSpec>) -> Self {
        use designllm_contracts::ErrorValues;
        let Some(spec) = error else {
            return Self::default();
        };
        let mut parts = Self { count: spec.sample_count_column.clone(), level: spec.confidence_level.clone(), ..Self::default() };
        match &spec.values {
            ErrorValues::Symmetric { column } => parts.column = Some(column.clone()),
            // The minus and plus amounts of an asymmetric error are amounts, not bounds: a range takes its own bounds.
            ErrorValues::Asymmetric { .. } => {}
            ErrorValues::Bounds { lower_column, upper_column } => {
                parts.lower = Some(lower_column.clone());
                parts.upper = Some(upper_column.clone());
            }
        }
        parts
    }
}

/// The `error_kind` name of a plot error kind.
fn error_kind_name(kind: designllm_contracts::PlotErrorKind) -> &'static str {
    use designllm_contracts::PlotErrorKind;
    match kind {
        PlotErrorKind::Sd => "sd",
        PlotErrorKind::Sem => "sem",
        PlotErrorKind::Ci => "ci",
        PlotErrorKind::Range => "range",
    }
}

/// The parts after the error kind changes to `kind`. The new kind keeps the columns it had. `spares` are the value columns
/// (numeric, not on an axis): a range takes two different ones for its bounds, a kind with one column takes the first.
/// SEM takes the first count column when it has no sample count: the engine refuses SEM without one and never estimates it.
fn error_kind_change(parts: &ErrorParts, kind: &str, spares: &[&str], count: Option<&str>) -> ErrorParts {
    let mut next = parts.clone();
    if kind == "range" {
        if next.lower.is_none() {
            let lower = spares.iter().copied().find(|spare| Some(*spare) != next.upper.as_deref());
            next.lower = lower.map(str::to_owned);
        }
        if next.upper.is_none() {
            let upper = spares.iter().copied().find(|spare| Some(*spare) != next.lower.as_deref());
            next.upper = upper.map(str::to_owned);
        }
    } else if next.column.is_none() {
        next.column = spares.first().copied().map(str::to_owned);
    }
    if kind == "sem" && next.count.is_none() {
        next.count = count.map(str::to_owned);
    }
    next
}

/// The `figure.plot` change that sets the error to `kind` with the parts the kind uses: the bounds of range, the one column
/// of sd, sem and ci, the sample count of SEM and the level of CI. `figure_plot` clears a part that a request leaves out.
fn error_request(panel_id: &str, kind: &str, parts: &ErrorParts) -> serde_json::Value {
    let mut change = serde_json::Map::new();
    change.insert("panel_id".into(), json!(panel_id));
    change.insert("error_kind".into(), json!(kind));
    let mut put = |key: &str, value: Option<&str>| {
        if let Some(value) = value {
            change.insert(key.into(), json!(value));
        }
    };
    match kind {
        "range" => {
            put("error_lower_column", parts.lower.as_deref());
            put("error_upper_column", parts.upper.as_deref());
        }
        "sd" | "sem" | "ci" => {
            put("error_column", parts.column.as_deref());
            if kind == "sem" {
                put("error_count_column", parts.count.as_deref());
            }
            if kind == "ci" {
                put("confidence_level", Some(parts.level.as_deref().unwrap_or("0.95")));
            }
        }
        _ => {}
    }
    serde_json::Value::Object(change)
}

/// The `figure.plot` change for one field of an axis: `{axis}_{field}`, such as `x_scale` or `y_unit`.
fn axis_field_request(panel_id: &str, axis: &str, field: &str, value: serde_json::Value) -> serde_json::Value {
    let mut change = serde_json::Map::new();
    change.insert("panel_id".into(), json!(panel_id));
    change.insert(format!("{axis}_{field}"), value);
    serde_json::Value::Object(change)
}

/// An axis range being typed. The engine takes both bounds or neither, so an end waits here until the pair can be sent.
/// `base` holds the stored `(minimum, maximum)` texts the draft was typed against; `""` is automatic.
#[derive(Clone, Debug, Default, PartialEq)]
struct RangeDraft {
    base: (String, String),
    min: Option<String>,
    max: Option<String>,
}

/// The `figure.plot` change for an axis range: both ends automatic (`""`, sent as null, which returns the axis to
/// automatic) or both numbers. None for a partial pair and for a text that is not a number.
fn range_change(panel_id: &str, axis: &str, min: &str, max: &str) -> Option<serde_json::Value> {
    let end = |text: &str| -> Option<serde_json::Value> {
        if text.trim().is_empty() {
            Some(serde_json::Value::Null)
        } else {
            widgets::parse_number(text).map(|number| json!(number))
        }
    };
    let (low, high) = (end(min)?, end(max)?);
    if low.is_null() != high.is_null() {
        return None;
    }
    let mut change = serde_json::Map::new();
    change.insert("panel_id".into(), json!(panel_id));
    change.insert(format!("{axis}_minimum"), low);
    change.insert(format!("{axis}_maximum"), high);
    Some(serde_json::Value::Object(change))
}

/// The draft after the ends a field committed this frame (`None`: not committed), and the change to send when the pair is
/// sendable and differs from the stored range. A text that is not a number drops its end, so the stored end shows again.
fn range_commit(panel_id: &str, axis: &str, mut draft: RangeDraft, low: Option<String>, high: Option<String>) -> (RangeDraft, Option<serde_json::Value>) {
    let typed = |text: String| {
        let text = text.trim().to_owned();
        (text.is_empty() || widgets::parse_number(&text).is_some()).then_some(text)
    };
    if let Some(text) = low {
        draft.min = typed(text);
    }
    if let Some(text) = high {
        draft.max = typed(text);
    }
    let min = draft.min.as_deref().unwrap_or(&draft.base.0);
    let max = draft.max.as_deref().unwrap_or(&draft.base.1);
    let Some(change) = range_change(panel_id, axis, min, max) else {
        return (draft, None);
    };
    let stored = (widgets::parse_number(&draft.base.0), widgets::parse_number(&draft.base.1));
    let unchanged = (widgets::parse_number(min), widgets::parse_number(max)) == stored;
    (RangeDraft { base: draft.base, ..RangeDraft::default() }, (!unchanged).then_some(change))
}

/// Keeps a range draft while one of its ends waits. A draft with no end waiting is forgotten.
fn store_range_draft(ui: &mut Ui, key: Id, draft: RangeDraft) {
    ui.data_mut(|data| {
        if draft.min.is_none() && draft.max.is_none() {
            data.remove::<RangeDraft>(key);
        } else {
            data.insert_temp(key, draft);
        }
    });
}

/// The columns that can be a sample count: the count type, which the engine checks.
fn count_columns(columns: &[designllm_contracts::DataColumn]) -> Vec<designllm_contracts::DataColumn> {
    columns.iter().filter(|column| column.data_type == "count").cloned().collect()
}

/// The columns an error can read: numeric columns that are not on an axis. The engine refuses any other column.
fn error_value_columns<'a>(columns: &'a [designllm_contracts::DataColumn], x_column: &str, y_columns: &[String]) -> Vec<&'a designllm_contracts::DataColumn> {
    columns.iter().filter(|column| column.data_type == "numeric" && column.id != x_column && !y_columns.contains(&column.id)).collect()
}

/// The height of a section head: 34, or 30 when the window is lower than 640 (§11 좁은 창, tokens
/// `section-header.height-short`).
/// The path section's draft for one node: the two fields, the stored texts they were started from, and the parser's
/// reason for the last refused apply. It lives in the window's memory.
#[derive(Clone, Default)]
struct NodePathDraft {
    /// The artifact revision the base texts were read at; None before the first frame.
    revision: Option<u64>,
    base_d: String,
    base_view_box: String,
    d: String,
    view_box: String,
    error: Option<String>,
}

/// Brings a draft to the stored path of `revision`. Text that still describes the path the draft started from follows the
/// stored path; text the user changed is kept. Both are compared as paths, so a re-spelled path counts as unchanged.
fn refresh_path_draft(draft: &mut NodePathDraft, revision: u64, stored: &PathSpec) {
    let untouched = draft.revision.is_none() || draft_matches_base(draft);
    let (stored_d, stored_view_box) = (path_data_text(stored), view_box_text(&stored.view_box));
    if untouched {
        draft.d = stored_d.clone();
        draft.view_box = stored_view_box.clone();
        draft.error = None;
    }
    draft.base_d = stored_d;
    draft.base_view_box = stored_view_box;
    draft.revision = Some(revision);
}

/// Whether the draft's text still reads as the path its base texts read as.
fn draft_matches_base(draft: &NodePathDraft) -> bool {
    match (path_from_fields(&draft.d, &draft.view_box), path_from_fields(&draft.base_d, &draft.base_view_box)) {
        (Ok(typed), Ok(base)) => typed == base,
        _ => false,
    }
}

/// A single-line field of the path section, drawn as the other property fields are.
fn path_text_field(ui: &mut Ui, palette: &theme::Palette, id: Id, width: f32, text: &mut String, enabled: bool) -> bool {
    widgets::edit_line(ui, palette, text, widgets::Field { width: width.max(40.0), id: Some(id), enabled, ..Default::default() }).changed()
}

/// The path the two fields describe. The view box is read first, as the insert form reads it, then the data against it.
/// The refusal texts are the insert form's, the length limit included.
fn path_from_fields(d: &str, view_box: &str) -> Result<PathSpec, String> {
    super::kind_commands::check_path_data_length(d)?;
    let view_box = super::kind_commands::parse_view_box(view_box)?;
    PathSpec::from_svg(d, view_box).map_err(|error| format!("경로 데이터를 넣지 않았습니다. {}", super::kind_commands::path_reason(&error, d)))
}

/// The path as SVG path data, in the text the fields show: absolute commands, coordinates in user units with at most three
/// decimals. Read back by the parser, it gives the same path.
fn path_data_text(path: &PathSpec) -> String {
    use designllm_contracts::PathCommand;
    use std::fmt::Write as _;
    let mut text = String::new();
    for command in &path.commands {
        if !text.is_empty() {
            text.push(' ');
        }
        match *command {
            PathCommand::MoveTo { x, y } => {
                let _ = write!(text, "M {} {}", path_units(x), path_units(y));
            }
            PathCommand::LineTo { x, y } => {
                let _ = write!(text, "L {} {}", path_units(x), path_units(y));
            }
            PathCommand::CubicTo { x1, y1, x2, y2, x, y } => {
                let _ = write!(
                    text,
                    "C {} {} {} {} {} {}",
                    path_units(x1), path_units(y1), path_units(x2), path_units(y2), path_units(x), path_units(y)
                );
            }
            PathCommand::Close => text.push('Z'),
        }
    }
    text
}

/// The view box as `min-x min-y width height` in user units, the text the insert form takes.
fn view_box_text(view_box: &designllm_contracts::Rect) -> String {
    format!("{} {} {} {}", path_units(view_box.x), path_units(view_box.y), path_units(view_box.width), path_units(view_box.height))
}

/// A stored value (thousandths of a user unit) as user units, with at most three decimals and no trailing zeros.
fn path_units(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.unsigned_abs();
    match magnitude % 1000 {
        0 => format!("{sign}{}", magnitude / 1000),
        fraction => format!("{sign}{}.{}", magnitude / 1000, format!("{fraction:03}").trim_end_matches('0')),
    }
}

fn section_height(ui: &Ui) -> f32 {
    if ui.ctx().content_rect().height() < 640.0 {
        theme::SECTION_HEADER_SHORT
    } else {
        theme::SECTION_HEADER
    }
}

fn commands_available(id: &str) -> bool {
    crate::commands::available(id)
}

/// The unit a stroke width is shown in (§8): whole CSS px where the document's unit is px, pt with one decimal
/// in mm documents. Returns (EMU per shown unit, label, decimals).
pub(super) fn stroke_unit(unit: Unit) -> (f64, &'static str, usize) {
    match unit {
        Unit::Px => (EMU_PER_PX, "px", 0),
        Unit::Mm => (EMU_PER_PT as f64, "pt", 1),
    }
}

/// Whether every node of a multi-selection has the primary node's value (an empty group always agrees).
fn agree<N, T: PartialEq>(group: &[N], node: &N, pick: impl Fn(&N) -> T) -> bool {
    let own = pick(node);
    group.iter().all(|other| pick(other) == own)
}

fn parse_image_crop_fields(fields: &[String; 4]) -> Option<designllm_contracts::PixelRect> {
    Some(designllm_contracts::PixelRect {
        x: fields[0].parse().ok()?,
        y: fields[1].parse().ok()?,
        width: fields[2].parse().ok()?,
        height: fields[3].parse().ok()?,
    })
}

fn image_crop_copy_payload(editor: &super::ImageCropEditor) -> Option<String> {
    let (width, height) = editor.dimensions?;
    if let Some(requested) = editor.requested_crop {
        return Some(match requested {
            Some(rect) => image_crop_copy_payload_fields(&super::state::image_crop_fields(rect), width, height),
            None => format!("전체 이미지로 초기화 (원본 {}×{} px)", width, height),
        });
    }
    Some(image_crop_copy_payload_fields(editor.draft.as_ref()?, width, height))
}

fn image_crop_copy_payload_fields(fields: &[String; 4], width: u32, height: u32) -> String {
    format!("X={}, Y={}, 폭={}, 높이={} px (원본 {}×{} px)", fields[0], fields[1], fields[2], fields[3], width, height)
}

fn size_mode_label(mode: &str) -> &'static str {
    match mode {
        "hug" => "콘텐츠 맞춤",
        "fill" => "꽉 채우기",
        _ => "고정",
    }
}

fn size_mode_icon(mode: &str) -> &'static str {
    match mode {
        "hug" => "size-hug",
        "fill" => "size-fill",
        _ => "size-fixed",
    }
}

fn text_width(ui: &Ui, text: &str, font: egui::FontId) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    ui.painter().layout_no_wrap(text.to_owned(), font, Color32::BLACK).size().x
}

/// A section head (mockup `.sh`): a 1 px `line` across the panel above every section but the first, then a 34 px
/// row with the 13 px bold title and, at the right, an optional `.sha` action (16 px icon + 12 px label, `ink-2`).
/// Returns true when the action was clicked.
pub(super) fn section_head(ui: &mut Ui, palette: &theme::Palette, first: &mut bool, title: &str, action: Option<(&str, &str)>) -> bool {
    if *first {
        ui.add_space(4.0);
    } else {
        let top = ui.cursor().top();
        let span = ui.max_rect();
        ui.painter().hline((span.left() - 12.0)..=(span.right() + 12.0), top + 0.5, Stroke::new(1.0, palette.line));
        ui.add_space(1.0);
    }
    *first = false;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), section_height(ui)), Sense::hover());
    ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, title, theme::display_font(true, 13.0), palette.ink);
    let Some((icon, label)) = action else {
        return false;
    };
    let font = theme::display_font(false, 12.0);
    let width = 4.0 + 16.0 + 4.0 + text_width(ui, label, font.clone()) + 6.0;
    let button = Rect::from_min_size(Pos2::new(rect.right() - width, rect.center().y - 12.0), vec2(width, 24.0));
    let response = ui.interact(button, ui.id().with(("section-action", title)), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(button, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
    }
    icons::paint(ui, Rect::from_min_size(Pos2::new(button.left() + 4.0, button.center().y - 8.0), Vec2::splat(16.0)), icon, palette.ink_2);
    ui.painter().text(Pos2::new(button.left() + 24.0, button.center().y), Align2::LEFT_CENTER, label, font, palette.ink_2);
    response.clicked()
}

/// A 32 px property row (mockup `.row`): the label in the 76 px column (12 px `ink-2`), then the controls with 8 px
/// between them. Without a label the controls take the whole row.
pub(super) fn row<R>(ui: &mut Ui, palette: &theme::Palette, label: Option<&str>, add: impl FnOnce(&mut Ui) -> R) -> R {
    row_with(ui, palette, label, theme::LABEL_COLUMN, add)
}

/// `row` with another label column (the fill popover uses 40).
fn row_with<R>(ui: &mut Ui, palette: &theme::Palette, label: Option<&str>, column: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(vec2(width, theme::PROPERTY_ROW), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(vec2(width, theme::PROPERTY_ROW));
        ui.spacing_mut().item_spacing.x = 8.0;
        if let Some(label) = label {
            let (rect, _) = ui.allocate_exact_size(vec2(column - 8.0, theme::PROPERTY_ROW), Sense::hover());
            ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, label, theme::display_font(false, 12.0), palette.ink_2);
        }
        add(ui)
    })
    .inner
}

/// A 28 px number field `width` wide (mockup `.fld`): an optional prefix (`X`, `Y`) and the unit in 12 px `ink-3`
/// inside the field. `value` None means the selection disagrees: the field is empty and shows `여러 값` in `ink-3`.
/// A typed number is committed when the field loses focus (Enter or a click elsewhere).
#[allow(clippy::too_many_arguments)]
pub(super) fn field(ui: &mut Ui, palette: &theme::Palette, id: Id, width: f32, prefix: &str, value: Option<f64>, unit: &str, decimals: usize, enabled: bool) -> Option<f64> {
    field_with_hint(ui, palette, id, width, prefix, value, unit, decimals, enabled, "여러 값")
}

/// `field` whose empty state shows `hint` (`자동` for an axis bound the engine chooses).
#[allow(clippy::too_many_arguments)]
fn field_with_hint(ui: &mut Ui, palette: &theme::Palette, id: Id, width: f32, prefix: &str, value: Option<f64>, unit: &str, decimals: usize, enabled: bool, hint: &str) -> Option<f64> {
    let draft_id = id.with("draft");
    // §8: mm values always show one decimal (2.0, 86.0); other units drop trailing zeros.
    let formatted = value
        .map(|value| if unit == "mm" { format!("{value:.decimals$}") } else { widgets::format_number(value, decimals) })
        .unwrap_or_default();
    let mut draft = ui.data(|data| data.get_temp::<String>(draft_id)).unwrap_or(formatted);
    let small = theme::display_font(false, 12.0);
    let prefix_width = text_width(ui, prefix, small.clone());
    let unit_width = text_width(ui, unit, small.clone());
    let left = 8.0 + if prefix.is_empty() { 0.0 } else { prefix_width + 6.0 };
    let right = 8.0 + if unit.is_empty() { 0.0 } else { unit_width + 6.0 };
    let hint = if value.is_none() { hint } else { "" };
    let margin = egui::Margin { left: left.round() as i8, right: right.round() as i8, top: 4, bottom: 4 };
    let response = widgets::edit_line(ui, palette, &mut draft, widgets::Field { hint, width: width.max(40.0), id: Some(id), enabled, margin: Some(margin), ..Default::default() });
    if matches!(prefix, "폭" | "열") {
        crate::ui_record::note(ui, prefix, &response);
    }
    let rect = response.rect;
    if !prefix.is_empty() {
        ui.painter().text(Pos2::new(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, prefix, small.clone(), palette.ink_3);
    }
    if !unit.is_empty() {
        ui.painter().text(Pos2::new(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, unit, small, palette.ink_3);
    }
    if response.has_focus() {
        ui.data_mut(|data| data.insert_temp(draft_id, draft.clone()));
    } else {
        ui.data_mut(|data| data.remove::<String>(draft_id));
    }
    if !response.lost_focus() {
        return None;
    }
    let parsed = widgets::parse_number(&draft)?;
    match value {
        Some(value) if (parsed - value).abs() <= 1e-9 => None,
        _ => Some(parsed),
    }
}

/// A one-line text field in the panel style (label, note title). Returns the text when it is committed changed.
fn text_line(ui: &mut Ui, palette: &theme::Palette, id: Id, width: f32, value: &str, enabled: bool) -> Option<String> {
    text_line_with_hint(ui, palette, id, width, value, "", enabled)
}

fn text_line_with_hint(
    ui: &mut Ui,
    palette: &theme::Palette,
    id: Id,
    width: f32,
    value: &str,
    hint: &str,
    enabled: bool,
) -> Option<String> {
    let draft_id = id.with("draft");
    let mut draft = ui.data(|data| data.get_temp::<String>(draft_id)).unwrap_or_else(|| value.to_owned());
    let hint = if !hint.is_empty() && value.is_empty() { hint } else { "" };
    let response = widgets::edit_line(ui, palette, &mut draft, widgets::Field { hint, width: width.max(40.0), id: Some(id), enabled, ..Default::default() });
    if response.has_focus() {
        ui.data_mut(|data| data.insert_temp(draft_id, draft.clone()));
    } else {
        ui.data_mut(|data| data.remove::<String>(draft_id));
    }
    (response.lost_focus() && draft != value).then_some(draft)
}

fn text_line_draft(
    ui: &mut Ui,
    palette: &theme::Palette,
    id: Id,
    state_key: &str,
    revision: u64,
    width: f32,
    value: &str,
    hint: &str,
) -> (String, bool) {
    let state_id = id.with("draft");
    let saved = ui.data(|data| data.get_temp::<(String, u64, String)>(state_id));
    let mut draft = saved
        .filter(|(saved_key, saved_revision, _)| saved_key == state_key && *saved_revision == revision)
        .map(|(_, _, draft)| draft)
        .unwrap_or_else(|| value.to_owned());
    let hint = if !hint.is_empty() && value.is_empty() { hint } else { "" };
    let response = widgets::edit_line(ui, palette, &mut draft, widgets::Field { hint, width: width.max(40.0), id: Some(id), ..Default::default() });
    ui.data_mut(|data| data.insert_temp(state_id, (state_key.to_owned(), revision, draft.clone())));
    let enter = ui.input(|input| input.key_pressed(egui::Key::Enter)) && (response.has_focus() || response.lost_focus());
    (draft, enter)
}

/// A colour field (mockup `.fld` with `.sw`): a 16 px swatch inside the field, then `#RRGGBB`. `value` None means the
/// selection disagrees (empty, `여러 값`). Returns the normalised colour when a valid one is committed.
pub(super) fn colour_field(ui: &mut Ui, palette: &theme::Palette, id: Id, width: f32, value: Option<&str>) -> Option<String> {
    colour_field_at(ui, palette, id, width, value).0
}

/// `colour_field` that also returns the field's rectangle (the fill row anchors its popover there).
fn colour_field_at(ui: &mut Ui, palette: &theme::Palette, id: Id, width: f32, value: Option<&str>) -> (Option<String>, Rect) {
    let draft_id = id.with("draft");
    let mut draft = ui.data(|data| data.get_temp::<String>(draft_id)).unwrap_or_else(|| value.map(widgets::normalise_hex).unwrap_or_default());
    let swatch = value.and_then(widgets::parse_hex);
    let left: i8 = if swatch.is_some() { 30 } else { 8 };
    let hint = if value.is_none() { "여러 값" } else { "" };
    let response = widgets::edit_line(ui, palette, &mut draft, widgets::Field {
        hint,
        width: width.max(60.0),
        id: Some(id),
        margin: Some(egui::Margin { left, right: 8, top: 4, bottom: 4 }),
        ..Default::default()
    });
    if let Some(colour) = swatch {
        let sample = Rect::from_min_size(Pos2::new(response.rect.left() + 8.0, response.rect.center().y - 8.0), Vec2::splat(16.0));
        ui.painter().rect_filled(sample, CornerRadius::same(theme::RADIUS_SMALL), colour);
        ui.painter().rect_stroke(sample, CornerRadius::same(theme::RADIUS_SMALL), Stroke::new(1.0, palette.swatch_ring), StrokeKind::Inside);
    }
    if response.has_focus() {
        ui.data_mut(|data| data.insert_temp(draft_id, draft.clone()));
    } else {
        ui.data_mut(|data| data.remove::<String>(draft_id));
    }
    let rect = response.rect;
    if !response.lost_focus() || widgets::parse_hex(&draft).is_none() {
        return (None, rect);
    }
    let new = widgets::normalise_hex(&draft);
    ((value.map(widgets::normalise_hex).as_deref() != Some(new.as_str())).then_some(new), rect)
}

/// One row of the document style list.
struct DocStyleEntry {
    id: String,
    label: String,
    user: bool,
}

/// The built-in DOCX paragraph styles in list order with their Korean names (tokens `component.doc-style-list`).
const DOCX_STYLE_NAMES: &[(&str, &str)] = &[
    ("Normal", "본문"),
    ("Title", "제목"),
    ("Subtitle", "부제"),
    ("Heading1", "제목 1"),
    ("Heading2", "제목 2"),
    ("Heading3", "제목 3"),
    ("Heading4", "제목 4"),
    ("Heading5", "제목 5"),
    ("Heading6", "제목 6"),
    ("Quote", "인용"),
    ("IntenseQuote", "강한 인용"),
    ("ListParagraph", "목록 문단"),
    ("Caption", "캡션"),
    ("NoSpacing", "간격 없음"),
    ("TOCHeading", "목차 제목"),
    ("TOC1", "목차 1"),
    ("TOC2", "목차 2"),
    ("TOC3", "목차 3"),
    ("Header", "머리글"),
    ("Footer", "바닥글"),
    ("FootnoteText", "각주 글"),
    ("EndnoteText", "미주 글"),
];

/// The paragraph styles a person picks from (tokens `component.doc-style-list`): in a DOCX the built-in paragraph
/// styles the document defines (named in Korean, in the table's order) and then, after a divider, the other styles
/// its paragraphs use (가나다 order). Styles no paragraph uses and that are not built-in paragraph styles — character,
/// table and list styles such as `DefaultParagraphFont` or `TableGrid` — are left out. The model carries no style
/// type or `w:qFormat` flag, so "used by a paragraph" stands in for them. HWPX and HWP keep their own style names.
fn doc_style_list(
    document: &designllm_contracts::FlowDocument,
    kind: ArtifactKind,
    engine: Option<&[designllm_contracts::FlowStyleInfo]>,
) -> Vec<DocStyleEntry> {
    fn collect(blocks: &[designllm_contracts::FlowBlock], used: &mut std::collections::BTreeSet<String>) {
        for block in blocks {
            match block {
                designllm_contracts::FlowBlock::Paragraph(paragraph) => {
                    used.insert(paragraph.style_ref.clone().unwrap_or_else(|| String::from("Normal")));
                }
                designllm_contracts::FlowBlock::Table(table) => {
                    for cell in table.rows.iter().flatten() {
                        collect(&cell.blocks, used);
                    }
                }
                _ => {}
            }
        }
    }
    let mut used = std::collections::BTreeSet::new();
    for section in &document.sections {
        collect(&section.blocks, &mut used);
    }
    if let Some(engine) = engine {
        return engine_style_list(engine, &used, kind);
    }
    if kind != ArtifactKind::Docx {
        return document
            .styles
            .keys()
            .filter(|id| used.contains(*id) || document.styles.len() <= 12)
            .map(|id| DocStyleEntry { id: id.clone(), label: id.clone(), user: false })
            .collect();
    }
    let mut entries: Vec<DocStyleEntry> = DOCX_STYLE_NAMES
        .iter()
        .filter(|(id, _)| document.styles.contains_key(*id))
        .map(|(id, label)| DocStyleEntry { id: (*id).to_owned(), label: (*label).to_owned(), user: false })
        .collect();
    let mut users: Vec<DocStyleEntry> = used
        .iter()
        .filter(|id| document.styles.contains_key(*id) && !DOCX_STYLE_NAMES.iter().any(|(known, _)| known == id))
        .map(|id| DocStyleEntry { id: id.clone(), label: id.clone(), user: true })
        .collect();
    users.sort_by(|a, b| a.label.cmp(&b.label));
    entries.extend(users);
    entries
}

/// tokens `component.doc-style-list` from the engine's list: paragraph styles only, not hidden, that a paragraph
/// uses or that are quick built-ins. Built-ins take the Korean names of the table (DOCX) in its order, then the other
/// built-ins in document order; user styles follow after a divider by their display name in 가나다 order. HWPX and
/// HWP keep the document's names.
fn engine_style_list(engine: &[designllm_contracts::FlowStyleInfo], used: &std::collections::BTreeSet<String>, kind: ArtifactKind) -> Vec<DocStyleEntry> {
    let picked: Vec<&designllm_contracts::FlowStyleInfo> = engine
        .iter()
        .filter(|style| style.kind == designllm_contracts::FlowStyleKind::Paragraph && !style.hidden)
        // "쓰인 것 + 빠른 스타일(w:qFormat)": a quick style is listed whether built-in or the document's own.
        .filter(|style| used.contains(&style.id) || style.quick)
        .collect();
    let shown = |style: &designllm_contracts::FlowStyleInfo| style.display_name.clone().filter(|name| !name.is_empty()).unwrap_or_else(|| style.id.clone());
    let korean = |id: &str| if kind == ArtifactKind::Docx { DOCX_STYLE_NAMES.iter().position(|(known, _)| *known == id) } else { None };
    let mut builtins: Vec<(usize, usize, DocStyleEntry)> = picked
        .iter()
        .enumerate()
        .filter(|(_, style)| style.builtin)
        .map(|(order, style)| match korean(&style.id) {
            Some(rank) => (rank, order, DocStyleEntry { id: style.id.clone(), label: DOCX_STYLE_NAMES[rank].1.to_owned(), user: false }),
            None => (DOCX_STYLE_NAMES.len(), order, DocStyleEntry { id: style.id.clone(), label: shown(style), user: false }),
        })
        .collect();
    builtins.sort_by_key(|(rank, order, _)| (*rank, *order));
    let mut users: Vec<DocStyleEntry> = picked
        .iter()
        .filter(|style| !style.builtin)
        .map(|style| DocStyleEntry { id: style.id.clone(), label: shown(style), user: true })
        .collect();
    users.sort_by(|a, b| a.label.cmp(&b.label));
    builtins.into_iter().map(|(_, _, entry)| entry).chain(users).collect()
}

/// What the fill popover asks for.
enum FillChoice {
    Solid(String),
    RemoveGradient,
    Gradient { from: String, to: String, angle: f64 },
}

/// The command a fill choice runs, with its arguments. The popover sends it with `invoke_ui`.
fn fill_choice_command(choice: &FillChoice) -> (&'static str, serde_json::Value) {
    match choice {
        FillChoice::Solid(colour) => ("props.fill", json!({ "color": colour })),
        FillChoice::RemoveGradient => ("props.gradient_remove", json!({})),
        FillChoice::Gradient { from, to, angle } => ("props.gradient", json!({ "from": from, "to": to, "angle": angle })),
    }
}

/// The fill popover's choices. `단색` is offered when the node has no gradient, or when a gradient can be removed.
fn fill_items(gradient_set: bool, removable: bool) -> Vec<(&'static str, &'static str)> {
    let mut items = Vec::new();
    if !gradient_set || removable {
        items.push(("fill-solid", "단색"));
    }
    items.push(("fill-gradient", "그라디언트"));
    items
}

/// Minimum width of the first breakpoint of a grid that has none: a common tablet width.
const FIRST_BREAKPOINT_WIDTH: u32 = 768;
/// How much wider a new breakpoint is than the widest one.
const BREAKPOINT_STEP: u32 = 128;

/// The change a breakpoint control emits: the node's whole layout (`SetLayout`), one change set and one undo step.
fn layout_operation(node_id: &str, layout: designllm_contracts::LayoutSpec) -> designllm_contracts::Operation {
    designllm_contracts::Operation::SetLayout { node_id: node_id.to_string(), layout }
}

/// The egui ids of the width and columns fields of each breakpoint row, in the order of the list. A row is named by its minimum
/// width, not by its place: a width edit re-sorts the list, and an id by place would hand the focus and the typed text of one
/// breakpoint's field to the breakpoint that takes its place. The engine requires strictly increasing widths (canvas
/// `model.rs`), so each width names one row.
fn breakpoint_field_ids(node_id: &str, spec: &designllm_contracts::LayoutSpec) -> Vec<(Id, Id)> {
    spec.breakpoints.iter().map(|breakpoint| breakpoint_row_ids(node_id, breakpoint.min_width_css_px)).collect()
}

fn breakpoint_row_ids(node_id: &str, width: u32) -> (Id, Id) {
    (Id::new(("breakpoint-width", node_id, width)), Id::new(("breakpoint-columns", node_id, width)))
}

/// One breakpoint row as the focus follow compares it: the width, the columns it shows and every other value of the breakpoint.
#[derive(Clone, PartialEq)]
struct BreakpointRow {
    width: u32,
    columns: u32,
    rest: (Option<String>, Option<i64>, Option<i64>, Option<String>, Option<Justify>),
}

impl BreakpointRow {
    fn of(breakpoint: &designllm_contracts::Breakpoint, base_columns: u32) -> Self {
        // No `..`: a field added to `Breakpoint` stops the build here until the follow compares it too.
        let designllm_contracts::Breakpoint { min_width_css_px, mode, gap, padding, align, grid_columns, justify } = breakpoint;
        Self { width: *min_width_css_px, columns: grid_columns.unwrap_or(base_columns), rest: (mode.clone(), *gap, *padding, align.clone(), *justify) }
    }

    fn same_but_width(&self, other: &Self) -> bool {
        self.columns == other.columns && self.rest == other.rest
    }
}

/// Keeps the focus and the typed text of a breakpoint field when another connection changes only its row's width. The ids follow
/// the width, so such a change moves the row to new ids and the focused field would lose both. The rows drawn in the pass before
/// are kept in egui's temp data under the artifact and the node. The focus moves only when that is the one row that changed, at the
/// same place, with every value but the width the same and to ids no row had: then it is the same breakpoint (the engine keeps the
/// list sorted). Any other change (a re-sort, a removal, an added row, another value of the row) drops the focus and the text, so
/// nothing typed for one breakpoint is committed to another. A removal and an addition at the same place whose breakpoints differ
/// only in width (`[768, 1024]` to `[500, 1024]`, all else equal) cannot be told from a width change and is followed.
fn follow_focused_breakpoint(ui: &mut Ui, artifact_id: &str, node_id: &str, rows: Vec<BreakpointRow>, ids: Vec<(Id, Id)>, locked: bool) -> (Vec<(Id, Id)>, bool) {
    let memory_id = Id::new(("breakpoint-rows-last-frame", artifact_id, node_id));
    let pass = ui.ctx().cumulative_pass_nr();
    let previous: Option<(u64, Vec<BreakpointRow>)> = ui.data(|data| data.get_temp(memory_id));
    let mut discarded_draft = false;
    if let (Some((drawn, previous)), Some(focused)) = (previous, ui.memory(|memory| memory.focused())) {
        if drawn + 1 == pass {
            discarded_draft = move_breakpoint_focus(ui, node_id, &previous, &rows, &ids, focused, locked);
        }
    }
    ui.data_mut(|data| data.insert_temp(memory_id, (pass, rows)));
    (ids, discarded_draft)
}

fn move_breakpoint_focus(ui: &mut Ui, node_id: &str, previous: &[BreakpointRow], rows: &[BreakpointRow], ids: &[(Id, Id)], focused: Id, locked: bool) -> bool {
    let holds = |(width, columns): &(Id, Id)| *width == focused || *columns == focused;
    let previous_ids: Vec<(Id, Id)> = previous.iter().map(|row| breakpoint_row_ids(node_id, row.width)).collect();
    let Some(index) = previous_ids.iter().position(holds) else { return false };
    let new_index = ids.iter().position(holds);
    if !locked && new_index.is_some_and(|new_index| rows[new_index] == previous[index]) {
        return false;
    }
    let on_width = previous_ids[index].0 == focused;
    let shown = if on_width { previous[index].width } else { previous[index].columns };
    let draft = ui.data_mut(|data| data.remove_temp::<String>(focused.with("draft")))
        .filter(|text| *text != widgets::format_number(f64::from(shown), 0) && text.trim().parse::<u32>().ok() != Some(shown));
    let changed: Vec<usize> = (0..rows.len()).filter(|&row| previous.get(row) != Some(&rows[row])).collect();
    let same_breakpoint = !locked && new_index.is_none() && previous.len() == rows.len() && changed == [index] && previous[index].same_but_width(&rows[index]) && !previous_ids.contains(&ids[index]);
    if !same_breakpoint {
        ui.memory_mut(|memory| memory.surrender_focus(focused));
        return draft.is_some();
    }
    let target = if on_width { ids[index].0 } else { ids[index].1 };
    // Only text the user typed moves. The value the field showed does not, nor a text left under the new ids: the field shows the
    // new value, and leaving it writes nothing back over the change.
    ui.data_mut(|data| {
        data.remove::<String>(target.with("draft"));
        if let Some(text) = draft {
            data.insert_temp(target.with("draft"), text);
        }
    });
    ui.memory_mut(|memory| memory.request_focus(target));
    false
}

/// The layout with one breakpoint added after the widest one, with the grid's columns. Refused when the list is full.
fn breakpoint_added(spec: &designllm_contracts::LayoutSpec) -> Result<designllm_contracts::LayoutSpec, &'static str> {
    if spec.breakpoints.len() >= designllm_contracts::MAX_BREAKPOINTS {
        return Err("중단점은 16개까지 둘 수 있습니다");
    }
    let width = spec
        .breakpoints
        .iter()
        .map(|breakpoint| breakpoint.min_width_css_px)
        .max()
        .map_or(FIRST_BREAKPOINT_WIDTH, |widest| widest.saturating_add(BREAKPOINT_STEP));
    let mut next = spec.clone();
    next.breakpoints.push(designllm_contracts::Breakpoint {
        min_width_css_px: width,
        grid_columns: Some(spec.grid_columns.unwrap_or(2)),
        ..designllm_contracts::Breakpoint::default()
    });
    Ok(next)
}

/// Whether no node of the selection (`node` and the rest of `group`) is locked.
fn all_unlocked(node: &NodeProperties, group: &[NodeProperties]) -> bool {
    node.locked != Some(true) && group.iter().all(|other| other.locked != Some(true))
}

/// The id of a page size field (`axis` "w" or "h") of the page `page_id`.
fn page_field_id(page_id: &str, axis: &str) -> Id {
    Id::new(("page-field", page_id, axis))
}

/// The refusal of a breakpoint width past what the layout stores (u32 CSS px).
const BREAKPOINT_WIDTH_TOO_LARGE: &str = "중단점 폭은 4,294,967,295px 이하여야 합니다";

/// A typed breakpoint value as the whole number the layout stores: rounded, a negative value as 0, and `None` past `u32::MAX`
/// (a cast would store `u32::MAX` without a word).
fn breakpoint_whole(value: f64) -> Option<u32> {
    let whole = value.round().max(0.0);
    (whole <= f64::from(u32::MAX)).then_some(whole as u32)
}

/// The layout with the minimum width of breakpoint `index` set. The list stays in ascending order, since the engine needs
/// strictly increasing widths, so a width that another breakpoint already has is refused.
fn breakpoint_width_set(spec: &designllm_contracts::LayoutSpec, index: usize, width: u32) -> Result<designllm_contracts::LayoutSpec, &'static str> {
    if width == 0 {
        return Err("중단점 폭은 1px 이상이어야 합니다");
    }
    if spec.breakpoints.iter().enumerate().any(|(other, breakpoint)| other != index && breakpoint.min_width_css_px == width) {
        return Err("같은 폭의 중단점이 이미 있습니다");
    }
    let mut next = spec.clone();
    let breakpoint = next.breakpoints.get_mut(index).ok_or("중단점을 찾지 못했습니다")?;
    breakpoint.min_width_css_px = width;
    next.breakpoints.sort_by_key(|breakpoint| breakpoint.min_width_css_px);
    Ok(next)
}

/// The layout with the columns of breakpoint `index` set, from 1 to 24.
fn breakpoint_columns_set(spec: &designllm_contracts::LayoutSpec, index: usize, columns: u32) -> Result<designllm_contracts::LayoutSpec, &'static str> {
    if !(1..=designllm_contracts::MAX_GRID_COLUMNS).contains(&columns) {
        return Err("열 수는 1에서 24 사이여야 합니다");
    }
    let mut next = spec.clone();
    let breakpoint = next.breakpoints.get_mut(index).ok_or("중단점을 찾지 못했습니다")?;
    breakpoint.grid_columns = Some(columns);
    Ok(next)
}

/// The layout without breakpoint `index`. The other breakpoints stay as they are.
fn breakpoint_removed(spec: &designllm_contracts::LayoutSpec, index: usize) -> Result<designllm_contracts::LayoutSpec, &'static str> {
    if index >= spec.breakpoints.len() {
        return Err("중단점을 찾지 못했습니다");
    }
    let mut next = spec.clone();
    next.breakpoints.remove(index);
    Ok(next)
}

/// The fill row of a node painted with a gradient (tokens `component.fill-popover.panel-row-when-gradient`): a
/// field-like button with a two-colour swatch, `#FROM → #TO` and the angle at the right. Returns its rectangle and
/// its response (it opens the popover).
fn gradient_field(ui: &mut Ui, palette: &theme::Palette, width: f32, gradient: &designllm_contracts::Gradient) -> (Rect, egui::Response) {
    let (rect, response) = ui.allocate_exact_size(vec2(width.max(60.0), theme::FIELD_HEIGHT), Sense::click());
    let corner = CornerRadius::same(theme::RADIUS_CONTROL);
    ui.painter().rect(rect, corner, if response.hovered() { palette.surface_2 } else { palette.field }, Stroke::new(1.0, palette.line_strong), StrokeKind::Inside);
    let colour_of = |index: usize| gradient.stops.get(index).map(|stop| stop.color.clone()).unwrap_or_default();
    let (from, to) = (colour_of(0), colour_of(gradient.stops.len().saturating_sub(1)));
    let sample = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.center().y - 8.0), Vec2::splat(16.0));
    let (left, right) = sample.split_left_right_at_fraction(0.5);
    ui.painter().rect_filled(left, CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 }, widgets::parse_hex(&from).unwrap_or(palette.ink_3));
    ui.painter().rect_filled(right, CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 }, widgets::parse_hex(&to).unwrap_or(palette.ink_3));
    ui.painter().rect_stroke(sample, CornerRadius::same(theme::RADIUS_SMALL), Stroke::new(1.0, palette.swatch_ring), StrokeKind::Inside);
    let font = theme::display_font(false, 13.0);
    let mut x = rect.left() + 30.0;
    for (text, ink) in [(widgets::normalise_hex(&from), palette.ink), (String::from(" → "), palette.ink_3), (widgets::normalise_hex(&to), palette.ink)] {
        let width = text_width(ui, &text, font.clone());
        ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, text, font.clone(), ink);
        x += width;
    }
    let angle = format!("{}°", widgets::format_number(f64::from(gradient.angle_millideg) / 1000.0, 0));
    ui.painter().text(Pos2::new(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, angle, theme::display_font(false, 12.0), palette.ink_3);
    (rect, response.on_hover_cursor(egui::CursorIcon::PointingHand))
}

/// The fill popover (tokens `component.fill-popover`): 240 wide, its right edge on the fill field's right edge, 4
/// below it. `단색 · 그라디언트` on top; a gradient shows `색 1`, `색 2`, `각도` (label column 40). `단색` is hidden on a
/// gradient node while `props.gradient_remove` is not wired.
fn fill_popover(
    palette: &theme::Palette,
    swatch: &egui::Response,
    field_rect: Rect,
    id: Id,
    solid: Option<&str>,
    gradient: Option<&designllm_contracts::Gradient>,
    force_open: bool,
) -> Option<FillChoice> {
    let mut choice = None;
    let popup = egui::Popup::from_toggle_button_response(swatch);
    let popup = if force_open { popup.open_memory(Some(egui::SetOpenCommand::Bool(true))) } else { popup };
    popup
        .anchor(field_rect)
        .align(egui::RectAlign::BOTTOM_END)
        .gap(4.0)
        // tokens `fill-popover.box.width`: 240 outside, border included (egui's popup width is the outer width).
        .width(240.0)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let removable = commands_available("props.gradient_remove");
            let items = fill_items(gradient.is_some(), removable);
            let on: Vec<bool> = items.iter().map(|(icon, _)| (*icon == "fill-gradient") == gradient.is_some()).collect();
            if let Some(index) = seg(ui, palette, &items, &on) {
                match (items[index].0, gradient.is_some()) {
                    ("fill-solid", true) => choice = Some(FillChoice::RemoveGradient),
                    ("fill-gradient", false) => {
                        let from = solid.and_then(widgets::parse_hex).map(|_| widgets::normalise_hex(solid.unwrap_or_default())).unwrap_or_else(|| String::from("#FFFFFF"));
                        choice = Some(FillChoice::Gradient { from, to: String::from("#2F64E0"), angle: 90.0 });
                    }
                    _ => {}
                }
            }
            ui.add_space(8.0);
            match gradient {
                None => {
                    if let Some(new) = row_with(ui, palette, Some("색"), 40.0, |ui| {
                        let width = ui.available_width();
                        colour_field(ui, palette, id.with("solid"), width, solid)
                    }) {
                        choice = Some(FillChoice::Solid(new));
                    }
                }
                Some(gradient) => {
                    let colour_of = |index: usize| gradient.stops.get(index).map(|stop| stop.color.clone()).unwrap_or_default();
                    let (from, to) = (colour_of(0), colour_of(gradient.stops.len().saturating_sub(1)));
                    let angle = f64::from(gradient.angle_millideg) / 1000.0;
                    let new_from = row_with(ui, palette, Some("색 1"), 40.0, |ui| {
                        let width = ui.available_width();
                        colour_field(ui, palette, id.with("from"), width, Some(&from))
                    });
                    let new_to = row_with(ui, palette, Some("색 2"), 40.0, |ui| {
                        let width = ui.available_width();
                        colour_field(ui, palette, id.with("to"), width, Some(&to))
                    });
                    let new_angle = row_with(ui, palette, Some("각도"), 40.0, |ui| {
                        let width = ui.available_width();
                        field(ui, palette, id.with("angle"), width, "", Some(angle), "°", 0, true)
                    });
                    if new_from.is_some() || new_to.is_some() || new_angle.is_some() {
                        choice = Some(FillChoice::Gradient {
                            from: new_from.unwrap_or(from),
                            to: new_to.unwrap_or(to),
                            angle: new_angle.unwrap_or(angle),
                        });
                    }
                }
            }
        });
    choice
}

/// 모양 of a stroke (tokens `component.stroke-rows` ②): `실선 · 파선 · 점선`. Returns the dash picked when it changes.
pub(super) fn stroke_dash_row(ui: &mut Ui, palette: &theme::Palette, group: &[NodeProperties], node: &NodeProperties) -> Option<&'static str> {
    let current = agree(group, node, |other| other.style.stroke_dash).then_some(node.style.stroke_dash).flatten();
    let index = match current {
        Some(designllm_contracts::StrokeDash::Dashed) => Some(1),
        Some(designllm_contracts::StrokeDash::Dotted) => Some(2),
        _ if agree(group, node, |other| other.style.stroke_dash) => Some(0),
        _ => None,
    };
    let flags: Vec<bool> = (0..3).map(|candidate| Some(candidate) == index).collect();
    let chosen = row(ui, palette, Some("모양"), |ui| {
        seg(ui, palette, &[("border-solid", "실선"), ("border-dashed", "파선"), ("border-dotted", "점선")], &flags)
    })?;
    (Some(chosen) != index).then_some(["solid", "dashed", "dotted"][chosen])
}

/// A segmented control that fills the row (mockup `.seg`): a 32 px `surface-2` track, equal options with a 16 px
/// icon and a 12 px label; an option that is on is white with a `line` ring and `ink` text. `on` may mark several
/// (toggles such as 굵게 · 기울임). Returns the option clicked.
fn seg(ui: &mut Ui, palette: &theme::Palette, items: &[(&str, &str)], on: &[bool]) -> Option<usize> {
    // Each control's options take their ids from the control's own (unique) allocation, so two controls with the
    // same labels (세로 정렬 and 문단 정렬 both have `가운데`) never share an id.
    let (track, track_response) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
    let base = track_response.id;
    ui.painter().rect_filled(track, CornerRadius::same(theme::RADIUS_CHIP), palette.surface_2);
    let count = items.len().max(1) as f32;
    let width = (track.width() - 4.0 - 2.0 * (count - 1.0)) / count;
    let mut clicked = None;
    for (index, (icon, label)) in items.iter().enumerate() {
        let rect = Rect::from_min_size(Pos2::new(track.left() + 2.0 + index as f32 * (width + 2.0), track.top() + 2.0), vec2(width, 28.0));
        let response = ui.interact(rect, base.with(("seg", index)), Sense::click());
        let active = on.get(index).copied().unwrap_or(false);
        let ink = if active { palette.ink } else { palette.ink_2 };
        let corner = CornerRadius::same(theme::RADIUS_CONTROL);
        if active {
            ui.painter().rect_filled(rect, corner, palette.raised);
            ui.painter().rect_stroke(rect, corner, Stroke::new(1.0, palette.line), StrokeKind::Inside);
        } else if response.hovered() {
            ui.painter().rect_filled(rect, corner, palette.surface_3);
        }
        paint_icon_label(ui, rect, icon, label, ink);
        if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            clicked = Some(index);
        }
    }
    clicked
}

/// A 16 px icon and a 12 px label centred in `rect`, 4 px apart.
fn paint_icon_label(ui: &Ui, rect: Rect, icon: &str, label: &str, ink: Color32) {
    let font = theme::display_font(false, 12.0);
    let total = 16.0 + 4.0 + text_width(ui, label, font.clone());
    let left = rect.center().x - total / 2.0;
    icons::paint(ui, Rect::from_min_size(Pos2::new(left, rect.center().y - 8.0), Vec2::splat(16.0)), icon, ink);
    ui.painter().text(Pos2::new(left + 20.0, rect.center().y), Align2::LEFT_CENTER, label, font, ink);
}

/// A row of equal action buttons across the section (mockup `.acts`): 28 px, icon + label in `ink-2`, a `surface-2`
/// hover. Returns the one clicked.
fn acts(ui: &mut Ui, palette: &theme::Palette, items: &[(&str, &str)]) -> Option<usize> {
    let (strip, strip_response) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
    let base = strip_response.id;
    let count = items.len().max(1) as f32;
    // Mockup `.acts{gap:2px}` (lead decision, review item 11): 2 between the buttons of a row.
    const GAP: f32 = 2.0;
    let width = (strip.width() - GAP * (count - 1.0)) / count;
    let mut clicked = None;
    for (index, (icon, label)) in items.iter().enumerate() {
        let rect = Rect::from_min_size(Pos2::new(strip.left() + index as f32 * (width + GAP), strip.top()), vec2(width, 28.0));
        let response = ui.interact(rect, base.with(("acts", index)), Sense::click());
        if response.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
        }
        paint_icon_label(ui, rect, icon, label, palette.ink_2);
        if response.clicked() {
            clicked = Some(index);
        }
    }
    clicked
}

/// The 28 px row control that removes a value (mockup `.rm`, `minus`). Its hover text names the action (§7 L3).
fn remove_button(ui: &mut Ui, palette: &theme::Palette, hint: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
    }
    icons::paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(16.0)), "minus", palette.ink_2);
    response.on_hover_text(hint).clicked()
}

/// A field-like menu button filling the row (mockup `.fld` with an icon and `chev-down`) and its menu. `items` are
/// (value, icon, label); an empty icon means a text-only field and menu. Returns the value picked when it differs
/// from `current`.
fn menu_field(ui: &mut Ui, palette: &theme::Palette, items: &[(&'static str, &str, &str)], current: &str) -> Option<&'static str> {
    // No match: the selection disagrees (`여러 값`, no icon).
    let shown = items.iter().find(|(value, _, _)| *value == current).copied().unwrap_or(("", "", "여러 값"));
    let width = ui.available_width();
    let button = widgets::dropdown(ui, palette, shown.2, Some(shown.1).filter(|icon| !icon.is_empty()), width);
    let mut picked = None;
    egui::Popup::from_toggle_button_response(&button)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(width.max(180.0));
            for (value, icon, label) in items {
                if widgets::menu_item(ui, palette, Some(*icon).filter(|icon| !icon.is_empty()), label, "", *value == current).clicked() {
                    picked = Some(*value);
                    ui.close();
                }
            }
        });
    picked.filter(|value| *value != current)
}

/// The shadow field (mockup: swatch + `x, y · blur`): a field-like button that opens the offset and blur fields.
/// Returns (x, y, blur) in px when one of them is committed.
fn shadow_field(ui: &mut Ui, palette: &theme::Palette, id: Id, width: f32, shadow: &designllm_contracts::Shadow) -> Option<(f64, f64, f64)> {
    let (rect, response) = ui.allocate_exact_size(vec2(width.max(60.0), theme::FIELD_HEIGHT), Sense::click());
    let corner = CornerRadius::same(theme::RADIUS_CONTROL);
    let fill = if response.hovered() { palette.surface_2 } else { palette.surface };
    ui.painter().rect(rect, corner, fill, Stroke::new(1.0, palette.line_strong), StrokeKind::Inside);
    let colour = widgets::parse_hex(&shadow.color).unwrap_or(palette.ink_3);
    let sample = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.center().y - 8.0), Vec2::splat(16.0));
    ui.painter().rect_filled(sample, CornerRadius::same(theme::RADIUS_SMALL), colour);
    ui.painter().rect_stroke(sample, CornerRadius::same(theme::RADIUS_SMALL), Stroke::new(1.0, palette.swatch_ring), StrokeKind::Inside);
    let (x, y, blur) = (shadow.x as f64 / EMU_PER_PX, shadow.y as f64 / EMU_PER_PX, shadow.blur as f64 / EMU_PER_PX);
    let summary = format!("{}, {} · {}", widgets::format_number(x, 0), widgets::format_number(y, 0), widgets::format_number(blur, 0));
    ui.painter().text(Pos2::new(rect.left() + 30.0, rect.center().y), Align2::LEFT_CENTER, summary, theme::display_font(false, 13.0), palette.ink);
    let mut changed = None;
    egui::Popup::from_toggle_button_response(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(220.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            for (key, label, value) in [("x", "X", x), ("y", "Y", y), ("blur", "흐림", blur)] {
                if let Some(new) = row(ui, palette, Some(label), |ui| {
                    let room = ui.available_width();
                    field(ui, palette, id.with(key), room, "", Some(value), "px", 0, true)
                }) {
                    changed = Some(match key {
                        "x" => (new, y, blur),
                        "y" => (x, new, blur),
                        _ => (x, y, new),
                    });
                }
            }
        });
    changed
}
