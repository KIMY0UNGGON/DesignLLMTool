//! Raw persisted Artifact shape checks. Core runs this before an engine's permissive JSON restore
//! so a newer nested field cannot disappear before the store feature detector sees it.
//! Dynamic maps that are part of the contract remain dynamic; their typed values are still checked.

use crate::errors::err;
use designllm_contracts::{DltError, DltResult, ErrorCode, SCHEMA_VERSION};
use serde_json::Value;

pub(crate) fn check(bytes: &[u8]) -> DltResult<()> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| err(ErrorCode::IntegrityError, format!("snapshot is not Artifact JSON: {error}")))?;
    artifact(&value, "")
}

fn child(parent: &str, name: &str) -> String {
    let escaped = name.replace('~', "~0").replace('/', "~1");
    if parent.is_empty() { format!("/{escaped}") } else { format!("{parent}/{escaped}") }
}

fn unknown(path: &str) -> DltError {
    err(ErrorCode::SchemaUnsupported, format!("persisted snapshot contains an unknown field at {path}"))
}

#[cfg(test)]
thread_local! {
    static SCHEMA_FIELD_EXPECTATIONS: std::cell::RefCell<Option<(
        std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
        std::collections::BTreeSet<String>,
    )>> = const { std::cell::RefCell::new(None) };
}

fn keys(value: &Value, path: &str, model: &str, allowed: &[&str]) -> DltResult<()> {
    #[cfg(not(test))]
    let _ = model;
    #[cfg(test)]
    SCHEMA_FIELD_EXPECTATIONS.with(|state| {
        if let Some((expected, used)) = state.borrow_mut().as_mut() {
            let actual: std::collections::BTreeSet<String> = allowed.iter().map(|key| (*key).to_owned()).collect();
            let fields = expected.get(model).unwrap_or_else(|| panic!("snapshot guard checked unregistered closed object {model} at {path}"));
            assert_eq!(&actual, fields, "guard key set differs from Artifact schema at model {model}");
            used.insert(model.to_owned());

            if let Some(mutated) = value.as_object().cloned() {
                const EXTRA: &str = "__snapshot_guard_extra_key_probe__";
                let mut mutated = Value::Object(mutated);
                mutated.as_object_mut().unwrap().insert(EXTRA.to_owned(), Value::Null);
                let error = check_keys(&mutated, path, allowed).expect_err("a closed object must refuse an extra key");
                assert_eq!(error.code, ErrorCode::SchemaUnsupported, "model {model} must reject extra keys");
                assert!(error.message.contains(EXTRA), "model {model} extra-key error: {error:?}");
            }
        }
    });
    check_keys(value, path, allowed)
}

fn check_keys(value: &Value, path: &str, allowed: &[&str]) -> DltResult<()> {
    let Some(object) = value.as_object() else { return Ok(()) };
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) { return Err(unknown(&child(path, key))); }
    }
    Ok(())
}

fn field(value: &Value, name: &str, path: &str, check: fn(&Value, &str) -> DltResult<()>) -> DltResult<()> {
    if let Some(value) = value.get(name) { check(value, &child(path, name))?; }
    Ok(())
}

fn array(value: &Value, path: &str, check: fn(&Value, &str) -> DltResult<()>) -> DltResult<()> {
    if let Some(items) = value.as_array() {
        for (index, item) in items.iter().enumerate() { check(item, &child(path, &index.to_string()))?; }
    }
    Ok(())
}

fn map_values(value: &Value, path: &str, check: fn(&Value, &str) -> DltResult<()>) -> DltResult<()> {
    if let Some(object) = value.as_object() {
        for (key, value) in object { check(value, &child(path, key))?; }
    }
    Ok(())
}

fn artifact(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Artifact", &["schema_version", "id", "kind", "revision", "title", "engine_id", "origin", "body", "native", "capabilities", "figure_links", "design_system", "publishes_design_system", "diagnostics"])?;
    if value.get("schema_version").and_then(Value::as_u64).is_some_and(|version| version > u64::from(SCHEMA_VERSION)) {
        return Err(err(ErrorCode::SchemaUnsupported, format!("persisted Artifact at {path} has a newer schema_version")));
    }
    field(value, "body", path, body)?;
    field(value, "native", path, native_source)?;
    field(value, "capabilities", path, capability_report)?;
    field(value, "figure_links", path, |v, p| array(v, p, figure_link))?;
    field(value, "design_system", path, design_system_ref)?;
    field(value, "diagnostics", path, |v, p| array(v, p, diagnostic))?;
    Ok(())
}

fn body(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "ArtifactBody", &["type", "document"])?;
    let Some(kind) = value.get("type").and_then(Value::as_str) else { return Ok(()) };
    field(value, "document", path, match kind {
        "canvas" => canvas_document,
        "slides" => slide_document,
        "web_code" => web_document,
        "flow" => flow_document,
        "figure" => figure_document,
        _ => return Err(err(ErrorCode::SchemaUnsupported, format!("persisted Artifact body has unknown type {kind:?} at {}", child(path, "type")))),
    })
}

fn design_system_ref(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "DesignSystemRef", &["artifact_id", "revision"])
}

fn native_source(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "NativeSource", &["asset", "mappings", "preserved", "preservation"])?;
    field(value, "asset", path, asset_ref)?;
    field(value, "preservation", path, preservation_manifest)?;
    // `mappings` and `preserved` intentionally contain engine-owned JSON values.
    Ok(())
}

fn preservation_manifest(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "PreservationManifest", &["parts", "opaque_regions"])?;
    field(value, "parts", path, |v, p| map_values(v, p, part_fingerprint))?;
    field(value, "opaque_regions", path, |v, p| array(v, p, opaque_region))?;
    Ok(())
}

fn part_fingerprint(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "PartFingerprint", &["sha256", "content_type", "immutable"])
}

fn opaque_region(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "OpaqueRegion", &["part", "anchor", "sha256"])
}

fn asset_ref(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "AssetRef", &["sha256", "mime", "bytes"])
}

fn capability_report(value: &Value, path: &str) -> DltResult<()> {
    map_values(value, path, feature_support)
}

fn feature_support(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "FeatureSupport", &["read", "render", "edit", "preserve", "write", "reason"])
}

fn diagnostic(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Diagnostic", &["code", "message", "node_id", "blocking", "params"])
}

fn figure_link(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "FigureLink", &["figure_id", "figure_revision", "target_node_id", "representation", "render_hash"])
}

fn canvas_document(value: &Value, path: &str) -> DltResult<()> {
    canvas_like_document(value, path, "CanvasDocument")
}

fn slide_document(value: &Value, path: &str) -> DltResult<()> {
    canvas_like_document(value, path, "SlideDocument")
}

fn canvas_like_document(value: &Value, path: &str, model: &str) -> DltResult<()> {
    let fields: &[&str] = if model == "CanvasDocument" {
        &["pages", "board_memos", "page_sections", "layout_guides", "styles", "tokens", "bindings", "token_mode"]
    } else {
        &["pages", "styles", "theme"]
    };
    keys(value, path, model, fields)?;
    field(value, "pages", path, |v, p| array(v, p, page))?;
    field(value, "styles", path, |v, p| map_values(v, p, style))?;
    if model == "SlideDocument" {
        // `colors` is a dynamic map of slot name to colour.
        field(value, "theme", path, |v, p| keys(v, p, "ThemePatch", &["colors", "major_latin", "minor_latin", "major_ea", "minor_ea"]))?;
    }
    if model == "CanvasDocument" {
        field(value, "board_memos", path, |v, p| array(v, p, canvas_board_memo))?;
        field(value, "page_sections", path, |v, p| array(v, p, canvas_page_section))?;
        field(value, "layout_guides", path, |v, p| map_values(v, p, canvas_page_guides))?;
        field(value, "tokens", path, |v, p| map_values(v, p, token_def))?;
        field(value, "bindings", path, |v, p| array(v, p, token_binding))?;
        field(value, "token_mode", path, token_mode)?;
    }
    Ok(())
}

fn canvas_board_memo(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "CanvasBoardMemo", &["kind", "id", "page_id", "bounds", "text", "color"])?;
    if let Some(kind) = value.get("kind").and_then(Value::as_str) {
        if !matches!(kind, "memo" | "title") {
            return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown CanvasBoardMemoKind {kind:?} at {}", child(path, "kind"))));
        }
    }
    field(value, "bounds", path, rect)
}

fn canvas_page_section(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "CanvasPageSection", &["id", "name", "start_page_id"])
}

fn canvas_page_guides(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "CanvasPageGuides", &["columns", "rows"])?;
    field(value, "columns", path, canvas_axis_guides)?;
    field(value, "rows", path, canvas_axis_guides)
}

fn canvas_axis_guides(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "CanvasAxisGuides", &["count", "spacing_emu", "leading_margin_emu", "trailing_margin_emu"])
}

fn token_def(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "TokenDef", &["name", "values"])?;
    field(value, "values", path, token_values)
}

fn token_values(value: &Value, path: &str) -> DltResult<()> {
    let Some(kind) = value.get("kind").and_then(Value::as_str) else {
        return keys(value, path, "TokenValues::missing_tag", &["kind"]);
    };
    if !matches!(kind, "color" | "font_family" | "font_size" | "spacing" | "radius") {
        return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown TokenValues {kind:?} at {}", child(path, "kind"))));
    }
    keys(value, path, "TokenValues", &["kind", "light", "dark"])
}

fn token_mode(value: &Value, path: &str) -> DltResult<()> {
    if value.is_null() || matches!(value.as_str(), Some("light" | "dark")) { return Ok(()); }
    if let Some(mode) = value.as_str() {
        return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown TokenMode {mode:?} at {path}")));
    }
    Ok(())
}

