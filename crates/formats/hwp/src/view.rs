//! A comparable view of HWP content built twice: from rhwp's model and from the independent
//! record parser. Agreement on open marks the projection trustworthy; agreement after save
//! shows the writer wrote what the model says.
use crate::inspect::{self, Hwp5, RecNode, TextUnit};
use rhwp::model::control::{Control, Field, FieldType};
use rhwp::model::document::Document;
use rhwp::model::paragraph::{ColumnBreakType, Paragraph as RParagraph};
use rhwp::model::style::{Alignment, LineSpacingType};
use rhwp::parser::tags::FIELD_HYPERLINK;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocView { pub sections: Vec<SectionView> }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionView { pub page: Option<PageView>, pub paras: Vec<ParaView> }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PageView { pub width: u32, pub height: u32, pub margin_left: u32, pub margin_right: u32, pub margin_top: u32, pub margin_bottom: u32, pub margin_header: u32, pub margin_footer: u32, pub margin_gutter: u32, pub landscape: bool }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CharStyle {
    pub face: String, pub base_size: i32, pub relative_size: u8, pub bold: bool, pub italic: bool, pub color: u32,
    /// Field id of the innermost hyperlink field around the character (`CONTINUED_FIELD` before the end of a field that
    /// began in an earlier paragraph). The link travels with its characters, so every check that keeps the style of
    /// untouched characters also keeps where the link starts and ends.
    pub link: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParaFmt { pub alignment: u32, pub margin_left: i32, pub margin_right: i32, pub indent: i32, pub spacing_before: i32, pub spacing_after: i32, pub line_spacing_type: u32, pub line_spacing: i64 }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CtrlView { pub id: String, pub pos: usize, pub field: Option<FieldV> }

/// Id of the hyperlink field control. It is the only field the two parsers compare; any other field keeps reading
/// "????" on the rhwp side, so a paragraph that holds one stays read differently and its save is refused.
const HYPERLINK_ID: &str = "%hlk";

/// The CTRL_HEADER of a hyperlink field (HWP 5 table 154): properties, extra properties, the command (target), the field id
/// and the trailing memo index (None when the record ends after the field id; rhwp always writes one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldV { pub properties: u32, pub extra: u8, pub command: String, pub field_id: u32, pub memo_index: Option<u32> }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParaView {
    /// Visible text: tab '\t', line break '\n', hyphen '-', NBSP U+00A0, fixed space U+2007.
    pub text: String,
    /// Extended controls in stream order with the scalar position they precede.
    pub ctrls: Vec<CtrlView>,
    /// Inline 8-unit marks that are not text (field end, title mark, ...): (code, position).
    pub marks: Vec<(u16, usize)>,
    /// Style runs: (start scalar index, style), merged when equal.
    pub styles: Vec<(usize, CharStyle)>,
    pub format: Option<ParaFmt>,
    /// The explicit page break bit (0x04) of the paragraph header: what the saved records must carry.
    pub page_break: bool,
    pub tables: Vec<TableV>,
    pub pictures: Vec<PictureV>,
    /// Why the independent parser cannot verify the paragraph's hyperlink fields (a hyperlink without an end in its
    /// paragraph, nested fields, a shared field id, ...). Only the independent side sets it, so such a paragraph always
    /// reads differently: it is preserved but not editable, and the save check refuses a document that holds it.
    pub unverifiable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableV { pub rows: u16, pub cols: u16, pub cells: Vec<CellV> }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CellV { pub row: u16, pub col: u16, pub row_span: u16, pub col_span: u16, pub width: u32, pub height: u32, pub paras: Vec<ParaView> }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PictureV { pub bin_id: u16, pub width: u32, pub height: u32, pub crop: [i32; 4] }

/// Link value of the characters before a field end whose field began in an earlier paragraph. The end carries no id, and
/// the field may be of any type; the value keeps where the end sits.
const CONTINUED_FIELD: u32 = u32::MAX;

/// Field id of the innermost hyperlink over each of `n` characters. `links` are (start, end, field id) in the order their
/// fields begin (continued fields first), so a later (inner) field overwrites an earlier one.
fn link_per_char(n: usize, links: &[(usize, usize, u32)]) -> Vec<Option<u32>> {
    let mut out = vec![None; n];
    for &(start, end, id) in links {
        for slot in out.iter_mut().take(end).skip(start) { *slot = Some(id); }
    }
    out
}

fn merge_styles(per_char: Vec<CharStyle>, links: &[(usize, usize, u32)]) -> Vec<(usize, CharStyle)> {
    let link = link_per_char(per_char.len(), links);
    let mut out: Vec<(usize, CharStyle)> = Vec::new();
    for (i, mut s) in per_char.into_iter().enumerate() {
        s.link = link[i];
        if out.last().map(|(_, last)| *last == s).unwrap_or(false) { continue; }
        out.push((i, s));
    }
    out
}

pub fn alignment_code(a: &Alignment) -> u32 {
    match a { Alignment::Justify => 0, Alignment::Left => 1, Alignment::Right => 2, Alignment::Center => 3, Alignment::Distribute => 4, Alignment::Split => 5 }
}

pub fn line_spacing_code(t: &LineSpacingType) -> u32 {
    match t { LineSpacingType::Percent => 0, LineSpacingType::Fixed => 1, LineSpacingType::SpaceOnly => 2, LineSpacingType::Minimum => 3 }
}

/// Four-character id rhwp's control corresponds to (None when the variant carries a variable id).
pub fn control_id(c: &Control) -> Option<&'static str> {
    Some(match c {
        Control::SectionDef(_) => "secd", Control::ColumnDef(_) => "cold", Control::Table(_) => "tbl ",
        Control::Shape(_) | Control::Picture(_) => "gso ", Control::Header(_) => "head", Control::Footer(_) => "foot",
        Control::Footnote(_) => "fn  ", Control::Endnote(_) => "en  ", Control::AutoNumber(_) => "atno",
        Control::NewNumber(_) => "nwno", Control::PageNumberPos(_) => "pgnp", Control::Bookmark(_) => "bokm",
        Control::IndexMark(_) => "idxm", Control::PageNumCtrl(_) => "pgct",
        Control::Ruby(_) => "tdut", Control::CharOverlap(_) => "tcps", Control::PageHide(_) => "pghd",
        Control::HiddenComment(_) => "tcmt", Control::Equation(_) => "eqed", Control::Form(_) => "form",
        _ => return None,
    })
}

