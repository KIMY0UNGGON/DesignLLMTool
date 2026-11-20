//! Refusals for contract v1.2 values that PresentationML does not store or draw yet (P0). A change or a write that
//! carries such a value is refused as a whole. The message and the diagnostic name the fields, and nothing is dropped
//! silently (R09). The reader never produces these values, so existing decks are unaffected.

use designllm_contracts::{
    Diagnostic, DltError, DltResult, ErrorCode, FillRule, Interaction, Node, NodeKind, Page, PathCommand, PathSpec, Shadow,
    Style, StylePatch,
};
use designllm_package::{is_cfb_container, opc, Package};

use crate::editable::{parse_colour, MAX_COORDINATE};

/// Diagnostic code of every v1.2 refusal.
pub const UNSUPPORTED_V12: &str = "UNSUPPORTED_V12";
/// Refused before ZIP parsing: CFB bytes are not a PPTX package.
pub const COMPOUND_CONTAINER_UNSUPPORTED: &str = "pptx.compound_container_unsupported";
/// Refused because the package contains signature markers this codec cannot validate or renew.
pub const DIGITAL_SIGNATURE_UNSUPPORTED: &str = "pptx.digital_signature_unsupported";
/// Structural error while inspecting OPC signature metadata.
pub const OPC_ERROR: &str = "PPTX_OPC_ERROR";

/// Opens a PPTX package after checking its container and OPC signature markers.
pub fn open_package(bytes: &[u8]) -> DltResult<Package> {
    if is_cfb_container(bytes) {
        return Err(refuse_package(
            COMPOUND_CONTAINER_UNSUPPORTED,
            "PPTX input uses the OLE Compound File container, not a PPTX ZIP package; the CFB header alone does not identify the document type or protection state",
        ));
    }
    let package = Package::open(bytes)?;
    check_package(&package)?;
    Ok(package)
}

/// Refuses signature markers and malformed signature metadata in an already opened package.
pub fn check_package(package: &Package) -> DltResult<()> {
    match opc::has_digital_signature(package) {
        Ok(false) => Ok(()),
        Ok(true) => Err(refuse_package(
            DIGITAL_SIGNATURE_UNSUPPORTED,
            "PPTX package has OPC digital-signature markers; this codec cannot validate or renew signatures, so it is refused",
        )),
        Err(error) => {
            let message = format!("PPTX signature metadata is malformed and cannot be checked: {}", error.message);
            let mut refusal = DltError::new(ErrorCode::InvalidArgument, message.clone());
            refusal.diagnostics.push(Diagnostic {
                code: OPC_ERROR.into(),
                message,
                node_id: None,
                blocking: true,
                params: Default::default(),
            });
            Err(refusal)
        }
    }
}

fn refuse_package(code: &str, message: &str) -> DltError {
    let mut error = DltError::unsupported(message);
    error.diagnostics.push(Diagnostic {
        code: code.into(),
        message: message.into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    });
    error
}

/// An UNSUPPORTED_OPERATION error whose diagnostic repeats the message and names the node, when there is one.
pub fn refuse(message: impl Into<String>, node_id: Option<&str>) -> DltError {
    let message = message.into();
    let mut error = DltError::unsupported(message.clone());
    error.diagnostics.push(Diagnostic {
        code: UNSUPPORTED_V12.into(),
        message,
        node_id: node_id.map(str::to_string),
        blocking: true,
        params: Default::default(),
    });
    error
}

/// Refuses a style that carries a v1.2 value. `context` completes "not supported {context} yet".
pub fn check_style(style: &Style, context: &str, node_id: Option<&str>) -> DltResult<()> {
    let names: Vec<String> = style.non_default_v12_fields().into_iter().map(|field| field.name().to_string()).collect();
    if names.is_empty() {
        return Ok(());
    }
    Err(refuse(format!("style {} is not supported {context} yet", names.join(", ")), node_id))
}

/// Refuses a patch that sets or clears a v1.2 field, and every patch that clears a field (P0).
pub fn check_patch(patch: &StylePatch, context: &str, node_id: Option<&str>) -> DltResult<()> {
    if !patch.clear.is_empty() {
        let names: Vec<String> = patch.clear.iter().copied().map(|field| field.name().to_string()).collect();
        return Err(refuse(format!("clearing style fields ({}) is not supported {context} yet", names.join(", ")), node_id));
    }
    let names: Vec<String> = patch.set_fields().into_iter().filter(|field| field.is_v12()).map(|field| field.name().to_string()).collect();
    if names.is_empty() {
        return Ok(());
    }
    Err(refuse(format!("style {} is not supported {context} yet", names.join(", ")), node_id))
}

