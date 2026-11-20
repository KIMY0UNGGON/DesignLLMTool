//! Projection of the native HWP model into the shared FlowDocument, with stable ids.
//!
//! - Section ↔ FlowDocument section ("s{n}").
//! - Paragraph text ↔ Paragraph block; runs follow character-shape changes; inline objects
//!   (equation, footnote/endnote mark, auto number, ruby, character overlap) are protected atoms.
//! - Tables ↔ Table blocks, pictures with stored BinData ↔ Image blocks, drawing objects and
//!   unknown objects ↔ Opaque blocks (preserved, not editable). Objects located before the first
//!   character are emitted before the paragraph text, others after it. A paragraph that starts a
//!   page (explicit page break) is emitted before all its objects, so they stay on its page; such a
//!   paragraph keeps its block even without text, to carry the break.
//! - Headers, footers and note bodies are preserved but not shown (reported).
use crate::view::alignment_code;
use designllm_contracts::*;
use designllm_flow::line_spacing::{model_percent, percent_line_pitch};
use designllm_flow::OBJECT_REPLACEMENT;
use rhwp::model::control::Control;
use rhwp::model::document::Document;
use rhwp::model::paragraph::{ColumnBreakType, Paragraph as RParagraph};
use rhwp::model::style::{BorderLineType, Fill, FillType, LineSpacingType};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocKind { Section, Paragraph, Table, Cell, Image, Opaque, Atom }

/// Native location: [section, paragraph, (control, cell, paragraph)*] with a trailing control
/// (and cell) index for object/cell nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeLoc { pub kind: LocKind, pub path: Vec<usize> }

#[derive(Debug, Clone, Default)]
pub struct IdMap { pub by_id: BTreeMap<Id, NodeLoc>, by_path: BTreeMap<(Vec<usize>, u8), Id> }

fn kind_key(k: LocKind) -> u8 { k as u8 }

impl IdMap {
    pub fn insert(&mut self, id: Id, loc: NodeLoc) {
        self.by_path.insert((loc.path.clone(), kind_key(loc.kind)), id.clone());
        self.by_id.insert(id, loc);
    }
    pub fn id_at(&self, kind: LocKind, path: &[usize]) -> Option<&Id> { self.by_path.get(&(path.to_vec(), kind_key(kind))) }
    pub fn loc(&self, id: &str) -> Option<&NodeLoc> { self.by_id.get(id) }
    pub fn to_mappings(&self) -> BTreeMap<Id, serde_json::Value> {
        self.by_id.iter().map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or(serde_json::Value::Null))).collect()
    }
    pub fn from_mappings(m: &BTreeMap<Id, serde_json::Value>) -> DltResult<Self> {
        let mut map = IdMap::default();
        for (id, v) in m {
            let loc: NodeLoc = serde_json::from_value(v.clone()).map_err(|e| DltError::new(ErrorCode::IntegrityError, format!("HWP mapping for {id} is invalid: {e}")))?;
            map.insert(id.clone(), loc);
        }
        Ok(map)
    }
    /// Rebuild the path index after locations were edited in place.
    pub fn rebuild(&mut self) {
        self.by_path = self.by_id.iter().map(|(id, loc)| ((loc.path.clone(), kind_key(loc.kind)), id.clone())).collect();
    }
    /// Shift top-level paragraph indices >= `from` in `section` by `delta`, dropping removed ones.
    pub fn shift_paragraphs(&mut self, section: usize, from: usize, delta: isize, remove: Option<usize>) {
        let mut drop = Vec::new();
        for (id, loc) in self.by_id.iter_mut() {
            if loc.path.len() < 2 || loc.path[0] != section { continue; }
            if Some(loc.path[1]) == remove { drop.push(id.clone()); continue; }
            if loc.path[1] >= from { loc.path[1] = (loc.path[1] as isize + delta) as usize; }
        }
        for id in drop { self.by_id.remove(&id); }
        self.rebuild();
    }
}

/// How the projection treats image payloads.
pub enum Assets<'a> { Store(&'a dyn AssetStore), HashOnly }