/// The hyperlink field rhwp read from a `%hlk` control. Other fields are not compared.
fn hyperlink_field(c: &Control) -> Option<&Field> {
    match c {
        Control::Field(f) if f.field_type == FieldType::Hyperlink && f.ctrl_id == FIELD_HYPERLINK => Some(f),
        _ => None,
    }
}

/// Fields open after paragraph `p`, given those `open` where it begins (outermost first, true for a hyperlink): a field end
/// that continues an earlier paragraph closes the latest open field, and a field that does not end in its paragraph stays
/// open.
fn open_after(open: &mut Vec<bool>, p: &RParagraph) {
    for _ in &p.orphan_field_ends { open.pop(); }
    for (k, c) in p.controls.iter().enumerate() {
        if matches!(c, Control::Field(_)) && !p.field_ranges.iter().any(|r| r.control_idx == k) { open.push(hyperlink_field(c).is_some()); }
    }
}

/// Link ranges of paragraph `index` of the list `paras` (a section or a cell); see `links_of`.
pub(crate) fn link_ranges(paras: &[RParagraph], index: usize) -> Vec<(usize, usize, u32)> {
    let mut open = Vec::new();
    for p in &paras[..index] { open_after(&mut open, p); }
    links_of(&paras[index], &open)
}

/// Link ranges of a paragraph in its character positions, (start, end, link value), in the order their links begin, given
/// the fields `open` where it begins. A hyperlink open from an earlier paragraph covers the whole paragraph; a hyperlink's
/// field end that continues an earlier paragraph bounds the link from the paragraph start (only a hyperlink's end:
/// `begin_ctrl_id` names the field). Then each hyperlink that begins here, in control order, up to its end or the
/// paragraph end. The view and the text edit check both read links through it.
fn links_of(p: &RParagraph, open: &[bool]) -> Vec<(usize, usize, u32)> {
    let n = p.text.chars().count();
    let mut open = open.to_vec();
    let mut links = Vec::new();
    for o in &p.orphan_field_ends {
        open.pop();
        if o.begin_ctrl_id == FIELD_HYPERLINK { links.push((0, o.char_idx.min(n), CONTINUED_FIELD)); }
    }
    if open.contains(&true) { links.insert(0, (0, n, CONTINUED_FIELD)); }
    let positions = p.control_text_positions();
    for (k, c) in p.controls.iter().enumerate() {
        let Some(f) = hyperlink_field(c) else { continue };
        let (start, end) = match p.field_ranges.iter().find(|r| r.control_idx == k) {
            Some(r) => (r.start_char_idx, r.end_char_idx),
            None => (positions.get(k).copied().unwrap_or(0).min(n), n),
        };
        links.push((start, end, f.field_id));
    }
    links
}

