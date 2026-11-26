use super::*;
use crate::theme;
use designllm_contracts::{FigureDocument, PanelBody, figure_rules::{figure_output_profile, figure_png_pixels, FigureOutputProfile, FIGURE_OUTPUT_PROFILES, FIGURE_OUTPUT_PROFILE_DISCLAIMER}};

/// The colours the renderer gives a plot's series: one definition, in contracts, for the renderer and this window.
pub(super) use designllm_contracts::figure_rules::series_colors;

/// Whether two colours are the same colour (decoded to RGBA): one definition, shared with the renderer's series colours.
use designllm_contracts::figure_rules::same_colour;

/// What an output profile changes: the operations of its one batch, and the legend items whose colour was kept because it
/// stood for different series in different plots (the window says so).
pub(super) struct ProfileChange {
    pub operations: Vec<Operation>,
    pub kept_legend_items: Vec<String>,
}

/// The message for legend items kept as they were (`ProfileChange::kept_legend_items`), naming them.
pub(super) fn kept_legend_notice(labels: &[String]) -> Option<String> {
    (!labels.is_empty()).then(|| format!("범례 항목 ‘{}’은 패널마다 다른 계열의 색이라 그대로 두었습니다", labels.join(", ")))
}

/// `profile_change`'s operations (the tests read them alone).
#[cfg(test)]
pub(super) fn profile_style_operations(figure: &FigureDocument, profile: &FigureOutputProfile) -> Vec<Operation> {
    profile_change(figure, profile).operations
}

/// The style changes an output profile makes, in one batch. Every plot panel takes the profile's first colour, so series
/// `j` has the same colour in every panel. A common legend's swatch that showed series `j` of the plots (its colour before
/// the change) takes series `j`'s new colour, in the same batch; a swatch that matches no series keeps its colour, and so
/// does one whose colour stood for series that now take different colours: which series it named cannot be told.
pub(super) fn profile_change(figure: &FigureDocument, profile: &FigureOutputProfile) -> ProfileChange {
    let mut operations = Vec::new();
    let mut kept_legend_items = Vec::new();
    let first = profile.palette[0];
    for panel in &figure.panels {
        let patch = StylePatch {
            font_size: Some(profile.font_size_emu), stroke_width: Some(profile.stroke_width_emu),
            fill: matches!(&panel.body, PanelBody::Plot(_)).then(|| first.to_owned()),
            ..StylePatch::default()
        };
        operations.push(Operation::SetStyle { node_ids: vec![panel.id.clone()], patch });
        if let PanelBody::Diagram(diagram) = &panel.body {
            let ids = diagram.nodes.iter().map(|node| node.id.clone()).chain(diagram.edges.iter().map(|edge| edge.id.clone())).collect::<Vec<_>>();
            if !ids.is_empty() {
                operations.push(Operation::SetStyle { node_ids: ids, patch: StylePatch {
                    font_size: Some(profile.font_size_emu), stroke_width: Some(profile.stroke_width_emu),
                    stroke: Some(profile.palette[0].to_owned()), ..StylePatch::default()
                }});
            }
        }
    }
    if let Some(old) = figure.common_legend.as_ref() {
        let plots: Vec<(&str, usize)> = figure.panels.iter().filter_map(|panel| match &panel.body {
            PanelBody::Plot(plot) => Some((panel.style.fill.as_str(), plot.y_columns.len())),
            _ => None,
        }).collect();
        let mut legend = old.clone();
        let mut changed = false;
        for item in &mut legend.items {
            // Every place the swatch's colour has among the plots' series: (series index, that plot's series count).
            let places: Vec<(usize, usize)> = plots.iter().flat_map(|(fill, count)| {
                series_colors(fill, *count).into_iter().enumerate()
                    .filter(|(_, colour)| same_colour(colour, &item.color))
                    .map(|(j, _)| (j, *count))
                    .collect::<Vec<_>>()
            }).collect();
            // The colour each of those series takes now. The swatch follows them when they all agree (the same series, or
            // series whose new colours coincide); otherwise which series it named cannot be told, and it is kept.
            let news: Vec<String> = places.iter().map(|(j, count)| series_colors(first, *count)[*j].clone()).collect();
            if news.iter().any(|colour| !same_colour(colour, &news[0])) {
                kept_legend_items.push(item.label.clone());
                continue;
            }
            if let Some(new) = news.first().cloned() {
                if !same_colour(&new, &item.color) {
                    item.color = new;
                    changed = true;
                }
            }
        }
        if changed {
            operations.push(Operation::SetCommonLegend { expected: Some(old.clone()), legend: Some(legend) });
        }
    }
    ProfileChange { operations, kept_legend_items }
}