/// Refuses a shadow on a shape whose preset is not `rect` or `ellipse`. The reader models a shadow only on those two presets.
/// Any other effect list stays as written, so a shadow written on another preset could not be read back. The node model
/// has no preset, so the caller passes the `geometry` of the shape's mapping record.
pub fn check_shadow_preset(preset: &str, node_id: &str, context: &str) -> DltResult<()> {
    if matches!(preset, "rect" | "ellipse") {
        return Ok(());
    }
    Err(refuse(format!("shadows on {preset} shapes are not supported {context} yet; only rectangles and ellipses take a shadow"), Some(node_id)))
}

/// Refuses the shadows of a node that the canonical form cannot hold (see `editable`): a rectangle or ellipse kind (the
/// preset is checked by `check_shadow_preset`), at most one shadow, no spread, an offset on one axis, and a colour of
/// `#RRGGBB` or `#RRGGBBAA`. A blur is written with a loss entry instead of being refused. An empty list is always
/// accepted, so a shadow can be cleared on any node.
pub fn check_shadows(shadows: &[Shadow], kind: NodeKind, node_id: &str, context: &str) -> DltResult<()> {
    let Some(shadow) = shadows.first() else { return Ok(()) };
    let id = Some(node_id);
    if !matches!(kind, NodeKind::Rectangle | NodeKind::Ellipse) {
        return Err(refuse(
            format!("shadows on {kind:?} nodes are not supported {context} yet; only rectangles and ellipses take a shadow"),
            id,
        ));
    }
    if shadows.len() > 1 {
        return Err(refuse(format!("more than one shadow is not supported {context} yet"), id));
    }
    if shadow.spread != 0 {
        return Err(refuse(format!("shadow spread is not supported {context} yet: PowerPoint has no spread"), id));
    }
    if shadow.x != 0 && shadow.y != 0 {
        return Err(refuse(
            format!("a diagonal shadow offset is not supported {context} yet: PowerPoint stores a distance and a direction, so the offset must lie on one axis"),
            id,
        ));
    }
    if shadow.blur < 0 {
        return Err(DltError::invalid(format!("{node_id}: a shadow blur cannot be negative")));
    }
    let max = MAX_COORDINATE as u64;
    if [shadow.x, shadow.y, shadow.blur].iter().any(|value| value.unsigned_abs() > max) {
        return Err(refuse(format!("a shadow value is outside the range that PowerPoint stores {context}"), id));
    }
    if parse_colour(&shadow.color).is_none() {
        return Err(DltError::invalid(format!("{node_id}: shadow colour {:?} must be #RRGGBB or #RRGGBBAA", shadow.color)));
    }
    Ok(())
}

/// Refuses a vector path that the canonical custom geometry cannot hold (see `editable`): the nonzero fill rule, a view box
/// at the origin, a positive size, at least one command whose first command is `moveTo`, and coordinates in the DrawingML
/// range.
pub fn check_path(path: &PathSpec, node_id: &str, context: &str) -> DltResult<()> {
    let id = Some(node_id);
    if path.fill_rule != FillRule::Nonzero {
        return Err(refuse(format!("an even-odd fill rule is not supported {context} yet: PowerPoint keeps no fill rule"), id));
    }
    if path.view_box.x != 0 || path.view_box.y != 0 {
        return Err(refuse(
            format!("a view box with an origin other than 0, 0 is not supported {context} yet: PowerPoint keeps only the size of a path"),
            id,
        ));
    }
    if path.view_box.width <= 0 || path.view_box.height <= 0 {
        return Err(DltError::invalid(format!("{node_id}: the path view box must be positive")));
    }
    let max = MAX_COORDINATE as u64;
    if path.view_box.width.unsigned_abs() > max || path.view_box.height.unsigned_abs() > max {
        return Err(refuse(format!("the path size is outside the range that PowerPoint stores {context}"), id));
    }
    if path.commands.is_empty() {
        return Err(refuse(format!("an empty vector path is not supported {context} yet"), id));
    }
    if !matches!(path.commands.first(), Some(PathCommand::MoveTo { .. })) {
        return Err(refuse(format!("a vector path that does not start with moveTo is not supported {context} yet"), id));
    }
    let in_range = |value: i64| value.unsigned_abs() <= max;
    let outside = path.commands.iter().any(|command| match *command {
        PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => !in_range(x) || !in_range(y),
        PathCommand::CubicTo { x1, y1, x2, y2, x, y } => [x1, y1, x2, y2, x, y].into_iter().any(|value| !in_range(value)),
        PathCommand::Close => false,
    });
    if outside {
        return Err(refuse(format!("a path coordinate is outside the range that PowerPoint stores {context}"), id));
    }
    Ok(())
}

