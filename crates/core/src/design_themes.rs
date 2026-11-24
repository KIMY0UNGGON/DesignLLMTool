use std::collections::BTreeSet;

use designllm_contracts::design_themes::{DesignThemeEntry, DesignThemesApplyRequest, DesignThemesRecordRequest, DesignThemesRecordResult, DesignThemesSearchRequest, DesignThemesSearchResult};
use designllm_contracts::{ArtifactBody, BindTarget, CanvasDocument, ChangeSet, DltResult, JobRef, Node, NodeKind, Operation, Page, ThemePatch, TokenRef, TokenSource, TokenValues};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::errors::{integrity, invalid, sql, unsupported};

const PX: f64 = 9_525.0;

fn color_id(name: &str) -> String {
    let key = if name.len() <= 56 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
        format!("n-{name}")
    } else { format!("h-{}", &designllm_contracts::sha256(name.as_bytes())[..40]) };
    format!("color.{key}")
}

fn role_color<'a>(entry: &'a DesignThemeEntry, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| entry.colors.keys().find(|key| key.eq_ignore_ascii_case(name)).map(String::as_str))
}

fn put(doc: &CanvasDocument, ops: &mut Vec<Operation>, base: String, values: TokenValues) -> String {
    let mut id = base.clone();
    let mut suffix = 0u64;
    while doc.tokens.get(&id).is_some_and(|token| token.values != values &&
        (std::mem::discriminant(&token.values) != std::mem::discriminant(&values)
            || doc.bindings.iter().any(|binding| binding.token.source == TokenSource::Local && binding.token.token_id == id)))
        || ops.iter().any(|op| matches!(op, Operation::PutToken { token_id, .. } if token_id == &id)) {
        suffix += 1;
        id = format!("{}.{suffix}", &base[..base.len().min(48)]);
    }
    let name = doc.tokens.get(&id).map_or_else(|| id.clone(), |token| token.name.clone());
    ops.push(Operation::PutToken { name, token_id: id.clone(), values });
    id
}

fn bind(ops: &mut Vec<Operation>, node: &Node, target: BindTarget, id: String) {
    ops.push(Operation::BindToken { node_ids: vec![node.id.clone()], target, token: TokenRef { source: TokenSource::Local, token_id: id } });
}

fn has_text(node: &Node) -> bool {
    matches!(node.kind, NodeKind::Text) || (matches!(node.kind, NodeKind::Rectangle | NodeKind::Ellipse) && !node.text.trim().is_empty())
}

fn text_sizes(nodes: &[Node], sizes: &mut std::collections::BTreeMap<i64, usize>, colors: &mut std::collections::BTreeMap<String, usize>) {
    for node in nodes {
        if has_text(node) {
            *sizes.entry(node.style.font_size).or_default() += 1;
            *colors.entry(node.style.color.to_ascii_lowercase()).or_default() += 1;
        }
        text_sizes(&node.children, sizes, colors);
    }
}