pub struct Projection { pub document: FlowDocument, pub ids: IdMap, pub diagnostics: Vec<Diagnostic>, pub asset_count: usize }

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role { Block, Atom, Hidden }

pub(crate) fn role(c: &Control) -> Role {
    match c {
        Control::Table(_) | Control::Picture(_) | Control::Shape(_) | Control::Form(_) | Control::Unknown(_) => Role::Block,
        Control::Equation(_) | Control::Footnote(_) | Control::Endnote(_) | Control::AutoNumber(_) | Control::Ruby(_) | Control::CharOverlap(_) => Role::Atom,
        _ => Role::Hidden,
    }
}

/// The inline atoms of a native paragraph in control order, as (native character position, control index). The position is
/// clamped to the text. `Builder::paragraph_blocks` places the atoms with this list, and the link spans read the same one.
pub(crate) fn atoms_of(p: &RParagraph) -> Vec<(usize, usize)> {
    atoms_at(p, &p.control_text_positions(), p.text.chars().count())
}

/// [`atoms_of`] with the paragraph's `control_text_positions` and its character count `n` already read.
fn atoms_at(p: &RParagraph, positions: &[usize], n: usize) -> Vec<(usize, usize)> {
    p.controls
        .iter()
        .enumerate()
        .filter(|(_, c)| role(c) == Role::Atom)
        .map(|(k, _)| (positions.get(k).copied().unwrap_or(0).min(n), k))
        .collect()
}

/// The text of a native paragraph as the flow shows it: its characters, with one U+FFFC before each atom at its position
/// (after the last character for an atom at the end). `atoms` is the list of [`atoms_of`].
pub(crate) fn logical_text_of(p: &RParagraph, atoms: &[(usize, usize)]) -> String {
    let chars: Vec<char> = p.text.chars().collect();
    let mut text = String::new();
    for (index, c) in chars.iter().enumerate() {
        text.extend(std::iter::repeat(OBJECT_REPLACEMENT).take(atoms.iter().filter(|&&(pos, _)| pos == index).count()));
        text.push(*c);
    }
    text.extend(std::iter::repeat(OBJECT_REPLACEMENT).take(atoms.iter().filter(|&&(pos, _)| pos == chars.len()).count()));
    text
}

fn label(c: &Control) -> &'static str {
    match c {
        Control::Shape(_) => "그리기 개체 (drawing object)", Control::Form(_) => "양식 개체 (form object)", Control::Equation(_) => "수식 (equation)",
        Control::Footnote(_) => "각주 (footnote)", Control::Endnote(_) => "미주 (endnote)", Control::AutoNumber(_) => "자동 번호 (auto number)",
        Control::Ruby(_) => "덧말 (ruby)", Control::CharOverlap(_) => "글자 겹침 (character overlap)", Control::Picture(_) => "그림 (picture)",
        _ => "알 수 없는 개체 (unknown object)",
    }
}

pub use designllm_flow::line_spacing::hu_to_emu as hu;

/// Paragraph margin/spacing/indent: stored doubled; odd values are character-unit based.
pub fn para_margin_emu(stored: i32) -> (i64, bool) {
    if stored % 2 == 0 { (hu(i64::from(stored / 2)), true) } else { (hu(i64::from((stored - 1) / 2) * 1000), false) }
}

pub fn color_hex(c: u32) -> String { format!("#{:02X}{:02X}{:02X}", c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF) }