/// Inline marks of a native paragraph that are not text, sorted: field ends (code 4), including ends that continue an
/// earlier paragraph, and title marks (code 8), as (code, position).
pub(crate) fn native_marks(p: &RParagraph) -> Vec<(u16, usize)> {
    let mut marks: Vec<(u16, usize)> = Vec::new();
    for r in &p.field_ranges { marks.push((4, r.end_char_idx)); }
    for o in &p.orphan_field_ends { marks.push((4, o.char_idx)); }
    for t in &p.title_marks { marks.push((8, t.char_idx)); }
    marks.sort_unstable();
    marks
}

// ------------------------------------------------------------------ rhwp side

pub fn rhwp_view(doc: &Document) -> DocView {
    DocView {
        sections: doc.sections.iter().map(|s| {
            let pd = &s.section_def.page_def;
            SectionView {
                page: Some(PageView { width: pd.width as u32, height: pd.height as u32, margin_left: pd.margin_left as u32, margin_right: pd.margin_right as u32, margin_top: pd.margin_top as u32, margin_bottom: pd.margin_bottom as u32, margin_header: pd.margin_header as u32, margin_footer: pd.margin_footer as u32, margin_gutter: pd.margin_gutter as u32, landscape: pd.landscape }),
                paras: rhwp_paras(doc, &s.paragraphs),
            }
        }).collect(),
    }
}

pub fn rhwp_char_style(doc: &Document, id: u32) -> CharStyle {
    match doc.doc_info.char_shapes.get(id as usize) {
        Some(cs) => CharStyle {
            face: doc.doc_info.font_faces.first().and_then(|f| f.get(cs.font_ids[0] as usize)).map(|f| f.name.clone()).unwrap_or_default(),
            base_size: cs.base_size, relative_size: cs.relative_sizes[0], bold: cs.bold, italic: cs.italic, color: cs.text_color, link: None,
        },
        None => CharStyle { face: String::new(), base_size: 0, relative_size: 0, bold: false, italic: false, color: 0, link: None },
    }
}

pub fn rhwp_para_fmt(doc: &Document, id: u16) -> Option<ParaFmt> {
    doc.doc_info.para_shapes.get(id as usize).map(|ps| ParaFmt {
        alignment: alignment_code(&ps.alignment), margin_left: ps.margin_left, margin_right: ps.margin_right, indent: ps.indent,
        spacing_before: ps.spacing_before, spacing_after: ps.spacing_after, line_spacing_type: line_spacing_code(&ps.line_spacing_type), line_spacing: i64::from(ps.line_spacing),
    })
}

/// The explicit page break bit the HWP writer stores: a non-zero raw break byte is written as it is. Otherwise the
/// column type gives the byte, and a synthesized page break is not written (serializer/body_text.rs).
fn explicit_page_break(p: &RParagraph) -> bool {
    if p.raw_break_type != 0 { p.raw_break_type & 0x04 != 0 } else { p.column_type == ColumnBreakType::Page && !p.page_break_synthesized }
}

/// The paragraphs of a list (a section or a cell). Links continue from earlier paragraphs of the same list only.
fn rhwp_paras(doc: &Document, paras: &[RParagraph]) -> Vec<ParaView> {
    let mut open = Vec::new();
    paras.iter().map(|p| { let v = rhwp_para(doc, p, &open); open_after(&mut open, p); v }).collect()
}

/// A paragraph, given the fields `open` where it begins (see `links_of`).
pub fn rhwp_para(doc: &Document, p: &RParagraph, open: &[bool]) -> ParaView {
    let n = p.text.chars().count();
    let per_char: Vec<CharStyle> = (0..n).map(|i| rhwp_char_style(doc, p.char_shape_id_at(i).unwrap_or(0))).collect();
    let positions = p.control_text_positions();
    let mut ctrls = Vec::new();
    let mut tables = Vec::new();
    let mut pictures = Vec::new();
    for (k, c) in p.controls.iter().enumerate() {
        let field = hyperlink_field(c);
        let id = if field.is_some() { HYPERLINK_ID } else { control_id(c).unwrap_or("????") };
        let field = field.map(|f| FieldV { properties: f.properties, extra: f.extra_properties, command: f.command.clone(), field_id: f.field_id, memo_index: Some(f.memo_index) });
        ctrls.push(CtrlView { id: id.into(), pos: positions.get(k).copied().unwrap_or(0).min(n), field });
        match c {
            Control::Table(t) => {
                let mut cells: Vec<CellV> = t.cells.iter().map(|cell| CellV {
                    row: cell.row, col: cell.col, row_span: cell.row_span, col_span: cell.col_span, width: cell.width as u32, height: cell.height as u32,
                    paras: rhwp_paras(doc, &cell.paragraphs),
                }).collect();
                cells.sort_by_key(|c| (c.row, c.col));
                tables.push(TableV { rows: t.row_count, cols: t.col_count, cells });
            }
            Control::Picture(pic) => pictures.push(PictureV { bin_id: pic.image_attr.bin_data_id, width: pic.common.width as u32, height: pic.common.height as u32, crop: [pic.crop.left, pic.crop.top, pic.crop.right, pic.crop.bottom] }),
            _ => {}
        }
    }
    ParaView { text: p.text.clone(), ctrls, marks: native_marks(p), styles: merge_styles(per_char, &links_of(p, open)), format: rhwp_para_fmt(doc, p.para_shape_id), page_break: explicit_page_break(p), tables, pictures, unverifiable: None }
}

