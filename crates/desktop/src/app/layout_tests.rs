use eframe::egui::{
    CentralPanel, Context, Frame, Margin, Pos2, RawInput, Rect, Sense, SidePanel, Vec2, ViewportId,
};

fn raw_input(scale: f32) -> RawInput {
    let mut input = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 560.0))),
        ..Default::default()
    };
    input
        .viewports
        .get_mut(&ViewportId::ROOT)
        .expect("root viewport exists")
        .native_pixels_per_point = Some(scale);
    input
}

#[test]
fn narrow_workbench_keeps_preview_area_at_common_dpi_scales() {
    for scale in [1.0, 1.5, 2.0] {
        let ctx = Context::default();
        let mut left = Rect::NOTHING;
        let mut right = Rect::NOTHING;
        let mut preview = Rect::NOTHING;
        let mut center = Rect::NOTHING;

        let output = ctx.run(raw_input(scale), |ctx| {
            let left_panel = SidePanel::left("test-work-list")
                .resizable(true)
                .default_width(230.0)
                .min_width(180.0)
                .max_width(330.0)
                .frame(Frame::new().inner_margin(Margin::same(14)))
                .show(ctx, |ui| {
                    ui.allocate_space(Vec2::new(150.0, 300.0));
                });
            left = left_panel.response.rect;
            let right_panel = SidePanel::right("test-properties")
                .resizable(true)
                .default_width(310.0)
                .min_width(230.0)
                .max_width(420.0)
                .frame(Frame::new().inner_margin(Margin::same(14)))
                .show(ctx, |ui| {
                    ui.allocate_space(Vec2::new(150.0, 300.0));
                });
            right = right_panel.response.rect;
            CentralPanel::default()
                .frame(Frame::new().inner_margin(16.0))
                .show(ctx, |ui| {
                    center = ui.max_rect();
                    preview = ui
                        .allocate_exact_size(Vec2::new(160.0, 120.0), Sense::hover())
                        .0;
                });
        });

        assert!((output.pixels_per_point - scale).abs() < 0.001);
        assert!(left.left() >= 0.0 && left.right() < right.left());
        assert!(right.right() <= 800.0);
        assert!(preview.width() >= 160.0 && preview.height() >= 120.0);
        assert!(preview.left() >= left.right() && preview.right() <= right.left());
        assert!(preview.top() >= 0.0 && preview.bottom() <= 560.0);
    }
}