impl DesktopApp {
    pub(super) fn apply_figure_output_profile(&mut self, args: &Value) -> Result<(), String> {
        let id = args.get("profile").and_then(Value::as_str).ok_or_else(|| "출력 프로필을 지정해 주세요".to_owned())?;
        let profile = figure_output_profile(id).ok_or_else(|| "알 수 없는 출력 프로필입니다".to_owned())?;
        let Some(Artifact { body: ArtifactBody::Figure(figure), .. }) = self.artifact.as_ref() else {
            return Err("열린 Figure에서만 출력 프로필을 적용할 수 있습니다".into());
        };
        let ProfileChange { operations, kept_legend_items } = profile_change(figure, profile);
        if operations.is_empty() { return Err("스타일을 적용할 패널이 없습니다".into()); }
        let artifact_id = self.active.as_ref().ok_or("작업물을 아직 읽는 중입니다")?.id.clone();
        if self.context.is_none() { return Err("작업물을 아직 읽는 중입니다".into()); }
        // A recoloured common legend is content (SetCommonLegend); the styles are design.
        let mut intents: std::collections::BTreeSet<Intent> = [Intent::Design].into_iter().collect();
        if operations.iter().any(|operation| matches!(operation, Operation::SetCommonLegend { .. })) {
            intents.insert(Intent::Content);
        }
        self.apply_operations_for(operations, intents, PendingAction::FigureProfile {
            artifact_id, profile_id: profile.id.to_owned(), dpi: profile.default_dpi, kept_legend_items,
        });
        Ok(())
    }

    pub(super) fn draw_figure_output_profile_section(&mut self, ui: &mut Ui, palette: &theme::Palette, figure: &FigureDocument, first: &mut bool) {
        super::properties::section_head(ui, palette, first, "출력 프로필", None);
        ui.label(crate::i18n::tr(&format!("크기: {:.1} × {:.1} mm", figure.size.width as f64 / EMU_PER_MM as f64, figure.size.height as f64 / EMU_PER_MM as f64)));
        ui.label(crate::i18n::tr(&format!("DPI: {}", self.view_ref().figure_export_dpi)));
        if let Ok((width, height)) = figure_png_pixels(figure.size, self.view_ref().figure_export_dpi) {
            ui.label(crate::i18n::tr(&format!("{width} × {height} px")));
        }
        ui.label(crate::i18n::tr(&FIGURE_OUTPUT_PROFILE_DISCLAIMER));
        let label = self.view_ref().figure_output_profile.as_deref().and_then(figure_output_profile).map_or("프로필 선택", |profile| profile.label);
        let mut chosen = None;
        egui::ComboBox::from_id_salt("figure-output-profile").selected_text(crate::i18n::tr(&label)).show_ui(ui, |ui| {
            for profile in FIGURE_OUTPUT_PROFILES {
                if ui.selectable_label(self.view_ref().figure_output_profile.as_deref() == Some(profile.id), crate::i18n::tr(&profile.label)).clicked() { chosen = Some(profile.id); }
            }
        });
        if let Some(id) = chosen { self.invoke_ui("figure.output_profile", json!({ "profile": id })); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_figure_presets_choose_size_and_dpi() {
        for tile in ["figure", "diagram"] {
            let mut harness = super::super::test_support::Harness::new();
            harness.app.home.tile = tile.into();
            harness.app.invoke("home.preset", json!({ "preset": "high_res_print" })).expect("the profile is accepted");
            harness.app.create_from_home_command(&json!({ "tile": tile })).expect("the figure is requested");
            let requests = harness.sent();
            let request = requests.iter().find(|(_, method, _)| method == "artifact.create").expect("the create request");
            let profile = figure_output_profile("high_res_print").unwrap();
            assert_eq!(request.2["size"], json!({ "width": profile.width_emu, "height": profile.height_emu }));
            let pending: Vec<(Option<&str>, u32)> = harness.app.pending_figure_create_profiles.values().map(|(id, dpi)| (id.as_deref(), *dpi)).collect();
            assert_eq!(pending, vec![(Some(profile.id), profile.default_dpi)]);
            harness.answer(&requests, "artifact.create", Ok(super::super::test_support::summary("created-figure", "figure", 0)));
            assert!(harness.app.pending_figure_create_profiles.is_empty());
            assert_eq!(harness.app.view_ref().figure_export_dpi, 600);
            assert_eq!(harness.app.view_ref().figure_output_profile.as_deref(), Some("high_res_print"));
        }
    }
}
