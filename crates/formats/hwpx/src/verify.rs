//! Verification of a saved package. The output is reopened through the same reader as open, and
//! its parts, opaque regions, model, header, manifest, caches, preview parts, unmodeled markup, and
//! figure links are compared with the source and with the target model. A failed check is a
//! blocking diagnostic.
use std::collections::{BTreeMap, BTreeSet, HashSet};

use designllm_contracts::{
    AssetStore, Diagnostic, EMU_PER_HWPUNIT, FigureLink, FlowBlock, FlowDocument, Node, PartFingerprint, PreservationVerification,
};
use designllm_package::parts::{compare_parts, PartState};
use designllm_package::Package;
use serde_json::Value;

use crate::diag;
use crate::doc::{self, Loaded, Region};
use crate::header::{child_els, is_el, paragraph_font_size, NativeLine};
use crate::ops::{all_blocks, new_host_line, paragraph_line, NativeLines};
use crate::package::{self, Role, HEADER, PREVIEW_IMAGE, PREVIEW_TEXT};
use crate::section::{cache_keys, PBlock, PImage, PSection};
use crate::writer::{format_eq, preview_text};
use crate::xmlx::{declared_namespace_attributes, Xml, NS_HH};

fn strip_ids(value: &mut Value, canonical_single: bool) {
    match value {
        Value::Object(map) => {
            map.remove("id");
            if canonical_single && map.get("line_spacing_percent").and_then(Value::as_u64) == Some(100) {
                map.remove("line_spacing_percent");
            }
            for v in map.values_mut() {
                strip_ids(v, canonical_single);
            }
        }
        Value::Array(items) => {
            for v in items {
                strip_ids(v, canonical_single);
            }
        }
        _ => {}
    }
}

/// Semantic form of a document: every field except identifiers.
fn normalize(doc: &FlowDocument, canonical_single: bool) -> Value {
    let mut value = serde_json::to_value(doc).unwrap_or(Value::Null);
    strip_ids(&mut value, canonical_single);
    value
}

/// Path of the first difference between two normalized values, for diagnostics.
fn first_difference(a: &Value, b: &Value, path: &str) -> Option<String> {
    compare_value(a, b, path, false)
}

fn compare_value(a: &Value, b: &Value, path: &str, paragraph: bool) -> Option<String> {
    if a == b {
        return None;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                match (x.get(k), y.get(k)) {
                    (Some(p), Some(q)) => {
                        if paragraph && k == "runs" && same_runs_after_omitting_empty(p, q) {
                            continue;
                        }
                        let paragraph_value = k == "value" && x.get("type").and_then(Value::as_str) == Some("paragraph")
                            && x.get("type") == y.get("type");
                        if let Some(found) = compare_value(p, q, &format!("{path}/{k}"), paragraph_value) {
                            return Some(found);
                        }
                    }
                    _ => return Some(format!("{path}/{k}")),
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!("{path} (length {} vs {})", x.len(), y.len()));
            }
            x.iter().zip(y).enumerate().find_map(|(i, (p, q))| first_difference(p, q, &format!("{path}[{i}]")))
        }
        _ => Some(path.to_string()),
    }
}

fn same_runs_after_omitting_empty(a: &Value, b: &Value) -> bool {
    let (Some(x), Some(y)) = (a.as_array(), b.as_array()) else { return false };
    x.iter().chain(y).any(|r| r["text"] == "" && r["protected"] == false)
        && visible_runs(x).is_some_and(|runs| Some(runs) == visible_runs(y))
}

// With an empty run omitted, neighboring text of the same style may reopen as one run.
fn visible_runs(runs: &[Value]) -> Option<Vec<(String, &Value, bool)>> {
    let mut visible: Vec<(String, &Value, bool)> = Vec::new();
    for run in runs {
        let text = run.get("text")?.as_str()?;
        let style = run.get("style")?;
        let protected = run.get("protected")?.as_bool()?;
        if !protected && text.is_empty() {
            continue;
        }
        match visible.last_mut() {
            Some((prefix, old_style, false)) if !protected && *old_style == style => prefix.push_str(text),
            _ => visible.push((text.to_string(), style, protected)),
        }
    }
    Some(visible)
}

fn region_map(regions: &[Region]) -> BTreeMap<(String, String), String> {
    regions.iter().map(|r| ((r.part.clone(), r.anchor.clone()), r.sha256.clone())).collect()
}

/// Anchor without its occurrence suffix (`…#k` at the end).
pub(crate) fn base_anchor(anchor: &str) -> &str {
    match anchor.rfind('#') {
        Some(i) if anchor[i + 1..].bytes().all(|b| b.is_ascii_digit()) && i + 1 < anchor.len() => &anchor[..i],
        _ => anchor,
    }
}

fn block_id(block: &FlowBlock) -> &str {
    match block {
        FlowBlock::Paragraph(p) => &p.id,
        FlowBlock::Table(t) => &t.id,
        FlowBlock::Image(n) => &n.id,
        FlowBlock::Opaque { id, .. } => id,
    }
}