// ------------------------------------------------------------------ independent side

struct Ctx<'a> { h: &'a Hwp5, recs: &'a [inspect::Record], shapes: Vec<inspect::CharShapeView>, para_shapes: Vec<inspect::ParaShapeView>, faces: Vec<Vec<String>> }

pub fn inspect_view(h: &Hwp5) -> Result<DocView, String> {
    let mut sections = Vec::new();
    for path in h.section_paths() {
        let recs = &h.records[&path];
        let tree = inspect::build_tree(recs).map_err(|e| format!("{path}: {e}"))?;
        let ctx = Ctx { h, recs, shapes: h.char_shapes(), para_shapes: h.para_shapes(), faces: h.face_names() };
        let mut page = None;
        let mut paras = Vec::new();
        let mut open = Vec::new();
        for node in &tree {
            if recs[node.index].tag != inspect::TAG_PARA_HEADER { return Err(format!("{path}: top-level record {} is not a paragraph", node.index)); }
            if page.is_none() { page = find_page_def(&ctx, node); }
            paras.push(inspect_para(&ctx, node, &mut open)?);
        }
        sections.push(SectionView { page, paras });
    }
    let _ = h;
    Ok(DocView { sections })
}

fn find_page_def(ctx: &Ctx<'_>, node: &RecNode) -> Option<PageView> {
    for child in &node.children {
        let r = &ctx.recs[child.index];
        if r.tag == inspect::TAG_CTRL_HEADER && r.payload.get(0..4).map(|b| inspect::ctrl_id_string(u32::from_le_bytes(b.try_into().unwrap())) == "secd").unwrap_or(false) {
            for g in &child.children {
                let pr = &ctx.recs[g.index];
                if pr.tag == inspect::TAG_PAGE_DEF {
                    let p = inspect::parse_page_def(&pr.payload)?;
                    return Some(PageView { width: p.width, height: p.height, margin_left: p.margin_left, margin_right: p.margin_right, margin_top: p.margin_top, margin_bottom: p.margin_bottom, margin_header: p.margin_header, margin_footer: p.margin_footer, margin_gutter: p.margin_gutter, landscape: p.landscape() });
                }
            }
        }
    }
    None
}

fn style_of(ctx: &Ctx<'_>, id: u32) -> CharStyle {
    match ctx.shapes.get(id as usize) {
        Some(cs) => CharStyle {
            face: ctx.faces.first().and_then(|f| f.get(cs.face_ids[0] as usize)).cloned().unwrap_or_default(),
            base_size: cs.base_size, relative_size: cs.relative_sizes[0], bold: cs.bold(), italic: cs.italic(), color: cs.text_color, link: None,
        },
        None => CharStyle { face: String::new(), base_size: 0, relative_size: 0, bold: false, italic: false, color: 0, link: None },
    }
}