/// Paragraph format the projection shows for paragraph shape `id` when its largest run is
/// `max_size` EMU (percent line spacing scales with it).
pub fn paragraph_format_of(doc: &Document, id: u16, max_size: i64) -> ParagraphFormat {
    let Some(ps) = doc.doc_info.para_shapes.get(id as usize) else { return ParagraphFormat::default() };
    let align = match alignment_code(&ps.alignment) { 1 => "left", 2 => "right", 3 => "center", _ => "justify" }.to_string();
    let (before, _) = para_margin_emu(ps.spacing_before);
    let (after, _) = para_margin_emu(ps.spacing_after);
    let (indent, _) = para_margin_emu(ps.indent);
    let line_spacing = match ps.line_spacing_type {
        // The rule HWPX shares (`designllm_flow::line_spacing`): 100 percent is single spacing (0); other
        // percentages scale the largest font.
        LineSpacingType::Percent => percent_line_pitch(max_size, i64::from(ps.line_spacing)),
        LineSpacingType::Fixed => hu(i64::from(ps.line_spacing) / 2).max(0),
        LineSpacingType::SpaceOnly => (max_size + hu(i64::from(ps.line_spacing) / 2)).max(0),
        LineSpacingType::Minimum => hu(i64::from(ps.line_spacing) / 2).max(max_size),
    };
    let line_spacing_percent = matches!(ps.line_spacing_type, LineSpacingType::Percent).then(|| model_percent(i64::from(ps.line_spacing))).flatten();
    ParagraphFormat { align, space_before: before.max(0), space_after: after.max(0), line_spacing, indent, line_spacing_percent, ..Default::default() }
}

/// Style the projection gives every HWP table (borders and fills are not modelled).
pub fn table_style() -> Style { Style { stroke: "#000000".into(), stroke_width: 4_320, fill: "none".into(), ..Style::default() } }

pub fn char_style(doc: &Document, id: u32) -> Style {
    let mut style = Style::default();
    if let Some(cs) = doc.doc_info.char_shapes.get(id as usize) {
        let face = doc.doc_info.font_faces.first().and_then(|f| f.get(cs.font_ids[0] as usize)).map(|f| f.name.clone()).unwrap_or_default();
        if !face.trim().is_empty() { style.font_family = face; }
        let rel = if cs.relative_sizes[0] == 0 { 100 } else { i64::from(cs.relative_sizes[0]) };
        style.font_size = hu(i64::from(cs.base_size) * rel / 100).max(1);
        style.color = color_hex(cs.text_color);
        style.bold = cs.bold;
        style.italic = cs.italic;
    }
    style.fill = "none".into();
    style
}

struct Builder<'a> {
    doc: &'a Document,
    prev: Option<&'a IdMap>,
    ids: IdMap,
    diags: Vec<Diagnostic>,
    seen: BTreeSet<(String, String)>,
    assets: Assets<'a>,
    asset_count: usize,
    fresh: bool,
}

impl<'a> Builder<'a> {
    fn diag(&mut self, code: &str, message: impl Into<String>, node: Option<&str>, blocking: bool) {
        let message = message.into();
        if self.seen.insert((code.to_string(), message.clone())) {
            self.diags.push(Diagnostic { code: code.into(), message, node_id: node.map(str::to_string), blocking, params: Default::default() });
        }
    }

    fn id(&mut self, kind: LocKind, path: &[usize]) -> Id {
        let id = match self.prev.and_then(|p| p.id_at(kind, path)) {
            Some(existing) => existing.clone(),
            None if self.fresh => {
                let prefix = match kind { LocKind::Section => "s", LocKind::Paragraph => "p", LocKind::Table => "t", LocKind::Cell => "c", LocKind::Image => "i", LocKind::Opaque => "o", LocKind::Atom => "a" };
                format!("{prefix}{}", path.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("."))
            }
            None => format!("n{}", new_id()),
        };
        self.ids.insert(id.clone(), NodeLoc { kind, path: path.to_vec() });
        id
    }

    fn page_setup(&mut self, s: usize) -> PageSetup {
        let pd = &self.doc.sections[s].section_def.page_def;
        let (w, h) = if pd.landscape { (pd.height, pd.width) } else { (pd.width, pd.height) };
        let setup = PageSetup {
            size: Size { width: hu(i64::from(w)), height: hu(i64::from(h)) },
            margin_left: hu(i64::from(pd.margin_left) + i64::from(pd.margin_gutter)),
            margin_right: hu(i64::from(pd.margin_right)),
            margin_top: hu(i64::from(pd.margin_header) + i64::from(pd.margin_top)),
            margin_bottom: hu(i64::from(pd.margin_footer) + i64::from(pd.margin_bottom)),
        };
        if pd.margin_gutter != 0 { self.diag("hwp_gutter_merged", "The binding gutter is shown as part of the left margin", None, false); }
        setup
    }