fn bind_nodes(nodes: &[Node], page: &Page, origin: (i128, i128), body_size: i64, single: bool, dominant: &str, doc: &CanvasDocument, on_fill: bool, fills: &mut Vec<(i128, i128, i128, i128)>, text: Option<&str>, background: Option<&str>, fonts: &[String; 2], radii: &[(String, i64)], ops: &mut Vec<Operation>, notes: &mut Vec<Value>) {
    for node in nodes {
        let x = origin.0 + i128::from(node.bounds.x);
        let y = origin.1 + i128::from(node.bounds.y);
        let filled = matches!(node.kind, NodeKind::Rectangle | NodeKind::Ellipse | NodeKind::Path) && (node.style.gradient.is_some() || !matches!(node.style.fill.to_ascii_lowercase().as_str(), "none" | "transparent" | ""));
        if has_text(node) {
            let cx = x + i128::from(node.bounds.width) / 2;
            let cy = y + i128::from(node.bounds.height) / 2;
            let reason = if doc.bindings.iter().any(|b| b.node_id == node.id && b.target == BindTarget::StyleColor && (b.token.source != TokenSource::Local || Some(b.token.token_id.as_str()) != text)) { Some("bound_color") }
                else if on_fill || filled || fills.iter().any(|&(l, t, r, b)| cx >= l && cx <= r && cy >= t && cy <= b) { Some("filled_shape") }
                else if !node.style.color.eq_ignore_ascii_case(dominant) { Some("explicit_color") } else { None };
            if let Some(reason) = reason {
                notes.push(serde_json::json!({"kind":"skipped_text_color", "node_id":node.id, "reason":reason, "value":node.style.color}));
            } else if let Some(id) = text { bind(ops, node, BindTarget::StyleColor, id.into()); }
            else { notes.push(serde_json::json!({"kind":"skipped_text_color", "node_id":node.id, "reason":"no_text_color", "value":node.style.color})); }
            let heading = single || i128::from(node.style.font_size) * 10 >= i128::from(body_size) * 13;
            let font = if heading { &fonts[0] } else { &fonts[1] };
            if let Some(binding) = doc.bindings.iter().find(|b| b.node_id == node.id && b.target == BindTarget::StyleFontFamily && (b.token.source != TokenSource::Local || &b.token.token_id != font)) {
                notes.push(serde_json::json!({"kind":"skipped_font", "node_id":node.id, "reason":"bound_font", "value":node.style.font_family, "token":binding.token}));
            } else { bind(ops, node, BindTarget::StyleFontFamily, font.clone()); }
        }
        let mut is_background = false;
        if matches!(node.kind, NodeKind::Rectangle | NodeKind::Ellipse | NodeKind::Path) {
            let left = (origin.0 + i128::from(node.bounds.x)).max(0);
            let top = (origin.1 + i128::from(node.bounds.y)).max(0);
            let right = (origin.0 + i128::from(node.bounds.x) + i128::from(node.bounds.width)).min(i128::from(page.size.width));
            let bottom = (origin.1 + i128::from(node.bounds.y) + i128::from(node.bounds.height)).min(i128::from(page.size.height));
            let area = (right - left).max(0) * (bottom - top).max(0);
            let page_area = i128::from(page.size.width) * i128::from(page.size.height);
            if page_area > 0 && area * 5 >= page_area * 4 {
                is_background = true;
                if node.style.gradient.is_some() {
                    notes.push(serde_json::json!({"kind":"gradient_kept", "node_id":node.id, "value":node.style.gradient}));
                } else if let Some(id) = background {
                    if let Some(binding) = doc.bindings.iter().find(|b| b.node_id == node.id && b.target == BindTarget::StyleFill && (b.token.source != TokenSource::Local || b.token.token_id != id)) {
                        notes.push(serde_json::json!({"kind":"skipped_background", "node_id":node.id, "reason":"bound_fill", "value":node.style.fill, "token":binding.token}));
                    } else { bind(ops, node, BindTarget::StyleFill, id.into()); }
                }
            }
            if matches!(node.kind, NodeKind::Rectangle) {
                if let Some(radius) = node.style.corner_radius.filter(|r| *r > 0) {
                    if let Some((id, _)) = radii.iter().min_by_key(|(_, value)| (i128::from(*value) - i128::from(radius)).abs()) {
                        bind(ops, node, BindTarget::StyleCornerRadius, id.clone());
                    }
                }
            }
        }
        if filled && !is_background { fills.push((x, y, x + i128::from(node.bounds.width), y + i128::from(node.bounds.height))); }
        bind_nodes(&node.children, page, (x, y), body_size, single, dominant, doc, on_fill || (filled && !is_background), fills, text, background, fonts, radii, ops, notes);
    }
}