/// A paragraph. `open` holds the fields open where it begins (outermost first, true for a hyperlink) and is left with
/// those open where it ends; fields continue within one paragraph list (a section or a cell).
fn inspect_para(ctx: &Ctx<'_>, node: &RecNode, open: &mut Vec<bool>) -> Result<ParaView, String> {
    let header = inspect::para_header(&ctx.recs[node.index].payload).ok_or("short PARA_HEADER")?;
    let mut text = String::new();
    let mut utf16_of_char: Vec<u32> = Vec::new();
    let mut ctrls = Vec::new();
    let mut marks = Vec::new();
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut ctrl_nodes: Vec<&RecNode> = Vec::new();
    // Field begins (code 3) still open: (position, ordinal of the begin control, is a hyperlink). A field end (code 4)
    // closes the latest.
    let mut open_fields: Vec<(usize, usize, bool)> = Vec::new();
    // The first hyperlink structure the check cannot verify.
    let mut unverifiable: Option<String> = None;
    let mut refuse = |reason: &str| { if unverifiable.is_none() { unverifiable = Some(reason.to_string()); } };
    // Closed fields: (begin position, end position, ordinal of the begin control).
    let mut field_pairs: Vec<(usize, usize, usize)> = Vec::new();
    // Field ends whose field began in an earlier paragraph: (position, the end names the hyperlink id).
    let mut continued: Vec<(usize, bool)> = Vec::new();
    for child in &node.children {
        let r = &ctx.recs[child.index];
        match r.tag {
            inspect::TAG_PARA_TEXT => {
                let mut pos16 = 0u32;
                for unit in inspect::text_units(&r.payload) {
                    match unit {
                        TextUnit::Char(c) => { utf16_of_char.push(pos16); text.push(c); pos16 += c.len_utf16() as u32; }
                        TextUnit::Mark(m) => {
                            let visible = match m { 10 => Some('\n'), 24 => Some('-'), 30 => Some('\u{00A0}'), 31 => Some('\u{2007}'), _ => None };
                            if let Some(c) = visible { utf16_of_char.push(pos16); text.push(c); }
                            pos16 += 1;
                        }
                        TextUnit::Ctrl { code, id, .. } => {
                            if code == 9 { utf16_of_char.push(pos16); text.push('\t'); }
                            else if inspect::is_extended(code) {
                                let pos = text.chars().count();
                                let id = inspect::ctrl_id_string(id);
                                let hyperlink = id == HYPERLINK_ID;
                                if hyperlink && code != 3 { refuse("a hyperlink control is not a field begin"); }
                                if code == 3 {
                                    if open_fields.iter().any(|f| f.2 || hyperlink) { refuse("a hyperlink field is nested with another field"); }
                                    open_fields.push((pos, ctrls.len(), hyperlink));
                                }
                                ctrls.push(CtrlView { id, pos, field: None });
                            } else {
                                let pos = text.chars().count();
                                if code == 4 {
                                    match open_fields.pop() {
                                        Some((start, k, _)) => field_pairs.push((start, pos, k)),
                                        None => { open.pop(); continued.push((pos, inspect::ctrl_id_string(id) == HYPERLINK_ID)); }
                                    }
                                }
                                marks.push((code, pos));
                            }
                            pos16 += 8;
                        }
                    }
                }
            }
            inspect::TAG_PARA_CHAR_SHAPE => runs = r.payload.chunks_exact(8).map(|c| (u32::from_le_bytes(c[0..4].try_into().unwrap()), u32::from_le_bytes(c[4..8].try_into().unwrap()))).collect(),
            inspect::TAG_CTRL_HEADER => ctrl_nodes.push(child),
            _ => {}
        }
    }
    let shape_at = |pos16: u32| -> u32 {
        let mut id = runs.first().map(|r| r.1).unwrap_or(0);
        for &(start, sid) in &runs { if start <= pos16 { id = sid; } else { break; } }
        id
    };
    let per_char: Vec<CharStyle> = utf16_of_char.iter().map(|&p| style_of(ctx, shape_at(p))).collect();
    // A hyperlink is read from its own CTRL_HEADER: the k-th extended control owns the k-th header.
    let hyperlinks = ctrls.iter().filter(|c| c.id == HYPERLINK_ID).count();
    if hyperlinks > 0 && ctrls.len() != ctrl_nodes.len() { refuse("the controls do not match their CTRL_HEADER records"); }
    for (c, n) in ctrls.iter_mut().zip(&ctrl_nodes) {
        if c.id != HYPERLINK_ID { continue; }
        c.field = hyperlink_header(&ctx.recs[n.index].payload);
        if c.field.is_none() { refuse("a hyperlink CTRL_HEADER cannot be read"); }
    }
    if open_fields.iter().any(|f| f.2) { refuse("a hyperlink field has no end in its paragraph"); }
    if continued.len() > 1 { refuse("more than one field end continues an earlier paragraph"); }
    let mut ids: Vec<u32> = ctrls.iter().filter_map(|c| c.field.as_ref().map(|f| f.field_id)).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) { refuse("two hyperlink fields share a field id"); }
    // Links in the order they begin: a hyperlink still open from an earlier paragraph covers the paragraph, a hyperlink's
    // end that continues an earlier paragraph bounds it from the start, then the hyperlinks that begin here, up to their
    // end or the paragraph end. Fields that do not end here stay open for the next paragraph.
    let n = text.chars().count();
    let mut links: Vec<(usize, usize, u32)> = Vec::new();
    if open.contains(&true) { links.push((0, n, CONTINUED_FIELD)); }
    links.extend(continued.iter().filter(|c| c.1).map(|c| (0, c.0, CONTINUED_FIELD)));
    open.extend(open_fields.iter().map(|f| f.2));
    field_pairs.extend(open_fields.iter().map(|&(start, k, _)| (start, n, k)));
    field_pairs.sort_unstable_by_key(|&(_, _, k)| k);
    links.extend(field_pairs.iter().filter_map(|&(start, end, k)| Some((start, end, ctrls.get(k)?.field.as_ref()?.field_id))));
    let mut tables = Vec::new();
    let mut pictures = Vec::new();
    for c in ctrl_nodes {
        let r = &ctx.recs[c.index];
        let id = r.payload.get(0..4).map(|b| inspect::ctrl_id_string(u32::from_le_bytes(b.try_into().unwrap()))).unwrap_or_default();
        if id == "tbl " { tables.push(inspect_table(ctx, c)?); }
        if id == "gso " {
            if let Some(pic) = find_picture(ctx, c) {
                let w = r.payload.get(16..20).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
                let hgt = r.payload.get(20..24).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
                let i32_at = |o: usize| pic.get(o..o + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
                pictures.push(PictureV { bin_id: inspect::picture_bin_id(pic).unwrap_or(0), width: w, height: hgt, crop: [i32_at(44), i32_at(48), i32_at(52), i32_at(56)] });
            }
        }
    }
    let format = ctx.para_shapes.get(header.para_shape_id as usize).map(|ps| ParaFmt {
        alignment: ps.alignment(), margin_left: ps.margin_left, margin_right: ps.margin_right, indent: ps.indent, spacing_before: ps.spacing_before, spacing_after: ps.spacing_after,
        line_spacing_type: ps.line_spacing_type(), line_spacing: ps.line_spacing_value(),
    });
    marks.sort_unstable();
    let _ = ctx.h;
    Ok(ParaView { text, ctrls, marks, styles: merge_styles(per_char, &links), format, page_break: header.break_type & 0x04 != 0, tables, pictures, unverifiable })
}

