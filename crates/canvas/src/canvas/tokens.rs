use super::{model, ops};
use crate::common::{blocking, invalid, unsupported, valid_color, valid_id, valid_key, MAX_EXTENT_EMU};
use designllm_contracts::{
    Artifact, ArtifactBody, BindTarget, CanvasDocument, DltError, DltResult, LayoutSpec, Node, Operation, PaddingSide,
    Style, StyleField, StylePatch, TokenBinding, TokenDef, TokenMode, TokenRef, TokenSource, TokenValues,
};
use std::collections::{BTreeMap, BTreeSet};

fn invalid_token(code: &str, message: impl Into<String>) -> DltError {
    invalid(code, message)
}

fn token_mode(doc: &CanvasDocument) -> TokenMode {
    doc.token_mode.unwrap_or(TokenMode::Light)
}

fn token_name_valid(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 64 && !name.chars().any(char::is_control)
}

fn validate_font_family(value: &str) -> DltResult<()> {
    if value.len() > 128 {
        return Err(invalid_token("TOKEN_FONT_FAMILY", "font-family token values must be at most 128 bytes"));
    }
    let mut style = Style::default();
    style.font_family = value.to_owned();
    model::validate_style(&style).map_err(|error| invalid_token("TOKEN_FONT_FAMILY", error.message))
}

fn validate_style_value(field: StyleField, value: i64) -> DltResult<()> {
    let mut style = Style::default();
    match field {
        StyleField::FontSize => style.font_size = value,
        StyleField::CornerRadius => style.corner_radius = Some(value),
        _ => return Err(invalid_token("TOKEN_TARGET", "token target is not a numeric style field")),
    }
    model::validate_style(&style).map_err(|error| invalid_token("TOKEN_VALUE_RANGE", error.message))
}