/// Expected line-segment cache removal, derived from the source model and the target model.
/// Mirrors the writer's policy: an edited block and every block after it lose their caches; a
/// style change or a page change affects the whole document.
fn expected_cache_removals(orig: &Loaded, target: &FlowDocument, lines: &NativeLines) -> Vec<Vec<bool>> {
    let orig_styles_changed = orig.flow.styles != target.styles;
    let mut out = Vec::new();
    let mut downstream = false;
    for (osec, tsec) in orig.sections.iter().zip(&target.sections) {
        let mut first: Option<usize> = None;
        let page_changed = {
            let a = &osec.setup;
            let b = &tsec.setup;
            a.size != b.size
                || a.margin_left != b.margin_left
                || a.margin_top != b.margin_top
                || a.margin_right != b.margin_right
                || a.margin_bottom != b.margin_bottom
        };
        if page_changed {
            first = Some(0);
        }
        let mut last_orig: Option<usize> = None;
        for tb in &tsec.blocks {
            let tid = block_id(tb);
            match osec.blocks.iter().position(|b| b.id() == tid) {
                Some(oi) => {
                    last_orig = Some(oi);
                    let same = normalize_block(&osec.blocks[oi], tb) && !has_line_override(&osec.blocks[oi], lines);
                    if !same {
                        first = Some(first.map_or(oi, |f| f.min(oi)));
                    }
                }
                None => {
                    let boundary = last_orig.unwrap_or(0);
                    first = Some(first.map_or(boundary, |f| f.min(boundary)));
                }
            }
        }
        // A block that the target drops moves the caches of every block after it.
        let target_ids: HashSet<&str> = tsec.blocks.iter().map(block_id).collect();
        for (oi, block) in osec.blocks.iter().enumerate() {
            if !target_ids.contains(block.id()) {
                first = Some(first.map_or(oi, |f| f.min(oi)));
            }
        }
        if orig_styles_changed || downstream {
            first = Some(0);
        }
        if first.is_some() {
            downstream = true;
        }
        let flags: Vec<bool> = (0..osec.blocks.len()).map(|i| first.map(|f| i >= f).unwrap_or(false)).collect();
        out.push(flags);
    }
    out
}

fn has_line_override(block: &PBlock, lines: &NativeLines) -> bool {
    match block {
        PBlock::Para(p) => lines.get(&p.id).is_some_and(|l| !l.approximated()),
        PBlock::Table(t) => t.rows.iter().flatten().flat_map(|c| &c.blocks).any(|b| has_line_override(b, lines)),
        _ => false,
    }
}

/// Compares an original block with its target counterpart in the normalized form.
fn normalize_block(orig: &PBlock, target: &FlowBlock) -> bool {
    let a = crate::section::flow_block(orig);
    let va = normalize_flow_block(&a);
    let vb = normalize_flow_block(target);
    first_difference(&va, &vb, "").is_none()
}

fn normalize_flow_block(block: &FlowBlock) -> Value {
    let mut v = serde_json::to_value(block).unwrap_or(Value::Null);
    strip_ids(&mut v, false);
    v
}

fn bin_referenced(out: &Loaded, name: &str) -> bool {
    out.manifest.items.iter().any(|i| i.href == name)
}