/// The hyperlink field of a CTRL_HEADER payload: id `%hlk`, properties (4), extra properties (1), command length (2), the
/// UTF-16 command, field id (4) and an optional memo index (4). Any other id or length is not a readable hyperlink.
fn hyperlink_header(p: &[u8]) -> Option<FieldV> {
    let u32_at = |o: usize| p.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if inspect::ctrl_id_string(u32_at(0)?) != HYPERLINK_ID { return None; }
    let properties = u32_at(4)?;
    let extra = *p.get(8)?;
    let units = usize::from(u16::from_le_bytes([*p.get(9)?, *p.get(10)?]));
    let id_at = 11 + 2 * units;
    let command: Vec<u16> = p.get(11..id_at)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let field_id = u32_at(id_at)?;
    let memo_index = match p.len() - (id_at + 4) { 0 => None, 4 => Some(u32_at(id_at + 4)?), _ => return None };
    Some(FieldV { properties, extra, command: String::from_utf16_lossy(&command), field_id, memo_index })
}

/// SHAPE_COMPONENT_PICTURE payload of a gso control (one that carries a BinData id).
fn find_picture<'a>(ctx: &Ctx<'a>, node: &RecNode) -> Option<&'a [u8]> {
    for child in &node.children {
        let r = &ctx.recs[child.index];
        if r.tag == inspect::TAG_SHAPE_COMPONENT_PICTURE { return inspect::picture_bin_id(&r.payload).map(|_| r.payload.as_slice()); }
        if let Some(found) = find_picture(ctx, child) { return Some(found); }
    }
    None
}

fn inspect_table(ctx: &Ctx<'_>, node: &RecNode) -> Result<TableV, String> {
    let mut rows = 0;
    let mut cols = 0;
    let mut cells: Vec<CellV> = Vec::new();
    // The cell being read and the fields open in its paragraph list.
    let mut pending: Option<(CellV, Vec<bool>)> = None;
    for child in &node.children {
        let r = &ctx.recs[child.index];
        match r.tag {
            inspect::TAG_TABLE => { if let Some(t) = inspect::parse_table(&r.payload) { rows = t.rows; cols = t.cols; } }
            inspect::TAG_LIST_HEADER => {
                if let Some((cell, _)) = pending.take() { cells.push(cell); }
                let c = inspect::parse_cell(&r.payload).ok_or("short cell LIST_HEADER")?;
                pending = Some((CellV { row: c.row, col: c.col, row_span: c.row_span, col_span: c.col_span, width: c.width, height: c.height, paras: vec![] }, Vec::new()));
            }
            inspect::TAG_PARA_HEADER => {
                if let Some((cell, open)) = pending.as_mut() { cell.paras.push(inspect_para(ctx, child, open)?); }
            }
            _ => {}
        }
    }
    if let Some((cell, _)) = pending.take() { cells.push(cell); }
    cells.sort_by_key(|c| (c.row, c.col));
    Ok(TableV { rows, cols, cells })
}