fn token_source(value: &Value, path: &str) -> DltResult<()> {
    if value.is_null() || matches!(value.as_str(), Some("local" | "shared")) { return Ok(()); }
    if let Some(source) = value.as_str() {
        return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown TokenSource {source:?} at {path}")));
    }
    Ok(())
}

fn padding_side(value: &Value, path: &str) -> DltResult<()> {
    if value.is_null() || matches!(value.as_str(), Some("top" | "right" | "bottom" | "left")) { return Ok(()); }
    if let Some(side) = value.as_str() {
        return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown PaddingSide {side:?} at {path}")));
    }
    Ok(())
}

fn token_ref(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "TokenRef", &["source", "token_id"])?;
    field(value, "source", path, token_source)
}

fn token_binding(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "TokenBinding", &["node_id", "target", "token"])?;
    field(value, "target", path, bind_target)?;
    field(value, "token", path, token_ref)
}

fn bind_target(value: &Value, path: &str) -> DltResult<()> {
    let Some(kind) = value.get("kind").and_then(Value::as_str) else {
        return keys(value, path, "BindTarget::missing_tag", &["kind"]);
    };
    let (model, fields): (&str, &[&str]) = match kind {
        "style_color" | "style_fill" | "style_stroke" | "style_font_family" | "style_font_size" | "style_corner_radius"
        | "layout_gap" | "layout_row_gap" | "layout_padding" => {
            let model = match kind {
                "style_color" => "BindTarget::style_color", "style_fill" => "BindTarget::style_fill", "style_stroke" => "BindTarget::style_stroke",
                "style_font_family" => "BindTarget::style_font_family", "style_font_size" => "BindTarget::style_font_size", "style_corner_radius" => "BindTarget::style_corner_radius",
                "layout_gap" => "BindTarget::layout_gap", "layout_row_gap" => "BindTarget::layout_row_gap", _ => "BindTarget::layout_padding",
            };
            (model, &["kind"])
        }
        "layout_padding_side" => ("BindTarget::layout_padding_side", &["kind", "side"]),
        "breakpoint_gap" => ("BindTarget::breakpoint_gap", &["kind", "min_width_css_px"]),
        "breakpoint_padding" => ("BindTarget::breakpoint_padding", &["kind", "min_width_css_px"]),
        _ => return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown BindTarget {kind:?} at {}", child(path, "kind")))),
    };
    keys(value, path, model, fields)?;
    if kind == "layout_padding_side" { field(value, "side", path, padding_side)?; }
    Ok(())
}

fn page(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Page", &["id", "size", "nodes", "notes", "name", "role"])?;
    field(value, "size", path, size)?;
    field(value, "nodes", path, |v, p| array(v, p, node))?;
    field(value, "role", path, page_role)
}

fn size(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "Size", &["width", "height"]) }
fn rect(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "Rect", &["x", "y", "width", "height"]) }
fn page_role(value: &Value, path: &str) -> DltResult<()> {
    if value.is_null() { return Ok(()) }
    match value.as_str() {
        Some("component") => Ok(()),
        Some(role) => Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown PageRole {role:?} at {path}"))),
        None => Err(err(ErrorCode::IntegrityError, format!("snapshot PageRole at {path} is not a string"))),
    }
}

const NODE_OVERRIDE_KEYS: &[&str] = &["text", "style", "hidden"];

fn node_override(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "NodeOverride", NODE_OVERRIDE_KEYS)?;
    field(value, "style", path, style_patch)
}

fn repeat_spec(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "RepeatSpec", &["table_id", "component_page_id", "bindings"])?;
    field(value, "bindings", path, |v, p| array(v, p, repeat_binding))
}

fn repeat_binding(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "RepeatBinding", &["source_node_id", "column"])
}

fn show_condition(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "ShowCondition", &["column", "op", "value"])?;
    field(value, "op", path, condition_op)
}

/// The four comparisons of this build. Another value is refused, so no build reads an unknown comparison.
fn condition_op(value: &Value, path: &str) -> DltResult<()> {
    match value.as_str() {
        Some("equals" | "not_equals" | "is_empty" | "is_not_empty") => Ok(()),
        Some(op) => Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown ConditionOp {op:?} at {path}"))),
        None => Err(err(ErrorCode::IntegrityError, format!("snapshot ConditionOp at {path} is not a string"))),
    }
}

fn instance_ref(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "InstanceRef", &["component_page_id", "overrides"])?;
    field(value, "overrides", path, |v, p| map_values(v, p, node_override))
}

fn canvas_table(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "CanvasTable", &["rows", "header_row_id", "column_widths", "cell_spans"])?;
    field(value, "rows", path, |v, p| array(v, p, canvas_table_row))?;
    field(value, "column_widths", path, |v, p| array(v, p, canvas_table_width))?;
    field(value, "cell_spans", path, |v, p| map_values(v, p, canvas_table_span))
}

fn canvas_table_span(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "CanvasTableSpan", &["row_span", "column_span"])?;
    for name in ["row_span", "column_span"] {
        if !value.get(name).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).is_some_and(|n| n > 0) {
            return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has an invalid Canvas table span at {path}.{name}")));
        }
    }
    Ok(())
}

fn canvas_table_width(value: &Value, path: &str) -> DltResult<()> {
    if value.as_u64().and_then(|weight| u32::try_from(weight).ok()).is_some_and(|weight| weight > 0) {
        Ok(())
    } else {
        Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has an invalid Canvas table width at {path}")))
    }
}

fn canvas_table_row(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "CanvasTableRow", &["id", "cell_ids"])
}

// NodeExt is flattened into Node JSON, so its keys are listed at this exact parent boundary.
const NODE_KEYS: &[&str] = &[
    "id", "kind", "bounds", "text", "style", "asset", "children", "layout", "interactions", "variants",
    "path", "icon", "image_fit", "image_crop_pixel_rect", "video_poster_asset", "alt", "name", "hidden", "locked", "list", "instance", "table",
    "repeat", "condition", "math",
];

fn node(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Node", NODE_KEYS)?;
    field(value, "bounds", path, rect)?;
    field(value, "style", path, style)?;
    field(value, "asset", path, asset_ref)?;
    field(value, "children", path, |v, p| array(v, p, node))?;
    field(value, "layout", path, layout_spec)?;
    field(value, "interactions", path, |v, p| array(v, p, interaction))?;
    field(value, "variants", path, |v, p| map_values(v, p, style_patch))?;
    field(value, "path", path, path_spec)?;
    field(value, "icon", path, icon_ref)?;
    field(value, "image_crop_pixel_rect", path, pixel_rect)?;
    field(value, "video_poster_asset", path, asset_ref)?;
    field(value, "instance", path, instance_ref)?;
    field(value, "table", path, canvas_table)?;
    field(value, "repeat", path, repeat_spec)?;
    field(value, "condition", path, show_condition)?;
    field(value, "math", path, math_syntax)
}

/// The one math syntax of this build. Another value is refused, so no build reads a newer syntax as its own.
fn math_syntax(value: &Value, path: &str) -> DltResult<()> {
    if value.is_null() { return Ok(()) }
    match value.as_str() {
        Some("tex_subset") => Ok(()),
        Some(syntax) => Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown MathSyntax {syntax:?} at {path}"))),
        None => Err(err(ErrorCode::IntegrityError, format!("snapshot MathSyntax at {path} is not a string"))),
    }
}

fn style(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Style", &[
        "font_family", "font_size", "color", "fill", "stroke", "stroke_width", "bold", "italic", "align",
        "corner_radius", "opacity_milli", "shadows", "gradient", "line_height_milli", "letter_spacing", "vertical_align",
        "font_weight", "stroke_dash", "stroke_cap", "stroke_join", "underline", "strikethrough", "stroke_start", "stroke_end",
    ])?;
    field(value, "shadows", path, |v, p| array(v, p, shadow))?;
    field(value, "gradient", path, gradient)
}

fn style_patch(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "StylePatch", &[
        "font_family", "font_size", "color", "fill", "stroke", "stroke_width", "bold", "italic", "align",
        "corner_radius", "opacity_milli", "shadows", "gradient", "line_height_milli", "letter_spacing", "vertical_align",
        "font_weight", "stroke_dash", "stroke_cap", "stroke_join", "underline", "strikethrough", "stroke_start", "stroke_end", "clear",
    ])?;
    field(value, "shadows", path, |v, p| array(v, p, shadow))?;
    field(value, "gradient", path, gradient)
}

fn shadow(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "Shadow", &["x", "y", "blur", "spread", "color"]) }
fn gradient(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Gradient", &["angle_millideg", "stops"])?;
    field(value, "stops", path, |v, p| array(v, p, gradient_stop))
}
fn gradient_stop(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "GradientStop", &["pos_milli", "color"]) }
fn path_spec(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "PathSpec", &["view_box", "fill_rule", "commands"])?;
    field(value, "view_box", path, rect)?;
    field(value, "commands", path, |v, p| array(v, p, path_command))
}
fn path_command(value: &Value, path: &str) -> DltResult<()> {
    let Some(kind) = value.get("type").and_then(Value::as_str) else { return keys(value, path, "PathCommand::missing_tag", &["type"]) };
    let fields: &[&str] = match kind {
        "move_to" | "line_to" => &["type", "x", "y"],
        "cubic_to" => &["type", "x1", "y1", "x2", "y2", "x", "y"],
        "close" => &["type"],
        _ => return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown PathCommand {kind:?} at {}", child(path, "type")))),
    };
    let model = match kind {
        "move_to" => "PathCommand::move_to",
        "line_to" => "PathCommand::line_to",
        "cubic_to" => "PathCommand::cubic_to",
        "close" => "PathCommand::close",
        _ => "PathCommand::unknown",
    };
    keys(value, path, model, fields)
}
fn icon_ref(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "IconRef", &["set", "name", "version"]) }