    fn paragraph_format(&mut self, p: &RParagraph, max_size: i64, pid: &str) -> ParagraphFormat {
        // A page break the parser synthesized from a natural page edge is not stored in HWP, so it is not a break here.
        let page_break_before = p.column_type == ColumnBreakType::Page && !p.page_break_synthesized;
        let Some(ps) = self.doc.doc_info.para_shapes.get(p.para_shape_id as usize) else { return ParagraphFormat { page_break_before, ..ParagraphFormat::default() } };
        let (b_ok, a_ok, i_ok) = (ps.spacing_before % 2 == 0, ps.spacing_after % 2 == 0, ps.indent % 2 == 0);
        if !(b_ok && a_ok && i_ok) { self.diag("hwp_char_unit_spacing", "Paragraph spacing given in character units is approximated", Some(pid), false); }
        if ps.margin_left != 0 || ps.margin_right != 0 { self.diag("hwp_paragraph_margin_not_rendered", "Paragraph left/right margins are preserved but not shown", Some(pid), false); }
        if matches!(ps.line_spacing_type, LineSpacingType::SpaceOnly) { self.diag("hwp_line_spacing_approximated", "Line spacing 'space only' is approximated", Some(pid), false); }
        if matches!(ps.line_spacing_type, LineSpacingType::Minimum) { self.diag("hwp_line_spacing_approximated", "Line spacing 'minimum' is approximated", Some(pid), false); }
        ParagraphFormat { page_break_before, ..paragraph_format_of(self.doc, p.para_shape_id, max_size) }
    }