fn canvas_operations(entry: &DesignThemeEntry, doc: &CanvasDocument, notes: &mut Vec<Value>) -> DltResult<Vec<Operation>> {
    if entry.fonts.heading.len() > 128 || entry.fonts.body.len() > 128 { return Err(invalid("canvas font family exceeds 128 bytes")); }
    let mut ops = Vec::new();
    let colors: std::collections::BTreeMap<_, _> = entry.colors.iter().map(|(name, color)| {
        (name.as_str(), put(doc, &mut ops, color_id(name), TokenValues::Color { light: color.clone(), dark: color.clone() }))
    }).collect();
    let fonts = [("heading", &entry.fonts.heading), ("body", &entry.fonts.body)].map(|(name, face)| {
        put(doc, &mut ops, format!("font.{name}"), TokenValues::FontFamily { light: face.clone(), dark: face.clone() })
    });
    for (index, value) in entry.type_scale_px.iter().enumerate() {
        let emu = (value * PX).round() as i64;
        if (1..=12_700_000).contains(&emu) { put(doc, &mut ops, format!("size.{index}"), TokenValues::FontSize { light: emu, dark: emu }); }
        else { notes.push(serde_json::json!({"kind":"skipped_type_size", "value_px":value, "reason":"out_of_range"})); }
    }
    let radii: Vec<_> = entry.radius_px.iter().enumerate().map(|(index, n)| {
        let emu = (n * PX).round() as i64;
        (put(doc, &mut ops, format!("radius.{index}"), TokenValues::Radius { light: emu, dark: emu }), emu)
    }).collect();
    for (index, value) in entry.spacing_px.iter().enumerate() {
        let emu = (value * PX).round() as i64;
        put(doc, &mut ops, format!("spacing.{index}"), TokenValues::Spacing { light: emu, dark: emu });
    }
    let background_name = role_color(entry, &["background", "canvas", "surface", "bg"]);
    let text_name = role_color(entry, &["text", "ink", "foreground", "on-background"])
        .or_else(|| colors.keys().copied().find(|name| Some(*name) != background_name));
    let text = text_name.and_then(|name| colors.get(name)).map(String::as_str);
    let background = background_name.and_then(|name| colors.get(name)).map(String::as_str);
    let stored_sizes: Vec<_> = entry.type_scale_px.iter().copied().filter(|value| (1..=12_700_000).contains(&((value * PX).round() as i64))).collect();
    notes.push(serde_json::json!({"kind":"stored_steps", "type_scale_px":stored_sizes, "spacing_px":entry.spacing_px, "binding":"manual"}));
    notes.push(serde_json::json!({"kind":"applied_theme", "colors":entry.colors, "fonts":entry.fonts, "radius_px":entry.radius_px}));
    let mut colors_used = std::collections::BTreeMap::new();
    let mut all_sizes = std::collections::BTreeMap::new();
    for page in &doc.pages { text_sizes(&page.nodes, &mut all_sizes, &mut colors_used); }
    let dominant = colors_used.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0))).map_or(String::new(), |(color, _)| color);
    for page in &doc.pages {
        let mut sizes = std::collections::BTreeMap::new();
        text_sizes(&page.nodes, &mut sizes, &mut std::collections::BTreeMap::new());
        let single = sizes.values().sum::<usize>() == 1;
        let body_size = sizes.keys().next().copied().unwrap_or(0);
        bind_nodes(&page.nodes, page, (0, 0), body_size, single, &dominant, doc, false, &mut Vec::new(), text, background, &fonts, &radii, &mut ops, notes);
    }
    for reason in ["bound_color", "filled_shape", "explicit_color", "no_text_color"] {
        let count = notes.iter().filter(|n| n["kind"] == "skipped_text_color" && n["reason"] == reason).count();
        if count > 0 { notes.push(serde_json::json!({"kind":"skipped_text_color_count", "reason":reason, "count":count})); }
    }
    for (kind, count_kind) in [("skipped_font", "skipped_font_count"), ("skipped_background", "skipped_background_count")] {
        let count = notes.iter().filter(|n| n["kind"] == kind).count();
        if count > 0 { notes.push(serde_json::json!({"kind":count_kind, "count":count})); }
    }
    Ok(ops)
}