fn layout_spec(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "LayoutSpec", &[
        "mode", "width_mode", "height_mode", "gap", "padding", "align", "min_width", "max_width", "min_height", "max_height",
        "breakpoints", "justify", "padding_sides", "grid_columns", "row_gap", "align_self", "grid_span",
    ])?;
    field(value, "breakpoints", path, |v, p| array(v, p, breakpoint))
}
fn breakpoint(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Breakpoint", &["min_width_css_px", "mode", "gap", "padding", "align", "grid_columns", "justify"])
}

fn interaction(value: &Value, path: &str) -> DltResult<()> {
    let Some(action) = value.get("action").and_then(Value::as_str) else { return keys(value, path, "Interaction::missing_tag", &["action"]) };
    let fields: &[&str] = match action {
        "navigate_page" => &["action", "page_id"],
        "toggle_visibility" => &["action", "node_id"],
        "set_variant" => &["action", "node_id", "variant"],
        "open_url" => &["action", "url"],
        "back" => &["action"],
        _ => return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown Interaction {action:?} at {}", child(path, "action")))),
    };
    let model = match action {
        "navigate_page" => "Interaction::navigate_page",
        "toggle_visibility" => "Interaction::toggle_visibility",
        "set_variant" => "Interaction::set_variant",
        "open_url" => "Interaction::open_url",
        "back" => "Interaction::back",
        _ => "Interaction::unknown",
    };
    keys(value, path, model, fields)
}

fn web_document(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "WebDocument", &["entry", "files"])?;
    field(value, "files", path, |v, p| map_values(v, p, web_file))
}
fn web_file(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "WebFile", &["content", "sha256"]) }

fn flow_document(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "FlowDocument", &["sections", "styles"])?;
    field(value, "sections", path, |v, p| array(v, p, section))?;
    field(value, "styles", path, |v, p| map_values(v, p, style))
}
fn section(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Section", &["id", "setup", "blocks"])?;
    field(value, "setup", path, page_setup)?;
    field(value, "blocks", path, |v, p| array(v, p, flow_block))
}
fn page_setup(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "PageSetup", &["size", "margin_left", "margin_top", "margin_right", "margin_bottom"])?;
    field(value, "size", path, size)
}
fn flow_block(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "FlowBlock", &["type", "value"])?;
    let Some(kind) = value.get("type").and_then(Value::as_str) else { return Ok(()) };
    field(value, "value", path, match kind {
        "paragraph" => paragraph,
        "table" => table,
        "image" => node,
        "opaque" => |v, p| keys(v, p, "FlowBlock::opaque.value", &["id", "label"]),
        _ => return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown FlowBlock {kind:?} at {}", child(path, "type")))),
    })
}
fn paragraph(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Paragraph", &["id", "runs", "style_ref", "format"])?;
    field(value, "runs", path, |v, p| array(v, p, text_run))?;
    field(value, "format", path, paragraph_format)
}
fn text_run(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "TextRun", &["id", "text", "style", "protected"])?;
    field(value, "style", path, style)
}
fn paragraph_format(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "ParagraphFormat", &["align", "space_before", "space_after", "line_spacing", "indent", "page_break_before", "line_spacing_percent"])
}
fn table(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Table", &["id", "column_widths", "rows", "style"])?;
    field(value, "style", path, style)?;
    field(value, "rows", path, |v, p| array(v, p, |row, row_path| array(row, row_path, table_cell)))
}
fn table_cell(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "TableCell", &["id", "blocks", "column_span", "row_span"])?;
    field(value, "blocks", path, |v, p| array(v, p, flow_block))
}

fn figure_document(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "FigureDocument", &["size", "panels", "caption", "datasets", "evidence", "source_scopes", "common_legend", "visual_review"])?;
    field(value, "size", path, size)?;
    field(value, "panels", path, |v, p| array(v, p, figure_panel))?;
    field(value, "datasets", path, |v, p| array(v, p, dataset))?;
    field(value, "evidence", path, |v, p| array(v, p, evidence))?;
    field(value, "source_scopes", path, |v, p| array(v, p, source_coverage))?;
    field(value, "common_legend", path, common_legend)?;
    field(value, "visual_review", path, visual_review)
}
fn common_legend(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "CommonLegend", &["items", "placement", "title"])?;
    field(value, "items", path, |v, p| array(v, p, common_legend_item))?;
    field(value, "placement", path, common_legend_placement)
}
fn common_legend_item(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "CommonLegendItem", &["label", "color"])
}
/// The three placements of this build. Another value is refused, so no build drops a legend's placement silently.
fn common_legend_placement(value: &Value, path: &str) -> DltResult<()> {
    if matches!(value.as_str(), Some("bottom" | "top" | "right")) {
        return Ok(());
    }
    Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown CommonLegendPlacement {value} at {}", path)))
}
fn visual_review(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "VisualReview", &["state", "note", "approved_digest"])?;
    field(value, "state", path, visual_review_state)
}
/// The three states of this build. Another value is refused, so no build reads an unknown review state as a review.
fn visual_review_state(value: &Value, path: &str) -> DltResult<()> {
    if matches!(value.as_str(), Some("pending" | "approved" | "needs_revision")) {
        return Ok(());
    }
    Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown VisualReviewState {value} at {}", path)))
}
fn figure_panel(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "FigurePanel", &["id", "bounds", "label", "body", "style"])?;
    field(value, "bounds", path, rect)?;
    field(value, "style", path, style)?;
    field(value, "body", path, panel_body)
}
fn panel_body(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "PanelBody", &["type", "value"])?;
    let Some(kind) = value.get("type").and_then(Value::as_str) else { return Ok(()) };
    field(value, "value", path, match kind {
        "diagram" => diagram,
        "plot" => plot_spec,
        "image" => figure_image,
        _ => return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown PanelBody {kind:?} at {}", child(path, "type")))),
    })
}
fn diagram(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Diagram", &["nodes", "edges", "evidence_ids", "node_origins"])?;
    field(value, "nodes", path, |v, p| array(v, p, node))?;
    field(value, "edges", path, |v, p| array(v, p, edge))?;
    // evidence_ids and node_origins are intentionally ID-keyed maps with primitive values.
    Ok(())
}
fn edge(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Edge", &["id", "from", "to", "label", "evidence_ids", "origin", "style"])?;
    field(value, "style", path, style)
}
fn plot_spec(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "PlotSpec", &["kind", "dataset_ref", "x_column", "y_columns", "x_axis", "y_axis", "error", "display"])?;
    field(value, "dataset_ref", path, dataset_ref)?;
    field(value, "x_axis", path, axis_spec)?;
    field(value, "y_axis", path, axis_spec)?;
    field(value, "error", path, error_spec)?;
    field(value, "display", path, plot_display)
}
/// Null is an absent optional field, as everywhere else in the model. Any other value must be the one mode this build
/// knows. A mode it does not know is refused, so no build drops it silently.
fn plot_display(value: &Value, path: &str) -> DltResult<()> {
    if value.is_null() || value.as_str() == Some("error_key") {
        return Ok(());
    }
    Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown PlotDisplayMode {value} at {path}")))
}
fn dataset_ref(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "DatasetRef", &["dataset_id", "version_hash"]) }
fn axis_spec(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "AxisSpec", &["label", "unit", "scale", "minimum", "maximum", "tick_step"])
}
fn error_spec(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "ErrorSpec", &["kind", "values", "sample_count_column", "confidence_level"])?;
    field(value, "values", path, error_values)
}
fn error_values(value: &Value, path: &str) -> DltResult<()> {
    let Some(kind) = value.get("type").and_then(Value::as_str) else { return keys(value, path, "ErrorValues::missing_tag", &["type"]) };
    let fields = match kind {
        "symmetric" => &["type", "column"][..],
        "asymmetric" => &["type", "minus_column", "plus_column"][..],
        "bounds" => &["type", "lower_column", "upper_column"][..],
        _ => return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown ErrorValues {kind:?} at {}", child(path, "type")))),
    };
    let model = match kind {
        "symmetric" => "ErrorValues::symmetric",
        "asymmetric" => "ErrorValues::asymmetric",
        "bounds" => "ErrorValues::bounds",
        _ => "ErrorValues::unknown",
    };
    keys(value, path, model, fields)
}
fn figure_image(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "FigureImage", &["asset", "origin", "source_ref", "pixel_width", "pixel_height", "crop_pixel_rect", "rotation_millidegrees", "calibration"])?;
    field(value, "asset", path, asset_ref)?;
    field(value, "crop_pixel_rect", path, pixel_rect)?;
    field(value, "calibration", path, calibration)
}
fn pixel_rect(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "PixelRect", &["x", "y", "width", "height"]) }
fn calibration(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "Calibration", &["length", "unit", "pixel_span", "evidence_id"]) }
fn dataset(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Dataset", &["id", "version_hash", "original_asset_hash", "origin", "columns", "rows", "derived_from", "transforms"])?;
    field(value, "columns", path, |v, p| array(v, p, data_column))?;
    field(value, "rows", path, |v, p| array(v, p, data_row))?;
    field(value, "derived_from", path, dataset_ref)?;
    field(value, "transforms", path, |v, p| array(v, p, data_transform))
}
fn data_column(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "DataColumn", &["id", "name", "data_type", "unit"]) }
fn data_row(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "DataRow", &["id", "cells"])?;
    field(value, "cells", path, |v, p| map_values(v, p, data_cell))
}
fn data_cell(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "DataCell", &["original", "parsed_decimal", "state"]) }
fn data_transform(value: &Value, path: &str) -> DltResult<()> {
    let Some(kind) = value.get("type").and_then(Value::as_str) else { return keys(value, path, "DataTransform::missing_tag", &["type"]) };
    let fields: &[&str] = match kind {
        "select_rows" => &["type", "row_ids"],
        "sort" => &["type", "column_id", "descending"],
        "unit_scale" => &["type", "column_id", "numerator", "denominator", "output_unit"],
        "edit_cell" => &["type", "row_id", "column_id", "previous", "replacement"],
        "add_column" => &["type", "name", "data_type", "unit", "fill"],
        "add_rows" => &["type", "after_row_id", "rows"],
        "remove_row" => &["type", "row_id", "previous"],
        "remove_column" => &["type", "column_id", "previous_name"],
        _ => return Err(err(ErrorCode::SchemaUnsupported, format!("snapshot has unknown DataTransform {kind:?} at {}", child(path, "type")))),
    };
    let model = match kind {
        "select_rows" => "DataTransform::select_rows",
        "sort" => "DataTransform::sort",
        "unit_scale" => "DataTransform::unit_scale",
        "edit_cell" => "DataTransform::edit_cell",
        "add_column" => "DataTransform::add_column",
        "add_rows" => "DataTransform::add_rows",
        "remove_row" => "DataTransform::remove_row",
        "remove_column" => "DataTransform::remove_column",
        _ => "DataTransform::unknown",
    };
    keys(value, path, model, fields)
}
fn evidence(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "Evidence", &["id", "source_hash", "origin", "physical_page_index", "region", "pdf_geometry", "text", "review_state"])?;
    field(value, "region", path, rect)?;
    field(value, "pdf_geometry", path, pdf_page_geometry)
}
fn pdf_page_geometry(value: &Value, path: &str) -> DltResult<()> {
    keys(value, path, "PdfPageGeometry", &["media_box", "crop_box", "user_unit", "rotation_degrees", "printed_page_label"])?;
    field(value, "media_box", path, pdf_box)?;
    field(value, "crop_box", path, pdf_box)
}
fn pdf_box(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "PdfBox", &["x_min", "y_min", "x_max", "y_max"]) }
fn source_coverage(value: &Value, path: &str) -> DltResult<()> { keys(value, path, "SourceCoverage", &["source_hash", "physical_page_indices"]) }