/// Human-readable first difference between two views (None when equal).
pub fn first_difference(a: &DocView, b: &DocView) -> Option<String> {
    if a == b { return None; }
    if a.sections.len() != b.sections.len() { return Some(format!("section count {} vs {}", a.sections.len(), b.sections.len())); }
    for (si, (x, y)) in a.sections.iter().zip(&b.sections).enumerate() {
        if x.page != y.page { return Some(format!("section {si} page definition differs: {:?} vs {:?}", x.page, y.page)); }
        if x.paras.len() != y.paras.len() { return Some(format!("section {si} paragraph count {} vs {}", x.paras.len(), y.paras.len())); }
        for (pi, (p, q)) in x.paras.iter().zip(&y.paras).enumerate() {
            if let Some(d) = para_difference(p, q) { return Some(format!("section {si} paragraph {pi}: {d}")); }
        }
    }
    Some("views differ".into())
}

pub fn para_difference(p: &ParaView, q: &ParaView) -> Option<String> {
    if p.unverifiable != q.unverifiable {
        let reason = p.unverifiable.as_deref().or(q.unverifiable.as_deref()).unwrap_or_default();
        return Some(format!("hyperlink fields cannot be verified: {reason}"));
    }
    if p.text != q.text { return Some(format!("text {:?} vs {:?}", p.text, q.text)); }
    if p.ctrls != q.ctrls { return Some(format!("controls {:?} vs {:?}", p.ctrls, q.ctrls)); }
    if p.marks != q.marks { return Some(format!("inline marks {:?} vs {:?}", p.marks, q.marks)); }
    if p.styles != q.styles { return Some(format!("character styles {:?} vs {:?}", p.styles, q.styles)); }
    if p.format != q.format { return Some(format!("paragraph format {:?} vs {:?}", p.format, q.format)); }
    if p.page_break != q.page_break { return Some(format!("explicit page break {} vs {}", p.page_break, q.page_break)); }
    if p.pictures != q.pictures { return Some(format!("pictures {:?} vs {:?}", p.pictures, q.pictures)); }
    if p.tables.len() != q.tables.len() { return Some(format!("table count {} vs {}", p.tables.len(), q.tables.len())); }
    for (ti, (s, t)) in p.tables.iter().zip(&q.tables).enumerate() {
        if (s.rows, s.cols, s.cells.len()) != (t.rows, t.cols, t.cells.len()) { return Some(format!("table {ti} grid {}x{}/{} vs {}x{}/{}", s.rows, s.cols, s.cells.len(), t.rows, t.cols, t.cells.len())); }
        for (ci, (c, d)) in s.cells.iter().zip(&t.cells).enumerate() {
            if (c.row, c.col, c.row_span, c.col_span, c.width, c.height) != (d.row, d.col, d.row_span, d.col_span, d.width, d.height) {
                return Some(format!("table {ti} cell {ci} geometry ({},{} span {}x{} size {}x{}) vs ({},{} span {}x{} size {}x{})", c.row, c.col, c.row_span, c.col_span, c.width, c.height, d.row, d.col, d.row_span, d.col_span, d.width, d.height));
            }
            if c.paras.len() != d.paras.len() { return Some(format!("table {ti} cell {ci} paragraph count {} vs {}", c.paras.len(), d.paras.len())); }
            for (k, (x, y)) in c.paras.iter().zip(&d.paras).enumerate() {
                if let Some(diff) = para_difference(x, y) { return Some(format!("table {ti} cell {ci} paragraph {k}: {diff}")); }
            }
        }
    }
    None
}

#[cfg(test)]
mod hyperlink_header_tests {
    use super::{hyperlink_header, FieldV};

    /// A `%hlk` CTRL_HEADER with `units` as the stored command length, then `command`, the field id and `tail`.
    fn header(units: u16, command: &str, tail: &[u8]) -> Vec<u8> {
        let mut p = u32::from_be_bytes(*b"%hlk").to_le_bytes().to_vec();
        p.extend_from_slice(&7u32.to_le_bytes());
        p.push(2);
        p.extend_from_slice(&units.to_le_bytes());
        for u in command.encode_utf16() { p.extend_from_slice(&u.to_le_bytes()); }
        p.extend_from_slice(&41u32.to_le_bytes());
        p.extend_from_slice(tail);
        p
    }