/// The picture with this id, top level or inside a table cell, of the source model.
fn picture_in<'a>(blocks: &'a [PBlock], id: &str) -> Option<&'a PImage> {
    for block in blocks {
        match block {
            PBlock::Image(image) if image.id == id => return Some(image),
            PBlock::Table(table) => {
                for cell in table.rows.iter().flatten() {
                    if let Some(found) = picture_in(&cell.blocks, id) {
                        return Some(found);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Section part name and picture of the source model with this id.
fn find_picture<'a>(sections: &'a [PSection], id: &str) -> Option<(&'a str, &'a PImage)> {
    sections.iter().find_map(|section| picture_in(&section.blocks, id).map(|picture| (section.part.as_str(), picture)))
}

/// The picture with this id in the target model, top level or inside a table cell.
fn target_picture<'a>(blocks: &'a [FlowBlock], id: &str) -> Option<&'a Node> {
    for block in blocks {
        match block {
            FlowBlock::Image(node) if node.id == id => return Some(node),
            FlowBlock::Table(table) => {
                for cell in table.rows.iter().flatten() {
                    if let Some(found) = target_picture(&cell.blocks, id) {
                        return Some(found);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn target_picture_in_doc<'a>(doc: &'a FlowDocument, id: &str) -> Option<&'a Node> {
    doc.sections.iter().find_map(|section| target_picture(&section.blocks, id))
}

/// Anchors that a verified save changes on purpose: the protected children of a picture whose image a
/// figure update replaced, and every element under a paragraph that the change removed.
struct Exemptions {
    hosts: Vec<String>,
    children: HashSet<(String, String)>,
}

impl Exemptions {
    fn new(orig: &Loaded, target: &FlowDocument, figure_links: &[FigureLink]) -> Self {
        let mut hosts = Vec::new();
        for (osec, tsec) in orig.sections.iter().zip(&target.sections) {
            let target_ids: HashSet<&str> = tsec.blocks.iter().map(block_id).collect();
            for block in &osec.blocks {
                if !target_ids.contains(block.id()) {
                    hosts.push(block.host_key().to_string());
                }
            }
        }
        let mut children = HashSet::new();
        for link in figure_links {
            let Some((part, picture)) = find_picture(&orig.sections, &link.target_node_id) else { continue };
            let target_sha = target_picture_in_doc(target, &link.target_node_id).and_then(|n| n.asset.as_ref()).map(|a| a.sha256.clone());
            if target_sha.as_deref() != Some(picture.asset.sha256.as_str()) {
                if let Some(keys) = &picture.figure_keys {
                    for key in keys {
                        children.insert((part.to_string(), key.clone()));
                    }
                }
            }
        }
        Exemptions { hosts, children }
    }

    fn covers(&self, part: &str, anchor: &str) -> bool {
        self.children.contains(&(part.to_string(), anchor.to_string()))
            || self.hosts.iter().any(|host| anchor_matches_prefix(anchor, host))
    }
}

/// Owner of unmodeled markup: the nearest element with its own anchor. Runs and text elements
/// (`~n` keys) have no anchor of their own and belong to their paragraph.
fn owner_anchor<'a>(xml: &Xml<'a>, node: roxmltree::Node<'_, 'a>) -> Option<String> {
    let mut current = Some(node);
    while let Some(n) = current {
        if let Some(key) = xml.key(n) {
            if !key.starts_with('~') {
                return Some(key.to_string());
            }
        }
        current = n.parent();
    }
    None
}

/// Unmodeled markup of every anchored element: prefixed attributes (for example `hx:mark`) and
/// namespace declarations, as written, keyed by the anchor of the element that carries them.
fn foreign_inventory(xml: &Xml<'_>) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for node in xml.doc.descendants().filter(|n| n.is_element()) {
        let Some(owner) = owner_anchor(xml, node) else { continue };
        let qname = xml.qname(node);
        let entries = out.entry(owner).or_default();
        let (start, _) = xml.span(node);
        for declaration in declared_namespace_attributes(xml.src, start, xml.start_tag_end(node)) {
            entries.insert(format!("{qname} {declaration}"));
        }
        for attr in node.attributes().filter(|a| a.namespace().is_some()) {
            entries.insert(format!("{qname} {}", &xml.src[attr.range()]));
        }
    }
    out.retain(|_, entries| !entries.is_empty());
    out
}

/// Whether the picture elements of a saved part record the displayed size: `hp:orgSz`, `hp:imgClip`,
/// and `hp:imgDim` in HWPUNIT on the native grid, with the crop covering the whole image.
fn picture_geometry_matches(xml: &Xml<'_>, keys: &[String; 4], width_emu: i64, height_emu: i64) -> bool {
    if width_emu % EMU_PER_HWPUNIT != 0 || height_emu % EMU_PER_HWPUNIT != 0 {
        return false;
    }
    let (w, h) = (width_emu / EMU_PER_HWPUNIT, height_emu / EMU_PER_HWPUNIT);
    let has = |key: &str, pairs: &[(&str, i64)]| -> bool {
        xml.node(key)
            .map(|node| pairs.iter().all(|(name, value)| node.attribute(*name) == Some(value.to_string().as_str())))
            .unwrap_or(false)
    };
    has(&keys[1], &[("width", w), ("height", h)])
        && has(&keys[2], &[("left", 0), ("top", 0), ("right", w), ("bottom", h)])
        && has(&keys[3], &[("dimwidth", w), ("dimheight", h)])
}

/// Anchors of the `hp:ctrl` elements that belong to paragraph `node` itself (not to a table or an
/// object inside it), in document order. Runs are transparent, so every such ctrl is `<para>/hp:ctrl#k`.
fn paragraph_ctrl_keys(xml: &Xml<'_>, node: roxmltree::Node<'_, '_>) -> Vec<String> {
    let Some(base) = xml.key(node) else { return Vec::new() };
    let prefix = format!("{base}/hp:ctrl#");
    node.descendants()
        .filter_map(|n| xml.key(n))
        .filter(|k| matches!(k.strip_prefix(prefix.as_str()), Some(rest) if !rest.contains('/')))
        .map(str::to_string)
        .collect()
}

/// Pairs the ctrl anchors of one paragraph before and after a save, in document order. Only equal
/// counts are paired: otherwise the ordinary region checks report the difference.
fn pair_ctrl_shifts(part: &str, before: &[String], after: &[String], maps: &mut Vec<(String, String, String)>) {
    if before.len() != after.len() {
        return;
    }
    for (from, to) in before.iter().zip(after) {
        if from != to {
            maps.push((part.to_string(), from.clone(), to.clone()));
        }
    }
}

/// Anchor prefix mappings `(part, from_prefix, to_prefix)` for the section head, and the problems
/// found in it. When the section's leading `hp:secPr` moves to a new first paragraph, its adjacent
/// `hp:ctrl` must move with it, and the ctrl siblings that join or leave a retained paragraph are
/// renumbered (`#k` shifts by one). A head that did not move as a whole is a problem.
fn section_head_relocations(orig_pkg: &Package, out_pkg: &Package, orig: &Loaded, out: &Loaded) -> (Vec<(String, String, String)>, Vec<String>) {
    let mut maps = Vec::new();
    let mut problems = Vec::new();
    for osec in &orig.sections {
        let Some(osec_first) = osec.blocks.first() else { continue };
        let Some(usec) = out.sections.iter().find(|s| s.part == osec.part) else { continue };
        let Some(out_first) = usec.blocks.first() else { continue };
        if osec_first.id() == out_first.id() {
            continue;
        }
        let (Ok(ob), Ok(ub)) = (orig_pkg.part(&osec.part), out_pkg.part(&osec.part)) else { continue };
        let (Ok(os), Ok(us)) = (std::str::from_utf8(ob), std::str::from_utf8(ub)) else { continue };
        let (Ok(xo), Ok(xu)) = (Xml::parse(os), Xml::parse(us)) else { continue };
        let (Some(op), Some(up)) = (xo.node(osec_first.host_key()), xu.node(out_first.host_key())) else { continue };
        let Some((o_sec, o_ctrl)) = crate::section::leading_section_head(op) else { continue };
        let Some(o_sec_key) = xo.key(o_sec).map(str::to_string) else { continue };
        let o_ctrl_key = o_ctrl.and_then(|c| xo.key(c)).map(str::to_string);
        let Some((u_sec, u_ctrl)) = crate::section::leading_section_head(up) else {
            problems.push(format!("{}: hp:secPr did not move to the new first paragraph", osec.part));
            continue;
        };
        if let Some(uk) = xu.key(u_sec) {
            maps.push((osec.part.clone(), o_sec_key, uk.to_string()));
        }
        let u_ctrl_key = u_ctrl.and_then(|c| xu.key(c)).map(str::to_string);
        match (&o_ctrl_key, &u_ctrl_key) {
            (Some(from), Some(to)) => maps.push((osec.part.clone(), from.clone(), to.clone())),
            (Some(from), None) => problems.push(format!("{} {from}: the section ctrl did not move with hp:secPr", osec.part)),
            _ => {}
        }

        // The new first paragraph: a retained paragraph gains the moved ctrl in front of its own ctrls.
        if let Some(ob) = osec.blocks.iter().find(|b| b.id() == out_first.id()) {
            if let Some(oh) = xo.node(ob.host_key()) {
                let after: Vec<String> = paragraph_ctrl_keys(&xu, up).into_iter().filter(|k| Some(k) != u_ctrl_key.as_ref()).collect();
                pair_ctrl_shifts(&osec.part, &paragraph_ctrl_keys(&xo, oh), &after, &mut maps);
            }
        }
        // The original first paragraph, if it is retained: its moved ctrl left, so later ctrls shift back.
        if let Some(ub) = usec.blocks.iter().find(|b| b.id() == osec_first.id()) {
            if let Some(uh) = xu.node(ub.host_key()) {
                let before: Vec<String> = paragraph_ctrl_keys(&xo, op).into_iter().filter(|k| Some(k) != o_ctrl_key.as_ref()).collect();
                pair_ctrl_shifts(&osec.part, &before, &paragraph_ctrl_keys(&xu, uh), &mut maps);
            }
        }
    }
    (maps, problems)
}

fn anchor_matches_prefix(anchor: &str, prefix: &str) -> bool {
    anchor == prefix || (anchor.starts_with(prefix) && anchor[prefix.len()..].starts_with('/'))
}

fn remap_anchor(maps: &[(String, String, String)], part: &str, anchor: &str) -> String {
    for (p, from, to) in maps {
        if p == part && anchor_matches_prefix(anchor, from) {
            return format!("{to}{}", &anchor[from.len()..]);
        }
    }
    anchor.to_string()
}

/// Parsed blocks in the same document order as `ops::all_blocks`, including objects inside cells.
fn source_blocks<'a>(blocks: &'a [PBlock], out: &mut Vec<&'a PBlock>) {
    for block in blocks {
        out.push(block);
        if let PBlock::Table(table) = block {
            for cell in table.rows.iter().flatten() {
                source_blocks(&cell.blocks, out);
            }
        }
    }
}

/// Sets the line pitch of the paragraphs in `pitches`, top level and inside table cells.
fn set_pitches(blocks: &mut [FlowBlock], pitches: &BTreeMap<String, i64>) {
    for block in blocks {
        match block {
            FlowBlock::Paragraph(p) => {
                if let Some(pitch) = pitches.get(&p.id) {
                    p.format.line_spacing = *pitch;
                }
            }
            FlowBlock::Table(table) => {
                for cell in table.rows.iter_mut().flatten() {
                    set_pitches(&mut cell.blocks, pitches);
                }
            }
            _ => {}
        }
    }
}

/// The parsed header of a package, when it parses.
fn header_xml(pkg: &Package) -> Option<Xml<'_>> {
    std::str::from_utf8(pkg.part(HEADER).ok()?).ok().and_then(|src| Xml::parse(src).ok())
}

/// Attributes of a paragraph shape that the model carries: alignment, paragraph spacing and indent, line pitch,
/// and the shape's page break.
fn modeled_shape_attribute(element: &str, attribute: &str) -> bool {
    matches!(
        (element, attribute),
        ("align", "horizontal") | ("prev" | "next" | "intent", "value") | ("lineSpacing", "type" | "value") | ("breakSetting", "pageBreakBefore")
    )
}

/// Canonical text of an element: qualified names, attributes sorted (the `id` attribute left out, and the modeled
/// ones too when `masked`), then the children in order with whitespace-only text left out.
fn canonical_element(node: roxmltree::Node<'_, '_>, masked: bool, out: &mut String) {
    let name = node.tag_name();
    out.push_str(&format!("<{{{}}}{}", name.namespace().unwrap_or(""), name.name()));
    let mut attributes: Vec<String> = node
        .attributes()
        .filter(|a| a.name() != "id" && !(masked && modeled_shape_attribute(name.name(), a.name())))
        .map(|a| format!("{{{}}}{}={:?}", a.namespace().unwrap_or(""), a.name(), a.value()))
        .collect();
    attributes.sort();
    for attribute in &attributes {
        out.push(' ');
        out.push_str(attribute);
    }
    out.push('>');
    for child in node.children() {
        if child.is_element() {
            canonical_element(child, masked, out);
        } else if child.is_text() {
            let text = child.text().unwrap_or("").trim();
            if !text.is_empty() {
                out.push_str(&format!("{text:?}"));
            }
        } else {
            out.push_str(&format!("#{:?}", child.text().unwrap_or("")));
        }
    }
    out.push_str("</>");
}

/// Canonical form of the paragraph shape element with this anchor in a parsed header.
fn canonical_shape(header: &Xml<'_>, key: &str, masked: bool) -> Option<String> {
    let node = header.node(key)?;
    let mut out = String::new();
    canonical_element(node, masked, &mut out);
    Some(out)
}

fn check_native_line(id: &str, saved: &NativeLine, wanted: &NativeLine, branches_ok: bool, problems: &mut Vec<String>) {
    if saved != wanted {
        problems.push(format!("{id} line spacing {} {} differs from expected {} {}", saved.kind, saved.value, wanted.kind, wanted.value));
    } else if !branches_ok {
        problems.push(format!("{id} line spacing differs between compatibility branches"));
    }
}

/// Check native kinds before comparing the projected pitches. New blocks acquire XML-derived IDs on
/// reopen, so pair the target and output in document order (including cells). The model check verifies structure.
fn check_glyph_pitches(orig_pkg: &Package, out_pkg: &Package, orig: &Loaded, out: &Loaded, target: &FlowDocument,
                       lines: &NativeLines, carriers: &HashSet<&str>) -> (BTreeMap<String, i64>, Vec<String>) {
    let mut original = Vec::new();
    for section in &orig.sections {
        source_blocks(&section.blocks, &mut original);
    }
    let sources: BTreeMap<_, _> = original.into_iter().map(|b| (b.id(), b)).collect();
    let mut saved = Vec::new();
    for section in &out.sections {
        source_blocks(&section.blocks, &mut saved);
    }
    let orig_header = header_xml(orig_pkg);
    let out_header = header_xml(out_pkg);
    let mut pitches = BTreeMap::new();
    let mut problems = Vec::new();
    let targets = all_blocks(target);
    for (index, (tb, sb)) in targets.iter().copied().zip(saved).enumerate() {
        let (FlowBlock::Paragraph(tp), PBlock::Para(sp)) = (tb, sb) else {
            let host = match (tb, sb) {
                (FlowBlock::Table(_), PBlock::Table(t)) => &t.host_shape,
                (FlowBlock::Image(_), PBlock::Image(i)) => &i.host_shape,
                _ => continue,
            };
            let id = block_id(tb);
            if sources.contains_key(id) {
                continue;
            }
            let folded = index > 0 && carriers.contains(block_id(targets[index - 1]));
            match (host, folded) {
                (Some(shape), false) => match new_host_line(id, None, &orig.header, lines) {
                    Ok(wanted) => check_native_line(id, &shape.line, &wanted, shape.uniform_line, &mut problems),
                    Err(error) => problems.push(format!("{id} line spacing: {}", error.message)),
                },
                // The reader took the preceding carrier's paraPr from this very hp:p; its line was checked above.
                (None, true) => {}
                _ => problems.push(format!("{id} page break carrier differs from the planned host")),
            }
            continue;
        };
        let id = &tp.id;
        let Some(saved_shape) = out.header.para_pr(sp.para_pr) else { continue };
        let source = sources.get(id.as_str()).copied().and_then(|b| match b {
            PBlock::Para(p) => orig.header.para_pr(p.para_pr).map(|s| (p, s)),
            _ => None,
        });
        let wanted = match paragraph_line(tp, source.map(|(p, s)| (&p.format, &s.line)), lines) {
            Ok(line) => line,
            Err(error) => {
                problems.push(format!("{id} line spacing: {error}"));
                continue;
            }
        };
        let mut unchanged = false;
        let base = match source {
            Some((op, shape)) => {
                unchanged = !lines.contains_key(id) && wanted == shape.line && designllm_flow::line_spacing::same_line_spacing(&tp.format, &op.format);
                let masked = !format_eq(&tp.format, &op.format) || !unchanged;
                let source_form = orig_header.as_ref().and_then(|h| canonical_shape(h, &shape.key, masked));
                let saved_form = out_header.as_ref().and_then(|h| canonical_shape(h, &saved_shape.key, masked));
                if source_form.is_none() || source_form != saved_form {
                    problems.push(format!("{id} paragraph shape differs from the source in content the model does not carry"));
                }
                op.orig_cps.first().and_then(|cp| orig.header.char_pr(*cp))
            }
            None => orig.header.char_prs.first(),
        };
        check_native_line(id, &saved_shape.line, &wanted, unchanged || saved_shape.uniform_line, &mut problems);
        let size = paragraph_font_size(&tp.runs, base.map(|c| c.style.font_size));
        let expected = wanted.pitch(size).0;
        if unchanged {
            if sp.format.line_spacing != expected {
                problems.push(format!("{id} line pitch {} differs from {expected}, derived from its paraPr and the target glyph sizes", sp.format.line_spacing));
            }
            pitches.insert(sp.id.clone(), tp.format.line_spacing);
        } else if sp.format.line_spacing != expected || tp.format.line_spacing != expected {
            problems.push(format!(
                "{id} line_spacing: line pitch {} (target {}) differs from {expected}, derived from its percentage and base character shape or native spacing",
                sp.format.line_spacing, tp.format.line_spacing
            ));
        }
    }
    (pitches, problems)
}

/// Verifies `output` against `target`, using the original bytes as the reference.
pub fn verify(
    target: &FlowDocument,
    lines: &NativeLines,
    original: &[u8],
    output: &[u8],
    assets: &dyn AssetStore,
    figure_links: &[FigureLink],
) -> PreservationVerification {
    let mut report = PreservationVerification::default();
    let blocking = |report: &mut PreservationVerification, code: &str, message: String| {
        report.diagnostics.push(Diagnostic { code: code.to_string(), message, node_id: None, blocking: true, params: Default::default() });
    };

    if let Err(e) = package::check_mimetype(output) {
        blocking(&mut report, diag::VERIFY_REPARSE, e.message);
        return report;
    }
    let (orig_pkg, orig) = match doc::load_bytes(original, assets) {
        Ok(v) => v,
        Err(e) => {
            blocking(&mut report, diag::VERIFY_REPARSE, format!("source cannot be read: {}", e.message));
            return report;
        }
    };
    let (out_pkg, mut out) = match doc::load_bytes(output, assets) {
        Ok(v) => v,
        Err(e) => {
            blocking(&mut report, diag::VERIFY_REPARSE, format!("output does not reparse: {}", e.message));
            return report;
        }
    };

    // Parts: immutable parts keep their payload; editable parts are checked through the model.
    let (checked, problems) = source_part_problems(&orig.fingerprints.parts, &out.fingerprints.parts);
    report.checked_parts += checked;
    for (code, message) in problems {
        blocking(&mut report, code, message);
    }
    for name in out.fingerprints.parts.keys() {
        if !orig.fingerprints.parts.contains_key(name) {
            report.checked_parts += 1;
            let allowed = name.starts_with("BinData/") && bin_referenced(&out, name);
            if !allowed {
                blocking(&mut report, diag::VERIFY_PART_UNEXPECTED, format!("{name} was added without a model change"));
            }
        }
    }

    // Manifest: every original item is kept as written; additions are only embedded binaries.
    report.checked_parts += 1;
    for item in &orig.manifest.items {
        let kept = out
            .manifest
            .items
            .iter()
            .any(|o| o.id == item.id && o.href == item.href && o.media_type == item.media_type);
        if !kept {
            blocking(&mut report, diag::VERIFY_MANIFEST, format!("manifest item {} changed or disappeared", item.id));
        }
    }
    for item in &out.manifest.items {
        if !orig.manifest.items.iter().any(|o| o.id == item.id) && !item.href.starts_with("BinData/") {
            blocking(&mut report, diag::VERIFY_MANIFEST, format!("manifest item {} added without a binary", item.id));
        }
    }

    // Opaque regions: every original region keeps its anchor and bytes; counts per anchor are kept.
    // The regions that a figure update or a removed paragraph changes on purpose are exempt.
    // Leading secPr/ctrl relocated to a new first paragraph are verified at their new anchor.
    let (relocations, head_problems) = section_head_relocations(&orig_pkg, &out_pkg, &orig, &out);
    for problem in head_problems {
        blocking(&mut report, diag::VERIFY_REGION_MISSING, problem);
    }
    let exempt = Exemptions::new(&orig, target, figure_links);
    let orig_regions: BTreeMap<(String, String), String> = region_map(&orig.regions)
        .into_iter()
        .map(|((part, anchor), sha)| {
            let mapped = remap_anchor(&relocations, &part, &anchor);
            ((part, mapped), sha)
        })
        .filter(|((part, anchor), _)| !exempt.covers(part, anchor))
        .collect();
    let out_regions: BTreeMap<(String, String), String> =
        region_map(&out.regions).into_iter().filter(|((part, anchor), _)| !exempt.covers(part, anchor)).collect();
    for ((part, anchor), sha) in &orig_regions {
        report.checked_regions += 1;
        match out_regions.get(&(part.clone(), anchor.clone())) {
            Some(o) if o == sha => {}
            Some(_) => blocking(&mut report, diag::VERIFY_REGION_CHANGED, format!("{part} {anchor} bytes changed")),
            None => blocking(&mut report, diag::VERIFY_REGION_MISSING, format!("{part} {anchor} is missing")),
        }
    }
    let mut orig_counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for ((part, anchor), _) in &orig_regions {
        *orig_counts.entry((part.clone(), base_anchor(anchor).to_string())).or_default() += 1;
    }
    let mut out_counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for ((part, anchor), _) in &out_regions {
        let base = base_anchor(anchor).to_string();
        if orig_counts.contains_key(&(part.clone(), base.clone())) {
            *out_counts.entry((part.clone(), base)).or_default() += 1;
        }
    }
    for (key, count) in &orig_counts {
        let actual = out_counts.get(key).copied().unwrap_or(0);
        if actual != *count {
            blocking(&mut report, diag::VERIFY_REGION_COUNT, format!("{} {}: {actual} regions, expected {count}", key.0, key.1));
        }
    }

    // Unmodeled markup: a prefixed attribute or a namespace declaration that an anchored element had
    // must still be there, unchanged. Elements that the save adds are not compared.
    for name in orig.fingerprints.parts.keys().filter(|n| matches!(package::role_of(n.as_str()), Role::Editable)) {
        let (Ok(before), Ok(after)) = (orig_pkg.part(name), out_pkg.part(name)) else { continue };
        let (Ok(before), Ok(after)) = (std::str::from_utf8(before), std::str::from_utf8(after)) else { continue };
        let (Ok(xo), Ok(xu)) = (Xml::parse(before), Xml::parse(after)) else { continue };
        let inv_before: BTreeMap<String, BTreeSet<String>> = foreign_inventory(&xo)
            .into_iter()
            .map(|(anchor, entries)| (remap_anchor(&relocations, name, &anchor), entries))
            .collect();
        let inv_after = foreign_inventory(&xu);
        let mut anchors: BTreeSet<String> = inv_before.keys().cloned().collect();
        anchors.extend(
            inv_after
                .keys()
                .filter(|a| xo.node(a.as_str()).is_some() || relocations.iter().any(|(p, _, to)| p == name && anchor_matches_prefix(a, to)))
                .cloned(),
        );
        let empty = BTreeSet::new();
        for anchor in anchors {
            let expected = inv_before.get(&anchor).unwrap_or(&empty);
            let found = inv_after.get(&anchor).unwrap_or(&empty);
            if expected != found {
                blocking(
                    &mut report,
                    diag::VERIFY_FOREIGN_ATTRIBUTE,
                    format!("{name} {anchor}: unmodeled attributes or namespace declarations changed (expected {expected:?}, found {found:?})"),
                );
            }
        }
    }

    // Figure links: the target is a picture of the document and shows the render asset. A picture whose
    // image a figure update replaced also records the new size in its image elements.
    for link in figure_links {
        let Some(node) = target_picture_in_doc(target, &link.target_node_id) else {
            blocking(&mut report, diag::VERIFY_FIGURE_LINK, format!("figure link target {} is not a picture of the document", link.target_node_id));
            continue;
        };
        if node.asset.as_ref().map(|a| a.sha256.as_str()) != Some(link.render_hash.as_str()) {
            blocking(&mut report, diag::VERIFY_FIGURE_LINK, format!("figure link target {} does not show render {}", link.target_node_id, link.render_hash));
        }
        let Some((part, picture)) = find_picture(&orig.sections, &link.target_node_id) else { continue };
        let Some(keys) = &picture.figure_keys else { continue };
        if picture.asset.sha256 == link.render_hash {
            continue;
        }
        let geometry_ok = match std::str::from_utf8(out_pkg.part(part).unwrap_or(&[])).ok().and_then(|s| Xml::parse(s).ok()) {
            Some(xml) => picture_geometry_matches(&xml, keys, node.bounds.width, node.bounds.height),
            None => false,
        };
        if !geometry_ok {
            blocking(
                &mut report,
                diag::VERIFY_FIGURE_LINK,
                format!("picture {} shows render {} but its image elements do not record its size", link.target_node_id, link.render_hash),
            );
        }
    }

    // Model: the reparsed document must equal the target in every field except identifiers. A line pitch that
    // follows the glyphs is checked against its derived value, then compared as the target's value.
    let carriers = crate::writer::folded_carriers(&orig.sections, target, lines);
    let (glyph_pitches, pitch_problems) = check_glyph_pitches(&orig_pkg, &out_pkg, &orig, &out, target, lines, &carriers);
    for problem in pitch_problems {
        blocking(&mut report, diag::VERIFY_SEMANTIC, problem);
    }
    for section in &mut out.flow.sections {
        set_pitches(&mut section.blocks, &glyph_pitches);
    }
    let a = normalize(target, true);
    let b = normalize(&out.flow, true);
    if let Some(at) = first_difference(&a, &b, "") {
        blocking(&mut report, diag::VERIFY_SEMANTIC, format!("reparsed model differs from the target at {at}"));
    }

    // Header: list counts and font counts match their children.
    for (list, declared, actual) in &out.header.item_counts {
        if *declared != Some(*actual) {
            blocking(&mut report, diag::VERIFY_HEADER_COUNT, format!("{list}: itemCnt {declared:?}, children {actual}"));
        }
    }
    if let Ok(src) = std::str::from_utf8(out_pkg.part(HEADER).unwrap_or(&[])) {
        if let Ok(xml) = Xml::parse(src) {
            let root = xml.doc.root_element();
            for list in root.children().filter(|c| is_el(*c, NS_HH, "refList")).flat_map(|r| child_els(r, NS_HH, "fontfaces")) {
                for face in child_els(list, NS_HH, "fontface") {
                    let declared = xml.attr_u32(face, "fontCnt");
                    let actual = child_els(face, NS_HH, "font").len() as u32;
                    if declared != Some(actual) {
                        blocking(&mut report, diag::VERIFY_HEADER_COUNT, format!("fontface {:?}: fontCnt {declared:?}, fonts {actual}", face.attribute("lang")));
                    }
                }
            }
        }
    }

    // Caches: removed exactly where the policy says, kept everywhere else.
    let removals = expected_cache_removals(&orig, target, lines);
    for (si, osec) in orig.sections.iter().enumerate() {
        let src = match std::str::from_utf8(out_pkg.part(&osec.part).unwrap_or(&[])) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let xml = match Xml::parse(src) {
            Ok(x) => x,
            Err(_) => continue,
        };
        for (bi, block) in osec.blocks.iter().enumerate() {
            let expect_removed = removals.get(si).and_then(|f| f.get(bi)).copied().unwrap_or(false);
            for key in cache_keys(block) {
                let present = xml.node(&key).is_some();
                if expect_removed == present {
                    blocking(
                        &mut report,
                        diag::VERIFY_CACHE_POLICY,
                        format!("{} {key}: cache {} but policy expects {}", osec.part, if present { "kept" } else { "removed" }, if expect_removed { "removed" } else { "kept" }),
                    );
                }
            }
        }
    }

    // Derived preview parts: regenerated or removed only when the content changed.
    let changed = first_difference(&normalize(&orig.flow, false), &normalize(target, false), "").is_some()
        || orig.sections.iter().flat_map(|s| &s.blocks).any(|b| has_line_override(b, lines));
    let expected_text = preview_text(target, &carriers);
    if changed {
        if out_pkg.has_part(PREVIEW_IMAGE) {
            blocking(&mut report, diag::VERIFY_DERIVED, "PrvImage.png is stale and must be removed".into());
        }
        if orig_pkg.has_part(PREVIEW_TEXT) {
            match out_pkg.part(PREVIEW_TEXT) {
                Ok(bytes) if bytes == expected_text.as_bytes() => {}
                _ => blocking(&mut report, diag::VERIFY_DERIVED, "PrvText.txt does not match the edited text".into()),
            }
        }
    } else {
        for (name, state) in kept_preview_problems(&orig.fingerprints.parts, &out.fingerprints.parts) {
            let message = match state {
                PartState::Missing => format!("{name} is missing although the content did not change"),
                _ => format!("{name} changed although the content did not"),
            };
            blocking(&mut report, diag::VERIFY_DERIVED, message);
        }
    }

    report
}

fn role_of_name(name: &str) -> Role {
    package::role_of(name)
}

/// Source parts other than derived previews, compared with the saved parts. Each one counts as checked. An
/// immutable part must keep its payload hash; an editable part only has to be present, its content is checked
/// through the model. Problems are in part name order, as codes and messages.
fn source_part_problems(
    orig: &BTreeMap<String, PartFingerprint>,
    out: &BTreeMap<String, PartFingerprint>,
) -> (u32, Vec<(&'static str, String)>) {
    let expected = orig
        .iter()
        .filter(|(name, _)| role_of_name(name) != Role::Derived)
        .map(|(name, fp)| (name.as_str(), fp.sha256.as_str()));
    let states = compare_parts(expected, |name| out.get(name).map(|fp| fp.sha256.as_str()));
    let problems = states
        .iter()
        .filter_map(|&(name, state)| match (state, role_of_name(name)) {
            (PartState::Changed, Role::Immutable) => Some((diag::VERIFY_PART_CHANGED, format!("{name} payload changed"))),
            (PartState::Missing, _) => Some((diag::VERIFY_PART_MISSING, format!("{name} is missing"))),
            _ => None,
        })
        .collect();
    (states.len() as u32, problems)
}

/// Preview parts of the source whose saved payload changed or is missing, in the order text, image. Used when
/// the content did not change, so the previews must be kept as they were.
fn kept_preview_problems<'a>(orig: &'a BTreeMap<String, PartFingerprint>, out: &BTreeMap<String, PartFingerprint>) -> Vec<(&'a str, PartState)> {
    let expected = [PREVIEW_TEXT, PREVIEW_IMAGE]
        .into_iter()
        .filter_map(|name| orig.get_key_value(name).map(|(name, fp)| (name.as_str(), fp.sha256.as_str())));
    compare_parts(expected, |name| out.get(name).map(|fp| fp.sha256.as_str()))
        .into_iter()
        .filter(|(_, state)| *state != PartState::Kept)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(entries: &[(&str, &str)]) -> BTreeMap<String, PartFingerprint> {
        entries
            .iter()
            .map(|(name, sha)| (name.to_string(), PartFingerprint { sha256: sha.to_string(), content_type: None, immutable: true }))
            .collect()
    }

    #[test]
    fn immutable_and_editable_source_parts_count_and_report_in_name_order() {
        let orig = parts(&[
            ("BinData/a.bmp", "1"),
            ("BinData/b.bmp", "2"),
            ("BinData/c.bmp", "3"),
            ("Contents/header.xml", "4"),
            ("Contents/section0.xml", "5"),
            ("Contents/section1.xml", "6"),
            (PREVIEW_TEXT, "7"),
            (PREVIEW_IMAGE, "8"),
        ]);
        let out = parts(&[("BinData/a.bmp", "1"), ("BinData/b.bmp", "9"), ("Contents/header.xml", "9"), ("Contents/section0.xml", "5")]);
        let (checked, problems) = source_part_problems(&orig, &out);
        assert_eq!(checked, 6, "derived previews are not counted; missing and changed parts are");
        let found: Vec<(&str, &str)> = problems.iter().map(|(code, message)| (*code, message.as_str())).collect();
        assert_eq!(
            found,
            vec![
                (diag::VERIFY_PART_CHANGED, "BinData/b.bmp payload changed"),
                (diag::VERIFY_PART_MISSING, "BinData/c.bmp is missing"),
                (diag::VERIFY_PART_MISSING, "Contents/section1.xml is missing"),
            ]
        );
    }

    #[test]
    fn kept_previews_report_a_change_and_a_loss_alike() {
        let orig = parts(&[(PREVIEW_IMAGE, "1"), (PREVIEW_TEXT, "2")]);
        assert!(kept_preview_problems(&orig, &orig).is_empty());
        let out = parts(&[(PREVIEW_TEXT, "9")]);
        assert_eq!(kept_preview_problems(&orig, &out), vec![(PREVIEW_TEXT, PartState::Changed), (PREVIEW_IMAGE, PartState::Missing)]);
        assert!(kept_preview_problems(&parts(&[]), &out).is_empty(), "a preview the source lacks is not checked");
    }

    #[test]
    fn base_anchor_strips_only_the_occurrence_suffix() {
        assert_eq!(base_anchor("hs:sec#0/hp:p[id=0]#2"), "hs:sec#0/hp:p[id=0]");
        assert_eq!(base_anchor("hs:sec#0"), "hs:sec");
    }

    #[test]
    fn first_difference_reports_a_path() {
        let a = serde_json::json!({"x": [1, 2], "y": "a"});
        let b = serde_json::json!({"x": [1, 3], "y": "a"});
        assert_eq!(first_difference(&a, &b, "").as_deref(), Some("/x[1]"));
        assert_eq!(first_difference(&a, &a, ""), None);
    }

    #[test]
    fn exemptions_cover_only_the_anchor_and_its_descendants() {
        let exempt = Exemptions { hosts: vec!["hs:sec#0/hp:p[id=5]#1".into()], children: HashSet::new() };
        assert!(exempt.covers("Contents/section0.xml", "hs:sec#0/hp:p[id=5]#1"));
        assert!(exempt.covers("Contents/section0.xml", "hs:sec#0/hp:p[id=5]#1/hp:pic[id=9]#0/hc:img#0"));
        assert!(!exempt.covers("Contents/section0.xml", "hs:sec#0/hp:p[id=5]#10"));
        assert!(!exempt.covers("Contents/section0.xml", "hs:sec#0/hp:p[id=5]#1x"));
    }
}