fn validate_token_values(values: &TokenValues) -> DltResult<()> {
    match values {
        TokenValues::Color { light, dark } => {
            for value in [light, dark] {
                if !valid_color(value) { return Err(invalid_token("TOKEN_COLOR", format!("invalid color token value {value:?}"))); }
            }
        }
        TokenValues::FontFamily { light, dark } => {
            validate_font_family(light)?;
            validate_font_family(dark)?;
        }
        TokenValues::FontSize { light, dark } => {
            validate_style_value(StyleField::FontSize, *light)?;
            validate_style_value(StyleField::FontSize, *dark)?;
        }
        TokenValues::Spacing { light, dark } => {
            if [light, dark].iter().any(|value| !(0..=MAX_EXTENT_EMU).contains(value)) {
                return Err(invalid_token("TOKEN_SPACING", "spacing token values must be between 0 and 10000 mm"));
            }
        }
        TokenValues::Radius { light, dark } => {
            validate_style_value(StyleField::CornerRadius, *light)?;
            validate_style_value(StyleField::CornerRadius, *dark)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_definition(token_id: &str, definition: &TokenDef) -> DltResult<()> {
    if !valid_key(token_id) {
        return Err(invalid_token("TOKEN_ID", "token id must use letters, numbers, `_`, `-`, or `.` and be at most 64 bytes"));
    }
    if !token_name_valid(&definition.name) {
        return Err(invalid_token("TOKEN_NAME", "token name must contain 1 to 64 bytes and no control characters"));
    }
    validate_token_values(&definition.values)
}

fn target_matches_values(target: &BindTarget, values: &TokenValues) -> bool {
    match target {
        BindTarget::StyleColor | BindTarget::StyleFill | BindTarget::StyleStroke => matches!(values, TokenValues::Color { .. }),
        BindTarget::StyleFontFamily => matches!(values, TokenValues::FontFamily { .. }),
        BindTarget::StyleFontSize => matches!(values, TokenValues::FontSize { .. }),
        BindTarget::StyleCornerRadius => matches!(values, TokenValues::Radius { .. }),
        BindTarget::LayoutGap | BindTarget::LayoutRowGap | BindTarget::LayoutPadding
        | BindTarget::LayoutPaddingSide { .. } | BindTarget::BreakpointGap { .. } | BindTarget::BreakpointPadding { .. } => {
            matches!(values, TokenValues::Spacing { .. })
        }
    }
}

fn target_label(target: &BindTarget) -> String {
    match target {
        BindTarget::StyleColor => "style.color".into(),
        BindTarget::StyleFill => "style.fill".into(),
        BindTarget::StyleStroke => "style.stroke".into(),
        BindTarget::StyleFontFamily => "style.font_family".into(),
        BindTarget::StyleFontSize => "style.font_size".into(),
        BindTarget::StyleCornerRadius => "style.corner_radius".into(),
        BindTarget::LayoutGap => "layout.gap".into(),
        BindTarget::LayoutRowGap => "layout.row_gap".into(),
        BindTarget::LayoutPadding => "layout.padding".into(),
        BindTarget::LayoutPaddingSide { side } => format!("layout.padding_side.{}", match side {
            PaddingSide::Top => "top", PaddingSide::Right => "right", PaddingSide::Bottom => "bottom", PaddingSide::Left => "left",
        }),
        BindTarget::BreakpointGap { min_width_css_px } => format!("breakpoint.gap@{min_width_css_px}"),
        BindTarget::BreakpointPadding { min_width_css_px } => format!("breakpoint.padding@{min_width_css_px}"),
    }
}

fn layout_for_target<'a>(node: &'a Node, target: &BindTarget) -> DltResult<&'a LayoutSpec> {
    let layout = node.layout.as_ref().ok_or_else(|| invalid_token("LAYOUT_NOT_SET", format!("{} requires a node with layout", target_label(target))))?;
    match target {
        BindTarget::LayoutPadding if layout.padding_sides.is_some() => {
            Err(invalid_token("TOKEN_PADDING_MODE_MISMATCH", "layout.padding cannot be bound while padding_sides is set"))
        }
        BindTarget::LayoutPaddingSide { .. } if layout.padding_sides.is_none() => {
            Err(invalid_token("TOKEN_PADDING_SIDES_NOT_SET", "set layout.padding_sides before binding an individual side"))
        }
        BindTarget::BreakpointGap { min_width_css_px } | BindTarget::BreakpointPadding { min_width_css_px }
            if !layout.breakpoints.iter().any(|breakpoint| breakpoint.min_width_css_px == *min_width_css_px) => {
            Err(invalid_token("TOKEN_BREAKPOINT_NOT_FOUND", format!("{} has no breakpoint at {min_width_css_px} CSS px", target_label(target))))
        }
        _ => Ok(layout),
    }
}

fn validate_target_shape(node: &Node, target: &BindTarget) -> DltResult<()> {
    match target {
        BindTarget::StyleColor | BindTarget::StyleFill | BindTarget::StyleStroke | BindTarget::StyleFontFamily
        | BindTarget::StyleFontSize | BindTarget::StyleCornerRadius => Ok(()),
        _ => layout_for_target(node, target).map(|_| ()),
    }
}

fn side_index(side: PaddingSide) -> usize {
    match side { PaddingSide::Top => 0, PaddingSide::Right => 1, PaddingSide::Bottom => 2, PaddingSide::Left => 3 }
}

fn apply_target_value(doc: &mut CanvasDocument, node_id: &str, target: &BindTarget, values: &TokenValues, mode: TokenMode, check_lock: bool) -> DltResult<()> {
    if !target_matches_values(target, values) {
        return Err(invalid_token("TOKEN_KIND_MISMATCH", format!("{} cannot use this token kind", target_label(target))));
    }
    if check_lock { ops::ensure_node_unlocked_in_doc(doc, node_id)?; }
    let node = model::find_node_in_doc_mut(doc, node_id)
        .ok_or_else(|| invalid_token("TOKEN_NODE_NOT_FOUND", format!("node {node_id} does not exist")))?;
    validate_target_shape(node, target)?;
    match (target, values) {
        (BindTarget::StyleColor, TokenValues::Color { light, dark }) => node.style.color.clone_from(if mode == TokenMode::Light { light } else { dark }),
        (BindTarget::StyleFill, TokenValues::Color { light, dark }) => node.style.fill.clone_from(if mode == TokenMode::Light { light } else { dark }),
        (BindTarget::StyleStroke, TokenValues::Color { light, dark }) => node.style.stroke.clone_from(if mode == TokenMode::Light { light } else { dark }),
        (BindTarget::StyleFontFamily, TokenValues::FontFamily { light, dark }) => node.style.font_family.clone_from(if mode == TokenMode::Light { light } else { dark }),
        (BindTarget::StyleFontSize, TokenValues::FontSize { light, dark }) => node.style.font_size = if mode == TokenMode::Light { *light } else { *dark },
        (BindTarget::StyleCornerRadius, TokenValues::Radius { light, dark }) => node.style.corner_radius = Some(if mode == TokenMode::Light { *light } else { *dark }),
        (target, TokenValues::Spacing { light, dark }) => {
            let value = if mode == TokenMode::Light { *light } else { *dark };
            let layout = node.layout.as_mut().ok_or_else(|| invalid_token("LAYOUT_NOT_SET", format!("{} requires a node with layout", target_label(target))))?;
            match target {
                BindTarget::LayoutGap => layout.gap = value,
                BindTarget::LayoutRowGap => layout.row_gap = Some(value),
                BindTarget::LayoutPadding => layout.padding = value,
                BindTarget::LayoutPaddingSide { side } => {
                    let sides = layout.padding_sides.as_mut().ok_or_else(|| invalid_token("TOKEN_PADDING_SIDES_NOT_SET", "set layout.padding_sides before binding an individual side"))?;
                    sides[side_index(*side)] = value;
                }
                BindTarget::BreakpointGap { min_width_css_px } => {
                    let breakpoint = layout.breakpoints.iter_mut().find(|breakpoint| breakpoint.min_width_css_px == *min_width_css_px)
                        .ok_or_else(|| invalid_token("TOKEN_BREAKPOINT_NOT_FOUND", format!("{} has no breakpoint at {min_width_css_px} CSS px", target_label(target))))?;
                    breakpoint.gap = Some(value);
                }
                BindTarget::BreakpointPadding { min_width_css_px } => {
                    let breakpoint = layout.breakpoints.iter_mut().find(|breakpoint| breakpoint.min_width_css_px == *min_width_css_px)
                        .ok_or_else(|| invalid_token("TOKEN_BREAKPOINT_NOT_FOUND", format!("{} has no breakpoint at {min_width_css_px} CSS px", target_label(target))))?;
                    breakpoint.padding = Some(value);
                }
                _ => return Err(invalid_token("TOKEN_KIND_MISMATCH", format!("{} cannot use a spacing token", target_label(target)))),
            }
            model::validate_layout(layout, node.kind)?;
            return Ok(());
        }
        _ => return Err(invalid_token("TOKEN_KIND_MISMATCH", format!("{} cannot use this token kind", target_label(target)))),
    }
    model::validate_style(&node.style)
}

fn add_error(out: &mut Vec<designllm_contracts::Diagnostic>, error: DltError, fallback: &str, node_id: Option<&str>) {
    if error.diagnostics.is_empty() {
        out.push(blocking(fallback, error.message, node_id.map(str::to_owned)));
    } else {
        for mut diagnostic in error.diagnostics {
            if diagnostic.node_id.is_none() { diagnostic.node_id = node_id.map(str::to_owned); }
            out.push(diagnostic);
        }
    }
}

pub(crate) fn validate_document(doc: &CanvasDocument) -> Vec<designllm_contracts::Diagnostic> {
    let mut out = Vec::new();
    let mut valid_tokens = BTreeSet::new();
    for (token_id, definition) in &doc.tokens {
        match validate_definition(token_id, definition) {
            Ok(()) => { valid_tokens.insert(token_id.as_str()); }
            Err(error) => add_error(&mut out, error, "TOKEN_DEFINITION", Some(token_id)),
        }
    }
    let mut previous: Option<(&str, &BindTarget)> = None;
    for binding in &doc.bindings {
        if previous.is_some_and(|(node_id, target)| (node_id, target) >= (binding.node_id.as_str(), &binding.target)) {
            out.push(blocking("TOKEN_BINDING_ORDER", "token bindings must be sorted and unique by node id and target", Some(binding.node_id.clone())));
        }
        previous = Some((binding.node_id.as_str(), &binding.target));
        if !valid_key(&binding.token.token_id) {
            out.push(blocking("TOKEN_ID", "token id is invalid", Some(binding.node_id.clone())));
            continue;
        }
        let Some(node) = model::find_node_in_doc(doc, &binding.node_id) else {
            out.push(blocking("TOKEN_NODE_NOT_FOUND", format!("node {} does not exist", binding.node_id), Some(binding.node_id.clone())));
            continue;
        };
        if let Err(error) = validate_target_shape(node, &binding.target) {
            add_error(&mut out, error, "TOKEN_TARGET", Some(&binding.node_id));
            continue;
        }
        if binding.token.source == TokenSource::Shared {
            // Core resolves shared values from the pinned publisher snapshot before commit.
            continue;
        }
        let Some(definition) = doc.tokens.get(&binding.token.token_id) else {
            out.push(blocking("TOKEN_NOT_FOUND", format!("token {} does not exist", binding.token.token_id), Some(binding.node_id.clone())));
            continue;
        };
        if !valid_tokens.contains(binding.token.token_id.as_str()) { continue; }
        if !target_matches_values(&binding.target, &definition.values) {
            out.push(blocking("TOKEN_KIND_MISMATCH", format!("{} cannot use this token kind", target_label(&binding.target)), Some(binding.node_id.clone())));
            continue;
        }
        if !binding_matches_value(node, binding, &definition.values, token_mode(doc)) {
            out.push(blocking("TOKEN_VALUE_MISMATCH", format!("{} does not match its bound token's active value", target_label(&binding.target)), Some(binding.node_id.clone())));
        }
    }
    out
}

pub(crate) fn validate_artifact_state(artifact: &Artifact) -> Vec<designllm_contracts::Diagnostic> {
    let ArtifactBody::Canvas(doc) = &artifact.body else { return Vec::new() };
    let mut out = Vec::new();
    if artifact.design_system.as_ref().is_some_and(|reference| !valid_id(&reference.artifact_id)) {
        out.push(blocking("DESIGN_SYSTEM_REF", "design system artifact id is invalid", None));
    }
    if artifact.design_system.as_ref().is_some_and(|reference| reference.artifact_id == artifact.id) {
        out.push(blocking("DESIGN_SYSTEM_CYCLE", "a design system cannot reference itself", None));
    }
    if artifact.publishes_design_system && artifact.design_system.is_some() {
        out.push(blocking("DESIGN_SYSTEM_CYCLE", "a design system publisher cannot consume another design system", None));
    }
    if artifact.design_system.is_none() && doc.bindings.iter().any(|binding| binding.token.source == TokenSource::Shared) {
        out.push(blocking("TOKEN_SHARED_SYSTEM_MISSING", "shared token bindings require an artifact design_system reference", None));
    }
    out
}

pub(crate) fn validate_shared_snapshot(doc: &CanvasDocument, tokens: &BTreeMap<String, TokenDef>) -> DltResult<()> {
    let mode = token_mode(doc);
    for binding in doc.bindings.iter().filter(|binding| binding.token.source == TokenSource::Shared) {
        let definition = tokens.get(&binding.token.token_id)
            .ok_or_else(|| invalid_token("TOKEN_NOT_FOUND", format!("shared token {} does not exist", binding.token.token_id)))?;
        validate_definition(&binding.token.token_id, definition)?;
        if !target_matches_values(&binding.target, &definition.values) {
            return Err(invalid_token("TOKEN_KIND_MISMATCH", format!("{} cannot use this shared token kind", target_label(&binding.target))));
        }
        let node = model::find_node_in_doc(doc, &binding.node_id)
            .ok_or_else(|| invalid_token("TOKEN_NODE_NOT_FOUND", format!("node {} does not exist", binding.node_id)))?;
        validate_target_shape(node, &binding.target)?;
        if !binding_matches_value(node, binding, &definition.values, mode) {
            return Err(invalid_token("TOKEN_VALUE_MISMATCH", format!("{} does not match the pinned shared token value", target_label(&binding.target))));
        }
    }
    Ok(())
}

fn binding_matches_value(node: &Node, binding: &TokenBinding, values: &TokenValues, mode: TokenMode) -> bool {
    match (&binding.target, values) {
        (BindTarget::StyleColor, TokenValues::Color { light, dark }) => node.style.color.as_str() == if mode == TokenMode::Light { light.as_str() } else { dark.as_str() },
        (BindTarget::StyleFill, TokenValues::Color { light, dark }) => node.style.fill.as_str() == if mode == TokenMode::Light { light.as_str() } else { dark.as_str() },
        (BindTarget::StyleStroke, TokenValues::Color { light, dark }) => node.style.stroke.as_str() == if mode == TokenMode::Light { light.as_str() } else { dark.as_str() },
        (BindTarget::StyleFontFamily, TokenValues::FontFamily { light, dark }) => node.style.font_family.as_str() == if mode == TokenMode::Light { light.as_str() } else { dark.as_str() },
        (BindTarget::StyleFontSize, TokenValues::FontSize { light, dark }) => node.style.font_size == if mode == TokenMode::Light { *light } else { *dark },
        (BindTarget::StyleCornerRadius, TokenValues::Radius { light, dark }) => node.style.corner_radius == Some(if mode == TokenMode::Light { *light } else { *dark }),
        (target, TokenValues::Spacing { light, dark }) => {
            let value = if mode == TokenMode::Light { *light } else { *dark };
            let Some(layout) = node.layout.as_ref() else { return false };
            match target {
                BindTarget::LayoutGap => layout.gap == value,
                BindTarget::LayoutRowGap => layout.row_gap == Some(value),
                BindTarget::LayoutPadding => layout.padding == value,
                BindTarget::LayoutPaddingSide { side } => layout.padding_sides.is_some_and(|sides| sides[side_index(*side)] == value),
                BindTarget::BreakpointGap { min_width_css_px } => layout.breakpoints.iter().find(|bp| bp.min_width_css_px == *min_width_css_px).is_some_and(|bp| bp.gap == Some(value)),
                BindTarget::BreakpointPadding { min_width_css_px } => layout.breakpoints.iter().find(|bp| bp.min_width_css_px == *min_width_css_px).is_some_and(|bp| bp.padding == Some(value)),
                _ => false,
            }
        }
        _ => false,
    }
}

fn preflight_binding_value(doc: &CanvasDocument, binding: &TokenBinding, values: &TokenValues, mode: TokenMode) -> DltResult<()> {
    if binding.token.source != TokenSource::Local {
        return Err(unsupported("TOKEN_SHARED_UNSUPPORTED", "shared token bindings require the workspace propagation path"));
    }
    if !target_matches_values(&binding.target, values) {
        return Err(invalid_token("TOKEN_KIND_MISMATCH", format!("{} cannot use this token kind", target_label(&binding.target))));
    }
    let node = model::find_node_in_doc(doc, &binding.node_id)
        .ok_or_else(|| invalid_token("TOKEN_NODE_NOT_FOUND", format!("node {} does not exist", binding.node_id)))?;
    validate_target_shape(node, &binding.target)?;
    if !binding_matches_value(node, binding, values, mode) {
        ops::ensure_node_unlocked_in_doc(doc, &binding.node_id)?;
    }
    Ok(())
}

fn preflight_component_token_operation(doc: &CanvasDocument, operation: &Operation) -> DltResult<()> {
    match operation {
        Operation::PutToken { token_id, name, values } => {
            validate_definition(token_id, &TokenDef { name: name.clone(), values: values.clone() })?;
            if doc.tokens.get(token_id).is_some_and(|previous| !same_kind(&previous.values, values)) {
                return Ok(()); // put_token reports the established kind error before changing anything.
            }
            let mode = token_mode(doc);
            for binding in doc.bindings.iter().filter(|binding| binding.token.source == TokenSource::Local && binding.token.token_id == *token_id) {
                preflight_binding_value(doc, binding, values, mode)?;
                let node = model::find_node_in_doc(doc, &binding.node_id).expect("preflight_binding_value checked node");
                if !binding_matches_value(node, binding, values, mode) {
                    super::components::ensure_unlocked_materialized_updates(doc, std::slice::from_ref(&binding.node_id))?;
                }
            }
        }
        Operation::SetTokenMode { mode } => {
            if token_mode(doc) == *mode { return Ok(()) }
            for binding in doc.bindings.iter().filter(|binding| binding.token.source == TokenSource::Local) {
                let definition = target_def(doc, &binding.token)?;
                preflight_binding_value(doc, binding, &definition.values, *mode)?;
                let node = model::find_node_in_doc(doc, &binding.node_id).expect("preflight_binding_value checked node");
                if !binding_matches_value(node, binding, &definition.values, *mode) {
                    super::components::ensure_unlocked_materialized_updates(doc, std::slice::from_ref(&binding.node_id))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn preflight_shared_sync(doc: &CanvasDocument, tokens: &BTreeMap<String, TokenDef>) -> DltResult<()> {
    let mode = token_mode(doc);
    for binding in doc.bindings.iter().filter(|binding| binding.token.source == TokenSource::Shared) {
        let definition = tokens.get(&binding.token.token_id)
            .ok_or_else(|| invalid_token("TOKEN_NOT_FOUND", format!("shared token {} does not exist", binding.token.token_id)))?;
        validate_definition(&binding.token.token_id, definition)?;
        if !target_matches_values(&binding.target, &definition.values) {
            return Err(invalid_token("TOKEN_KIND_MISMATCH", format!("{} cannot use this shared token kind", target_label(&binding.target))));
        }
        let node = model::find_node_in_doc(doc, &binding.node_id)
            .ok_or_else(|| invalid_token("TOKEN_NODE_NOT_FOUND", format!("node {} does not exist", binding.node_id)))?;
        validate_target_shape(node, &binding.target)?;
        if !binding_matches_value(node, binding, &definition.values, mode) {
            super::components::ensure_unlocked_materialized_updates(doc, std::slice::from_ref(&binding.node_id))?;
        }
    }
    Ok(())
}

fn preflight_shared_unbind(doc: &CanvasDocument) -> DltResult<()> {
    for binding in doc.bindings.iter().filter(|binding| binding.token.source == TokenSource::Shared) {
        super::components::ensure_unlocked_materialized_updates(doc, std::slice::from_ref(&binding.node_id))?;
    }
    Ok(())
}

fn target_def<'a>(doc: &'a CanvasDocument, token: &TokenRef) -> DltResult<&'a TokenDef> {
    if token.source != TokenSource::Local {
        return Err(unsupported("TOKEN_SHARED_UNSUPPORTED", "shared token bindings require the workspace propagation path"));
    }
    doc.tokens.get(&token.token_id).ok_or_else(|| invalid_token("TOKEN_NOT_FOUND", format!("token {} does not exist", token.token_id)))
}

fn put_token(doc: &mut CanvasDocument, token_id: &str, name: &str, values: &TokenValues) -> DltResult<Vec<String>> {
    let definition = TokenDef { name: name.to_owned(), values: values.clone() };
    validate_definition(token_id, &definition)?;
    if let Some(previous) = doc.tokens.get(token_id) {
        if !same_kind(&previous.values, values) {
            return Err(invalid_token("TOKEN_KIND_IMMUTABLE", format!("token {token_id} cannot change from {} to {}", kind_name(&previous.values), kind_name(values))));
        }
    }
    let mode = token_mode(doc);
    for binding in doc.bindings.iter().filter(|binding| binding.token.source == TokenSource::Local && binding.token.token_id == token_id) {
        preflight_binding_value(doc, binding, values, mode)?;
    }
    doc.tokens.insert(token_id.to_owned(), definition);
    let mut changed = BTreeSet::from([token_id.to_owned()]);
    let binding_count = doc.bindings.len();
    for index in 0..binding_count {
        if doc.bindings[index].token.source != TokenSource::Local || doc.bindings[index].token.token_id != token_id { continue; }
        let (node_id, target) = (doc.bindings[index].node_id.clone(), doc.bindings[index].target.clone());
        apply_target_value(doc, &node_id, &target, values, mode, false)?;
        changed.insert(node_id);
    }
    Ok(changed.into_iter().collect())
}

fn same_kind(left: &TokenValues, right: &TokenValues) -> bool {
    matches!((left, right),
        (TokenValues::Color { .. }, TokenValues::Color { .. })
        | (TokenValues::FontFamily { .. }, TokenValues::FontFamily { .. })
        | (TokenValues::FontSize { .. }, TokenValues::FontSize { .. })
        | (TokenValues::Spacing { .. }, TokenValues::Spacing { .. })
        | (TokenValues::Radius { .. }, TokenValues::Radius { .. }))
}

fn kind_name(values: &TokenValues) -> &'static str {
    match values {
        TokenValues::Color { .. } => "color", TokenValues::FontFamily { .. } => "font_family",
        TokenValues::FontSize { .. } => "font_size", TokenValues::Spacing { .. } => "spacing", TokenValues::Radius { .. } => "radius",
    }
}

fn remove_token(doc: &mut CanvasDocument, token_id: &str) -> DltResult<Vec<String>> {
    if !doc.tokens.contains_key(token_id) { return Err(invalid_token("TOKEN_NOT_FOUND", format!("token {token_id} does not exist"))); }
    let uses: BTreeSet<&str> = doc.bindings.iter()
        .filter(|binding| binding.token.source == TokenSource::Local && binding.token.token_id == token_id)
        .map(|binding| binding.node_id.as_str()).collect();
    if !uses.is_empty() {
        let names = uses.iter().copied().collect::<Vec<_>>().join(", ");
        let mut error = invalid_token("TOKEN_IN_USE", format!("token {token_id} is still bound to node(s) {names}"));
        error.affected_ids = uses.iter().map(|node_id| (*node_id).to_owned()).collect();
        error.diagnostics = uses.iter().map(|node_id| blocking("TOKEN_IN_USE", format!("token {token_id} is still bound to this node"), Some((*node_id).to_owned()))).collect();
        return Err(error);
    }
    doc.tokens.remove(token_id);
    Ok(vec![token_id.to_owned()])
}

fn set_mode(doc: &mut CanvasDocument, mode: TokenMode) -> DltResult<Vec<String>> {
    if token_mode(doc) == mode { return Ok(Vec::new()); }
    for binding in doc.bindings.iter().filter(|binding| binding.token.source == TokenSource::Local) {
        let definition = target_def(doc, &binding.token)?;
        preflight_binding_value(doc, binding, &definition.values, mode)?;
    }
    let token_ids: BTreeSet<&str> = doc.bindings.iter()
        .filter(|binding| binding.token.source == TokenSource::Local)
        .map(|binding| binding.token.token_id.as_str()).collect();
    let values_by_token: BTreeMap<String, TokenValues> = token_ids.into_iter().map(|token_id| {
        let values = doc.tokens.get(token_id).expect("preflight verified bound token").values.clone();
        (token_id.to_owned(), values)
    }).collect();
    let mut changed: BTreeSet<String> = doc.tokens.keys().cloned().collect();
    doc.token_mode = (mode == TokenMode::Dark).then_some(mode);
    let binding_count = doc.bindings.len();
    for index in 0..binding_count {
        if doc.bindings[index].token.source != TokenSource::Local { continue; }
        let (node_id, target, values) = {
            let binding = &doc.bindings[index];
            let values = values_by_token.get(binding.token.token_id.as_str()).expect("preflight verified bound token");
            (binding.node_id.clone(), binding.target.clone(), values)
        };
        apply_target_value(doc, &node_id, &target, values, mode, false)?;
        changed.insert(node_id);
    }
    Ok(changed.into_iter().collect())
}

fn bind(doc: &mut CanvasDocument, node_ids: &[String], target: &BindTarget, token: &TokenRef) -> DltResult<Vec<String>> {
    if node_ids.is_empty() { return Err(invalid_token("TOKEN_TARGETS", "no nodes were selected")); }
    if !valid_key(&token.token_id) { return Err(invalid_token("TOKEN_ID", "token id is invalid")); }
    let values = if token.source == TokenSource::Local { Some(target_def(doc, token)?.values.clone()) } else { None };
    if values.as_ref().is_some_and(|values| !target_matches_values(target, values)) {
        return Err(invalid_token("TOKEN_KIND_MISMATCH", format!("{} cannot use this token kind", target_label(target))));
    }
    let ids: BTreeSet<&str> = node_ids.iter().map(String::as_str).collect();
    let mode = token_mode(doc);
    for node_id in &ids {
        ops::ensure_node_unlocked_in_doc(doc, node_id)?;
        let node = model::find_node_in_doc(doc, node_id).ok_or_else(|| invalid_token("TOKEN_NODE_NOT_FOUND", format!("node {node_id} does not exist")))?;
        validate_target_shape(node, target)?;
        if let Some(values) = &values {
            let binding = TokenBinding { node_id: (*node_id).to_owned(), target: target.clone(), token: token.clone() };
            preflight_binding_value(doc, &binding, values, mode)?;
        }
    }
    super::components::preflight_source_binding_change(doc, node_ids, target, Some(token))?;
    for node_id in &ids {
        if let Some(values) = &values { apply_target_value(doc, node_id, target, values, mode, true)?; }
        doc.bindings.retain(|old| old.node_id.as_str() != *node_id || old.target != *target);
        doc.bindings.push(TokenBinding { node_id: (*node_id).to_owned(), target: target.clone(), token: token.clone() });
    }
    sort_bindings(doc);
    Ok(ids.into_iter().map(str::to_owned).collect())
}

pub(crate) fn sync_shared_bindings(doc: &mut CanvasDocument, tokens: &BTreeMap<String, TokenDef>) -> DltResult<Vec<String>> {
    preflight_shared_sync(doc, tokens)?;
    let mode = token_mode(doc);
    let mut plan: Vec<(String, BindTarget, &TokenValues, bool)> = Vec::new();
    for binding in doc.bindings.iter().filter(|binding| binding.token.source == TokenSource::Shared) {
        let definition = tokens.get(&binding.token.token_id)
            .ok_or_else(|| invalid_token("TOKEN_NOT_FOUND", format!("shared token {} does not exist", binding.token.token_id)))?;
        validate_definition(&binding.token.token_id, definition)?;
        if !target_matches_values(&binding.target, &definition.values) {
            return Err(invalid_token("TOKEN_KIND_MISMATCH", format!("{} cannot use this shared token kind", target_label(&binding.target))));
        }
        let node = model::find_node_in_doc(doc, &binding.node_id)
            .ok_or_else(|| invalid_token("TOKEN_NODE_NOT_FOUND", format!("node {} does not exist", binding.node_id)))?;
        validate_target_shape(node, &binding.target)?;
        let changed = !binding_matches_value(node, binding, &definition.values, mode);
        if changed { ops::ensure_node_unlocked_in_doc(doc, &binding.node_id)?; }
        plan.push((binding.node_id.clone(), binding.target.clone(), &definition.values, changed));
    }
    let mut changed_ids = BTreeSet::new();
    for (node_id, target, values, changed) in plan {
        if changed {
            apply_target_value(doc, &node_id, &target, &values, mode, false)?;
            changed_ids.insert(node_id);
        }
    }
    Ok(changed_ids.into_iter().collect())
}

pub(crate) fn unbind_shared_bindings(doc: &mut CanvasDocument) -> DltResult<Vec<String>> {
    preflight_shared_unbind(doc)?;
    let mut changed = BTreeSet::new();
    for binding in doc.bindings.iter().filter(|binding| binding.token.source == TokenSource::Shared) {
        model::find_node_in_doc(doc, &binding.node_id)
            .ok_or_else(|| invalid_token("TOKEN_NODE_NOT_FOUND", format!("node {} does not exist", binding.node_id)))?;
        ops::ensure_node_unlocked_in_doc(doc, &binding.node_id)?;
        changed.insert(binding.node_id.clone());
    }
    doc.bindings.retain(|binding| binding.token.source != TokenSource::Shared);
    sort_bindings(doc);
    Ok(changed.into_iter().collect())
}

fn unbind(doc: &mut CanvasDocument, node_ids: &[String], target: &BindTarget) -> DltResult<Vec<String>> {
    if node_ids.is_empty() { return Err(invalid_token("TOKEN_TARGETS", "no nodes were selected")); }
    let ids: BTreeSet<&str> = node_ids.iter().map(String::as_str).collect();
    for node_id in &ids {
        model::find_node_in_doc(doc, node_id).ok_or_else(|| invalid_token("TOKEN_NODE_NOT_FOUND", format!("node {node_id} does not exist")))?;
        ops::ensure_node_unlocked_in_doc(doc, node_id)?;
    }
    super::components::preflight_source_binding_change(doc, node_ids, target, None)?;
    let changed: Vec<String> = doc.bindings.iter()
        .filter(|binding| ids.contains(binding.node_id.as_str()) && binding.target == *target)
        .map(|binding| binding.node_id.clone())
        .collect();
    doc.bindings.retain(|binding| {
        let remove = ids.contains(binding.node_id.as_str()) && binding.target == *target;
        !remove
    });
    Ok(changed)
}

fn sort_bindings(doc: &mut CanvasDocument) {
    doc.bindings.sort_by(|left, right| left.node_id.cmp(&right.node_id).then_with(|| left.target.cmp(&right.target)));
}

pub(crate) fn unbind_style_fields(doc: &mut CanvasDocument, node_ids: &[String], patch: &StylePatch) -> Vec<String> {
    let nodes: BTreeSet<&str> = node_ids.iter().map(String::as_str).collect();
    let mut touched = patch.set_fields();
    touched.extend(patch.clear.iter().copied());
    let mut changed = BTreeSet::new();
    doc.bindings.retain(|binding| {
        let field = match &binding.target {
            BindTarget::StyleColor => Some(StyleField::Color), BindTarget::StyleFill => Some(StyleField::Fill),
            BindTarget::StyleStroke => Some(StyleField::Stroke), BindTarget::StyleFontFamily => Some(StyleField::FontFamily),
            BindTarget::StyleFontSize => Some(StyleField::FontSize), BindTarget::StyleCornerRadius => Some(StyleField::CornerRadius),
            _ => None,
        };
        let remove = nodes.contains(binding.node_id.as_str()) && field.is_some_and(|field| touched.contains(&field));
        if remove { changed.insert(binding.node_id.clone()); }
        !remove
    });
    changed.into_iter().collect()
}

pub(crate) fn unbind_layout_fields(doc: &mut CanvasDocument, node_id: &str) -> Vec<String> {
    let mut removed = false;
    doc.bindings.retain(|binding| {
        let is_layout = matches!(&binding.target,
            BindTarget::LayoutGap | BindTarget::LayoutRowGap | BindTarget::LayoutPadding | BindTarget::LayoutPaddingSide { .. }
            | BindTarget::BreakpointGap { .. } | BindTarget::BreakpointPadding { .. });
        let remove = binding.node_id == node_id && is_layout;
        removed |= remove;
        !remove
    });
    if removed { vec![node_id.to_owned()] } else { Vec::new() }
}

pub(crate) fn apply(doc: &mut CanvasDocument, operation: &Operation) -> DltResult<Vec<String>> {
    preflight_component_token_operation(doc, operation)?;
    match operation {
        Operation::PutToken { token_id, name, values } => put_token(doc, token_id, name, values),
        Operation::RemoveToken { token_id } => remove_token(doc, token_id),
        Operation::SetTokenMode { mode } => set_mode(doc, *mode),
        Operation::BindToken { node_ids, target, token } => bind(doc, node_ids, target, token),
        Operation::UnbindToken { node_ids, target } => unbind(doc, node_ids, target),
        other => Err(unsupported("TOKEN_OPERATION", format!("operation {} is not a token operation", other.name()))),
    }
}