#[cfg(test)]
mod tests {
    use super::check;
    use super::SCHEMA_FIELD_EXPECTATIONS;
    use designllm_contracts::Artifact;
    use serde_json::{json, Value};

    #[test]
    fn canvas_table_span_snapshot_checks_fields_and_positive_extents() {
        let table = json!({"rows": [], "cell_spans": {"cell": {"row_span": 1, "column_span": 2}}});
        assert!(super::canvas_table(&table, "table").is_ok());
        let mut invalid = table.clone();
        invalid["cell_spans"]["cell"]["column_span"] = json!(0);
        assert!(super::canvas_table(&invalid, "table").is_err());
        invalid["cell_spans"]["cell"]["column_span"] = json!(2);
        invalid["cell_spans"]["cell"]["unknown"] = json!(true);
        assert!(super::canvas_table(&invalid, "table").is_err());
    }

    fn schema_object_fields(schema: &Value, root: &Value) -> Option<std::collections::BTreeSet<String>> {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let pointer = reference.strip_prefix('#').unwrap_or(reference);
            return schema_object_fields(root.pointer(pointer)?, root);
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            return Some(properties.keys().cloned().collect());
        }
        if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
            let mut fields = std::collections::BTreeSet::new();
            for part in parts { fields.extend(schema_object_fields(part, root)?); }
            return Some(fields);
        }
        None
    }

    fn schema_property(schema: &Value, root: &Value, name: &str) -> Option<Value> {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let pointer = reference.strip_prefix('#').unwrap_or(reference);
            return schema_property(root.pointer(pointer)?, root, name);
        }
        if let Some(property) = schema.get("properties").and_then(|properties| properties.get(name)) {
            return Some(property.clone());
        }
        if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
            for part in parts { if let Some(property) = schema_property(part, root, name) { return Some(property); } }
        }
        None
    }

    fn tagged_value(branch: &Value, root: &Value, tag: &str) -> Option<String> {
        let schema = schema_property(branch, root, tag)?;
        schema.get("const").and_then(Value::as_str).or_else(|| {
            schema.get("enum").and_then(Value::as_array)?.iter().find_map(Value::as_str)
        }).map(str::to_owned)
    }

    fn schema_guard_field_sets(root: &Value) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
        let definitions = root.get("definitions").and_then(Value::as_object).expect("Artifact schema definitions");
        let mut expected = std::collections::BTreeMap::new();
        expected.insert("Artifact".to_owned(), schema_object_fields(root, root).expect("root Artifact properties"));
        for (name, definition) in definitions {
            if let Some(fields) = schema_object_fields(definition, root) {
                expected.insert(name.clone(), fields);
            }
        }

        let shared_wrapper_enums = ["ArtifactBody", "FlowBlock", "PanelBody", "TokenValues"];
        let tagged_enums = [
            ("ArtifactBody", "type"), ("PathCommand", "type"), ("Interaction", "action"),
            ("FlowBlock", "type"), ("PanelBody", "type"), ("ErrorValues", "type"), ("DataTransform", "type"),
            ("TokenValues", "kind"), ("BindTarget", "kind"),
        ];
        for (name, tag) in tagged_enums {
            let definition = definitions.get(name).unwrap_or_else(|| panic!("Artifact schema is missing tagged enum {name}"));
            let branches = definition.get("oneOf").or_else(|| definition.get("anyOf"))
                .and_then(Value::as_array).unwrap_or_else(|| panic!("tagged enum {name} has no schema variants"));
            let variant_fields: Vec<(String, std::collections::BTreeSet<String>, Value)> = branches.iter().map(|branch| {
                let variant = tagged_value(branch, root, tag).unwrap_or_else(|| panic!("cannot read {name} discriminator {tag} from {branch}"));
                let fields = schema_object_fields(branch, root).unwrap_or_else(|| panic!("tagged variant {name}::{variant} is not an object"));
                (variant, fields, branch.clone())
            }).collect();

            if shared_wrapper_enums.contains(&name) {
                let first = variant_fields.first().expect("tagged enum has a variant").1.clone();
                assert!(variant_fields.iter().all(|(_, fields, _)| fields == &first), "{name} variants have different wrapper properties; update the validator contract");
                expected.insert(name.to_owned(), first);
            } else {
                for (variant, fields, _) in &variant_fields {
                    expected.insert(format!("{name}::{variant}"), fields.clone());
                }
            }

            if name == "FlowBlock" {
                let (_, _, opaque) = variant_fields.iter().find(|(variant, _, _)| variant == "opaque").expect("opaque FlowBlock variant");
                let value_schema = schema_property(opaque, root, "value").expect("Opaque variant value schema");
                expected.insert("FlowBlock::opaque.value".to_owned(), schema_object_fields(&value_schema, root).expect("Opaque value is an object"));
            }
        }
        expected
    }

    fn schema_samples(schema: &Value, root: &Value, visiting: &mut Vec<String>) -> Vec<Value> {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let pointer = reference.strip_prefix('#').unwrap_or(reference);
            if visiting.iter().any(|seen| seen == pointer) {
                // Recursive Artifact fields are arrays, so an empty array is the finite,
                // typed representation. Do not insert null and then count guard-only success.
                return Vec::new();
            }
            let target = root.pointer(pointer).unwrap_or_else(|| panic!("unresolved Artifact schema ref {reference}"));
            visiting.push(pointer.to_owned());
            let values = schema_samples(target, root, visiting);
            visiting.pop();
            return values;
        }

        for union in ["oneOf", "anyOf"] {
            if let Some(alternatives) = schema.get(union).and_then(Value::as_array) {
                let values: Vec<Value> = alternatives.iter()
                    .flat_map(|alternative| schema_samples(alternative, root, visiting))
                    .collect();
                if !values.is_empty() { return values; }
            }
        }

        if let Some(alternatives) = schema.get("allOf").and_then(Value::as_array) {
            // schemars wraps a described `$ref` in a single-element `allOf`; its samples may be non-objects (enum strings).
            if let [only] = alternatives.as_slice() {
                let values = schema_samples(only, root, visiting);
                if !values.is_empty() { return values; }
            }
            let mut values = vec![Value::Object(serde_json::Map::new())];
            for part in alternatives {
                let part_values = schema_samples(part, root, visiting);
                let mut combined = Vec::new();
                for current in &values {
                    for addition in &part_values {
                        if let (Some(current), Some(addition)) = (current.as_object(), addition.as_object()) {
                            let mut merged = current.clone();
                            for (key, value) in addition {
                                if let Some(existing) = merged.get(key) {
                                    if existing != value { continue; }
                                }
                                merged.insert(key.clone(), value.clone());
                            }
                            combined.push(Value::Object(merged));
                        }
                    }
                }
                values = combined;
            }
            if !values.is_empty() { return values; }
        }

        if let Some(constant) = schema.get("const") { return vec![constant.clone()]; }
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return values.clone();
        }

        let schema_type = schema.get("type").and_then(Value::as_str).or_else(|| {
            schema.get("type").and_then(Value::as_array)
                .and_then(|types| types.iter().filter_map(Value::as_str).find(|kind| *kind != "null"))
        });
        if schema_type == Some("array") {
            let mut items = schema.get("items").map(|items| schema_samples(items, root, visiting)).unwrap_or_default();
            let minimum = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(first) = items.first().cloned() {
                while items.len() < minimum { items.push(first.clone()); }
            }
            return vec![Value::Array(items)];
        }

        let properties = schema.get("properties").and_then(Value::as_object);
        let is_object = schema_type == Some("object") || properties.is_some();
        if is_object {
            let mut base = serde_json::Map::new();
            let mut property_alternatives = Vec::new();
            if let Some(properties) = properties {
                for (name, property_schema) in properties {
                    let values = if name == "url" {
                        vec![json!("https://example.com")]
                    } else {
                        schema_samples(property_schema, root, visiting)
                    };
                    if let Some(first) = values.first() {
                        base.insert(name.clone(), first.clone());
                        property_alternatives.push((name.clone(), values));
                    }
                }
            }

            // Map schemas have additionalProperties but no named properties. Give the map a
            // dynamic key so the guard exercises its value visitor as well as the parent shape.
            if properties.is_none() {
                if let Some(value_schema) = schema.get("additionalProperties") {
                    if value_schema.is_object() {
                        let values = schema_samples(value_schema, root, visiting);
                        let key = schema.get("propertyNames")
                            .and_then(|names| names.get("enum"))
                            .and_then(Value::as_array)
                            .and_then(|keys| keys.iter().find_map(Value::as_str))
                            // `text` is a valid `Feature` map key and is also an ordinary key
                            // for the other dynamic maps reachable from Artifact.
                            .unwrap_or("text");
                        return values.into_iter().map(|value| {
                            let mut map = serde_json::Map::new();
                            map.insert(key.to_owned(), value);
                            Value::Object(map)
                        }).collect();
                    }
                }
            }

            let mut samples = vec![Value::Object(base.clone())];
            for (name, alternatives) in property_alternatives {
                for alternative in alternatives.into_iter().skip(1) {
                    let mut sample = base.clone();
                    sample.insert(name.clone(), alternative);
                    samples.push(Value::Object(sample));
                }
            }
            return samples;
        }

        match schema_type {
            Some("string") => vec![Value::String(String::new())],
            Some("integer") => vec![json!(schema.get("minimum").and_then(Value::as_f64).unwrap_or(0.0).ceil() as i64)],
            Some("number") => vec![schema.get("minimum").cloned().unwrap_or_else(|| json!(0))],
            Some("boolean") => vec![Value::Bool(false)],
            Some("null") => vec![Value::Null],
            _ => vec![Value::Null],
        }
    }

    fn canvas_snapshot() -> Value {
        json!({
            "schema_version": 1,
            "id": "artifact",
            "kind": "canvas",
            "revision": 0,
            "title": "snapshot",
            "engine_id": "designllm-canvas",
            "origin": "created",
            "body": {
                "type": "canvas",
                "document": {
                    "pages": [{
                        "id": "page",
                        "size": { "width": 100, "height": 100 },
                        "nodes": [{
                            "id": "line",
                            "kind": "line",
                            "bounds": { "x": 0, "y": 0, "width": 100, "height": 0 },
                            "text": "",
                            "style": {
                                "font_family": "A",
                                "font_size": 100,
                                "color": "#000000",
                                "fill": "transparent",
                                "stroke": "#000000",
                                "stroke_width": 1,
                                "bold": false,
                                "italic": false,
                                "align": "left",
                                "font_weight": null,
                                "stroke_start": null,
                                "stroke_end": null
                            },
                            "asset": null,
                            "children": [],
                            "layout": null,
                            "interactions": [],
                            "variants": { "future_variant_name": { "bold": true, "clear": [] } },
                            "list": null,
                            "alt": null
                        }],
                        "notes": "",
        "name": null,
        "role": null
                    }],
                    "styles": { "a user-named style": { "bold": true } }
                }
            },
            "native": {
                "asset": { "sha256": "hash", "mime": "application/octet-stream", "bytes": 1 },
                "mappings": { "engine-map": { "arbitrary": [1, null, false] } },
                "preserved": { "engine-key": { "future": "opaque" } },
                "preservation": { "parts": {}, "opaque_regions": [] }
            },
            "capabilities": { "text": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" } },
            "figure_links": [],
            "diagnostics": [{ "code": "d", "message": "m", "node_id": null, "blocking": false, "params": { "measured": 1 } }]
        })
    }

    #[test]
    fn known_null_defaults_and_intended_dynamic_maps_pass() {
        let bytes = serde_json::to_vec(&canvas_snapshot()).unwrap();
        check(&bytes).expect("known optional nulls and dynamic maps are not false positives");
    }

    #[test]
    fn canvas_page_guide_maps_accept_the_exact_fields_and_refuse_unknown_ones() {
        let mut snapshot = canvas_snapshot();
        snapshot["body"]["document"]["layout_guides"] = json!({
            "page": {
                "columns": { "count": 3, "spacing_emu": 12, "leading_margin_emu": 20, "trailing_margin_emu": 20 },
                "rows": null
            }
        });
        check(&serde_json::to_vec(&snapshot).unwrap()).expect("page guide fields are explicitly recognized");
        serde_json::from_value::<Artifact>(snapshot.clone()).expect("the typed guide map restores");

        snapshot["body"]["document"]["layout_guides"]["page"]["columns"]["future"] = Value::Bool(false);
        let error = check(&serde_json::to_vec(&snapshot).unwrap()).unwrap_err();
        assert!(error.message.contains("/layout_guides/page/columns/future"), "unknown guide fields fail closed: {error:?}");
    }

    #[test]
    fn unknown_top_level_and_nested_fields_fail_even_when_null_or_false() {
        let mut top = canvas_snapshot();
        top["design_system_ref"] = Value::Null;
        let error = check(&serde_json::to_vec(&top).unwrap()).unwrap_err();
        assert!(error.message.contains("/design_system_ref"), "{error:?}");

        let mut nested = canvas_snapshot();
        nested["body"]["document"]["pages"][0]["nodes"][0]["future_style_flag"] = Value::Bool(false);
        let error = check(&serde_json::to_vec(&nested).unwrap()).unwrap_err();
        assert!(error.message.contains("future_style_flag"), "{error:?}");

        let mut style = canvas_snapshot();
        style["body"]["document"]["pages"][0]["nodes"][0]["style"]["future_style"] = Value::Null;
        let error = check(&serde_json::to_vec(&style).unwrap()).unwrap_err();
        assert!(error.message.contains("future_style"), "{error:?}");

        for unknown_value in [Value::Null, Value::Bool(false)] {
            let mut child = complete_node();
            child["future_child_flag"] = unknown_value;
            let mut nested_child = canvas_snapshot();
            nested_child["body"]["document"]["pages"][0]["nodes"][0]["children"] = json!([child]);
            assert!(serde_json::from_value::<Artifact>(nested_child.clone()).is_ok(), "public serde intentionally ignores unknown fields");
            let error = check(&serde_json::to_vec(&nested_child).unwrap()).unwrap_err();
            assert!(error.message.contains("/children/0/future_child_flag"), "{error:?}");
        }
    }

    #[test]
    fn canvas_snapshot_guard_accepts_positive_widths_and_refuses_invalid_weights() {
        let mut valid = canvas_snapshot();
        valid["body"]["document"]["pages"][0]["nodes"][0]["table"] = json!({
            "rows": [{ "id": "row", "cell_ids": ["c1", "c2"] }],
            "header_row_id": null,
            "column_widths": [1, 2]
        });
        check(&serde_json::to_vec(&valid).unwrap()).expect("custom table weights survive snapshot validation");
        serde_json::from_value::<Artifact>(valid.clone()).expect("the weighted table snapshot is typed");

        for invalid_weight in [json!(0), json!(4_294_967_296u64), json!(-1), json!("1")] {
            let mut invalid = valid.clone();
            invalid["body"]["document"]["pages"][0]["nodes"][0]["table"]["column_widths"] = json!([1, invalid_weight]);
            let error = check(&serde_json::to_vec(&invalid).unwrap()).unwrap_err();
            assert_eq!(error.code, designllm_contracts::ErrorCode::SchemaUnsupported);
            assert!(error.message.contains("Canvas table width"), "{error:?}");
        }
    }

    #[test]
    fn canvas_token_snapshot_fields_are_closed_and_tag_checked() {
        let mut snapshot = canvas_snapshot();
        snapshot["body"]["document"]["tokens"] = json!({
            "brand.ink": { "name": "Brand ink", "values": { "kind": "color", "light": "#112233", "dark": "#445566" } },
            "pages": { "name": "Pages token id", "values": { "kind": "color", "light": "#000000", "dark": "#FFFFFF" } }
        });
        snapshot["body"]["document"]["token_mode"] = json!("dark");
        snapshot["body"]["document"]["bindings"] = json!([{
            "node_id": "page-node", "target": { "kind": "style_color" },
            "token": { "source": "local", "token_id": "brand.ink" }
        }]);
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        check(&bytes).expect("known local token fields pass raw preflight");
        serde_json::from_slice::<Artifact>(&bytes).expect("the known token snapshot also restores as a typed Artifact");

        let mut extra = snapshot.clone();
        extra["body"]["document"]["tokens"]["brand.ink"]["values"]["future"] = Value::Bool(false);
        let error = check(&serde_json::to_vec(&extra).unwrap()).unwrap_err();
        assert!(error.message.contains("/tokens/brand.ink/values/future"), "{error:?}");

        let mut unknown_values_kind = snapshot.clone();
        unknown_values_kind["body"]["document"]["tokens"]["brand.ink"]["values"]["kind"] = json!("future");
        let error = check(&serde_json::to_vec(&unknown_values_kind).unwrap()).unwrap_err();
        assert!(error.message.contains("unknown TokenValues"), "{error:?}");

        let mut unknown_target = snapshot.clone();
        unknown_target["body"]["document"]["bindings"][0]["target"]["kind"] = json!("future_target");
        let error = check(&serde_json::to_vec(&unknown_target).unwrap()).unwrap_err();
        assert!(error.message.contains("unknown BindTarget"), "{error:?}");

        let mut unknown_source = snapshot.clone();
        unknown_source["body"]["document"]["bindings"][0]["token"]["source"] = json!("future_source");
        let error = check(&serde_json::to_vec(&unknown_source).unwrap()).unwrap_err();
        assert!(error.message.contains("unknown TokenSource"), "{error:?}");

        let mut unknown_side = snapshot.clone();
        unknown_side["body"]["document"]["bindings"][0]["target"] = json!({ "kind": "layout_padding_side", "side": "center" });
        let error = check(&serde_json::to_vec(&unknown_side).unwrap()).unwrap_err();
        assert!(error.message.contains("unknown PaddingSide"), "{error:?}");

        let mut unknown_mode = snapshot;
        unknown_mode["body"]["document"]["token_mode"] = json!("sepia");
        let error = check(&serde_json::to_vec(&unknown_mode).unwrap()).unwrap_err();
        assert!(error.message.contains("unknown TokenMode"), "{error:?}");

        let slide_with_canvas_tokens = artifact_with_body("slides", json!({ "pages": [], "styles": {}, "tokens": {} }));
        let error = check(&serde_json::to_vec(&slide_with_canvas_tokens).unwrap()).unwrap_err();
        assert!(error.message.contains("/tokens"), "Canvas-only token fields must not be accepted in SlideDocument: {error:?}");
    }

    #[test]
    fn slide_theme_is_checked_and_a_newer_theme_field_is_refused() {
        let theme = json!({ "colors": { "accent1": "#112233" }, "major_latin": "Inter", "minor_ea": "맑은 고딕" });
        let themed = artifact_with_body("slides", json!({ "pages": [], "styles": {}, "theme": theme }));
        check(&serde_json::to_vec(&themed).unwrap()).expect("SlideDocument theme fields");
        serde_json::from_value::<Artifact>(themed.clone()).expect("the themed fixture is a typed Artifact");

        let mut newer = themed;
        newer["body"]["document"]["theme"]["fmt_scheme"] = json!({});
        let error = check(&serde_json::to_vec(&newer).unwrap()).unwrap_err();
        assert_eq!(error.code, designllm_contracts::ErrorCode::SchemaUnsupported);
        assert!(error.message.contains("/theme/fmt_scheme"), "{error:?}");

        let canvas_with_theme = artifact_with_body("canvas", json!({ "pages": [], "styles": {}, "theme": {} }));
        let error = check(&serde_json::to_vec(&canvas_with_theme).unwrap()).unwrap_err();
        assert!(error.message.contains("/theme"), "the slide theme field is not a Canvas field: {error:?}");
    }

    #[test]
    fn component_snapshot_accepts_parent_relative_instance_fields_and_rejects_origin() {
        let mut source = complete_node();
        source["id"] = json!("source-node");
        source["kind"] = json!("group");
        source["path"] = Value::Null;
        source["icon"] = Value::Null;
        let mut clone = complete_node();
        clone["id"] = json!("instance::source-node");
        clone["kind"] = json!("group");
        clone["path"] = Value::Null;
        clone["icon"] = Value::Null;
        let mut instance = complete_node();
        instance["id"] = json!("instance");
        instance["kind"] = json!("group");
        instance["path"] = Value::Null;
        instance["icon"] = Value::Null;
        instance["children"] = json!([clone]);
        instance["instance"] = json!({
            "component_page_id": "source",
            "overrides": { "source-node": { "text": "local", "style": complete_patch() } }
        });
        let mut snapshot = canvas_snapshot();
        snapshot["body"]["document"]["pages"] = json!([
            { "id": "source", "size": { "width": 100, "height": 100 }, "nodes": [source], "notes": "", "role": "component" },
            { "id": "target", "size": { "width": 100, "height": 100 }, "nodes": [instance], "notes": "", "role": null },
            { "id": "table-page", "size": { "width": 100, "height": 100 }, "nodes": [], "notes": "", "role": null }
        ]);
        let mut table = complete_node();
        table["id"] = json!("table");
        table["kind"] = json!("group");
        table["layout"]["mode"] = json!("grid");
        table["layout"]["grid_columns"] = json!(1);
        let mut cell = complete_node();
        cell["id"] = json!("cell");
        cell["kind"] = json!("rectangle");
        table["children"] = json!([cell]);
        table["table"] = json!({ "rows": [{ "id": "row", "cell_ids": ["cell"] }], "header_row_id": null });
        snapshot["body"]["document"]["pages"][2]["nodes"] = json!([table]);
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        check(&bytes).expect("the exact component fields pass raw snapshot validation");
        serde_json::from_slice::<Artifact>(&bytes).expect("the component snapshot restores as a typed artifact");

        let mut unknown_table = snapshot.clone();
        unknown_table["body"]["document"]["pages"][2]["nodes"][0]["table"]["future_column_schema"] = json!(false);
        let error = check(&serde_json::to_vec(&unknown_table).unwrap()).unwrap_err();
        assert!(error.message.contains("/table/future_column_schema"), "unknown Canvas table fields are refused before typed restore: {error:?}");

        snapshot["body"]["document"]["pages"][1]["nodes"][0]["instance"]["origin"] = json!({ "x": 1, "y": 2 });
        let error = check(&serde_json::to_vec(&snapshot).unwrap()).unwrap_err();
        assert!(error.message.contains("/instance/origin"), "{error:?}");
    }

    /// 45 2a: a repeat group, a show condition and an instance override that hides a node. This build checks their
    /// fields exactly; a build that predates them (the key lists without the new keys) refuses them before typed restore.
    #[test]
    fn repeat_condition_and_hidden_override_are_closed_keys_an_older_build_refuses() {
        let mut group = complete_node();
        group["id"] = json!("list");
        group["kind"] = json!("group");
        group["path"] = Value::Null;
        group["icon"] = Value::Null;
        group["repeat"] = json!({ "table_id": "table", "component_page_id": "card", "bindings": [{ "source_node_id": "title", "column": 1 }] });
        group["condition"] = json!({ "column": 0, "op": "not_equals", "value": "" });
        group["instance"] = json!({ "component_page_id": "card", "overrides": { "badge": { "hidden": true } } });
        let mut snapshot = canvas_snapshot();
        snapshot["body"]["document"]["pages"][0]["nodes"] = json!([group]);
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        check(&bytes).expect("this build knows the repeat, condition and hidden fields");
        serde_json::from_slice::<Artifact>(&bytes).expect("the snapshot restores as a typed artifact");

        let node = &snapshot["body"]["document"]["pages"][0]["nodes"][0];
        let older_node: Vec<&str> = super::NODE_KEYS.iter().copied().filter(|key| !matches!(*key, "repeat" | "condition")).collect();
        let older_override: Vec<&str> = super::NODE_OVERRIDE_KEYS.iter().copied().filter(|key| *key != "hidden").collect();
        let error = super::check_keys(node, "/node", &older_node).expect_err("an older build does not know repeat");
        assert_eq!(error.code, designllm_contracts::ErrorCode::SchemaUnsupported);
        assert!(error.message.contains("/node/condition") || error.message.contains("/node/repeat"), "{error:?}");
        let mut without_repeat = node.clone();
        without_repeat.as_object_mut().unwrap().remove("repeat");
        let error = super::check_keys(&without_repeat, "/node", &older_node).expect_err("an older build does not know condition");
        assert!(error.message.contains("/node/condition"), "{error:?}");
        let error = super::check_keys(&node["instance"]["overrides"]["badge"], "/override", &older_override).expect_err("an older build does not know hidden");
        assert_eq!(error.code, designllm_contracts::ErrorCode::SchemaUnsupported);
        assert!(error.message.contains("/override/hidden"), "{error:?}");

        for (pointer, key) in [
            ("/body/document/pages/0/nodes/0/repeat", "future_rows"),
            ("/body/document/pages/0/nodes/0/repeat/bindings/0", "format"),
            ("/body/document/pages/0/nodes/0/condition", "case"),
            ("/body/document/pages/0/nodes/0/instance/overrides/badge", "locked"),
        ] {
            let mut unknown = snapshot.clone();
            unknown.pointer_mut(pointer).unwrap().as_object_mut().unwrap().insert(key.into(), json!(true));
            let error = check(&serde_json::to_vec(&unknown).unwrap()).expect_err("an unknown nested key is refused");
            assert_eq!(error.code, designllm_contracts::ErrorCode::SchemaUnsupported);
            assert!(error.message.contains(&format!("{}/{key}", pointer.trim_start_matches("/body/document"))), "{pointer}: {error:?}");
        }
        let mut future_op = snapshot.clone();
        future_op["body"]["document"]["pages"][0]["nodes"][0]["condition"]["op"] = json!("greater_than");
        let error = check(&serde_json::to_vec(&future_op).unwrap()).expect_err("an unknown comparison is refused");
        assert_eq!(error.code, designllm_contracts::ErrorCode::SchemaUnsupported);
        assert!(error.message.contains("ConditionOp"), "{error:?}");
    }

    /// 18 2a: a figure Diagram math node. This build knows `math` and its one syntax; a build that predates it (the
    /// node key list without `math`) refuses the figure before typed restore instead of dropping the key.
    #[test]
    fn diagram_math_node_is_a_closed_key_an_older_build_refuses() {
        let mut equation = complete_node();
        equation["id"] = json!("eq");
        equation["kind"] = json!("text");
        equation["text"] = json!("x^2");
        equation["path"] = Value::Null;
        equation["icon"] = Value::Null;
        equation["math"] = json!("tex_subset");
        let diagram = json!({ "nodes": [equation], "edges": [], "evidence_ids": {} });
        let figure = json!({ "size": { "width": 100, "height": 100 }, "panels": [
            { "id": "d", "bounds": { "x": 0, "y": 0, "width": 1, "height": 1 }, "label": "", "body": { "type": "diagram", "value": diagram } }
        ], "caption": "", "datasets": [], "evidence": [] });
        let snapshot = artifact_with_body("figure", figure);
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        check(&bytes).expect("this build knows the math field");
        let typed: Artifact = serde_json::from_slice(&bytes).expect("the snapshot restores as a typed artifact");
        let designllm_contracts::ArtifactBody::Figure(document) = &typed.body else { panic!("a figure body") };
        let designllm_contracts::PanelBody::Diagram(restored) = &document.panels[0].body else { panic!("a diagram panel") };
        assert_eq!(restored.nodes[0].ext.math, Some(designllm_contracts::MathSyntax::TexSubset));

        let pointer = "/body/document/panels/0/body/value/nodes/0";
        let node = snapshot.pointer(pointer).unwrap();
        let older_node: Vec<&str> = super::NODE_KEYS.iter().copied().filter(|key| *key != "math").collect();
        let error = super::check_keys(node, "/node", &older_node).expect_err("an older build does not know math");
        assert_eq!(error.code, designllm_contracts::ErrorCode::SchemaUnsupported);
        assert!(error.message.contains("/node/math"), "{error:?}");
        let mut without_math = node.clone();
        without_math.as_object_mut().unwrap().remove("math");
        super::check_keys(&without_math, "/node", &older_node).expect("the same node without math is known to the older build");

        let mut future_syntax = snapshot.clone();
        future_syntax.pointer_mut(pointer).unwrap()["math"] = json!("tex_full");
        let error = check(&serde_json::to_vec(&future_syntax).unwrap()).expect_err("an unknown math syntax is refused");
        assert_eq!(error.code, designllm_contracts::ErrorCode::SchemaUnsupported);
        assert!(error.message.contains("MathSyntax") && error.message.contains(&format!("{}/math", pointer.trim_start_matches("/body/document"))), "{error:?}");
        let mut not_a_string = snapshot.clone();
        not_a_string.pointer_mut(pointer).unwrap()["math"] = json!(1);
        assert_eq!(check(&serde_json::to_vec(&not_a_string).unwrap()).unwrap_err().code, designllm_contracts::ErrorCode::IntegrityError);
        let mut null_math = snapshot.clone();
        null_math.pointer_mut(pointer).unwrap()["math"] = Value::Null;
        check(&serde_json::to_vec(&null_math).unwrap()).expect("null is an absent math syntax");
    }

    fn artifact_with_body(kind: &str, document: Value) -> Value {
        json!({
            "schema_version": 1, "id": "a", "kind": kind, "revision": 0,
            "title": "t", "engine_id": "engine", "origin": "created",
            "body": { "type": kind, "document": document },
            "native": null, "capabilities": {}, "figure_links": [], "diagnostics": []
        })
    }

    fn complete_style() -> Value {
        json!({
            "font_family": "F", "font_size": 1, "color": "#000000", "fill": "transparent", "stroke": "#000000",
            "stroke_width": 1, "bold": false, "italic": false, "align": "left", "corner_radius": null,
            "opacity_milli": null, "shadows": [{ "x": 0, "y": 0, "blur": 0, "spread": 0, "color": "#000000" }],
            "gradient": { "angle_millideg": 0, "stops": [{ "pos_milli": 0, "color": "#000000" }] },
            "line_height_milli": null, "letter_spacing": null, "vertical_align": null, "font_weight": null,
            "stroke_dash": null, "stroke_cap": null, "stroke_join": null, "underline": null, "strikethrough": null,
            "stroke_start": null, "stroke_end": null
        })
    }

    fn complete_patch() -> Value {
        json!({
            "font_family": null, "font_size": null, "color": null, "fill": null, "stroke": null, "stroke_width": null,
            "bold": null, "italic": null, "align": null, "corner_radius": null, "opacity_milli": null,
            "shadows": null, "gradient": null, "line_height_milli": null, "letter_spacing": null,
            "vertical_align": null, "font_weight": null, "stroke_dash": null, "stroke_cap": null,
            "stroke_join": null, "underline": null, "strikethrough": null, "stroke_start": null, "stroke_end": null,
            "clear": []
        })
    }

    fn complete_node() -> Value {
        json!({
            "id": "n", "kind": "line", "bounds": { "x": 0, "y": 0, "width": 1, "height": 0 }, "text": "",
            "style": complete_style(), "asset": null, "children": [],
            "layout": {
                "mode": "absolute", "width_mode": "fixed", "height_mode": "fixed", "gap": 0, "padding": 0, "align": "start",
                "min_width": null, "max_width": null, "min_height": null, "max_height": null,
                "breakpoints": [{ "min_width_css_px": 0, "mode": null, "gap": null, "padding": null, "align": null, "grid_columns": null, "justify": null }],
                "justify": null, "padding_sides": null, "grid_columns": null, "row_gap": null, "align_self": null, "grid_span": null
            },
            "interactions": [
                { "action": "navigate_page", "page_id": "p" },
                { "action": "toggle_visibility", "node_id": "n" },
                { "action": "set_variant", "node_id": "n", "variant": "hover" },
                { "action": "open_url", "url": "https://example.com" },
                { "action": "back" }
            ],
            "variants": { "dynamic-variant-name": complete_patch() },
            "path": { "view_box": { "x": 0, "y": 0, "width": 1, "height": 1 }, "fill_rule": "nonzero", "commands": [
                { "type": "move_to", "x": 0, "y": 0 }, { "type": "line_to", "x": 1, "y": 1 },
                { "type": "cubic_to", "x1": 0, "y1": 0, "x2": 1, "y2": 1, "x": 1, "y": 1 }, { "type": "close" }
            ] },
            "icon": { "set": "s", "name": "n", "version": "1" }, "image_fit": null, "image_crop_pixel_rect": null, "video_poster_asset": null, "alt": null,
            "name": null, "hidden": null, "locked": null, "list": null, "instance": null
        })
    }

    #[test]
    fn representative_artifact_body_fixtures_cover_typed_field_boundaries() {
        let node = complete_node();
        let page = json!({ "id": "p", "size": { "width": 100, "height": 100 }, "nodes": [node.clone()], "notes": "", "name": null });
        let canvas = json!({ "pages": [page.clone()], "styles": { "dynamic-style-name": complete_style() } });
        check(&serde_json::to_vec(&artifact_with_body("canvas", canvas.clone())).unwrap()).expect("Canvas model fields");
        check(&serde_json::to_vec(&artifact_with_body("slides", canvas)).unwrap()).expect("Slides model fields");

        let web = json!({ "entry": "index.html", "files": { "dynamic/path.html": { "content": "<p/>", "sha256": "hash" } } });
        check(&serde_json::to_vec(&artifact_with_body("web_code", web)).unwrap()).expect("WebCode dynamic paths");

        let paragraph = json!({
            "id": "para", "runs": [{ "id": "run", "text": "x", "style": complete_style(), "protected": false }],
            "style_ref": null, "format": { "align": "left", "space_before": 0, "space_after": 0, "line_spacing": 0, "indent": 0 }
        });
        let table = json!({ "id": "t", "column_widths": [1], "rows": [[{ "id": "cell", "blocks": [], "column_span": 1, "row_span": 1 }]], "style": complete_style() });
        let flow = json!({
            "sections": [{ "id": "s", "setup": { "size": { "width": 100, "height": 100 }, "margin_left": 0, "margin_top": 0, "margin_right": 0, "margin_bottom": 0 },
                "blocks": [
                    { "type": "paragraph", "value": paragraph }, { "type": "table", "value": table },
                    { "type": "image", "value": node.clone() }, { "type": "opaque", "value": { "id": "opaque", "label": "future-safe" } }
                ]
            }], "styles": { "dynamic-style-name": complete_style() }
        });
        check(&serde_json::to_vec(&artifact_with_body("flow", flow)).unwrap()).expect("Flow model fields");

        let dataset = json!({
            "id": "d", "version_hash": "v", "original_asset_hash": "h", "origin": "csv",
            "columns": [{ "id": "c", "name": "column", "data_type": "number", "unit": null }],
            "rows": [{ "id": "r", "cells": { "dynamic-column-id": { "original": "1", "parsed_decimal": "1", "state": "valid" } } }],
            "derived_from": null,
            "transforms": [
                { "type": "select_rows", "row_ids": ["r"] }, { "type": "sort", "column_id": "c", "descending": false },
                { "type": "unit_scale", "column_id": "c", "numerator": "1", "denominator": "1", "output_unit": "x" },
                { "type": "edit_cell", "row_id": "r", "column_id": "c", "previous": "1", "replacement": "2" },
                { "type": "add_column", "name": "new", "data_type": "text", "unit": null, "fill": null },
                { "type": "add_rows", "after_row_id": null, "rows": [{ "c": "3" }] },
                { "type": "remove_row", "row_id": "r", "previous": { "c": "1" } },
                { "type": "remove_column", "column_id": "c", "previous_name": "column" }
            ]
        });
        let evidence = json!({ "id": "e", "source_hash": "h", "origin": "pdf", "physical_page_index": null, "region": null, "pdf_geometry": {
            "media_box": { "x_min": 0, "y_min": 0, "x_max": 1, "y_max": 1 },
            "crop_box": { "x_min": 0, "y_min": 0, "x_max": 1, "y_max": 1 },
            "user_unit": "1", "rotation_degrees": 0, "printed_page_label": null
        }, "text": "evidence", "review_state": "reviewed" });
        let diagram = json!({ "nodes": [node.clone()], "edges": [{ "id": "edge", "from": "a", "to": "b", "label": "", "evidence_ids": [], "origin": "user", "style": complete_style() }], "evidence_ids": { "node": ["e"] }, "node_origins": { "node": "user" } });
        let plot = json!({ "kind": "bar", "dataset_ref": { "dataset_id": "d", "version_hash": "v" }, "x_column": "x", "y_columns": ["y"],
            "x_axis": { "label": "x", "unit": null, "scale": "linear", "minimum": null, "maximum": null, "tick_step": null },
            "y_axis": { "label": "y", "unit": null, "scale": "linear", "minimum": null, "maximum": null, "tick_step": null },
            "error": { "kind": "sd", "values": { "type": "symmetric", "column": "se" }, "sample_count_column": null, "confidence_level": null }
        });
        let image = json!({ "asset": { "sha256": "h", "mime": "image/png", "bytes": 1 }, "origin": "user", "source_ref": null,
            "pixel_width": 1, "pixel_height": 1, "crop_pixel_rect": { "x": 0, "y": 0, "width": 1, "height": 1 }, "rotation_millidegrees": 0,
            "calibration": { "length": "1", "unit": "mm", "pixel_span": "1", "evidence_id": null }
        });
        let figure = json!({
            "size": { "width": 100, "height": 100 },
            "panels": [
                { "id": "d", "bounds": { "x": 0, "y": 0, "width": 1, "height": 1 }, "label": "diagram", "body": { "type": "diagram", "value": diagram }, "style": complete_style() },
                { "id": "p", "bounds": { "x": 0, "y": 0, "width": 1, "height": 1 }, "label": "plot", "body": { "type": "plot", "value": plot }, "style": complete_style() },
                { "id": "i", "bounds": { "x": 0, "y": 0, "width": 1, "height": 1 }, "label": "image", "body": { "type": "image", "value": image }, "style": complete_style() }
            ],
            "caption": "caption", "datasets": [dataset], "evidence": [evidence], "source_scopes": [{ "source_hash": "h", "physical_page_indices": [0] }]
        });
        check(&serde_json::to_vec(&artifact_with_body("figure", figure)).unwrap()).expect("Figure model fields");
    }

    #[test]
    fn dynamic_map_keys_may_collide_with_model_field_names() {
        let mut snapshot = canvas_snapshot();
        snapshot["body"]["document"]["styles"] = json!({ "pages": complete_style() });
        snapshot["body"]["document"]["pages"][0]["nodes"][0]["variants"] = json!({ "style": complete_patch() });
        snapshot["native"]["mappings"] = json!({ "body": { "arbitrary": [1, false, null] } });
        snapshot["native"]["preserved"] = json!({ "type": { "opaque": null } });
        check(&serde_json::to_vec(&snapshot).unwrap()).expect("dynamic map keys are scoped to their parent model");
        serde_json::from_value::<Artifact>(snapshot).expect("the dynamic-map fixture is a typed Artifact");
    }

    #[test]
    fn tagged_enum_guards_reject_unknown_tags() {
        let mut unknown_body = canvas_snapshot();
        unknown_body["body"]["type"] = json!("future_body");
        assert!(check(&serde_json::to_vec(&unknown_body).unwrap()).unwrap_err().message.contains("unknown type"));

        let mut unknown_interaction = canvas_snapshot();
        unknown_interaction["body"]["document"]["pages"][0]["nodes"][0]["interactions"] = json!([{ "action": "future_action" }]);
        assert!(check(&serde_json::to_vec(&unknown_interaction).unwrap()).unwrap_err().message.contains("unknown Interaction"));

        let mut unknown_path = canvas_snapshot();
        unknown_path["body"]["document"]["pages"][0]["nodes"][0]["path"] = json!({
            "view_box": { "x": 0, "y": 0, "width": 1, "height": 1 },
            "fill_rule": "nonzero", "commands": [{ "type": "future_command" }]
        });
        assert!(check(&serde_json::to_vec(&unknown_path).unwrap()).unwrap_err().message.contains("unknown PathCommand"));

        let flow = json!({ "sections": [{ "id": "s", "setup": { "size": { "width": 1, "height": 1 }, "margin_left": 0, "margin_top": 0, "margin_right": 0, "margin_bottom": 0 }, "blocks": [{ "type": "future_block", "value": {} }] }], "styles": {} });
        assert!(check(&serde_json::to_vec(&artifact_with_body("flow", flow)).unwrap()).unwrap_err().message.contains("unknown FlowBlock"));

        let unknown_panel = json!({ "size": { "width": 1, "height": 1 }, "panels": [{ "id": "p", "bounds": { "x": 0, "y": 0, "width": 1, "height": 1 }, "label": "", "body": { "type": "future_panel", "value": {} } }] });
        assert!(check(&serde_json::to_vec(&artifact_with_body("figure", unknown_panel)).unwrap()).unwrap_err().message.contains("unknown PanelBody"));

        let unknown_error_values = json!({ "size": { "width": 1, "height": 1 }, "panels": [{ "id": "p", "bounds": { "x": 0, "y": 0, "width": 1, "height": 1 }, "label": "", "body": { "type": "plot", "value": {
            "kind": "bar", "dataset_ref": { "dataset_id": "d", "version_hash": "v" }, "x_column": "x", "y_columns": [],
            "x_axis": { "label": "x", "scale": "linear" }, "y_axis": { "label": "y", "scale": "linear" },
            "error": { "kind": "sd", "values": { "type": "future_error" } }
        } } }] });
        assert!(check(&serde_json::to_vec(&artifact_with_body("figure", unknown_error_values)).unwrap()).unwrap_err().message.contains("unknown ErrorValues"));

        let unknown_transform = json!({ "size": { "width": 1, "height": 1 }, "datasets": [{ "id": "d", "version_hash": "v", "original_asset_hash": "h", "origin": "test", "columns": [], "rows": [], "transforms": [{ "type": "future_transform" }] }] });
        assert!(check(&serde_json::to_vec(&artifact_with_body("figure", unknown_transform)).unwrap()).unwrap_err().message.contains("unknown DataTransform"));
    }

    #[test]
    fn plot_display_mode_is_checked_exactly() {
        let figure_with = |display: Option<Value>| {
            let mut plot = json!({ "kind": "bar", "dataset_ref": { "dataset_id": "d", "version_hash": "v" }, "x_column": "x", "y_columns": [],
                "x_axis": { "label": "x", "scale": "linear" }, "y_axis": { "label": "y", "scale": "linear" } });
            if let Some(display) = display {
                plot["display"] = display;
            }
            json!({ "size": { "width": 1, "height": 1 }, "panels": [{ "id": "p", "bounds": { "x": 0, "y": 0, "width": 1, "height": 1 },
                "label": "", "body": { "type": "plot", "value": plot } }] })
        };
        let check_figure = |figure: Value| check(&serde_json::to_vec(&artifact_with_body("figure", figure)).unwrap());
        check_figure(figure_with(None)).expect("a plot without a display mode");
        check_figure(figure_with(Some(json!("error_key")))).expect("the known display mode");
        check_figure(figure_with(Some(Value::Null))).expect("null is an absent display mode");
        let refused = check_figure(figure_with(Some(json!("error_key_v2")))).expect_err("an unknown display mode");
        assert_eq!(refused.code, designllm_contracts::ErrorCode::SchemaUnsupported);
        assert!(refused.message.contains("PlotDisplayMode"), "{}", refused.message);
    }

    #[test]
    fn schema_guard_covers_every_artifact_field_and_variant() {
        let schema = serde_json::to_value(schemars::schema_for!(designllm_contracts::Artifact)).unwrap();
        let expected_fields = schema_guard_field_sets(&schema);
        SCHEMA_FIELD_EXPECTATIONS.with(|state| {
            *state.borrow_mut() = Some((expected_fields.clone(), std::collections::BTreeSet::new()));
        });
        let samples = schema_samples(&schema, &schema, &mut Vec::new());
        assert!(samples.len() >= 5, "schema traversal should include each ArtifactBody variant");

        for (index, sample) in samples.iter().enumerate() {
            let bytes = serde_json::to_vec(sample).unwrap();
            check(&bytes).unwrap_or_else(|error| {
                panic!("Artifact schema sample {index} contains a field or enum variant the snapshot guard does not cover: {error}; sample={sample}")
            });
            serde_json::from_slice::<Artifact>(&bytes).unwrap_or_else(|error| {
                panic!("Artifact schema sample {index} is not a typed snapshot: {error}; sample={sample}")
            });
        }

        let (expected, used) = SCHEMA_FIELD_EXPECTATIONS.with(|state| state.borrow_mut().take().expect("schema field set expectations were installed"));
        let expected_names: std::collections::BTreeSet<String> = expected.keys().cloned().collect();
        assert_eq!(used, expected_names, "some Artifact schema object or tagged variant has no production guard call");
    }
}
