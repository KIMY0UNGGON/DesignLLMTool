//! Contract v1.2 in the renderer. P1 draws corner radius, opacity, outer shadows (Shape, Image), two-stop
//! gradients (Shape, Path), stroke dash/cap/join, Path primitives, image fit, letter spacing and line
//! height (shaping); vertical alignment is placed by the engine's layout ([`crate::text_block_offset`]).
//! What is still not drawn is refused by name (UNSUPPORTED_OPERATION, one blocking diagnostic per node)
//! instead of being drawn without it (R09): gradients on Text, Line and Image (contract), shadows on
//! Text, Line and Path (no box), stroke dash/cap/join on
//! images (no stroke), gradients other than two opaque stops, more than one shadow.
use designllm_contracts::*;

/// Diagnostic code of a v1.2 refusal.
pub const V12_UNSUPPORTED: &str = "v1_2_unsupported";

/// (node id or None, field names) -> UNSUPPORTED_OPERATION naming every field, one diagnostic each.
fn refusal(what: &str, found: Vec<(Option<String>, Vec<String>)>) -> DltResult<()> {
    if found.is_empty() { return Ok(()); }
    let detail = found.iter().map(|(node, names)| match node { Some(n) => format!("{n}: {}", names.join(", ")), None => names.join(", ") }).collect::<Vec<_>>().join("; ");
    let mut e = DltError::unsupported(format!("{what} does not support these contract v1.2 fields yet: {detail}"));
    e.affected_ids = found.iter().filter_map(|(node, _)| node.clone()).collect();
    e.diagnostics = found.into_iter().map(|(node, names)| Diagnostic { code: V12_UNSUPPORTED.into(), message: format!("{what} does not support {} yet", names.join(", ")), node_id: node, blocking: true, params: Default::default() }).collect();
    Err(e)
}

/// v1.2 drawing fields that take the v1.2 code path of the encoders (v1.1 styles keep the v1.1 path).
pub(crate) fn has_effects(style: &Style) -> bool {
    style.corner_radius.is_some() || style.opacity_milli.is_some() || !style.shadows.is_empty() || style.gradient.is_some()
        || style.stroke_dash.is_some() || style.stroke_cap.is_some() || style.stroke_join.is_some() || style.stroke_start.is_some() || style.stroke_end.is_some()
}

/// A gradient the encoders draw: exactly two opaque `#RRGGBB` stops in 0..=1000.
fn gradient_ok(g: &Gradient) -> bool {
    g.stops.len() == 2 && g.stops.iter().all(|s| s.pos_milli <= 1000 && matches!(crate::paint::parse_color(&s.color), Ok(Some(c)) if c.a == 255))
}

/// Fields of a primitive style that are not drawn, by primitive kind.
fn unsupported_names(style: &Style, kind: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut add = |f: StyleField| names.push(format!("style.{}", f.name()));
    if kind != "line" {
        if style.stroke_start.is_some() { add(StyleField::StrokeStart); }
        if style.stroke_end.is_some() { add(StyleField::StrokeEnd); }
    }
    if matches!(kind, "image" | "line" | "path") {
        if style.font_weight.is_some() { add(StyleField::FontWeight); }
        if style.underline.is_some() { add(StyleField::Underline); }
        if style.strikethrough.is_some() { add(StyleField::Strikethrough); }
    }
    match kind {
        "text" | "line" => {
            if style.gradient.is_some() { add(StyleField::Gradient); }
            if !style.shadows.is_empty() { add(StyleField::Shadows); }
        }
        "path" => if !style.shadows.is_empty() { add(StyleField::Shadows); },
        // An image style draws opacity, corner radius and shadows only; it has no paint or stroke of its own.
        "image" => {
            if style.gradient.is_some() { add(StyleField::Gradient); }
            if style.stroke_dash.is_some() { add(StyleField::StrokeDash); }
            if style.stroke_cap.is_some() { add(StyleField::StrokeCap); }
            if style.stroke_join.is_some() { add(StyleField::StrokeJoin); }
        }
        _ => {}
    }
    if let Some(g) = &style.gradient { if !gradient_ok(g) && !names.iter().any(|n| n.ends_with("gradient")) { names.push("style.gradient (exactly two opaque stops)".into()); } }
    if style.shadows.len() > 1 { names.push("style.shadows (more than one)".into()); }
    names
}

/// Primitive styles carrying v1.2 values the encoders do not draw.
pub(crate) fn refuse_scene(scene: &RenderScene) -> DltResult<()> {
    let mut found = Vec::new();
    for page in &scene.pages {
        for primitive in &page.primitives {
            let style = match primitive {
                Primitive::Path { style, .. }
                | Primitive::Text { style, .. }
                | Primitive::Line { style, .. }
                | Primitive::Shape { style, .. } => Some(style),
                Primitive::Image { style, .. } => style.as_ref(),
            };
            if let Some(style) = style { validate_font_weight(style)?; }
            let (node, names) = match primitive {
                Primitive::Path { node_id, style, .. } => (node_id, unsupported_names(style, "path")),
                Primitive::Text { node_id, style, .. } => (node_id, unsupported_names(style, "text")),
                Primitive::Line { node_id, style, .. } => (node_id, unsupported_names(style, "line")),
                Primitive::Shape { node_id, style, .. } => (node_id, unsupported_names(style, "shape")),
                // Every fit is drawn; of the style, opacity, corner radius (clip) and shadows are.
                Primitive::Image { node_id, style, .. } => (node_id, style.as_ref().map(|s| unsupported_names(s, "image")).unwrap_or_default()),
            };
            if !names.is_empty() { found.push((Some(node.clone()), names)); }
        }
    }
    refusal("The renderer", found)
}

pub(crate) fn validate_font_weight(style: &Style) -> DltResult<()> {
    if style.font_weight.is_some_and(|weight| !(100..=900).contains(&weight)) {
        return Err(DltError::invalid("style.font_weight must be between 100 and 900"));
    }
    Ok(())
}