    /// Blocks of one native paragraph at `path` (top level: [s, p]).
    fn paragraph_blocks(&mut self, p: &RParagraph, path: &[usize]) -> DltResult<Vec<FlowBlock>> {
        let pid = self.id(LocKind::Paragraph, path);
        let chars: Vec<char> = p.text.chars().collect();
        let n = chars.len();
        let positions = p.control_text_positions();
        let mut before: Vec<FlowBlock> = Vec::new();
        let mut after: Vec<FlowBlock> = Vec::new();
        let atoms = atoms_at(p, &positions, n);
        for (k, c) in p.controls.iter().enumerate() {
            let pos = positions.get(k).copied().unwrap_or(0).min(n);
            let mut cpath = path.to_vec();
            cpath.push(k);
            match role(c) {
                Role::Atom => {}
                Role::Block => {
                    let block = self.control_block(c, &cpath)?;
                    if pos == 0 && n > 0 { before.push(block); } else {
                        if pos > 0 && pos < n { self.diag("hwp_inline_object_moved", "An object inside a line of text is shown after the paragraph", None, false); }
                        after.push(block);
                    }
                }
                Role::Hidden => match c {
                    Control::Header(_) | Control::Footer(_) => self.diag("hwp_header_footer_not_rendered", "Headers and footers are preserved but not shown", None, true),
                    Control::HiddenComment(_) => self.diag("hwp_hidden_comment_preserved", "Hidden comments are preserved but not shown", None, false),
                    _ => {}
                },
            }
            if matches!(c, Control::Footnote(_) | Control::Endnote(_)) {
                self.diag("hwp_note_not_rendered", "Footnote/endnote text is preserved but not shown", None, true);
            }
        }
        // Runs: character-shape segments split at atom positions.
        let mut runs: Vec<TextRun> = Vec::new();
        let mut max_size = 0i64;
        let mut start = 0usize;
        let mut run_no = 0usize;
        let mut atom_iter = atoms.iter().peekable();
        let style_at = |i: usize| p.char_shape_id_at(i).unwrap_or(0);
        let mut i = 0usize;
        loop {
            while let Some(&&(apos, k)) = atom_iter.peek() {
                if apos != i { break; }
                if i > start {
                    let st = char_style(self.doc, style_at(start));
                    max_size = max_size.max(st.font_size);
                    runs.push(TextRun { style: st, ..TextRun::new(format!("{pid}.r{run_no}"), chars[start..i].iter().collect::<String>()) });
                    run_no += 1;
                }
                start = i;
                let mut apath = path.to_vec();
                apath.push(k);
                let aid = self.id(LocKind::Atom, &apath);
                let st = char_style(self.doc, style_at(i.min(n.saturating_sub(1))));
                runs.push(TextRun { style: st, protected: true, ..TextRun::new(aid, String::new()) });
                atom_iter.next();
            }
            if i >= n { break; }
            if i > start && style_at(i) != style_at(i - 1) {
                let st = char_style(self.doc, style_at(start));
                max_size = max_size.max(st.font_size);
                runs.push(TextRun { style: st, ..TextRun::new(format!("{pid}.r{run_no}"), chars[start..i].iter().collect::<String>()) });
                run_no += 1;
                start = i;
            }
            i += 1;
        }
        if n > start {
            let st = char_style(self.doc, style_at(start));
            max_size = max_size.max(st.font_size);
            runs.push(TextRun { style: st, ..TextRun::new(format!("{pid}.r{run_no}"), chars[start..n].iter().collect::<String>()) });
        }
        if runs.is_empty() {
            let st = char_style(self.doc, style_at(0));
            max_size = st.font_size;
            runs.push(TextRun { style: st, ..TextRun::new(format!("{pid}.r0"), String::new()) });
        }
        if max_size == 0 { max_size = runs.iter().map(|r| r.style.font_size).max().unwrap_or(10 * EMU_PER_PT); }
        let format = self.paragraph_format(p, max_size, &pid);
        let starts_page = format.page_break_before;
        let has_text_or_atoms = n > 0 || !atoms.is_empty();
        let object_only = !has_text_or_atoms && !after.is_empty();
        if object_only {
            // A lone inline (treat-as-character) picture is placed by the paragraph alignment.
            let blocks: Vec<&Control> = p.controls.iter().filter(|c| role(c) == Role::Block).collect();
            if let ([Control::Picture(pic)], [FlowBlock::Image(node)]) = (blocks.as_slice(), after.as_mut_slice()) {
                if pic.common.treat_as_char {
                    let code = self.doc.doc_info.para_shapes.get(p.para_shape_id as usize).map(|ps| alignment_code(&ps.alignment)).unwrap_or(1);
                    node.style.align = match code { 2 => "right", 3 => "center", _ => "left" }.into();
                }
            }
        }
        let mut out = Vec::new();
        if object_only && !starts_page {
            // Object-only paragraph: the anchor paragraph has no visible text. `before` is empty without text.
            self.ids.by_id.remove(&pid);
            self.ids.rebuild();
            out.append(&mut before);
        } else {
            let paragraph = FlowBlock::Paragraph(Paragraph { id: pid, runs, style_ref: None, format });
            if starts_page {
                // The break is carried by the paragraph, so the objects in front of its text follow it onto its page.
                out.push(paragraph);
                out.append(&mut before);
            } else {
                out.append(&mut before);
                out.push(paragraph);
            }
        }
        out.extend(after);
        Ok(out)
    }