fn slides_operations(entry: &DesignThemeEntry) -> DltResult<Vec<Operation>> {
    let mut patch = ThemePatch { major_latin: Some(entry.fonts.heading.clone()), minor_latin: Some(entry.fonts.body.clone()), major_ea: Some(entry.fonts.heading.clone()), minor_ea: Some(entry.fonts.body.clone()), ..Default::default() };
    let mut remaining = Vec::new();
    for (name, color) in &entry.colors {
        let slot = match name.to_ascii_lowercase().as_str() {
            "background" | "canvas" => Some("lt1"), "text" | "ink" => Some("dk1"),
            "primary" => Some("accent1"), "secondary" => Some("accent2"),
            _ => designllm_contracts::THEME_COLOR_SLOTS.iter().copied().find(|slot| slot.eq_ignore_ascii_case(name)),
        };
        if let Some(slot) = slot.filter(|slot| !patch.colors.contains_key(*slot)) { patch.colors.insert(slot.to_owned(), color.clone()); }
        else { remaining.push(color); }
    }
    for color in remaining {
        let slot = ["accent1", "accent2", "accent3", "accent4", "accent5", "accent6"].into_iter().chain(designllm_contracts::THEME_COLOR_SLOTS)
            .find(|slot| !patch.colors.contains_key(*slot)).ok_or_else(|| invalid("slides themes support at most 12 colors"))?;
        patch.colors.insert(slot.to_owned(), color.clone());
    }
    Ok(vec![Operation::SetTheme { patch }])
}

impl crate::Workspace {
    pub(crate) fn apply_design_theme(&mut self, params: Value, bound: Option<&JobRef>) -> DltResult<Value> {
        crate::receipts::reject_duplicate_set(&params, "allowed_intents")?;
        let mut request: DesignThemesApplyRequest = crate::parse(params)?;
        if request.workspace_id != self.id { return Err(invalid("workspace_id does not match this workspace")); }
        if request.artifact_id.trim().is_empty() || request.allowed_intents.is_empty() { return Err(invalid("artifact_id and allowed_intents are required")); }
        let job = crate::pipeline::effective_job(bound, request.job_ref.as_ref(), self.caller.as_deref())?;
        request.job_ref = job.clone();
        let mut keyed = crate::to_value(&request)?;
        if let Some(deps) = keyed.get_mut("read_dependencies").and_then(Value::as_array_mut) {
            let mut sorted: Vec<_> = deps.drain(..).map(|dep| designllm_contracts::canonical_json(&dep).map(|key| (key, dep))).collect::<DltResult<_>>()?;
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            if sorted.windows(2).any(|pair| pair[0].0 == pair[1].0) { return Err(invalid("read_dependencies contains duplicates")); }
            *deps = sorted.into_iter().map(|(_, dep)| dep).collect();
        }
        let key = crate::receipts::key_for("design_themes.apply", &request.request_id, &keyed)?;
        if let Some(found) = crate::store::find_receipt(&self.conn, &key)? { return found; }
        let (_, _, artifact, _) = self.load_revision(&request.artifact_id, Some(request.base_revision))?;
        let stored: Option<String> = self.conn.query_row("SELECT entry_json FROM design_themes WHERE id = ?1", [&request.theme_id], |row| row.get(0)).optional().map_err(sql)?;
        let stored = stored.ok_or_else(|| crate::errors::not_found("design theme not found"))?;
        let entry: DesignThemeEntry = serde_json::from_str(&stored).map_err(|e| integrity(format!("invalid stored design theme: {e}")))?;
        validate(&entry).map_err(|_| integrity("invalid stored design theme"))?;
        let mut notes = Vec::new();
        let operations = match &artifact.body {
            ArtifactBody::Canvas(doc) => canvas_operations(&entry, doc, &mut notes)?,
            ArtifactBody::Slides(_) => {
                notes.push(serde_json::json!({"kind":"applied_theme", "colors":entry.colors, "fonts":entry.fonts}));
                notes.push(serde_json::json!({"kind":"skipped_steps", "reason":"slides_unsupported", "type_scale_px":entry.type_scale_px, "radius_px":entry.radius_px, "spacing_px":entry.spacing_px}));
                slides_operations(&entry)?
            },
            _ => return Err(unsupported("design themes apply only to Canvas and Slides")),
        };
        let change = ChangeSet { request_id: if request.dry_run { designllm_contracts::new_id() } else { request.request_id }, actor: request.actor, workspace_id: request.workspace_id, artifact_id: request.artifact_id,
            base_revision: request.base_revision, allowed_intents: request.allowed_intents, job_ref: request.job_ref, read_dependencies: request.read_dependencies, operations };
        self.check_change_shape(&change, "")?;
        crate::receipts::change_key("design_themes.apply", &change, job.as_ref())?;
        if request.dry_run {
            self.validate_changes(crate::to_value(&change)?, job.as_ref())?;
            let result = serde_json::json!({"operations":&change.operations, "change_set":&change, "notes":notes});
            crate::store::insert_receipt(&self.conn, &key, Some(&change.artifact_id), &crate::store::StoredResult::Committed { result: result.clone() })?;
            return Ok(result);
        }
        self.apply_keyed_with_notes(&change, key.clone(), job, None, Some(notes))?;
        crate::store::find_receipt(&self.conn, &key)?.ok_or_else(|| integrity("committed design theme receipt is missing"))?
    }
}