    #[test]
    fn a_hyperlink_header_is_read_with_or_without_the_memo_index() {
        let want = |memo_index| Some(FieldV { properties: 7, extra: 2, command: "https://a.example".into(), field_id: 41, memo_index });
        assert_eq!(hyperlink_header(&header(17, "https://a.example", &[])), want(None));
        assert_eq!(hyperlink_header(&header(17, "https://a.example", &9u32.to_le_bytes())), want(Some(9)));
        // A record without the memo index and one with a zero memo index are different records.
        assert_eq!(hyperlink_header(&header(17, "https://a.example", &0u32.to_le_bytes())), want(Some(0)));
        assert_ne!(want(Some(0)), want(None));
    }

    #[test]
    fn a_forged_length_or_id_is_not_a_hyperlink() {
        // The stored length runs past the record, or leaves bytes the layout does not explain.
        assert_eq!(hyperlink_header(&header(40, "https://a.example", &[])), None);
        assert_eq!(hyperlink_header(&header(16, "https://a.example", &[])), None);
        assert_eq!(hyperlink_header(&header(17, "https://a.example", &[0; 2])), None);
        assert_eq!(hyperlink_header(&header(17, "https://a.example", &[0; 8])), None);
        // Short records and other field ids.
        assert_eq!(hyperlink_header(&header(17, "https://a.example", &[])[..12]), None);
        let mut other = header(17, "https://a.example", &[]);
        other[0..4].copy_from_slice(&u32::from_be_bytes(*b"%hlK").to_le_bytes());
        assert_eq!(hyperlink_header(&other), None);
    }
}

#[cfg(test)]
mod link_range_tests {
    use super::{link_ranges, CONTINUED_FIELD};
    use rhwp::document_core::hyperlink::HyperlinkTarget;
    use rhwp::model::paragraph::OrphanFieldEnd;
    use rhwp::parser::tags::{FIELD_CLICKHERE, FIELD_HYPERLINK};

    /// Three paragraphs "Hello World!", "가나다", "라마바" with a hyperlink field that begins over "He" and, after
    /// `change`, may run on into the next paragraphs.
    fn three_paragraphs(change: impl FnOnce(&mut Vec<rhwp::model::paragraph::Paragraph>)) -> Vec<rhwp::model::paragraph::Paragraph> {
        let bytes = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello_world.hwp")).unwrap();
        let mut core = crate::edit::open_core(&bytes).unwrap();
        let length = core.document().sections[0].paragraphs[0].text.chars().count();
        core.split_paragraph_native(0, 0, length, None).unwrap();
        core.split_paragraph_native(0, 1, 0, None).unwrap();
        core.insert_text_native(0, 1, 0, "가나다").unwrap();
        core.insert_text_native(0, 2, 0, "라마바").unwrap();
        core.insert_hyperlink_native(&HyperlinkTarget::body(0, 0), 0, 2, "https://example.com/").unwrap();
        let mut paras = core.document().sections[0].paragraphs[..3].to_vec();
        change(&mut paras);
        paras
    }

    fn end(at: usize, begin: u32) -> OrphanFieldEnd { OrphanFieldEnd { char_idx: at, begin_ctrl_id: begin, ..Default::default() } }

    #[test]
    fn a_link_over_three_paragraphs_has_its_range_in_each() {
        let paras = three_paragraphs(|p| {
            p[0].field_ranges.clear();
            p[2].orphan_field_ends.push(end(2, FIELD_HYPERLINK));
        });
        let ranges: Vec<_> = (0..3).map(|i| link_ranges(&paras, i).into_iter().map(|r| (r.0, r.1, r.2 == CONTINUED_FIELD)).collect::<Vec<_>>()).collect();
        assert_eq!(ranges, [vec![(0, 12, false)], vec![(0, 3, true)], vec![(0, 2, true)]]);
    }

    #[test]
    fn a_field_end_of_another_field_is_not_a_link() {
        let paras = three_paragraphs(|p| p[1].orphan_field_ends.push(end(2, FIELD_CLICKHERE)));
        assert_eq!(link_ranges(&paras, 1), []);
        assert_eq!(link_ranges(&paras, 0).len(), 1, "the closed link of the first paragraph");
        assert_eq!(link_ranges(&paras, 2), [], "a closed link does not run on");
    }
}