    fn control_block(&mut self, c: &Control, cpath: &[usize]) -> DltResult<FlowBlock> {
        match c {
            Control::Table(t) => {
                let tid = self.id(LocKind::Table, cpath);
                let cols = t.col_count.max(1) as usize;
                let mut widths: Vec<Option<i64>> = vec![None; cols];
                let mut irregular = false;
                for cell in &t.cells {
                    if cell.col_span == 1 && (cell.col as usize) < cols {
                        let w = hu(i64::from(cell.width));
                        match widths[cell.col as usize] { None => widths[cell.col as usize] = Some(w), Some(x) if x != w => irregular = true, _ => {} }
                    }
                }
                // Columns only covered by spanning cells share the remaining span width.
                for cell in &t.cells {
                    let (c0, span) = (cell.col as usize, cell.col_span.max(1) as usize);
                    if span > 1 && c0 + span <= cols {
                        let known: i64 = (c0..c0 + span).filter_map(|k| widths[k]).sum();
                        let unknown: Vec<usize> = (c0..c0 + span).filter(|k| widths[*k].is_none()).collect();
                        if !unknown.is_empty() {
                            let rest = (hu(i64::from(cell.width)) - known).max(unknown.len() as i64);
                            for (j, k) in unknown.iter().enumerate() { widths[*k] = Some(rest / unknown.len() as i64 + if j == 0 { rest % unknown.len() as i64 } else { 0 }); }
                        }
                    }
                }
                let column_widths: Vec<i64> = widths.iter().map(|w| w.unwrap_or(10 * EMU_PER_MM).max(1)).collect();
                if irregular { self.diag("hwp_table_irregular_widths", "A table with different cell widths in one column is shown on a regular grid", Some(&tid), false); }
                let mut cells: Vec<(usize, &rhwp::model::table::Cell)> = t.cells.iter().enumerate().collect();
                cells.sort_by_key(|(_, c)| (c.row, c.col));
                let mut rows: Vec<Vec<TableCell>> = vec![Vec::new(); t.row_count.max(1) as usize];
                for (ci, cell) in cells {
                    let mut cell_path = cpath.to_vec();
                    cell_path.push(ci);
                    let cid = self.id(LocKind::Cell, &cell_path);
                    let mut blocks = Vec::new();
                    for (k, cp) in cell.paragraphs.iter().enumerate() {
                        let mut ppath = cell_path.clone();
                        ppath.push(k);
                        blocks.extend(self.paragraph_blocks(cp, &ppath)?);
                    }
                    let row = (cell.row as usize).min(rows.len() - 1);
                    rows[row].push(TableCell { id: cid, blocks, column_span: u32::from(cell.col_span.max(1)), row_span: u32::from(cell.row_span.max(1)) });
                }
                let style = table_style();
                self.diag("hwp_table_borders_approximated", "Table borders and fills are shown with a uniform thin line", None, false);
                Ok(FlowBlock::Table(Table { id: tid, column_widths, rows, style }))
            }
            Control::Picture(pic) => {
                let bin = pic.image_attr.bin_data_id;
                let content = self.doc.bin_data_content.iter().find(|b| b.id == bin);
                match content {
                    Some(b) => {
                        let bytes = b.data.load();
                        let ext = b.extension.to_ascii_lowercase();
                        let mime = match ext.as_str() { "png" => "image/png", "jpg" | "jpeg" => "image/jpeg", "bmp" => "image/bmp", "gif" => "image/gif", "wmf" => "image/wmf", "emf" => "image/emf", _ => "application/octet-stream" };
                        let asset = match &self.assets {
                            Assets::Store(store) => store.put(&bytes, mime)?,
                            Assets::HashOnly => AssetRef { sha256: sha256(&bytes), mime: mime.into(), bytes: bytes.len() as u64 },
                        };
                        self.asset_count += 1;
                        let iid = self.id(LocKind::Image, cpath);
                        if !pic.common.treat_as_char { self.diag("hwp_object_position_approximated", "Floating pictures are shown in text order", Some(&iid), false); }
                        let node = Node { id: iid, kind: NodeKind::Image, bounds: Rect { x: 0, y: 0, width: hu(i64::from(pic.common.width)).max(1), height: hu(i64::from(pic.common.height)).max(1) }, text: String::new(), style: Style { align: "left".into(), ..Style::default() }, asset: Some(asset), children: vec![], layout: None, interactions: vec![], variants: BTreeMap::new(), ext: Default::default() };
                        Ok(FlowBlock::Image(node))
                    }
                    None => {
                        let oid = self.id(LocKind::Opaque, cpath);
                        self.diag("hwp_picture_without_data", "A picture without stored image data is preserved as an object", Some(&oid), false);
                        Ok(FlowBlock::Opaque { id: oid, label: label(c).into() })
                    }
                }
            }
            other => {
                let oid = self.id(LocKind::Opaque, cpath);
                // The message names the object: diagnostics with the same code and message are reported once.
                self.diag(page_losses::OBJECT_NOT_RENDERED, format!("{} {oid} is preserved and shown only as a placeholder", label(other)), Some(&oid), true);
                Ok(FlowBlock::Opaque { id: oid, label: label(other).into() })
            }
        }
    }