fn validate(entry: &DesignThemeEntry) -> DltResult<()> {
    if entry.is_valid() { Ok(()) } else { Err(invalid("invalid design theme entry")) }
}

fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).map(str::to_lowercase).collect()
}

// Score is the number of distinct query words present in either the tags or the summary.
fn score(entry: &DesignThemeEntry, query: &BTreeSet<String>) -> usize {
    let mut words = tokens(&entry.summary);
    for tag in &entry.tags { words.extend(tokens(tag)); }
    query.intersection(&words).count()
}

pub(crate) fn search(conn: &Connection, request: DesignThemesSearchRequest) -> DltResult<Value> {
    if request.limit == 0 || request.limit > 20 || request.query.chars().count() > 300 || request.query.chars().any(char::is_control) {
        return Err(invalid("invalid design theme search"));
    }
    let query = tokens(&request.query);
    let mut stmt = conn.prepare("SELECT entry_json FROM design_themes").map_err(sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(sql)?;
    let mut matches = Vec::new();
    for row in rows {
        let entry: DesignThemeEntry = serde_json::from_str(&row.map_err(sql)?).map_err(|e| integrity(format!("invalid stored design theme: {e}")))?;
        validate(&entry).map_err(|_| integrity("invalid stored design theme"))?;
        let rank = score(&entry, &query);
        if query.is_empty() || rank > 0 { matches.push((rank, entry)); }
    }
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.recorded_at.cmp(&a.1.recorded_at)).then_with(|| a.1.id.cmp(&b.1.id)));
    crate::to_value(&DesignThemesSearchResult { entries: matches.into_iter().take(request.limit as usize).map(|(_, entry)| entry).collect() })
}

