use super::*;
use crate::theme;
use designllm_contracts::{FigureDocument, PanelBody, figure_rules::{figure_output_profile, figure_png_pixels, FigureOutputProfile, FIGURE_OUTPUT_PROFILES, FIGURE_OUTPUT_PROFILE_DISCLAIMER}};

pub(super) fn profile_style_operations(figure: &FigureDocument, profile: &FigureOutputProfile) -> Vec<Operation> {
    let mut operations = Vec::new();
    for (index, panel) in figure.panels.iter().enumerate() {
        let patch = StylePatch {
            font_size: Some(profile.font_size_emu), stroke_width: Some(profile.stroke_width_emu),
            fill: matches!(&panel.body, PanelBody::Plot(_)).then(|| profile.palette[index % profile.palette.len()].to_owned()),
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
    operations
}

impl DesktopApp {
    pub(super) fn apply_figure_output_profile(&mut self, args: &Value) -> Result<(), String> {
        let id = args.get("profile").and_then(Value::as_str).ok_or_else(|| "출력 프로필을 지정해 주세요".to_owned())?;
        let profile = figure_output_profile(id).ok_or_else(|| "알 수 없는 출력 프로필입니다".to_owned())?;
        let Some(Artifact { body: ArtifactBody::Figure(figure), .. }) = self.artifact.as_ref() else {
            return Err("열린 Figure에서만 출력 프로필을 적용할 수 있습니다".into());
        };
        let operations = profile_style_operations(figure, profile);
        if operations.is_empty() { return Err("스타일을 적용할 패널이 없습니다".into()); }
        let artifact_id = self.active.as_ref().ok_or("작업물을 아직 읽는 중입니다")?.id.clone();
        if self.context.is_none() { return Err("작업물을 아직 읽는 중입니다".into()); }
        self.apply_operations_for(operations, [Intent::Design].into_iter().collect(), PendingAction::FigureProfile {
            artifact_id, profile_id: profile.id.to_owned(), dpi: profile.default_dpi,
        });
        Ok(())
    }

    pub(super) fn draw_figure_output_profile_section(&mut self, ui: &mut Ui, palette: &theme::Palette, figure: &FigureDocument, first: &mut bool) {
        super::properties::section_head(ui, palette, first, "출력 프로필", None);
        ui.label(format!("크기: {:.1} × {:.1} mm", figure.size.width as f64 / EMU_PER_MM as f64, figure.size.height as f64 / EMU_PER_MM as f64));
        ui.label(format!("DPI: {}", self.view_ref().figure_export_dpi));
        if let Ok((width, height)) = figure_png_pixels(figure.size, self.view_ref().figure_export_dpi) {
            ui.label(format!("{width} × {height} px"));
        }
        ui.label(FIGURE_OUTPUT_PROFILE_DISCLAIMER);
        let label = self.view_ref().figure_output_profile.as_deref().and_then(figure_output_profile).map_or("프로필 선택", |profile| profile.label);
        let mut chosen = None;
        egui::ComboBox::from_id_salt("figure-output-profile").selected_text(label).show_ui(ui, |ui| {
            for profile in FIGURE_OUTPUT_PROFILES {
                if ui.selectable_label(self.view_ref().figure_output_profile.as_deref() == Some(profile.id), profile.label).clicked() { chosen = Some(profile.id); }
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
            assert_eq!(harness.app.pending_figure_create_profile.as_ref().map(|(_, id, dpi)| (id.as_deref(), *dpi)), Some((Some(profile.id), profile.default_dpi)));
            harness.answer(&requests, "artifact.create", Ok(super::super::test_support::summary("created-figure", "figure", 0)));
            assert!(harness.app.pending_figure_create_profile.is_none());
            assert_eq!(harness.app.view_ref().figure_export_dpi, 600);
            assert_eq!(harness.app.view_ref().figure_output_profile.as_deref(), Some("high_res_print"));
        }
    }
}