    /// Page content of section `s` that visual output does not draw: a page number position, the line or the fill of a
    /// page border fill, and master pages. One blocking loss for each, on section `sid`; the message names the section,
    /// because diagnostics with the same code and message are reported once. The export keeps them for every page
    /// selection (`page_losses::WHOLE_DOCUMENT_LOSSES`).
    fn page_losses(&mut self, s: usize, sid: &str) {
        let doc = self.doc;
        let section = &doc.sections[s];
        let def = &section.section_def;
        let (mut line, mut paint) = (false, false);
        let page_fills = std::iter::once(&def.page_border_fill).chain(def.extra_page_border_fills.iter());
        // `border_fill_id` counts from 1; 0 is no border fill.
        for fill in page_fills.filter_map(|p| usize::from(p.border_fill_id).checked_sub(1).and_then(|i| doc.doc_info.border_fills.get(i))) {
            line |= fill.borders.iter().any(|b| b.line_type != BorderLineType::None);
            paint |= paints(&fill.fill);
        }
        let numbered = section.paragraphs.iter().flat_map(|p| p.controls.iter()).any(|c| matches!(c, Control::PageNumberPos(n) if n.position != 0));
        let losses = [
            (numbered, page_losses::PAGE_NUMBER_NOT_RENDERED, "a page number"),
            (line && !def.hide_border, page_losses::PAGE_BORDER_NOT_RENDERED, "a page border"),
            (paint && !def.hide_fill, page_losses::PAGE_BACKGROUND_NOT_RENDERED, "a page background"),
            (!def.master_pages.is_empty() && !def.hide_master_page, page_losses::MASTER_PAGE_NOT_RENDERED, "master pages"),
        ];
        for (present, code, what) in losses {
            if present { self.diag(code, format!("Section {} has {what}, preserved but not shown", s + 1), Some(sid), true); }
        }
    }
}

/// Whether a fill paints the page: an image or gradient fill, a pattern (`pattern_type` > 0), or a solid color that is
/// neither transparent (a non-zero high byte, as rhwp reads it) nor white.
fn paints(fill: &Fill) -> bool {
    match fill.fill_type {
        FillType::None => false,
        FillType::Solid => fill.solid.is_some_and(|s| s.pattern_type > 0 || (s.background_color >> 24 == 0 && s.background_color != 0xFF_FFFF)),
        FillType::Image | FillType::Gradient => true,
    }
}

/// Project a native document. `prev` keeps ids of existing nodes; `fresh` assigns path-based
/// ids for an initial open, otherwise unknown nodes get new ids.
pub fn project(doc: &Document, prev: Option<&IdMap>, fresh: bool, assets: Assets<'_>) -> DltResult<Projection> {
    let mut b = Builder { doc, prev, ids: IdMap::default(), diags: Vec::new(), seen: BTreeSet::new(), assets, asset_count: 0, fresh };
    let mut sections = Vec::new();
    for (s, section) in doc.sections.iter().enumerate() {
        let sid = b.id(LocKind::Section, &[s]);
        let setup = b.page_setup(s);
        b.page_losses(s, &sid);
        let mut blocks = Vec::new();
        for (pi, p) in section.paragraphs.iter().enumerate() { blocks.extend(b.paragraph_blocks(p, &[s, pi])?); }
        sections.push(Section { id: sid, setup, blocks });
    }
    b.diag("hwp_language_fonts_approximated", "Each run uses its Hangul font for all scripts", None, false);
    Ok(Projection { document: FlowDocument { sections, styles: BTreeMap::new() }, ids: b.ids, diagnostics: b.diags, asset_count: b.asset_count })
}