pub(crate) fn record(conn: &mut Connection, request: DesignThemesRecordRequest) -> DltResult<Value> {
    let entry = request.entry;
    validate(&entry)?;
    let json = serde_json::to_string(&entry).map_err(|e| invalid(format!("invalid design theme: {e}")))?;
    let tx = conn.transaction().map_err(sql)?;
    tx.execute("INSERT INTO design_themes(id, recorded_at, entry_json) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET recorded_at=excluded.recorded_at, entry_json=excluded.entry_json", params![entry.id, entry.recorded_at as i64, json]).map_err(sql)?;
    tx.execute("DELETE FROM design_themes WHERE id IN (SELECT id FROM design_themes ORDER BY recorded_at DESC, id ASC LIMIT -1 OFFSET 500)", []).map_err(sql)?;
    tx.commit().map_err(sql)?;
    crate::to_value(&DesignThemesRecordResult { entry })
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::design_themes::DesignThemeFonts;
    use std::collections::BTreeMap;

    fn entry(id: &str, at: u64) -> DesignThemeEntry {
        DesignThemeEntry { id: id.into(), tags: vec!["calm blue".into()], summary: "quiet space".into(), colors: BTreeMap::from([("primary".into(), "#123ABC".into())]), fonts: DesignThemeFonts { heading: "A".into(), body: "B".into() }, type_scale_px: vec![16.0], radius_px: vec![], spacing_px: vec![], recorded_at: at }
    }

    #[test]
    fn theme_operations_keep_valid_steps_and_refuse_excess_slide_colors() {
        let mut theme = entry("theme", 1);
        theme.colors.insert("색".repeat(60), "#ABCDEF".into());
        theme.type_scale_px = vec![0.0, 16.0, 10_000.0];
        theme.radius_px = vec![4.0, 8.0];
        theme.spacing_px = vec![12.0];
        let doc: CanvasDocument = serde_json::from_value(serde_json::json!({"pages":[], "styles":{}})).unwrap();
        let ops = canvas_operations(&theme, &doc, &mut Vec::new()).unwrap();
        let ids: Vec<&str> = ops.iter().filter_map(|op| match op { Operation::PutToken { token_id, name, .. } => {
            assert!(token_id.len() <= 64 && name.len() <= 64); Some(token_id.as_str())
        }, _ => None }).collect();
        assert!(ids.contains(&"size.1") && !ids.contains(&"size.0") && !ids.contains(&"size.2"));
        assert!(ids.contains(&"radius.0") && ids.contains(&"spacing.0"));
        let mut occupied = doc;
        occupied.tokens.insert("size.1".into(), designllm_contracts::TokenDef { name: "Existing".into(), values: TokenValues::FontSize { light: 100_000, dark: 100_000 } });
        let ops = canvas_operations(&theme, &occupied, &mut Vec::new()).unwrap();
        assert!(ops.iter().any(|op| matches!(op, Operation::PutToken { token_id, name, values: TokenValues::FontSize { light: 152400, .. } } if token_id == "size.1" && name == "Existing")));
        occupied.tokens.get_mut("size.1").unwrap().values = TokenValues::FontSize { light: 152400, dark: 152400 };
        let ops = canvas_operations(&theme, &occupied, &mut Vec::new()).unwrap();
        assert!(ops.iter().any(|op| matches!(op, Operation::PutToken { token_id, name, .. } if token_id == "size.1" && name == "Existing")));
        assert!(matches!(&slides_operations(&theme).unwrap()[0], Operation::SetTheme { patch } if patch.colors.len() == 2));
        theme.colors = (0..13).map(|n| (format!("color-{n}"), "#123456".into())).collect();
        assert!(slides_operations(&theme).unwrap_err().message.contains("12"));
    }

    #[test]
    fn a_text_only_color_does_not_become_the_background() {
        let mut theme = entry("theme", 1);
        theme.colors = BTreeMap::from([("text".into(), "#123456".into())]);
        let doc: CanvasDocument = serde_json::from_value(serde_json::json!({"pages":[{"id":"p", "size":{"width":1000,"height":1000}, "nodes":[
            {"id":"bg","kind":"rectangle","bounds":{"x":0,"y":0,"width":1000,"height":1000}},
            {"id":"text","kind":"text","bounds":{"x":0,"y":0,"width":100,"height":100},"text":"body"}
        ]}], "styles":{}})).unwrap();
        let ops = canvas_operations(&theme, &doc, &mut Vec::new()).unwrap();
        assert!(ops.iter().any(|op| matches!(op, Operation::BindToken { target: BindTarget::StyleColor, .. })));
        assert!(!ops.iter().any(|op| matches!(op, Operation::BindToken { target: BindTarget::StyleFill, .. })));
    }

    #[test]
    fn protects_text_and_gradients_and_binds_shape_fonts() {
        let mut theme = entry("theme", 1);
        theme.colors = BTreeMap::from([("text".into(), "#123456".into()), ("background".into(), "#FFFFFF".into())]);
        theme.type_scale_px = vec![0.0, 16.0];
        theme.spacing_px = vec![8.0, 16.0];
        let doc: CanvasDocument = serde_json::from_value(serde_json::json!({"pages":[
            {"id":"p", "size":{"width":1000,"height":1000}, "nodes":[
                {"id":"bg","kind":"rectangle","bounds":{"x":0,"y":0,"width":1000,"height":1000},
                 "style":{"gradient":{"angle_millideg":0,"stops":[{"pos_milli":0,"color":"#000000"},{"pos_milli":1000,"color":"#FFFFFF"}]}}},
                {"id":"body1","kind":"text","text":"body","bounds":{"x":0,"y":0,"width":100,"height":100},"style":{"color":"#111111","font_size":100}},
                {"id":"body2","kind":"text","text":"body","bounds":{"x":0,"y":120,"width":100,"height":100},"style":{"color":"#111111","font_size":100}},
                {"id":"custom","kind":"text","text":"custom","bounds":{"x":0,"y":240,"width":100,"height":100},"style":{"color":"#FF0000","font_size":100}},
                {"id":"bound","kind":"text","text":"bound","bounds":{"x":0,"y":360,"width":100,"height":100},"style":{"color":"#111111","font_size":100}},
                {"id":"shape","kind":"rectangle","text":"button","bounds":{"x":200,"y":0,"width":200,"height":100},"style":{"color":"#111111","fill":"#333333","font_size":130}},
                {"id":"over","kind":"text","text":"overlay","bounds":{"x":220,"y":10,"width":100,"height":50},"style":{"color":"#111111","font_size":100}},
                {"id":"ellipse","kind":"ellipse","text":"label","bounds":{"x":450,"y":0,"width":100,"height":100},"style":{"font_size":100}}
            ]},
            {"id":"title-page", "size":{"width":1000,"height":1000},"nodes":[
                {"id":"title","kind":"text","text":"title","bounds":{"x":0,"y":0,"width":100,"height":100},"style":{"font_size":304800}}
            ]}
        ],"styles":{},"bindings":[{"node_id":"bound","target":{"kind":"style_color"},"token":{"source":"local","token_id":"custom.color"}}]})).unwrap();
        let mut notes = Vec::new();
        let ops = canvas_operations(&theme, &doc, &mut notes).unwrap();
        let bound = |id: &str, target: BindTarget, token: &str| ops.iter().any(|op| matches!(op, Operation::BindToken { node_ids, target: t, token: r } if node_ids.iter().any(|node_id| node_id == id) && *t == target && r.token_id == token));
        assert!(bound("body1", BindTarget::StyleColor, "color.n-text"));
        for id in ["custom", "bound", "shape", "over", "ellipse"] {
            assert!(!bound(id, BindTarget::StyleColor, "color.n-text"), "{id}");
        }
        assert!(bound("shape", BindTarget::StyleFontFamily, "font.heading"));
        assert!(bound("ellipse", BindTarget::StyleFontFamily, "font.body"));
        assert!(bound("title", BindTarget::StyleFontFamily, "font.heading"));
        assert!(!bound("bg", BindTarget::StyleFill, "color.n-background"));
        for (id, reason) in [("custom", "explicit_color"), ("bound", "bound_color"), ("shape", "filled_shape"), ("over", "filled_shape")] {
            assert!(notes.iter().any(|n| n["node_id"] == id && n["reason"] == reason), "{id}");
        }
        assert!(notes.iter().any(|n| n["kind"] == "gradient_kept" && n["node_id"] == "bg"));
        assert!(notes.iter().any(|n| n["kind"] == "stored_steps" && n["type_scale_px"] == serde_json::json!([16.0]) && n["spacing_px"] == serde_json::json!([8.0, 16.0])));
        assert!(notes.iter().any(|n| n["kind"] == "skipped_type_size" && n["value_px"] == 0.0));
        assert!(!ops.iter().any(|op| matches!(op, Operation::BindToken { target: BindTarget::StyleFontSize | BindTarget::LayoutGap, .. })));
    }

    #[test]
    fn keeps_other_font_and_background_bindings_and_reports_counts() {
        let mut theme = entry("theme", 1);
        theme.colors.insert("background".into(), "#FFFFFF".into());
        let doc: CanvasDocument = serde_json::from_value(serde_json::json!({"pages":[
            {"id":"p", "size":{"width":1000,"height":1000}, "nodes":[
                {"id":"bg","kind":"rectangle","bounds":{"x":0,"y":0,"width":1000,"height":1000}},
                {"id":"other-bg","kind":"rectangle","bounds":{"x":0,"y":0,"width":1000,"height":1000}},
                {"id":"text","kind":"text","text":"body","bounds":{"x":0,"y":0,"width":100,"height":100},"style":{"font_size":100}},
                {"id":"title","kind":"text","text":"title","bounds":{"x":0,"y":200,"width":100,"height":100},"style":{"font_size":200}}
            ]}],"styles":{},"bindings":[
                {"node_id":"bg","target":{"kind":"style_fill"},"token":{"source":"shared","token_id":"color.n-background"}},
                {"node_id":"other-bg","target":{"kind":"style_fill"},"token":{"source":"local","token_id":"color.n-background"}},
                {"node_id":"text","target":{"kind":"style_font_family"},"token":{"source":"local","token_id":"custom.font"}},
                {"node_id":"title","target":{"kind":"style_font_family"},"token":{"source":"local","token_id":"font.heading"}}
            ]})).unwrap();
        let mut notes = Vec::new();
        let ops = canvas_operations(&theme, &doc, &mut notes).unwrap();
        let binds = |id: &str, target: BindTarget| ops.iter().any(|op| matches!(op, Operation::BindToken { node_ids, target: t, .. } if node_ids.iter().any(|n| n == id) && *t == target));
        assert!(!binds("text", BindTarget::StyleFontFamily));
        assert!(binds("title", BindTarget::StyleFontFamily));
        assert!(!binds("bg", BindTarget::StyleFill));
        assert!(binds("other-bg", BindTarget::StyleFill));
        assert!(notes.iter().any(|n| n["kind"] == "skipped_font" && n["node_id"] == "text" && n["token"]["token_id"] == "custom.font"));
        assert!(notes.iter().any(|n| n["kind"] == "skipped_background" && n["node_id"] == "bg" && n["token"]["source"] == "shared"));
        assert!(notes.iter().any(|n| n["kind"] == "skipped_font_count" && n["count"] == 1));
        assert!(notes.iter().any(|n| n["kind"] == "skipped_background_count" && n["count"] == 1));
    }

    #[test]
    fn stores_upserts_ranks_and_evicts() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE design_themes(id TEXT PRIMARY KEY, recorded_at INTEGER NOT NULL, entry_json TEXT NOT NULL) STRICT;").unwrap();
        for n in 0..501 { record(&mut conn, DesignThemesRecordRequest { entry: entry(&format!("theme-{n:03}"), n) }).unwrap(); }
        assert_eq!(conn.query_row("SELECT count(*) FROM design_themes", [], |r| r.get::<_, u32>(0)).unwrap(), 500);
        let mut changed = entry("theme-500", 600);
        changed.tags.push("sunny".into());
        record(&mut conn, DesignThemesRecordRequest { entry: changed }).unwrap();
        let found = search(&conn, DesignThemesSearchRequest { query: "blue sunny".into(), limit: 2 }).unwrap();
        assert_eq!(found["entries"][0]["id"], "theme-500");
        assert_eq!(found["entries"].as_array().unwrap().len(), 2);
        assert_eq!(conn.query_row("SELECT count(*) FROM design_themes WHERE id='theme-000'", [], |r| r.get::<_, u32>(0)).unwrap(), 0);
        assert_eq!(search(&conn, DesignThemesSearchRequest { query: "missing".into(), limit: 20 }).unwrap()["entries"].as_array().unwrap().len(), 0);
        assert!(record(&mut conn, DesignThemesRecordRequest { entry: entry("INVALID", 1) }).is_err());
        let mut bad = entry("bad", 1); bad.colors.insert("primary".into(), "#xyzxyz".into());
        assert!(record(&mut conn, DesignThemesRecordRequest { entry: bad }).is_err());
        let mut bad = entry("bad", 1); bad.fonts.heading = "A\nB".into();
        assert!(record(&mut conn, DesignThemesRecordRequest { entry: bad }).is_err());
        let mut bad = entry("bad", 1); bad.tags = vec!["tag".into(); 17];
        assert!(record(&mut conn, DesignThemesRecordRequest { entry: bad }).is_err());
        let mut bad = entry("bad", 1); bad.spacing_px = vec![-1.0];
        assert!(record(&mut conn, DesignThemesRecordRequest { entry: bad }).is_err());
        assert!(search(&conn, DesignThemesSearchRequest { query: String::new(), limit: 21 }).is_err());
        assert_eq!(conn.query_row("SELECT count(*) FROM design_themes", [], |r| r.get::<_, u32>(0)).unwrap(), 500);
        let mut input = serde_json::to_value(DesignThemesRecordRequest { entry: entry("valid", 1) }).unwrap();
        input["entry"]["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<DesignThemesRecordRequest>(input).is_err());
    }
}