/// Refuses a vector path that carries a text field away from its default: a path has no text body, so the writer keeps none
/// of these fields and they would be lost on save without a refusal. The reader gives a path the defaults.
fn check_path_text(style: &Style, node_id: &str, context: &str) -> DltResult<()> {
    let base = Style::default();
    let differs = [
        ("font_family", style.font_family != base.font_family),
        ("font_size", style.font_size != base.font_size),
        ("color", style.color != base.color),
        ("bold", style.bold != base.bold),
        ("italic", style.italic != base.italic),
        ("align", style.align != base.align),
    ];
    let names: Vec<&str> = differs.into_iter().filter(|(_, differ)| *differ).map(|(name, _)| name).collect();
    if names.is_empty() {
        return Ok(());
    }
    Err(refuse(
        format!("text formatting ({}) of a vector path is not supported {context} yet: a path has no text", names.join(", ")),
        Some(node_id),
    ))
}

/// Refuses a node that uses a v1.2 kind, field, or value: a vector path without its geometry, with text, or with a text
/// field away from its default, the `NodeExt` fields, a style value, layout fields and grid mode, `OpenUrl` and `Back`
/// interactions, and variant patches. A vector path and a preset shape's shadow are checked in their canonical forms.
/// Children are checked too.
pub fn check_node(node: &Node, context: &str) -> DltResult<()> {
    let id = Some(node.id.as_str());
    if node.kind == NodeKind::Video {
        return Err(refuse(format!("video nodes (NodeKind::Video) are not supported {context} yet"), id));
    }
    if node.kind == NodeKind::Path {
        let Some(path) = &node.ext.path else {
            return Err(refuse(format!("vector paths (NodeKind::Path) without ext.path are not supported {context} yet"), id));
        };
        check_path(path, &node.id, context)?;
        if !node.text.is_empty() {
            return Err(refuse(format!("text in a vector path is not supported {context} yet"), id));
        }
        check_path_text(&node.style, &node.id, context)?;
    }
    // `ext.path` belongs to a vector path only, and it has been checked above.
    let mut ext = node.ext.set_fields();
    if node.kind == NodeKind::Path {
        ext.retain(|field| *field != "path");
    }
    if !ext.is_empty() {
        return Err(refuse(format!("node fields {} are not supported {context} yet", ext.join(", ")), id));
    }
    let mut style = node.style.clone();
    if node.kind == NodeKind::Line {
        style.stroke_start = None;
        style.stroke_end = None;
    }
    check_shadows(&style.shadows, node.kind, &node.id, context)?;
    style.shadows.clear();
    check_style(&style, context, id)?;
    if let Some(layout) = &node.layout {
        let fields = layout.v12_fields();
        if !fields.is_empty() {
            return Err(refuse(format!("layout {} is not supported {context} yet", fields.join(", ")), id));
        }
    }
    for interaction in &node.interactions {
        let name = match interaction {
            Interaction::OpenUrl { .. } => Some("open_url"),
            Interaction::Back => Some("back"),
            _ => None,
        };
        if let Some(name) = name {
            return Err(refuse(format!("interaction {name} is not supported {context} yet"), id));
        }
    }
    for patch in node.variants.values() {
        check_patch(patch, context, id)?;
    }
    for child in &node.children {
        check_node(child, context)?;
    }
    Ok(())
}

/// Refuses a page that carries a value the PPTX adapter does not preserve: component role, page name, or unsupported node fields.
pub fn check_page(page: &Page, context: &str) -> DltResult<()> {
    if page.role.is_some() {
        return Err(refuse(format!("component page roles are not supported {context} yet"), None));
    }
    if page.name.is_some() {
        return Err(refuse(format!("page names are not supported {context} yet"), None));
    }
    page.nodes.iter().try_for_each(|node| check_node(node, context))
}
