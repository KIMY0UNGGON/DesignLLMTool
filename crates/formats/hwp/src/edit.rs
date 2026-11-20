//! Native HWP edits. Operations become a replayable log of rhwp commands with native
//! coordinates; each command updates the guard plan so the saved file can be checked.
use crate::guard::{NamedStylePlan, PicturePlan, Plan};
use crate::inspect;
use crate::project::{self, IdMap, LocKind, NodeLoc};
use crate::view::{self, CharStyle, DocView, ParaView};
use designllm_contracts::*;
use designllm_contracts::media_header::{self, png_size, FIGURE_REPRESENTATIONS};
use designllm_flow::{find_paragraph, LogicalParagraph};
use rhwp::model::control::Control;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub type Core = rhwp::DocumentCore;

/// Paragraph address: top level, or one level inside a table cell (control, cell, paragraph).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParaAt { pub section: usize, pub para: usize, #[serde(default)] pub cell: Option<[usize; 3]> }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum NativeEdit {
    /// Replace `delete` characters at scalar `offset` (control characters excluded) with `text`.
    Text { at: ParaAt, offset: usize, delete: usize, text: String },
    CharFormat { at: ParaAt, start: usize, end: usize, props: Value },
    ParaFormat { at: ParaAt, props: Value },
    /// Makes the top-level paragraph `at` start a new page: its header gets the explicit page break bit (0x04).
    PageBreak { at: ParaAt },
    PageDef { section: usize, props: Value },
    ColumnWidths { section: usize, para: usize, control: usize, widths: Vec<u32> },
    InsertParagraph { section: usize, para: usize },
    DeleteParagraph { section: usize, para: usize },
    /// Table host paragraph at `para` plus the empty paragraph HWP keeps after a table.
    InsertTable { section: usize, para: usize, rows: u16, cols: u16 },
    /// New paragraph at `para` holding one inline (treat-as-character) picture of `asset`,
    /// `width` x `height` HWPUNIT. The pixel size is read from the image bytes.
    InsertPicture { section: usize, para: usize, asset: AssetRef, width: u32, height: u32, px_width: u32, px_height: u32, extension: String },
    /// Picture control `control` of top-level paragraph `para` shows `asset` at `width` x `height`
    /// HWPUNIT. Original BinData is never rewritten: an original picture gets a new BinData item;
    /// an item added earlier in this edit log is replaced in place.
    SetPicture { section: usize, para: usize, control: usize, asset: AssetRef, width: u32, height: u32 },
    /// Named paragraph style `style` (STYLE index) gets a new character shape: a copy of its current
    /// one with `props` (character format properties) applied, always appended. The style and the
    /// runs that follow it (paragraphs of that style, runs on the style's previous shape, in the
    /// body and in table cells) point to the copy; every other run keeps its shape.
    NamedStyleChar { style: usize, props: Value },
}

/// Named style changes one edit history may hold: each appends one character shape.
pub const MAX_NAMED_STYLE_SHAPES: usize = 64;

/// Inline placement of an inserted block picture (as Hancom stores 글자처럼 취급 pictures).
const INLINE_PICTURE: &str = r#"{"treatAsChar":true,"vertRelTo":"Para","horzRelTo":"Para","textWrap":"TopAndBottom","restrictInPage":true,"vertOffset":0,"horzOffset":0}"#;

/// HWP extension of an insertable image type.
pub fn image_extension(mime: &str) -> Option<&'static str> {
    match mime { "image/png" => Some("png"), "image/jpeg" => Some("jpg"), "image/bmp" => Some("bmp"), "image/gif" => Some("gif"), _ => None }
}

/// The table's common attributes before a width change, when its saved CTRL_HEADER bytes are still valid for the
/// model (raw bytes present, seal matches the common attributes). Only then can the width change keep those bytes:
/// rhwp re-serializes a table whose seal does not match, which writes the optional prevent-page-break field, so a
/// saved 42-byte CTRL_HEADER grows to 46 bytes.
fn table_raw_ctrl_kept(core: &Core, section: usize, para: usize, control: usize) -> Option<rhwp::model::shape::CommonObjAttr> {
    match core.document().sections.get(section).and_then(|s| s.paragraphs.get(para)).and_then(|p| p.controls.get(control)) {
        // rhwp's sync_ctrl_dimensions writes width and height into raw bytes 12..20 only when the raw bytes reach 20.
        Some(Control::Table(t)) if t.raw_ctrl_data.len() >= 20 && t.raw_ctrl_seal.is_some_and(|seal| seal == rhwp::model::raw_provenance::record_digest(&t.common)) => Some(t.common.clone()),
        _ => None,
    }
}

/// Whether two common attribute sets differ only in width and height.
fn same_except_size(a: &rhwp::model::shape::CommonObjAttr, b: &rhwp::model::shape::CommonObjAttr) -> bool {
    let mut a = a.clone();
    a.width = b.width;
    a.height = b.height;
    rhwp::model::raw_provenance::record_digest(&a) == rhwp::model::raw_provenance::record_digest(b)
}

/// After a width change on a table whose raw CTRL_HEADER was kept: rhwp has written the new width and height into
/// those raw bytes, so the seal follows the new common attributes and the raw bytes are written again. Only when
/// the width change touched nothing else; otherwise the seal stays stale and rhwp writes the model.
fn reseal_table_raw_ctrl(core: &mut Core, section: usize, para: usize, control: usize, before: &rhwp::model::shape::CommonObjAttr) {
    if let Some(Control::Table(t)) = core.document_mut().sections.get_mut(section).and_then(|s| s.paragraphs.get_mut(para)).and_then(|p| p.controls.get_mut(control)) {
        if same_except_size(&t.common, before) {
            t.raw_ctrl_seal = Some(rhwp::model::raw_provenance::record_digest(&t.common));
        }
    }
}

/// Image bytes of an asset, checked against the asset hash, and their pixel size.
fn image_bytes(assets: &dyn AssetStore, asset: &AssetRef) -> DltResult<(Vec<u8>, u32, u32)> {
    let data = assets.read(asset)?;
    if sha256(&data) != asset.sha256 || data.len() as u64 != asset.bytes { return Err(DltError::new(ErrorCode::IntegrityError, "Picture asset bytes do not match their hash")); }
    let (w, h) = designllm_render::image_pixel_size(&data).filter(|(w, h)| *w > 0 && *h > 0).ok_or_else(|| DltError::invalid(format!("The {} image has no readable pixel size", asset.mime)))?;
    Ok((data, w, h))
}

fn picture_mut(doc: &mut rhwp::model::document::Document, section: usize, para: usize, control: usize) -> DltResult<&mut rhwp::model::image::Picture> {
    match doc.sections.get_mut(section).and_then(|s| s.paragraphs.get_mut(para)).and_then(|p| p.controls.get_mut(control)) {
        Some(Control::Picture(pic)) => Ok(pic),
        _ => Err(DltError::new(ErrorCode::IntegrityError, "HWP picture mapping does not point to a picture")),
    }
}

/// Fresh-picture geometry for a new image: original = current = frame, corner points on the
/// frame, identity rendering, rotation centre in the middle, crop and image extent covering the
/// whole image (pixels x 75 HWPUNIT, rhwp's insertion contract). Requires no rotation or flip.
fn normalize_picture(doc: &mut rhwp::model::document::Document, section: usize, para: usize, control: usize, px_width: u32, px_height: u32) -> DltResult<()> {
    let pic = picture_mut(doc, section, para, control)?;
    let (w, h) = (pic.common.width, pic.common.height);
    let a = &mut pic.shape_attr;
    a.original_width = w; a.original_height = h; a.current_width = w; a.current_height = h;
    a.rotation_center = rhwp::model::Point { x: (w / 2) as i32, y: (h / 2) as i32 };
    a.raw_rendering.clear();
    a.render_tx = 0.0; a.render_ty = 0.0; a.render_sx = 1.0; a.render_sy = 1.0; a.render_b = 0.0; a.render_c = 0.0;
    pic.border_x = [0, 0, w as i32, 0];
    pic.border_y = [w as i32, h as i32, 0, h as i32];
    let dim = (px_width.saturating_mul(75), px_height.saturating_mul(75));
    pic.crop = rhwp::model::image::CropInfo { left: 0, top: 0, right: dim.0 as i32, bottom: dim.1 as i32 };
    pic.img_dim = dim;
    match pic.raw_picture_extra.len() {
        0 => {}
        17 | 18 => { pic.raw_picture_extra[9..13].copy_from_slice(&dim.0.to_le_bytes()); pic.raw_picture_extra[13..17].copy_from_slice(&dim.1.to_le_bytes()); }
        n => return Err(DltError::unsupported(format!("The picture record has {n} tail bytes (image effects); its image cannot be replaced"))),
    }
    doc.sections[section].raw_stream = None;
    Ok(())
}

/// BinData positions referenced by pictures in the body and in table cells.
fn picture_refs(doc: &rhwp::model::document::Document) -> std::collections::BTreeSet<u16> {
    fn walk(paras: &[rhwp::model::paragraph::Paragraph], out: &mut std::collections::BTreeSet<u16>) {
        for p in paras {
            for c in &p.controls {
                match c {
                    Control::Picture(pic) => { out.insert(pic.image_attr.bin_data_id); }
                    Control::Table(t) => for cell in &t.cells { walk(&cell.paragraphs, out); },
                    _ => {}
                }
            }
        }
    }
    let mut out = std::collections::BTreeSet::new();
    for s in &doc.sections { walk(&s.paragraphs, &mut out); }
    out
}

/// Number of trailing BinData items that edits added and no picture uses any more (a removed
/// inserted picture, or an original picture shown again with its own image).
pub fn unused_added_bin_data(doc: &rhwp::model::document::Document, original: usize) -> usize {
    let used = picture_refs(doc);
    let n = doc.doc_info.bin_data_list.len();
    if doc.bin_data_content.len() != n { return 0; }
    (original..n).rev().take_while(|k| !used.contains(&((k + 1) as u16))).count()
}

/// Drop those trailing items. Original items and anything followed by a used item stay, so no
/// picture reference changes. Returns the number removed.
pub fn prune_added_bin_data(doc: &mut rhwp::model::document::Document, original: usize) -> usize {
    let removed = unused_added_bin_data(doc, original);
    for _ in 0..removed {
        doc.doc_info.bin_data_list.pop();
        doc.bin_data_content.pop();
    }
    if removed > 0 { doc.doc_info.raw_stream = None; }
    removed
}

pub fn open_core(bytes: &[u8]) -> DltResult<Core> {
    rhwp::service::DocumentService::new()
        .with_title_scan(false)
        .with_max_bytes(Some(512 * 1024 * 1024))
        .open_bytes(bytes, &rhwp::service::OpenOptions::new())
        .map(|o| o.into_core())
        .map_err(|e| DltError::invalid(format!("The HWP file cannot be opened: {e}")))
}

fn native_err(context: &str, e: impl std::fmt::Display) -> DltError {
    DltError::new(ErrorCode::UnsupportedOperation, format!("{context}: {e}"))
}

fn shift(set: &mut std::collections::BTreeSet<(usize, usize)>, section: usize, from: usize, delta: isize) {
    let moved: Vec<(usize, usize)> = set.iter().filter(|(s, p)| *s == section && *p >= from).copied().collect();
    for k in &moved { set.remove(k); }
    for (s, p) in moved {
        let np = p as isize + delta;
        if np >= 0 { set.insert((s, np as usize)); }
    }
}

/// Move picture entries of paragraphs >= `from` in `section` by `delta`; drop those of `removed`.
fn shift_pictures(plan: &mut Plan, section: usize, from: usize, delta: isize, removed: Option<usize>) {
    let moved = |k: &(usize, usize, usize)| -> Option<(usize, usize, usize)> {
        if k.0 != section || k.1 < from { return Some(*k); }
        if Some(k.1) == removed { return None; }
        usize::try_from(k.1 as isize + delta).ok().map(|p| (k.0, p, k.2))
    };
    plan.pictures_replaced = plan.pictures_replaced.iter().filter(|k| Some(k.1) != removed || k.0 != section).filter_map(moved).collect();
    plan.pictures = std::mem::take(&mut plan.pictures).into_iter().filter(|(k, _)| Some(k.1) != removed || k.0 != section).filter_map(|(k, v)| moved(&k).map(|k| (k, v))).collect();
}

fn plan_insert(plan: &mut Plan, section: usize, at: usize) {
    if let Some(o) = plan.origin.get_mut(section) { o.insert(at.min(o.len()), None); }
    for set in [&mut plan.edited, &mut plan.subtree_edited, &mut plan.table_resized, &mut plan.page_break_edited] { shift(set, section, at, 1); }
    shift_pictures(plan, section, at, 1, None);
}

fn plan_delete(plan: &mut Plan, section: usize, at: usize) {
    if let Some(o) = plan.origin.get_mut(section) {
        if at < o.len() {
            if let Some(orig) = o.remove(at) { plan.deleted.insert((section, orig)); }
        }
    }
    for set in [&mut plan.edited, &mut plan.subtree_edited, &mut plan.table_resized, &mut plan.page_break_edited] {
        set.remove(&(section, at));
        shift(set, section, at + 1, -1);
    }
    shift_pictures(plan, section, at + 1, -1, Some(at));
}

fn mark_paragraph(plan: &mut Plan, at: &ParaAt) {
    if at.cell.is_some() { plan.subtree_edited.insert((at.section, at.para)); } else { plan.edited.insert((at.section, at.para)); }
    plan.docinfo_append.insert(inspect::TAG_CHAR_SHAPE);
    plan.docinfo_append.insert(inspect::TAG_PARA_SHAPE);
}

/// Apply one native edit to rhwp and record the permitted changes in `plan`.
pub fn apply_native(core: &mut Core, edit: &NativeEdit, assets: &dyn AssetStore, plan: &mut Plan) -> DltResult<()> {
    match edit {
        NativeEdit::Text { at, offset, delete, text } => {
            mark_paragraph(plan, at);
            match at.cell {
                None => {
                    if *delete > 0 && !text.is_empty() { core.replace_text_native(at.section, at.para, *offset, *delete, text).map_err(|e| native_err("text replace", e))?; }
                    else if *delete > 0 { core.delete_text_native(at.section, at.para, *offset, *delete).map_err(|e| native_err("text delete", e))?; }
                    else if !text.is_empty() { core.insert_text_native(at.section, at.para, *offset, text).map_err(|e| native_err("text insert", e))?; }
                }
                Some([c, cell, cp]) => {
                    if *delete > 0 { core.delete_text_in_cell_native(at.section, at.para, c, cell, cp, *offset, *delete).map_err(|e| native_err("cell text delete", e))?; }
                    if !text.is_empty() { core.insert_text_in_cell_native(at.section, at.para, c, cell, cp, *offset, text).map_err(|e| native_err("cell text insert", e))?; }
                }
            }
        }
        NativeEdit::CharFormat { at, start, end, props } => {
            mark_paragraph(plan, at);
            plan.docinfo_append.insert(inspect::TAG_FACE_NAME);
            let json = props.to_string();
            match at.cell {
                None => { core.apply_char_format_native(at.section, at.para, *start, *end, &json).map_err(|e| native_err("character format", e))?; }
                Some([c, cell, cp]) => { core.apply_char_format_in_cell_native(at.section, at.para, c, cell, cp, *start, *end, &json).map_err(|e| native_err("cell character format", e))?; }
            }
        }
        NativeEdit::ParaFormat { at, props } => {
            mark_paragraph(plan, at);
            let json = props.to_string();
            match at.cell {
                None => { core.apply_para_format_native(at.section, at.para, &json).map_err(|e| native_err("paragraph format", e))?; }
                Some([c, cell, cp]) => { core.apply_para_format_in_cell_native(at.section, at.para, c, cell, cp, &json).map_err(|e| native_err("cell paragraph format", e))?; }
            }
        }
        NativeEdit::PageBreak { at } => {
            if at.cell.is_some() { return Err(unsupported("A page break can only start a body paragraph of an HWP document")); }
            plan.edited.insert((at.section, at.para));
            plan.page_break_edited.insert((at.section, at.para));
            core.mark_page_break_at_paragraph_start_native(at.section, at.para).map_err(|e| native_err("page break", e))?;
        }
        NativeEdit::PageDef { section, props } => {
            plan.page_edited.insert(*section);
            core.set_page_def_native(*section, &props.to_string()).map_err(|e| native_err("page setup", e))?;
        }
        NativeEdit::ColumnWidths { section, para, control, widths } => {
            plan.table_resized.insert((*section, *para));
            let before = table_raw_ctrl_kept(core, *section, *para, *control);
            core.set_table_column_widths_native(*section, *para, *control, widths.clone()).map_err(|e| native_err("table column widths", e))?;
            if let Some(before) = before { reseal_table_raw_ctrl(core, *section, *para, *control, &before); }
        }
        NativeEdit::InsertParagraph { section, para } => {
            core.insert_paragraph_native(*section, *para).map_err(|e| native_err("paragraph insert", e))?;
            plan_insert(plan, *section, *para);
            plan.docinfo_append.insert(inspect::TAG_CHAR_SHAPE);
            plan.docinfo_append.insert(inspect::TAG_PARA_SHAPE);
        }
        NativeEdit::DeleteParagraph { section, para } => {
            core.delete_paragraph_native(*section, *para).map_err(|e| native_err("paragraph delete", e))?;
            plan_delete(plan, *section, *para);
        }
        NativeEdit::InsertTable { section, para, rows, cols } => {
            core.insert_paragraph_native(*section, *para).map_err(|e| native_err("table host paragraph", e))?;
            plan_insert(plan, *section, *para);
            let before = core.document().sections[*section].paragraphs.len();
            core.create_table_native(*section, *para, 0, *rows, *cols).map_err(|e| native_err("table insert", e))?;
            let after = core.document().sections[*section].paragraphs.len();
            for k in 0..after.saturating_sub(before) { plan_insert(plan, *section, *para + 1 + k); }
            for tag in [inspect::TAG_BORDER_FILL, inspect::TAG_CHAR_SHAPE, inspect::TAG_PARA_SHAPE] { plan.docinfo_append.insert(tag); }
        }
        NativeEdit::InsertPicture { section, para, asset, width, height, px_width, px_height, extension } => {
            let (data, pw, ph) = image_bytes(assets, asset)?;
            // 0 x 0 = recorded before pixel sizes were logged; the bytes decide.
            if (*px_width, *px_height) != (pw, ph) && (*px_width, *px_height) != (0, 0) { return Err(DltError::new(ErrorCode::IntegrityError, format!("Picture edit records {px_width}x{px_height} px but the image is {pw}x{ph} px"))); }
            core.insert_paragraph_native(*section, *para).map_err(|e| native_err("picture host paragraph", e))?;
            plan_insert(plan, *section, *para);
            let ids_before: Vec<u16> = core.document().bin_data_content.iter().map(|b| b.id).collect();
            // rhwp inserts a floating picture; it becomes an inline picture of its own paragraph.
            core.insert_picture_native(*section, *para, 0, &[], &data, *width, *height, pw, ph, extension, "", None, None).map_err(|e| native_err("picture insert", e))?;
            core.set_picture_properties_native(*section, *para, 0, INLINE_PICTURE).map_err(|e| native_err("inline picture", e))?;
            normalize_picture(core.document_mut(), *section, *para, 0, pw, ph)?;
            for b in &core.document().bin_data_content {
                if !ids_before.contains(&b.id) { plan.new_streams.insert(format!("/BinData/BIN{:04X}.{}", b.id, b.extension)); }
            }
            for tag in [inspect::TAG_BIN_DATA, inspect::TAG_CHAR_SHAPE, inspect::TAG_PARA_SHAPE, inspect::TAG_BORDER_FILL] { plan.docinfo_append.insert(tag); }
            plan.pictures.insert((*section, *para, 0), PicturePlan { asset: asset.clone(), inserted: true });
        }
        NativeEdit::SetPicture { section, para, control, asset, width, height } => {
            let (data, pw, ph) = image_bytes(assets, asset)?;
            let extension = image_extension(&asset.mime).ok_or_else(|| DltError::unsupported(format!("Images of type {} cannot be stored in HWP", asset.mime)))?;
            let (position, current) = {
                let pic = picture_mut(core.document_mut(), *section, *para, *control)?;
                if pic.shape_attr.rotation_angle.rem_euclid(360) != 0 || pic.shape_attr.horz_flip || pic.shape_attr.vert_flip { return Err(DltError::unsupported("Rotated or flipped pictures cannot get a new image")); }
                (pic.image_attr.bin_data_id, (pic.common.width, pic.common.height))
            };
            let index = rhwp_content_index(core.document(), position);
            let same = index.and_then(|i| core.document().bin_data_content.get(i)).map(|c| sha256(&c.data.load()) == asset.sha256).unwrap_or(false);
            if !same {
                let added = usize::from(position) > plan.original_bin_data && index == Some(usize::from(position) - 1);
                if added {
                    // An item this edit log added earlier: only this log's pictures use it.
                    let doc = core.document_mut();
                    let i = usize::from(position) - 1;
                    let storage = doc.bin_data_content[i].id;
                    doc.bin_data_content[i].data = rhwp::model::bin_data::BinDataBytes::Loaded(data.clone());
                    doc.bin_data_content[i].extension = extension.to_string();
                    let record = doc.doc_info.bin_data_list.get_mut(i).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "BinData record missing"))?;
                    record.extension = Some(extension.to_string());
                    record.raw_data = None;
                    doc.doc_info.raw_stream = None;
                    plan.new_streams.insert(format!("/BinData/BIN{storage:04X}.{extension}"));
                } else {
                    let ids_before: Vec<u16> = core.document().bin_data_content.iter().map(|b| b.id).collect();
                    core.assign_picture_image_native(*section, *para, &[], *control, &data, pw, ph, extension).map_err(|e| native_err("picture image", e))?;
                    for b in &core.document().bin_data_content {
                        if !ids_before.contains(&b.id) { plan.new_streams.insert(format!("/BinData/BIN{:04X}.{}", b.id, b.extension)); }
                    }
                    plan.docinfo_append.insert(inspect::TAG_BIN_DATA);
                }
            }
            if current != (*width, *height) {
                core.set_picture_properties_native(*section, *para, *control, &json!({"width": width, "height": height}).to_string()).map_err(|e| native_err("picture size", e))?;
            }
            normalize_picture(core.document_mut(), *section, *para, *control, pw, ph)?;
            if plan.origin.get(*section).and_then(|o| o.get(*para)).copied().flatten().is_some() { plan.pictures_replaced.insert((*section, *para, *control)); }
            let inserted = plan.pictures.get(&(*section, *para, *control)).map(|p| p.inserted).unwrap_or(false);
            plan.pictures.insert((*section, *para, *control), PicturePlan { asset: asset.clone(), inserted });
        }
        NativeEdit::NamedStyleChar { style, props } => {
            // The limit bounds the shapes one history adds (one per change); 64 is not derived from a measurement.
            if plan.named_styles.len() >= MAX_NAMED_STYLE_SHAPES {
                return Err(unsupported(format!("An HWP edit history holds at most {MAX_NAMED_STYLE_SHAPES} named style changes; each adds a character shape")));
            }
            let (old, new) = apply_named_style_char(core, *style, props)?;
            plan.named_styles.push(NamedStylePlan { style: *style, old, new });
            plan.docinfo_append.insert(inspect::TAG_CHAR_SHAPE);
        }
    }
    plan.char_shapes = Some(core.document().doc_info.char_shapes.len());
    Ok(())
}

/// Character shape changes of `props` (the `char_props` keys).
fn char_mods(props: &Value, doc: &rhwp::model::document::Document) -> DltResult<rhwp::model::style::CharShapeMods> {
    let mut mods = rhwp::model::style::CharShapeMods::default();
    let obj = props.as_object().ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "Named style edit has no properties"))?;
    for (key, v) in obj {
        let bad = || DltError::new(ErrorCode::IntegrityError, format!("Named style property {key} is invalid"));
        match key.as_str() {
            "bold" => mods.bold = Some(v.as_bool().ok_or_else(bad)?),
            "italic" => mods.italic = Some(v.as_bool().ok_or_else(bad)?),
            "fontSize" => mods.base_size = Some(v.as_i64().and_then(|x| i32::try_from(x).ok()).filter(|x| *x > 0).ok_or_else(bad)?),
            "textColor" => mods.text_color = Some(v.as_str().and_then(color_ref).ok_or_else(bad)?),
            "fontId" => {
                let id = v.as_u64().and_then(|x| u16::try_from(x).ok()).ok_or_else(bad)?;
                if doc.doc_info.font_faces.len() < 7 || doc.doc_info.font_faces.iter().any(|slot| slot.len() <= usize::from(id)) { return Err(bad()); }
                mods.font_id = Some(id);
            }
            _ => return Err(bad()),
        }
    }
    Ok(mods)
}

/// `#RRGGBB` as an HWP COLORREF (0x00BBGGRR): the one conversion of a character format colour.
fn color_ref(hex: &str) -> Option<u32> {
    let v = u32::from_str_radix(hex.strip_prefix('#')?, 16).ok().filter(|_| hex.len() == 7)?;
    Some(((v & 0xFF) << 16) | (v & 0xFF00) | ((v >> 16) & 0xFF))
}

/// Return the container of the first unreachable use. An unreadable value still blocks the edit.
fn serialized_use<T: Serialize>(value: &T, style: u8, old: u32, place: &'static str) -> Option<&'static str> {
    match serde_json::to_value(value) {
        Ok(v) => uses(&v, u64::from(style), u64::from(old), false, place),
        Err(_) => Some(place),
    }
}

/// A paragraph of `style` with a run on the style's shape `old`, a character overlap on `old` inside such a paragraph
/// (`in_style`), raw records holding either (master pages, section tails), or a control the model does not parse.
fn uses(v: &Value, style: u64, old: u64, in_style: bool, place: &'static str) -> Option<&'static str> {
    match v {
        Value::Object(m) => {
            if m.contains_key("Unknown") { return Some("an unknown control"); }
            let mut inner = in_style;
            if let (Some(s), Some(runs)) = (m.get("style_id").and_then(Value::as_u64), m.get("char_shapes").and_then(Value::as_array)) {
                inner = s == style;
                if inner && runs.iter().any(|r| r.get("char_shape_id").and_then(Value::as_u64) == Some(old)) { return Some(place); }
            }
            if inner && m.get("char_shape_ids").and_then(Value::as_array).map(|ids| ids.iter().any(|x| x.as_u64() == Some(old))).unwrap_or(false) { return Some("a character overlap"); }
            m.iter().find_map(|(key, x)| {
                let container = match key.as_str() {
                    "Header" => "a header", "Footer" => "a footer", "Footnote" => "a footnote", "Endnote" => "an endnote",
                    "Shape" => "drawing text", "text_box" => "a text box", "caption" => "a caption",
                    "SectionDef" | "master_pages" => "a master page", "HiddenComment" => "a hidden comment",
                    _ => place,
                };
                uses(x, style, old, inner, container)
            })
        }
        Value::Array(a) => {
            if !a.is_empty() && a.iter().all(|r| r.get("tag_id").is_some() && r.get("data").is_some()) { return raw_use(a, style, old, place); }
            a.iter().find_map(|x| uses(x, style, old, in_style, place))
        }
        _ => None,
    }
}

/// Raw HWP records (`RawRecord { tag_id, level, data }`): a PARA_HEADER of `style` (style id at byte 10) whose
/// PARA_CHAR_SHAPE has a run on `old`, or that holds a character overlap ('tcps') control.
fn raw_use(records: &[Value], style: u64, old: u64, place: &'static str) -> Option<&'static str> {
    const MEMO_LIST: u16 = 93;
    let mut in_style = false;
    let mut container = place;
    let mut after_memo = false;
    for r in records {
        let (Some(tag), Some(level)) = (r.get("tag_id").and_then(Value::as_u64), r.get("level").and_then(Value::as_u64)) else { return Some(container) };
        let data: Vec<u8> = r.get("data").and_then(Value::as_array).map(|d| d.iter().filter_map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok())).collect()).unwrap_or_default();
        let tag = u16::try_from(tag).unwrap_or(0);
        match tag {
            MEMO_LIST if level == 1 => container = "a memo",
            inspect::TAG_LIST_HEADER if level == 1 && !after_memo => container = place,
            inspect::TAG_PARA_HEADER => in_style = data.get(10).map(|s| u64::from(*s) == style).unwrap_or(true),
            inspect::TAG_PARA_CHAR_SHAPE if in_style => {
                if data.chunks_exact(8).any(|c| u64::from(u32::from_le_bytes(c[4..8].try_into().unwrap())) == old) { return Some(container); }
            }
            inspect::TAG_CTRL_HEADER if in_style && data.get(0..4) == Some(&b"spct"[..]) => return Some("a character overlap"),
            _ => {}
        }
        after_memo = tag == MEMO_LIST && level == 1;
    }
    None
}

/// Where a use of `style` sits that the change cannot follow (None: nowhere). Body paragraphs and table cells are
/// followed; a character overlap on the style's shape in one of their paragraphs, any other object holding a use
/// (text boxes, headers, footers, notes, captions, master pages) and any unparsed control are not.
fn unreachable_use(paras: &[rhwp::model::paragraph::Paragraph], style: u8, old: u32) -> Option<&'static str> {
    for p in paras {
        for c in &p.controls {
            match c {
                Control::Table(t) => {
                    if let Some(w) = t.cells.iter().find_map(|cell| unreachable_use(&cell.paragraphs, style, old)) { return Some(w); }
                    if let Some(place) = t.caption.as_ref().and_then(|cap| serialized_use(cap, style, old, "a table caption")) { return Some(place); }
                }
                Control::CharOverlap(o) => if p.style_id == style && o.char_shape_ids.contains(&old) { return Some("a character overlap"); },
                Control::Unknown(_) => return Some("an unknown control"),
                other => if let Some(place) = serialized_use(other, style, old, view::control_id(other).unwrap_or("an unparsed object").trim()) { return Some(place); },
            }
        }
    }
    None
}

/// Paragraphs a named style change repointed: (section, top-level paragraph, table cell path; empty = that paragraph).
type Repointed = Vec<(usize, usize, Vec<(usize, usize, usize)>)>;

#[allow(clippy::too_many_arguments)]
fn repoint(paras: &mut [rhwp::model::paragraph::Paragraph], style: u8, old: u32, new: u32, section: usize, host: Option<usize>, prefix: &[(usize, usize, usize)], out: &mut Repointed) {
    for (k, p) in paras.iter_mut().enumerate() {
        let (top, path) = match host {
            None => (k, Vec::new()),
            Some(h) => { let mut v = prefix.to_vec(); if let Some(last) = v.last_mut() { last.2 = k; } (h, v) }
        };
        if p.style_id == style && p.char_shapes.iter().any(|r| r.char_shape_id == old) {
            for r in p.char_shapes.iter_mut().filter(|r| r.char_shape_id == old) { r.char_shape_id = new; }
            out.push((section, top, path.clone()));
        }
        for (ci, c) in p.controls.iter_mut().enumerate() {
            if let Control::Table(t) = c {
                for (cell_index, cell) in t.cells.iter_mut().enumerate() {
                    let mut next = path.clone();
                    next.push((ci, cell_index, 0));
                    repoint(&mut cell.paragraphs, style, old, new, section, Some(top), &next, out);
                }
            }
        }
    }
}

fn paragraph_at_mut<'a>(doc: &'a mut rhwp::model::document::Document, section: usize, para: usize, path: &[(usize, usize, usize)]) -> Option<&'a mut rhwp::model::paragraph::Paragraph> {
    let mut p = doc.sections.get_mut(section)?.paragraphs.get_mut(para)?;
    for (c, cell, cp) in path {
        p = match p.controls.get_mut(*c)? { Control::Table(t) => t.cells.get_mut(*cell)?.paragraphs.get_mut(*cp)?, _ => return None };
    }
    Some(p)
}

/// The character shape id at UTF-16 position `pos` (the first run also covers what is before it).
fn run_id_at(runs: &[rhwp::model::paragraph::CharShapeRef], pos: u32) -> Option<u32> {
    runs.iter().rev().find(|r| r.start_pos <= pos).or(runs.first()).map(|r| r.char_shape_id)
}

/// Lays out the repointed paragraphs again as rhwp lays out a direct character shape change (its set_char_shape_id
/// calls: line segments, cell fit, the section's following line positions), on a range that already holds the new
/// shape. The runs stay exactly as the repoint left them.
fn relayout_repointed(core: &mut Core, new: u32, repointed: &Repointed) -> DltResult<()> {
    for (s, p, path) in repointed {
        let integrity = || DltError::new(ErrorCode::IntegrityError, "A repointed HWP paragraph is missing");
        let (before, range) = {
            let para = paragraph_at_mut(core.document_mut(), *s, *p, path).ok_or_else(integrity)?;
            let n = para.text.chars().count();
            let range = match (0..n).find(|&i| para.char_shape_id_at(i) == Some(new)) { Some(i) => (i, i + 1), None if n == 0 => (0, 0), None => continue };
            (para.char_shapes.clone(), range)
        };
        if path.is_empty() { core.set_char_shape_id_native(*s, *p, range.0, range.1, new) } else { core.set_char_shape_id_in_cell_by_path(*s, *p, path, range.0, range.1, new) }
            .map_err(|e| native_err("named style line layout", e))?;
        let para = paragraph_at_mut(core.document_mut(), *s, *p, path).ok_or_else(integrity)?;
        let positions: std::collections::BTreeSet<u32> = before.iter().chain(&para.char_shapes).map(|r| r.start_pos).collect();
        if positions.iter().any(|pos| run_id_at(&before, *pos) != run_id_at(&para.char_shapes, *pos)) {
            return Err(DltError::new(ErrorCode::IntegrityError, "Laying out a repointed HWP paragraph changed its character shapes"));
        }
        para.char_shapes = before;
    }
    Ok(())
}

fn apply_named_style_char(core: &mut Core, style: usize, props: &Value) -> DltResult<(u32, u32)> {
    let (old, new, repointed) = set_named_style_char(core.document_mut(), style, props)?;
    if !repointed.is_empty() {
        core.begin_batch_native().map_err(|e| native_err("begin named style layout", e))?;
        let layout = relayout_repointed(core, new, &repointed);
        // Leave batch mode even if a native paragraph command failed.
        let flush = core.end_batch_native().map_err(|e| native_err("finish named style layout", e));
        layout?;
        flush?;
    }
    Ok((old, new))
}

/// Copy-and-repoint for one named paragraph style; returns (previous, new) character shape ids and the repointed
/// paragraphs. Nothing changes when the style is used where the change cannot follow it.
fn set_named_style_char(doc: &mut rhwp::model::document::Document, style: usize, props: &Value) -> DltResult<(u32, u32, Repointed)> {
    let st = doc.doc_info.styles.get(style).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, format!("Named style {style} does not exist in the HWP document")))?;
    if st.style_type != 0 { return Err(unsupported(format!("'{}' is an HWP character style; HWP 5 runs do not record which character style they use, so it cannot be changed", st.local_name))); }
    let style_id = u8::try_from(style).map_err(|_| unsupported("Paragraphs cannot refer to HWP styles past index 255"))?;
    let old = u32::from(st.char_shape_id);
    let base = doc.doc_info.char_shapes.get(old as usize).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, format!("Named style '{}' refers to missing character shape {old}", st.local_name)))?;
    let shape = char_mods(props, doc)?.apply_to(base);
    let new = u16::try_from(doc.doc_info.char_shapes.len()).map_err(|_| unsupported("The HWP document has no room for another character shape"))?;
    let name = st.local_name.clone();
    for section in &doc.sections {
        let place = unreachable_use(&section.paragraphs, style_id, old)
            .or_else(|| serialized_use(&section.section_def, style_id, old, "a master page"))
            .or_else(|| serialized_use(&section.trailing_lists, style_id, old, "a section-end list"));
        if let Some(place) = place {
            if place == "an unknown control" { return Err(unsupported(format!("Cannot change named style '{name}': the document contains an unknown control whose style use cannot be checked"))); }
            return Err(unsupported(format!("Named style '{name}' is used in {place}; HWP named style changes reach body text and table cells only, so nothing was changed")));
        }
    }
    doc.doc_info.char_shapes.push(shape);
    doc.doc_info.styles[style].char_shape_id = new;
    let mut repointed = Vec::new();
    for (si, section) in doc.sections.iter_mut().enumerate() { repoint(&mut section.paragraphs, style_id, old, u32::from(new), si, None, &[], &mut repointed); }
    Ok((old, u32::from(new), repointed))
}

/// Index into `bin_data_content` of 1-based BinData position `id` (rhwp's lookup rule).
fn rhwp_content_index(doc: &rhwp::model::document::Document, id: u16) -> Option<usize> {
    let index = usize::from(id.checked_sub(1)?);
    if index < doc.bin_data_content.len() { Some(index) } else { doc.bin_data_content.iter().position(|c| c.id == id) }
}

// ------------------------------------------------------------------ translation

pub struct Translation { pub edits: Vec<NativeEdit>, pub diagnostics: Vec<Diagnostic>, pub new_ids: Vec<(Id, NodeLoc)> }

fn unsupported(msg: impl Into<String>) -> DltError { DltError::unsupported(msg) }

fn not_found(kind: &str, id: &str) -> DltError {
    let mut e = DltError::new(ErrorCode::NotFound, format!("{kind} {id} was not found in this HWP document"));
    e.affected_ids = vec![id.to_string()];
    e
}

fn para_at(ids: &IdMap, id: &str) -> DltResult<ParaAt> {
    let loc = ids.loc(id).ok_or_else(|| not_found("Paragraph", id))?;
    if loc.kind != LocKind::Paragraph { return Err(DltError::invalid(format!("{id} is not a paragraph"))); }
    match loc.path.as_slice() {
        [s, p] => Ok(ParaAt { section: *s, para: *p, cell: None }),
        [s, p, c, cell, cp] => Ok(ParaAt { section: *s, para: *p, cell: Some([*c, *cell, *cp]) }),
        _ => Err(unsupported("Editing paragraphs nested deeper than one table level is not supported for HWP")),
    }
}

fn native_para<'a>(doc: &'a rhwp::model::document::Document, at: &ParaAt) -> DltResult<&'a rhwp::model::paragraph::Paragraph> {
    let (paras, index) = native_list(doc, at)?;
    paras.get(index).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, if at.cell.is_some() { "HWP cell mapping points past the table" } else { "HWP mapping points past the document" }))
}

/// Native scalar offset of logical grapheme boundary `index` (atoms have no native characters).
fn scalar_of(lp: &LogicalParagraph, index: u32) -> usize {
    lp.graphemes[..index as usize].iter().filter(|g| !g.protected).map(|g| lp.text[g.logical.clone()].chars().count()).sum()
}

fn is_inline_object(c: &Control) -> bool {
    matches!(c, Control::Shape(_) | Control::Table(_) | Control::Picture(_) | Control::Equation(_) | Control::Form(_) | Control::Footnote(_) | Control::Endnote(_) | Control::CharOverlap(_) | Control::Ruby(_) | Control::AutoNumber(_))
}

fn is_field(c: &Control) -> bool { matches!(c, Control::Field(_)) }

/// The refusal of a page break on a table cell paragraph (an existing one or one of a new table).
fn cell_page_break_refused() -> DltError { unsupported("A page break cannot be set on a paragraph inside a table cell in HWP") }

/// The paragraph `at` points to, with the paragraph list it belongs to (a section or a table cell) and its index there.
fn native_list<'a>(doc: &'a rhwp::model::document::Document, at: &ParaAt) -> DltResult<(&'a [rhwp::model::paragraph::Paragraph], usize)> {
    let top = &doc.sections.get(at.section).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "HWP mapping points past the document"))?.paragraphs;
    match at.cell {
        None => Ok((top, at.para)),
        Some([c, cell, cp]) => match top.get(at.para).and_then(|p| p.controls.get(c)) {
            Some(Control::Table(t)) => t.cells.get(cell).map(|x| (x.paragraphs.as_slice(), cp)).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "HWP cell mapping points past the table")),
            _ => Err(DltError::new(ErrorCode::IntegrityError, "HWP cell mapping does not point to a table")),
        },
    }
}

/// Validate a text edit [a, b) + text against controls of paragraph `index` of the native list `paras`.
fn check_text_edit(paras: &[rhwp::model::paragraph::Paragraph], index: usize, lp: &LogicalParagraph, start: u32, a: usize, b: usize, text: &str) -> DltResult<()> {
    let p = &paras[index];
    let n = p.text.chars().count();
    let positions: Vec<usize> = p.control_text_positions().into_iter().map(|x| x.min(n)).collect();
    // Text inserted at the end `e` of a field that an inline object follows outside the field (after its end in the
    // stream) goes between the end and the object, outside the link. check_intent verifies that it did.
    let after_field_end = |e: usize| {
        !text.is_empty() && a == e && b == e && p.field_ranges.iter().any(|r| {
            r.start_char_idx < e && r.end_char_idx == e
                && p.controls.iter().enumerate().any(|(k, c)| positions[k] == e && is_inline_object(c) && k > r.control_idx + r.inner_slot_count)
        })
    };
    // A hyperlink is kept only when its characters and both of its boundaries stay as they are: the edit may not remove,
    // replace or add a character inside the link, delete the whole link, or put text at a link boundary.
    for (s, e, _) in view::link_ranges(paras, index) {
        if (b > s && a < e) || (!text.is_empty() && b >= s && a <= e && !after_field_end(e)) {
            return Err(unsupported("The edit changes hyperlink text or a hyperlink boundary; HWP text edits keep a hyperlink only when its characters and boundaries stay unchanged"));
        }
    }
    let mut marks: Vec<usize> = p.field_ranges.iter().map(|r| r.start_char_idx).collect();
    marks.extend(view::native_marks(p).into_iter().map(|m| m.1));
    for (k, c) in p.controls.iter().enumerate() {
        let pos = positions[k];
        if pos > a && pos < b { return Err(unsupported("The range contains an embedded object or field marker; HWP text edits cannot remove it")); }
        if is_field(c) && pos >= a && pos <= b && (pos > a || !text.is_empty()) {
            return Err(unsupported("Editing text at a field or hyperlink boundary is not supported for HWP"));
        }
    }
    for &m in &marks {
        if m > a && m < b { return Err(unsupported("The range crosses a field boundary")); }
        if !text.is_empty() && m == a && a != 0 && !after_field_end(m) { return Err(unsupported("Inserting text at a field boundary is not supported for HWP")); }
    }
    if text.is_empty() { return Ok(()); }
    // Insertion point: rhwp inserts before an inline object located there.
    for (k, c) in p.controls.iter().enumerate() {
        if positions[k] != a || !is_inline_object(c) { continue; }
        let ok = if project::role(c) == project::Role::Atom {
            // The logical position must be just before that atom.
            lp.graphemes.get(start as usize).map(|g| g.protected).unwrap_or(false)
        } else {
            a == n && n > 0
        };
        if !ok { return Err(unsupported("Inserting text next to an embedded object is ambiguous in HWP; insert inside the text instead")); }
        break;
    }
    Ok(())
}

fn hwp_text_ok(text: &str) -> DltResult<()> {
    designllm_flow::validate_edit_text(text)?;
    if text.contains('\t') || text.contains('\n') { return Err(unsupported("Inserting tabs or line breaks into HWP text is not supported yet")); }
    Ok(())
}

fn round_hu(emu: i64) -> i64 { quantize(emu, EMU_PER_HWPUNIT).map(|v| v / EMU_PER_HWPUNIT).unwrap_or(emu / EMU_PER_HWPUNIT) }

fn rounding_diag(code_target: &str, requested: i64, applied_hu: i64) -> Option<Diagnostic> {
    let applied = applied_hu * EMU_PER_HWPUNIT;
    (applied != requested).then(|| Diagnostic { code: diag_codes::NATIVE_UNIT_ROUNDING.into(), message: format!("{code_target}: requested {requested} EMU, stored {applied} EMU ({applied_hu} HWPUNIT)"), node_id: None, blocking: false, params: Default::default() })
}

/// rhwp char-format properties for a style patch, or an error for fields HWP text cannot take.
fn char_props(core: &mut Core, patch: &StylePatch, current_rel: u8, diags: &mut Vec<Diagnostic>) -> DltResult<Value> {
    let v12 = designllm_flow::guard::patch_v12(patch, "");
    if !v12.is_empty() { return Err(unsupported(format!("HWP character styles do not support the contract v1.2 fields {}", v12.join(", ")))); }
    if patch.fill.is_some() || patch.stroke.is_some() || patch.stroke_width.is_some() || patch.align.is_some() {
        return Err(unsupported("HWP character styles accept font_family, font_size, color, bold and italic (use set_paragraph_format for alignment)"));
    }
    let mut props = serde_json::Map::new();
    if let Some(b) = patch.bold { props.insert("bold".into(), json!(b)); }
    if let Some(i) = patch.italic { props.insert("italic".into(), json!(i)); }
    if let Some(size) = patch.font_size {
        if size <= 0 { return Err(DltError::invalid("Font size must be positive")); }
        let hu = round_hu(size);
        let rel = i64::from(if current_rel == 0 { 100 } else { current_rel });
        let base = (hu * 100 + rel / 2) / rel;
        if base * rel / 100 != hu { return Err(unsupported("The requested size cannot be stored exactly with this character's relative size")); }
        diags.extend(rounding_diag("font_size", size, hu));
        props.insert("fontSize".into(), json!(base));
    }
    if let Some(color) = &patch.color {
        let hex = color.strip_prefix('#').filter(|h| h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit())).ok_or_else(|| DltError::invalid("HWP text color must be #RRGGBB"))?;
        props.insert("textColor".into(), json!(format!("#{}", hex.to_ascii_uppercase())));
    }
    if let Some(family) = &patch.font_family {
        let name = family.trim();
        if name.is_empty() { return Err(DltError::invalid("Font family must not be empty")); }
        let id = core.find_or_create_font_id_native(name);
        if id < 0 { return Err(unsupported("The font could not be registered")); }
        let faces = &core.document().doc_info.font_faces;
        if faces.len() < 7 || faces.iter().any(|slot| slot.get(id as usize).map(|f| f.name.as_str()) != Some(name)) {
            return Err(unsupported(format!("Font '{name}' does not have the same face index in all HWP language slots; changing it would alter other scripts")));
        }
        props.insert("fontId".into(), json!(id));
    }
    if props.is_empty() { return Err(DltError::invalid("The style patch has no HWP character properties")); }
    Ok(Value::Object(props))
}

fn alignment_name(code: u32) -> &'static str { match code { 1 => "left", 2 => "right", 3 => "center", 4 => "distribute", 5 => "split", _ => "justify" } }

/// Character style and paragraph format a new paragraph at native index `q`, and the cells of a
/// new table there, are written with: rhwp gives them the last character shape and the paragraph
/// shape of the paragraph before (`Paragraph::new_empty_like`; table cells take the host's).
fn inherited_shapes(doc: &rhwp::model::document::Document, section: usize, q: usize) -> (Style, ParagraphFormat) {
    let template = doc.sections.get(section).and_then(|s| s.paragraphs.get(q.saturating_sub(1)));
    let char_shape = template.and_then(|p| p.char_shapes.last()).map(|c| c.char_shape_id).unwrap_or(0);
    let style = project::char_style(doc, char_shape);
    let format = template.map(|p| project::paragraph_format_of(doc, p.para_shape_id, style.font_size)).unwrap_or_default();
    (style, format)
}

/// v1.1 style fields of `wanted` that would not be written (`written` is what is). The default
/// style means "not given" and takes the written one.
fn unwritten_style_fields(wanted: &Style, written: &Style) -> Vec<&'static str> {
    if *wanted == Style::default() { return vec![]; }
    let color = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    [
        ("font_family", wanted.font_family == written.font_family), ("font_size", wanted.font_size == written.font_size),
        ("color", color(&wanted.color, &written.color)), ("fill", color(&wanted.fill, &written.fill)), ("stroke", color(&wanted.stroke, &written.stroke)),
        ("stroke_width", wanted.stroke_width == written.stroke_width), ("bold", wanted.bold == written.bold), ("italic", wanted.italic == written.italic),
        ("align", wanted.align == written.align),
    ]
    .into_iter().filter(|(_, same)| !same).map(|(name, _)| name).collect()
}

fn run_problems(runs: &[TextRun], written: &Style) -> Vec<(Id, Vec<String>)> {
    runs.iter().filter_map(|r| {
        let fields = unwritten_style_fields(&r.style, written);
        (!fields.is_empty()).then(|| (r.id.clone(), fields.into_iter().map(|f| format!("style.{f}")).collect()))
    }).collect()
}

/// Paragraph format fields that would not be written; the default format means "not given". `page_break_before` is
/// not compared: a body paragraph writes it as its explicit page break, and a table cell refuses it (see `translate`).
/// A line pitch of 0 without a percentage takes the written spacing (task 25, decision 1). A percentage must be the
/// written one (100 is single spacing), and a pitch given with it must be 0 or the written pitch.
fn format_problem(id: &str, wanted: &ParagraphFormat, written: &ParagraphFormat) -> Option<(Id, Vec<String>)> {
    let d = ParagraphFormat::default();
    if (&wanted.align, wanted.space_before, wanted.space_after, wanted.line_spacing, wanted.indent, wanted.line_spacing_percent) == (&d.align, d.space_before, d.space_after, d.line_spacing, d.indent, d.line_spacing_percent) { return None; }
    let percent_written = match wanted.line_spacing_percent {
        None => true,
        Some(100) => written.line_spacing_percent.is_none() && written.line_spacing == 0,
        Some(p) => written.line_spacing_percent == Some(p),
    };
    let fields: Vec<String> = [
        ("align", wanted.align == written.align), ("space_before", wanted.space_before == written.space_before), ("space_after", wanted.space_after == written.space_after),
        ("line_spacing", wanted.line_spacing == 0 || wanted.line_spacing == written.line_spacing), ("line_spacing_percent", percent_written), ("indent", wanted.indent == written.indent),
    ]
    .into_iter().filter(|(_, same)| !same).map(|(name, _)| format!("format.{name}")).collect();
    (!fields.is_empty()).then(|| (id.to_string(), fields))
}

/// A paragraph style name (`style_ref`) is not stored in HWP, so a new paragraph that names one is refused, not dropped.
fn style_ref_problem(id: &str, style_ref: Option<&str>) -> Option<(Id, Vec<String>)> {
    style_ref.map(|_| (id.to_string(), vec!["style_ref".to_string()]))
}

/// R09: values of a new paragraph or table that HWP would not write are refused, not replaced.
fn not_written(problems: Vec<(Id, Vec<String>)>, written: &Style) -> DltError {
    let detail = problems.iter().map(|(id, f)| format!("{id}: {}", f.join(", "))).collect::<Vec<_>>().join("; ");
    let mut e = unsupported(format!(
        "These values of the new block are not written to HWP: {detail}. A new paragraph and the cells of a new table take the preceding paragraph's character shape ({} {} EMU) and paragraph shape, and table borders and fills are not written; insert with the default style and format, then use set_text_style_range or set_paragraph_format",
        written.font_family, written.font_size));
    e.affected_ids = problems.iter().map(|(id, _)| id.clone()).collect();
    e.diagnostics = problems.into_iter().map(|(id, f)| Diagnostic { code: "hwp_insert_value_not_written".into(), message: format!("{} would not be written", f.join(", ")), node_id: Some(id), blocking: true, params: Default::default() }).collect();
    e
}

/// Flow block index -> native paragraph insertion index (only between paragraph groups).
fn native_insert_index(section: &Section, ids: &IdMap, index: usize, para_count: usize) -> DltResult<usize> {
    if index > section.blocks.len() { return Err(DltError::invalid(format!("Insert index {index} is past the {} blocks", section.blocks.len()))); }
    let para_of = |b: &FlowBlock| -> Option<usize> { ids.loc(designllm_flow::tree::block_id(b)).and_then(|l| l.path.get(1).copied()) };
    let q = if index == section.blocks.len() { para_count } else {
        let here = para_of(&section.blocks[index]).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "Block without HWP mapping"))?;
        if index > 0 && para_of(&section.blocks[index - 1]) == Some(here) {
            return Err(unsupported("A block cannot be inserted between a paragraph and the object anchored in it"));
        }
        here
    };
    if q == 0 { return Err(unsupported("Inserting before the first paragraph of an HWP section (which holds the section definition) is not supported")); }
    Ok(q)
}

pub fn translate(op: &Operation, body: &FlowDocument, ids: &IdMap, core: &mut Core, assets: &dyn AssetStore) -> DltResult<Translation> {
    designllm_flow::guard::refuse_v12_operations("HWP documents", std::slice::from_ref(op))?;
    let mut diags = Vec::new();
    let mut new_ids = Vec::new();
    let edits = match op {
        Operation::ReplaceText { range, text } => {
            hwp_text_ok(text)?;
            let at = para_at(ids, &range.paragraph_id)?;
            let fp = find_paragraph(body, &range.paragraph_id).ok_or_else(|| not_found("Paragraph", &range.paragraph_id))?;
            let lp = LogicalParagraph::new(fp);
            lp.text_spans(range.start, range.end)?;
            let (a, b) = (scalar_of(&lp, range.start), scalar_of(&lp, range.end));
            let (paras, index) = native_list(core.document(), &at)?;
            native_para(core.document(), &at)?;
            check_text_edit(paras, index, &lp, range.start, a, b, text)?;
            if a == b && text.is_empty() { vec![] } else { vec![NativeEdit::Text { at, offset: a, delete: b - a, text: text.clone() }] }
        }
        Operation::SetText { node_id, .. } => return Err(unsupported(format!("set_text on {node_id} would replace every run of an HWP paragraph; use replace_text on a range"))),
        Operation::SetTextStyleRange { range, patch } => {
            let at = para_at(ids, &range.paragraph_id)?;
            let fp = find_paragraph(body, &range.paragraph_id).ok_or_else(|| not_found("Paragraph", &range.paragraph_id))?;
            let lp = LogicalParagraph::new(fp);
            lp.check_range(range.start, range.end)?;
            if range.start == range.end { return Err(DltError::invalid("set_text_style_range needs a non-empty range")); }
            let (a, b) = (scalar_of(&lp, range.start), scalar_of(&lp, range.end));
            if a == b { return Err(unsupported("The range only covers embedded objects; their character style is not editable")); }
            let rel = native_para(core.document(), &at).map(|p| p.char_shape_id_at(a).unwrap_or(0)).ok().and_then(|id| core.document().doc_info.char_shapes.get(id as usize).map(|c| c.relative_sizes[0])).unwrap_or(100);
            let props = char_props(core, patch, rel, &mut diags)?;
            vec![NativeEdit::CharFormat { at, start: a, end: b, props }]
        }
        Operation::SetStyle { node_ids, patch } => {
            let mut out = Vec::new();
            for id in node_ids {
                let (pid, start, end) = if let Some((pid, _)) = id.rsplit_once(".r") {
                    let fp = find_paragraph(body, pid).ok_or_else(|| not_found("Run", id))?;
                    let k = fp.runs.iter().position(|r| &r.id == id).ok_or_else(|| not_found("Run", id))?;
                    let lp = LogicalParagraph::new(fp);
                    let (s, e) = lp.run_range(k);
                    (pid.to_string(), s, e)
                } else if let Some(fp) = find_paragraph(body, id) {
                    (id.clone(), 0, designllm_flow::grapheme_count(fp))
                } else {
                    return Err(unsupported(format!("set_style for HWP targets text runs or paragraphs; {id} is neither")));
                };
                let at = para_at(ids, &pid)?;
                let fp = find_paragraph(body, &pid).ok_or_else(|| not_found("Paragraph", &pid))?;
                let lp = LogicalParagraph::new(fp);
                let (a, b) = (scalar_of(&lp, start), scalar_of(&lp, end));
                if a == b { continue; }
                let rel = native_para(core.document(), &at).map(|p| p.char_shape_id_at(a).unwrap_or(0)).ok().and_then(|cid| core.document().doc_info.char_shapes.get(cid as usize).map(|c| c.relative_sizes[0])).unwrap_or(100);
                let props = char_props(core, patch, rel, &mut diags)?;
                out.push(NativeEdit::CharFormat { at, start: a, end: b, props });
            }
            if out.is_empty() { return Err(DltError::invalid("set_style matched no HWP text")); }
            out
        }
        Operation::SetParagraphFormat { paragraph_id, format } => {
            designllm_flow::validate_paragraph_format(format)?;
            let at = para_at(ids, paragraph_id)?;
            let fp = find_paragraph(body, paragraph_id).ok_or_else(|| not_found("Paragraph", paragraph_id))?;
            let current = &fp.format;
            let native = native_para(core.document(), &at)?;
            let fmt = view::rhwp_para_fmt(core.document(), native.para_shape_id).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "Paragraph shape missing"))?;
            let mut props = serde_json::Map::new();
            if format.align != current.align {
                let keep_distribute = format.align == "justify" && matches!(fmt.alignment, 4 | 5);
                if !keep_distribute { props.insert("alignment".into(), json!(format.align)); }
            }
            for (key, want, have, name) in [("spacingBefore", format.space_before, current.space_before, "space_before"), ("spacingAfter", format.space_after, current.space_after, "space_after"), ("indent", format.indent, current.indent, "indent")] {
                if want != have {
                    let hu = round_hu(want);
                    diags.extend(rounding_diag(name, want, hu));
                    props.insert(key.into(), json!(hu * 2));
                }
            }
            // Task 25: a percentage decides, written as Percent (100 is single spacing). Without one the pitch is the
            // request: 0 is single spacing, written as Percent 100, and any other pitch is Fixed, also when a
            // proportional paragraph has that pitch now. The native shape tells whether the spacing changes.
            let native_percent = (fmt.line_spacing_type == 0).then_some(fmt.line_spacing);
            let single = format.line_spacing_percent.is_none() && format.line_spacing == 0;
            let percent_edit = match format.line_spacing_percent {
                Some(p) => (native_percent != Some(i64::from(p))).then_some(i64::from(p)),
                None if single => (native_percent != Some(100)).then_some(100),
                None => None,
            };
            if let Some(p) = percent_edit {
                props.insert("lineSpacingType".into(), json!("Percent"));
                props.insert("lineSpacing".into(), json!(p));
                if single {
                    diags.push(Diagnostic { code: "hwp_line_spacing_mapped".into(), message: "Single line spacing is stored as 100% (proportional) in HWP".into(), node_id: Some(paragraph_id.clone()), blocking: false, params: Default::default() });
                }
            } else if !single && format.line_spacing_percent.is_none() && (current.line_spacing_percent.is_some() || format.line_spacing != current.line_spacing) {
                let hu = round_hu(format.line_spacing);
                diags.extend(rounding_diag("line_spacing", format.line_spacing, hu));
                props.insert("lineSpacingType".into(), json!("Fixed"));
                props.insert("lineSpacing".into(), json!(hu * 2));
            }
            let _ = alignment_name(fmt.alignment);
            let mut edits = Vec::new();
            if !props.is_empty() { edits.push(NativeEdit::ParaFormat { at: at.clone(), props: Value::Object(props) }); }
            if format.page_break_before != current.page_break_before {
                // Only the set direction is written: rhwp has no call that clears the explicit bit and relays out the
                // section, and a cleared bit without the relayout leaves stored line positions wrong. A cell paragraph
                // has no page break here. Both refusals are reported, never dropped.
                if at.cell.is_some() { return Err(cell_page_break_refused()); }
                if !format.page_break_before { return Err(unsupported("Removing a page break from an HWP paragraph is not supported; the break stays in the file")); }
                edits.push(NativeEdit::PageBreak { at });
            }
            edits
        }
        Operation::SetPageSetup { section_id, setup } => {
            if let Some(p) = designllm_flow::tree::page_setup_problem(setup) { return Err(DltError::invalid(p)); }
            let loc = ids.loc(section_id).filter(|l| l.kind == LocKind::Section).ok_or_else(|| not_found("Section", section_id))?;
            let s = loc.path[0];
            let pd = core.document().sections.get(s).map(|x| x.section_def.page_def.clone()).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "Section mapping invalid"))?;
            let (w, h) = (round_hu(setup.size.width), round_hu(setup.size.height));
            let (nw, nh) = if pd.landscape { (h, w) } else { (w, h) };
            let left = round_hu(setup.margin_left) - i64::from(pd.margin_gutter);
            let top = round_hu(setup.margin_top) - i64::from(pd.margin_header);
            let bottom = round_hu(setup.margin_bottom) - i64::from(pd.margin_footer);
            let right = round_hu(setup.margin_right);
            if left < 0 || top < 0 || bottom < 0 { return Err(unsupported("The margins are smaller than the HWP header/footer/gutter areas of this section")); }
            for (name, want, hu) in [("width", setup.size.width, w), ("height", setup.size.height, h), ("margin_left", setup.margin_left, round_hu(setup.margin_left)), ("margin_top", setup.margin_top, round_hu(setup.margin_top)), ("margin_right", setup.margin_right, right), ("margin_bottom", setup.margin_bottom, round_hu(setup.margin_bottom))] {
                diags.extend(rounding_diag(name, want, hu));
            }
            let props = json!({"width": nw, "height": nh, "marginLeft": left, "marginRight": right, "marginTop": top, "marginBottom": bottom});
            vec![NativeEdit::PageDef { section: s, props }]
        }
        Operation::SetTableDimensions { table_id, column_widths } => {
            let loc = ids.loc(table_id).filter(|l| l.kind == LocKind::Table).ok_or_else(|| not_found("Table", table_id))?;
            let [s, p, c] = loc.path.as_slice() else { return Err(unsupported("Resizing tables nested in cells is not supported for HWP")) };
            let cols = match core.document().sections[*s].paragraphs[*p].controls.get(*c) { Some(Control::Table(t)) => t.col_count as usize, _ => return Err(DltError::new(ErrorCode::IntegrityError, "Table mapping invalid")) };
            if column_widths.len() != cols { return Err(DltError::invalid(format!("The table has {cols} columns, {} widths were given", column_widths.len()))); }
            let mut widths = Vec::new();
            for w in column_widths {
                if *w <= 0 { return Err(DltError::invalid("Column widths must be positive")); }
                let hu = round_hu(*w);
                diags.extend(rounding_diag("column_width", *w, hu));
                widths.push(u32::try_from(hu).map_err(|_| DltError::invalid("Column width too large"))?);
            }
            vec![NativeEdit::ColumnWidths { section: *s, para: *p, control: *c, widths }]
        }
        Operation::InsertFlowBlock { section_id, index, block } => {
            let loc = ids.loc(section_id).filter(|l| l.kind == LocKind::Section).ok_or_else(|| not_found("Section", section_id))?;
            let s = loc.path[0];
            let section = body.sections.iter().find(|x| &x.id == section_id).ok_or_else(|| not_found("Section", section_id))?;
            let para_count = core.document().sections[s].paragraphs.len();
            let q = native_insert_index(section, ids, *index as usize, para_count)?;
            let existing: std::collections::BTreeSet<Id> = designllm_flow::tree::collect_ids(body).into_iter().collect();
            let mut block_ids = Vec::new();
            designllm_flow::tree::collect_block_ids(std::slice::from_ref(block), &mut block_ids);
            if let Some(dup) = block_ids.iter().find(|i| existing.contains(*i)) { return Err(DltError::new(ErrorCode::AlreadyExists, format!("Id {dup} already exists"))); }
            match block {
                FlowBlock::Paragraph(np) => {
                    if np.runs.iter().any(|r| r.protected) { return Err(unsupported("New HWP paragraphs cannot contain protected objects")); }
                    designllm_flow::line_spacing::validate_percent(&np.format)?;
                    let (style, format) = inherited_shapes(core.document(), s, q);
                    let mut problems = run_problems(&np.runs, &style);
                    problems.extend(format_problem(&np.id, &np.format, &format));
                    problems.extend(style_ref_problem(&np.id, np.style_ref.as_deref()));
                    if !problems.is_empty() { return Err(not_written(problems, &style)); }
                    let text: String = np.runs.iter().map(|r| r.text.as_str()).collect();
                    hwp_text_ok(&text)?;
                    let at = ParaAt { section: s, para: q, cell: None };
                    let mut edits = vec![NativeEdit::InsertParagraph { section: s, para: q }];
                    if !text.is_empty() { edits.push(NativeEdit::Text { at: at.clone(), offset: 0, delete: 0, text }); }
                    // The explicit page break is set on the new paragraph, as SetParagraphFormat sets it on an existing one.
                    if np.format.page_break_before { edits.push(NativeEdit::PageBreak { at }); }
                    new_ids.push((np.id.clone(), NodeLoc { kind: LocKind::Paragraph, path: vec![s, q] }));
                    diags.push(Diagnostic { code: "hwp_new_paragraph_style".into(), message: "A new HWP paragraph takes its character and paragraph shape from the preceding paragraph; apply styles with separate operations".into(), node_id: Some(np.id.clone()), blocking: false, params: Default::default() });
                    edits
                }
                FlowBlock::Table(t) => {
                    if t.rows.is_empty() || t.column_widths.is_empty() { return Err(DltError::invalid("A new table needs rows and column widths")); }
                    let (style, format) = inherited_shapes(core.document(), s, q);
                    let mut problems: Vec<(Id, Vec<String>)> = Vec::new();
                    let table_fields = unwritten_style_fields(&t.style, &project::table_style());
                    if !table_fields.is_empty() { problems.push((t.id.clone(), table_fields.into_iter().map(|f| format!("table.style.{f}")).collect())); }
                    for cell in t.rows.iter().flatten() {
                        for b in &cell.blocks {
                            if let FlowBlock::Paragraph(cp) = b {
                                designllm_flow::line_spacing::validate_percent(&cp.format)?;
                                if cp.format.page_break_before {
                                    let mut e = cell_page_break_refused();
                                    e.affected_ids = vec![cp.id.clone()];
                                    return Err(e);
                                }
                                problems.extend(run_problems(&cp.runs, &style));
                                problems.extend(format_problem(&cp.id, &cp.format, &format));
                                problems.extend(style_ref_problem(&cp.id, cp.style_ref.as_deref()));
                            }
                        }
                    }
                    if !problems.is_empty() { return Err(not_written(problems, &style)); }
                    let cols = t.column_widths.len();
                    if t.rows.iter().any(|r| r.len() != cols || r.iter().any(|c| c.column_span != 1 || c.row_span != 1)) { return Err(unsupported("New HWP tables must be a plain grid (no merged cells)")); }
                    let rows = u16::try_from(t.rows.len()).map_err(|_| DltError::invalid("Too many rows"))?;
                    let ncols = u16::try_from(cols).map_err(|_| DltError::invalid("Too many columns"))?;
                    let mut edits = vec![NativeEdit::InsertTable { section: s, para: q, rows, cols: ncols }];
                    let mut widths = Vec::new();
                    for w in &t.column_widths { let hu = round_hu(*w); diags.extend(rounding_diag("column_width", *w, hu)); widths.push(u32::try_from(hu.max(1)).map_err(|_| DltError::invalid("Column width too large"))?); }
                    edits.push(NativeEdit::ColumnWidths { section: s, para: q, control: 0, widths });
                    new_ids.push((t.id.clone(), NodeLoc { kind: LocKind::Table, path: vec![s, q, 0] }));
                    for (r, row) in t.rows.iter().enumerate() {
                        for (c, cell) in row.iter().enumerate() {
                            let ci = r * cols + c;
                            new_ids.push((cell.id.clone(), NodeLoc { kind: LocKind::Cell, path: vec![s, q, 0, ci] }));
                            let paras: Vec<&Paragraph> = cell.blocks.iter().filter_map(|b| match b { FlowBlock::Paragraph(p) => Some(p), _ => None }).collect();
                            if paras.len() != cell.blocks.len() || paras.len() > 1 { return Err(unsupported("New HWP table cells hold at most one text paragraph")); }
                            if let Some(cp) = paras.first() {
                                if cp.runs.iter().any(|r| r.protected) { return Err(unsupported("New HWP table cells cannot contain protected objects")); }
                                let text: String = cp.runs.iter().map(|r| r.text.as_str()).collect();
                                hwp_text_ok(&text)?;
                                new_ids.push((cp.id.clone(), NodeLoc { kind: LocKind::Paragraph, path: vec![s, q, 0, ci, 0] }));
                                if !text.is_empty() { edits.push(NativeEdit::Text { at: ParaAt { section: s, para: q, cell: Some([0, ci, 0]) }, offset: 0, delete: 0, text }); }
                            }
                        }
                    }
                    diags.push(Diagnostic { code: "hwp_table_trailing_paragraph".into(), message: "HWP keeps an empty paragraph after an inserted table".into(), node_id: Some(t.id.clone()), blocking: false, params: Default::default() });
                    edits
                }
                FlowBlock::Image(node) => {
                    if node.kind != NodeKind::Image {
                        let mut e = unsupported(format!("HWP picture blocks store image nodes only, not kind {:?}", node.kind));
                        e.affected_ids = vec![node.id.clone()];
                        return Err(e);
                    }
                    // The picture keeps the asset, size and alignment only; other node fields are refused (R09).
                    let mut unused: Vec<String> = [("text", !node.text.is_empty()), ("children", !node.children.is_empty()), ("interactions", !node.interactions.is_empty()), ("variants", !node.variants.is_empty()), ("layout", node.layout.is_some())]
                        .into_iter().filter(|(_, set)| *set).map(|(name, _)| name.to_string()).collect();
                    // Of the style only the alignment is written (left, center or right, by the paragraph).
                    let align = match node.style.align.as_str() { "center" | "right" => node.style.align.clone(), _ => "left".to_string() };
                    unused.extend(unwritten_style_fields(&node.style, &Style { align, ..Style::default() }).into_iter().map(|f| format!("style.{f}")));
                    if !unused.is_empty() {
                        let mut e = unsupported(format!("HWP pictures inserted here do not store {} (alternative text and canvas node fields)", unused.join(", ")));
                        e.affected_ids = vec![node.id.clone()];
                        return Err(e);
                    }
                    let asset = node.asset.clone().ok_or_else(|| DltError::invalid("A new image needs an asset"))?;
                    if node.bounds.width <= 0 || node.bounds.height <= 0 { return Err(DltError::invalid("A new image needs a positive size")); }
                    let extension = image_extension(&asset.mime).ok_or_else(|| unsupported(format!("Images of type {} cannot be inserted into HWP (png, jpeg, bmp, gif)", asset.mime)))?.to_string();
                    let (_, px_width, px_height) = image_bytes(assets, &asset)?;
                    let (w, h) = (round_hu(node.bounds.width), round_hu(node.bounds.height));
                    if w < 1 || h < 1 { return Err(DltError::invalid("The image is smaller than one HWPUNIT")); }
                    diags.extend(rounding_diag("image_width", node.bounds.width, w));
                    diags.extend(rounding_diag("image_height", node.bounds.height, h));
                    new_ids.push((node.id.clone(), NodeLoc { kind: LocKind::Image, path: vec![s, q, 0] }));
                    let mut edits = vec![NativeEdit::InsertPicture { section: s, para: q, asset, width: u32::try_from(w).map_err(|_| DltError::invalid("Image too wide"))?, height: u32::try_from(h).map_err(|_| DltError::invalid("Image too tall"))?, px_width, px_height, extension }];
                    // The picture is a character of its own paragraph: the paragraph alignment
                    // places it. The new paragraph inherits the preceding paragraph's shape.
                    let want = match node.style.align.as_str() { "center" => 3, "right" => 2, _ => 1 };
                    let inherited = core.document().sections[s].paragraphs.get(q - 1).and_then(|p| view::rhwp_para_fmt(core.document(), p.para_shape_id)).map(|f| match f.alignment { 2 | 3 => f.alignment, _ => 1 }).unwrap_or(1);
                    if want != inherited {
                        let name = match want { 3 => "center", 2 => "right", _ => "left" };
                        edits.push(NativeEdit::ParaFormat { at: ParaAt { section: s, para: q, cell: None }, props: json!({"alignment": name}) });
                    }
                    edits
                }
                FlowBlock::Opaque { .. } => return Err(unsupported("Opaque blocks preserve imported content and cannot be inserted")),
            }
        }
        Operation::RemoveNode { node_id } => {
            let loc = ids.loc(node_id).ok_or_else(|| not_found("Node", node_id))?;
            match (loc.kind, loc.path.as_slice()) {
                (LocKind::Paragraph, [s, p]) => {
                    if *p == 0 { return Err(unsupported("The first paragraph of an HWP section holds the section definition and cannot be removed")); }
                    if core.document().sections[*s].paragraphs.len() <= 1 { return Err(unsupported("A section needs at least one paragraph")); }
                    let native = &core.document().sections[*s].paragraphs[*p];
                    if !native.controls.is_empty() { return Err(unsupported("This paragraph anchors objects; removing it would remove them too")); }
                    vec![NativeEdit::DeleteParagraph { section: *s, para: *p }]
                }
                (LocKind::Image, [s, p, _]) => {
                    // A picture alone in its paragraph (a block picture) goes with that paragraph.
                    let native = core.document().sections.get(*s).and_then(|x| x.paragraphs.get(*p)).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "HWP picture mapping points past the document"))?;
                    if *p == 0 || native.controls.len() != 1 || !native.text.is_empty() {
                        return Err(unsupported("This picture shares its paragraph with text or other objects; HWP removes only pictures that stand alone in a paragraph"));
                    }
                    if core.document().sections[*s].paragraphs.len() <= 1 { return Err(unsupported("A section needs at least one paragraph")); }
                    vec![NativeEdit::DeleteParagraph { section: *s, para: *p }]
                }
                _ => return Err(unsupported("HWP supports removing whole text paragraphs and stand-alone pictures only")),
            }
        }
        Operation::UpdateFigureLink { link } => return translate_figure_link(link, body, ids, core, assets),
        Operation::SetNodeList { node_id, .. } => return Err(unsupported(format!("set_node_list on {node_id} is not supported for HWP documents"))),
        Operation::SetNodePath { node_id, .. } => return Err(unsupported(format!("set_node_path on {node_id} is not supported for HWP documents"))),
        Operation::SetInteractions { node_id, .. } => return Err(unsupported(format!("set_interactions on {node_id} is not supported for HWP documents"))),
        Operation::ReplaceImageAsset { node_id, .. } => {
            let mut error = unsupported(format!("replace_image_asset on {node_id} is a Canvas-only operation and is not supported for HWP documents"));
            error.affected_ids.push(node_id.clone());
            return Err(error);
        }
        Operation::SetNamedStyle { name, patch, clear_overrides } => {
            if *clear_overrides { return Err(unsupported("clear_overrides is not supported for HWP named styles: only runs using the style's previous character shape ID follow the change")); }
            let styles = &core.document().doc_info.styles;
            if styles.is_empty() { return Err(unsupported("This HWP document has no named styles")); }
            let found: Vec<usize> = styles.iter().enumerate().filter(|(_, s)| s.local_name == *name || s.english_name == *name).map(|(i, _)| i).collect();
            let style = match found.as_slice() {
                [] => return Err(not_found("Named style", name)),
                [one] => *one,
                _ => return Err(unsupported(format!("The HWP style name '{name}' names {} styles; it does not identify one", found.len()))),
            };
            let st = &styles[style];
            if st.style_type != 0 { return Err(unsupported(format!("'{name}' is an HWP character style; HWP 5 runs do not record which character style they use, so it cannot be changed"))); }
            let rel = core.document().doc_info.char_shapes.get(usize::from(st.char_shape_id)).map(|c| c.relative_sizes[0]).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, format!("Named style '{name}' refers to a missing character shape")))?;
            // A new face would be registered by char_props before the edit; replay starts from the file, so only faces the file has are used.
            if let Some(family) = &patch.font_family {
                let faces = &core.document().doc_info.font_faces;
                let present = faces.len() >= 7 && faces[0].iter().position(|f| f.name == family.trim()).map(|i| faces.iter().all(|slot| slot.get(i).map(|f| f.name.as_str()) == Some(family.trim()))).unwrap_or(false);
                if !present { return Err(unsupported(format!("Font '{}' is not registered in every language slot of this HWP document; named styles use registered fonts only", family.trim()))); }
            }
            let props = char_props(core, patch, rel, &mut diags)?;
            vec![NativeEdit::NamedStyleChar { style, props }]
        }
        other => return Err(unsupported(format!("Operation {} is not supported for HWP documents", serde_json::to_value(other).ok().and_then(|v| v["op"].as_str().map(str::to_string)).unwrap_or_default()))),
    };
    Ok(Translation { edits, diagnostics: diags, new_ids })
}

fn refuse(code: ErrorCode, message: impl Into<String>, ids: &[&str]) -> DltError {
    let mut e = DltError::new(code, message);
    e.affected_ids = ids.iter().map(|s| s.to_string()).collect();
    e
}

/// `media_header::aspect_height` with the HWP messages. A zero height is raised to 1 HWPUNIT by the caller.
pub fn aspect_height(width: i64, pixel_width: u32, pixel_height: u32) -> DltResult<i64> {
    media_header::aspect_height(width, pixel_width, pixel_height).map_err(|error| match error {
        media_header::AspectError::NonPositive => DltError::invalid("Picture width and render size must be positive"),
        media_header::AspectError::OutOfRange => DltError::invalid("Picture height is out of range"),
    })
}

/// `UpdateFigureLink` (docs/FIGURE_LINKS.md): the target must be a body picture of the candidate
/// state; the picture keeps its anchor, position and width, shows the render and takes the
/// render's aspect ratio. Every check runs before any native edit; the link itself is recorded
/// by the engine on the artifact.
pub fn translate_figure_link(link: &FigureLink, body: &FlowDocument, ids: &IdMap, core: &Core, assets: &dyn AssetStore) -> DltResult<Translation> {
    let target = link.target_node_id.as_str();
    match designllm_flow::find_node(body, target) {
        Some(designllm_flow::FlowNode::Image(_)) => {}
        Some(_) => return Err(refuse(ErrorCode::InvalidArgument, format!("{target} is not a picture; figure links target pictures"), &[target])),
        None => return Err(refuse(ErrorCode::NotFound, format!("Picture {target} was not found in this HWP document"), &[target])),
    }
    let loc = ids.loc(target).filter(|l| l.kind == LocKind::Image).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, format!("Picture {target} has no HWP mapping")))?;
    let [s, p, c] = loc.path.as_slice() else {
        return Err(refuse(ErrorCode::UnsupportedOperation, format!("Picture {target} is inside a table or another object; HWP figure links support pictures in the body text"), &[target]));
    };
    let (s, p, c) = (*s, *p, *c);
    if !FIGURE_REPRESENTATIONS.contains(&link.representation.as_str()) {
        return Err(refuse(ErrorCode::UnsupportedOperation, format!("Representation {:?} is not supported for HWP figure links; supported: {}", link.representation, FIGURE_REPRESENTATIONS.join(", ")), &[target]));
    }
    let asset = assets.lookup(&link.render_hash).map_err(|_| refuse(ErrorCode::NotFound, format!("Render {} is not in the workspace store", link.render_hash), &[target, &link.render_hash]))?;
    if asset.mime != "image/png" { return Err(refuse(ErrorCode::InvalidArgument, format!("Render {} has mime {}; representation png needs image/png", link.render_hash, asset.mime), &[target, &link.render_hash])); }
    let bytes = assets.read(&asset)?;
    if sha256(&bytes) != link.render_hash || bytes.len() as u64 != asset.bytes { return Err(refuse(ErrorCode::IntegrityError, format!("Render bytes do not match sha256 {}", link.render_hash), &[target, &link.render_hash])); }
    let (pw, ph) = png_size(&bytes).ok_or_else(|| refuse(ErrorCode::InvalidArgument, format!("Render {} is not a PNG image", link.render_hash), &[target, &link.render_hash]))?;
    let pic = match core.document().sections.get(s).and_then(|x| x.paragraphs.get(p)).and_then(|x| x.controls.get(c)) {
        Some(Control::Picture(pic)) => pic,
        _ => return Err(DltError::new(ErrorCode::IntegrityError, format!("Picture {target} does not map to an HWP picture"))),
    };
    if pic.shape_attr.rotation_angle.rem_euclid(360) != 0 || pic.shape_attr.horz_flip || pic.shape_attr.vert_flip {
        return Err(refuse(ErrorCode::UnsupportedOperation, format!("Picture {target} is rotated or flipped; HWP figure links need an upright picture"), &[target]));
    }
    if pic.image_attr.external_path.is_some() { return Err(refuse(ErrorCode::UnsupportedOperation, format!("Picture {target} shows a linked file"), &[target])); }
    if !matches!(pic.raw_picture_extra.len(), 0 | 17 | 18) {
        return Err(refuse(ErrorCode::UnsupportedOperation, format!("Picture {target} has image effects; its image cannot be replaced"), &[target]));
    }
    let width = i64::from(pic.common.width);
    if width < 1 { return Err(DltError::new(ErrorCode::IntegrityError, format!("Picture {target} has no width"))); }
    let height_emu = aspect_height(width * EMU_PER_HWPUNIT, pw, ph)?;
    let height = round_hu(height_emu).max(1);
    let mut diagnostics: Vec<Diagnostic> = rounding_diag("figure_height", height_emu, height).into_iter().map(|mut d| { d.node_id = Some(target.to_string()); d }).collect();
    let current = rhwp_content_index(core.document(), pic.image_attr.bin_data_id).and_then(|i| core.document().bin_data_content.get(i)).map(|b| sha256(&b.data.load()));
    let dim = (pw * 75, ph * 75);
    let fresh = current.as_deref() == Some(link.render_hash.as_str()) && i64::from(pic.common.height) == height
        && (pic.crop.left, pic.crop.top, pic.crop.right, pic.crop.bottom) == (0, 0, dim.0 as i32, dim.1 as i32) && pic.img_dim == dim
        && (pic.shape_attr.original_width, pic.shape_attr.original_height, pic.shape_attr.current_width, pic.shape_attr.current_height) == (pic.common.width, pic.common.height, pic.common.width, pic.common.height);
    let edits = if fresh { vec![] } else {
        let partial = pic.crop.left != 0 || pic.crop.top != 0 || (pic.img_dim != (0, 0) && (pic.crop.right as u32, pic.crop.bottom as u32) != pic.img_dim);
        if partial { diagnostics.push(Diagnostic { code: "hwp_picture_crop_reset".into(), message: format!("Picture {target} was cropped; the render is shown whole"), node_id: Some(target.to_string()), blocking: false, params: Default::default() }); }
        vec![NativeEdit::SetPicture { section: s, para: p, control: c, asset, width: pic.common.width, height: u32::try_from(height).map_err(|_| DltError::invalid("Picture too tall"))? }]
    };
    Ok(Translation { edits, diagnostics, new_ids: vec![] })
}

// ------------------------------------------------------------------ intent checks

fn chars(s: &str) -> Vec<char> { s.chars().collect() }

fn per_char(p: &ParaView) -> Vec<CharStyle> {
    let n = p.text.chars().count();
    let mut out = Vec::with_capacity(n);
    for (k, (start, style)) in p.styles.iter().enumerate() {
        let end = p.styles.get(k + 1).map(|x| x.0).unwrap_or(n);
        for _ in *start..end { out.push(style.clone()); }
    }
    out
}

fn para_ref<'a>(v: &'a DocView, at: &ParaAt, doc: &rhwp::model::document::Document) -> Option<&'a ParaView> {
    let top = v.sections.get(at.section)?.paras.get(at.para)?;
    match at.cell {
        None => Some(top),
        Some([c, cell, cp]) => {
            let ordinal = doc.sections[at.section].paragraphs[at.para].controls.iter().take(c).filter(|x| matches!(x, Control::Table(_))).count();
            let table = top.tables.get(ordinal)?;
            let native_cell = match doc.sections[at.section].paragraphs[at.para].controls.get(c) { Some(Control::Table(t)) => t.cells.get(cell)?, _ => return None };
            let vc = table.cells.iter().find(|x| x.row == native_cell.row && x.col == native_cell.col)?;
            vc.paras.get(cp)
        }
    }
}

fn mismatch(msg: impl Into<String>) -> DltError {
    let mut e = DltError::new(ErrorCode::UnsafeRoundtrip, msg);
    e.diagnostics.push(Diagnostic { code: "hwp_edit_result_mismatch".into(), message: e.message.clone(), node_id: None, blocking: true, params: Default::default() });
    e
}

fn same_except(before: &DocView, after: &DocView, at: &ParaAt) -> DltResult<()> {
    if before.sections.len() != after.sections.len() { return Err(mismatch("The section count changed")); }
    for (si, (a, b)) in before.sections.iter().zip(&after.sections).enumerate() {
        if a.page != b.page { return Err(mismatch(format!("Section {si} page setup changed unexpectedly"))); }
        if a.paras.len() != b.paras.len() { return Err(mismatch(format!("Section {si} paragraph count changed unexpectedly"))); }
        for (pi, (x, y)) in a.paras.iter().zip(&b.paras).enumerate() {
            if si == at.section && pi == at.para { continue; }
            if let Some(d) = view::para_difference(x, y) { return Err(mismatch(format!("Section {si} paragraph {pi} changed unexpectedly: {d}"))); }
        }
    }
    Ok(())
}

/// Cell heights follow their content; compare the rest of a table host paragraph.
fn same_host(before: &ParaView, after: &ParaView, cell_target: Option<(usize, (u16, u16), usize)>) -> DltResult<()> {
    let strip = |p: &ParaView| { let mut q = p.clone(); for t in q.tables.iter_mut() { for c in t.cells.iter_mut() { c.height = 0; } } q };
    let (mut x, mut y) = (strip(before), strip(after));
    if let Some((ordinal, (row, col), cp)) = cell_target {
        for v in [&mut x, &mut y] {
            if let Some(cell) = v.tables.get_mut(ordinal).and_then(|t| t.cells.iter_mut().find(|c| c.row == row && c.col == col)) {
                if let Some(p) = cell.paras.get_mut(cp) { p.text.clear(); p.ctrls.clear(); p.marks.clear(); p.styles.clear(); p.format = None; }
            }
        }
    }
    match view::para_difference(&x, &y) { Some(d) => Err(mismatch(format!("Table host paragraph changed unexpectedly: {d}"))), None => Ok(()) }
}

/// Verify that a native edit did exactly what it was asked to do.
pub fn check_intent(edit: &NativeEdit, before: &DocView, after: &DocView, doc_after: &rhwp::model::document::Document, plan: &Plan) -> DltResult<()> {
    match edit {
        NativeEdit::Text { at, .. } | NativeEdit::CharFormat { at, .. } => {
            same_except(before, after, at)?;
            let (pb, pa) = (para_ref(before, at, doc_after).ok_or_else(|| mismatch("Edited paragraph missing before"))?, para_ref(after, at, doc_after).ok_or_else(|| mismatch("Edited paragraph missing after"))?);
            if at.cell.is_some() {
                let host_b = &before.sections[at.section].paras[at.para];
                let host_a = &after.sections[at.section].paras[at.para];
                let [c, cell, cp] = at.cell.unwrap();
                let ordinal = doc_after.sections[at.section].paragraphs[at.para].controls.iter().take(c).filter(|x| matches!(x, Control::Table(_))).count();
                let native_cell = match doc_after.sections[at.section].paragraphs[at.para].controls.get(c) { Some(Control::Table(t)) => t.cells.get(cell).map(|x| (x.row, x.col)), _ => None }.ok_or_else(|| mismatch("Edited cell missing"))?;
                same_host(host_b, host_a, Some((ordinal, native_cell, cp)))?;
            }
            if let NativeEdit::Text { offset, delete, text, .. } = edit {
                let old = chars(&pb.text);
                let (a, b) = (*offset, *offset + *delete);
                let expected: String = old[..a].iter().chain(text.chars().collect::<Vec<_>>().iter()).chain(old[b..].iter()).collect();
                if pa.text != expected { return Err(mismatch(format!("HWP text after the edit is {:?}, expected {:?}", pa.text, expected))); }
                let added = text.chars().count();
                if pb.ctrls.len() != pa.ctrls.len() || pb.marks.len() != pa.marks.len() { return Err(mismatch("Controls or field markers were added or removed by a text edit")); }
                for (k, (cb, ca)) in pb.ctrls.iter().zip(&pa.ctrls).enumerate() {
                    let inline = matches!(cb.id.as_str(), "gso " | "tbl " | "eqed" | "form" | "fn  " | "en  " | "tdut" | "tcps" | "atno");
                    let want = if cb.pos < a { cb.pos } else if cb.pos > b { cb.pos - (b - a) + added } else if inline { a + added } else { a };
                    if ca.id != cb.id || ca.pos != want { return Err(mismatch(format!("Control {k} ({}) moved from {} to {} (expected {want})", cb.id, cb.pos, ca.pos))); }
                    if ca.field != cb.field { return Err(mismatch(format!("Control {k} ({}) changed its field record", cb.id))); }
                }
                // Inline marks (field ends, title marks) move like the text around them; a mark at the edit stays at its start
                // or follows the inserted text.
                for (k, (mb, ma)) in pb.marks.iter().zip(&pa.marks).enumerate() {
                    let ok = if mb.1 < a { ma.1 == mb.1 } else if mb.1 > b { ma.1 == mb.1 - (b - a) + added } else { ma.1 == a || ma.1 == a + added };
                    if ma.0 != mb.0 || !ok { return Err(mismatch(format!("Inline mark {k} (code {}) moved from {} to {}", mb.0, mb.1, ma.1))); }
                }
                if pb.format != pa.format || pb.tables != pa.tables && at.cell.is_none() || pb.pictures != pa.pictures { return Err(mismatch("A text edit changed paragraph format or objects")); }
                let (sb, sa) = (per_char(pb), per_char(pa));
                if sa.len() != expected.chars().count() || sb[..a] != sa[..a] || sb[b..] != sa[a + added..] { return Err(mismatch("A text edit changed the style of untouched characters")); }
                if sa[a..a + added].iter().any(|s| s.link.is_some()) { return Err(mismatch("Inserted text became part of a hyperlink")); }
            } else if let NativeEdit::CharFormat { start, end, props, .. } = edit {
                if pa.text != pb.text || pa.ctrls != pb.ctrls || pa.marks != pb.marks || pa.format != pb.format { return Err(mismatch("A character format edit changed text, controls or paragraph format")); }
                let (sb, sa) = (per_char(pb), per_char(pa));
                if sb.len() != sa.len() { return Err(mismatch("Character count changed")); }
                for i in 0..sb.len() {
                    let want = if i >= *start && i < *end { patched_style(sb[i].clone(), props, doc_after) } else { sb[i].clone() };
                    if sa[i] != want { return Err(mismatch(format!("Character {i} has style {:?}, expected {:?}", sa[i], want))); }
                }
            }
            Ok(())
        }
        NativeEdit::ParaFormat { at, props } => {
            same_except(before, after, at)?;
            let (pb, pa) = (para_ref(before, at, doc_after).ok_or_else(|| mismatch("Paragraph missing"))?, para_ref(after, at, doc_after).ok_or_else(|| mismatch("Paragraph missing"))?);
            let mut want = pb.format.clone().ok_or_else(|| mismatch("Paragraph format missing"))?;
            if let Some(a) = props.get("alignment").and_then(|v| v.as_str()) { want.alignment = match a { "left" => 1, "right" => 2, "center" => 3, "distribute" => 4, "split" => 5, _ => 0 }; }
            if let Some(v) = props.get("spacingBefore").and_then(|v| v.as_i64()) { want.spacing_before = v as i32; }
            if let Some(v) = props.get("spacingAfter").and_then(|v| v.as_i64()) { want.spacing_after = v as i32; }
            if let Some(v) = props.get("indent").and_then(|v| v.as_i64()) { want.indent = v as i32; }
            if let Some(t) = props.get("lineSpacingType").and_then(|v| v.as_str()) { want.line_spacing_type = match t { "Fixed" => 1, "SpaceOnly" => 2, "Minimum" => 3, _ => 0 }; }
            if let Some(v) = props.get("lineSpacing").and_then(|v| v.as_i64()) { want.line_spacing = v; }
            if pa.format.as_ref() != Some(&want) { return Err(mismatch(format!("Paragraph format is {:?}, expected {:?}", pa.format, want))); }
            if pa.text != pb.text || pa.styles != pb.styles || pa.ctrls != pb.ctrls || pa.marks != pb.marks { return Err(mismatch("A paragraph format edit changed text, character styles, controls or inline marks")); }
            Ok(())
        }
        NativeEdit::PageBreak { at } => {
            same_except(before, after, at)?;
            let (pb, pa) = (para_ref(before, at, doc_after).ok_or_else(|| mismatch("Paragraph missing"))?, para_ref(after, at, doc_after).ok_or_else(|| mismatch("Paragraph missing"))?);
            if pa.text != pb.text || pa.ctrls != pb.ctrls || pa.marks != pb.marks || pa.styles != pb.styles || pa.format != pb.format || pa.pictures != pb.pictures || pa.tables != pb.tables {
                return Err(mismatch("A page break edit changed paragraph text, format or objects"));
            }
            if !pa.page_break { return Err(mismatch("The paragraph does not start a new page after the page break edit")); }
            Ok(())
        }
        NativeEdit::PageDef { section, props } => {
            let mut want = before.sections.get(*section).and_then(|s| s.page.clone()).ok_or_else(|| mismatch("Page definition missing"))?;
            let get = |k: &str| props.get(k).and_then(|v| v.as_u64()).map(|v| v as u32);
            if let Some(v) = get("width") { want.width = v; }
            if let Some(v) = get("height") { want.height = v; }
            if let Some(v) = get("marginLeft") { want.margin_left = v; }
            if let Some(v) = get("marginRight") { want.margin_right = v; }
            if let Some(v) = get("marginTop") { want.margin_top = v; }
            if let Some(v) = get("marginBottom") { want.margin_bottom = v; }
            let got = after.sections.get(*section).and_then(|s| s.page.clone());
            if got.as_ref() != Some(&want) { return Err(mismatch(format!("Page definition is {got:?}, expected {want:?}"))); }
            let strip = |v: &DocView| { let mut v = v.clone(); for s in v.sections.iter_mut() { s.page = None; for p in s.paras.iter_mut() { for t in p.tables.iter_mut() { for c in t.cells.iter_mut() { c.height = 0; } } } } v };
            if let Some(d) = view::first_difference(&strip(before), &strip(after)) { return Err(mismatch(format!("Page setup changed content: {d}"))); }
            Ok(())
        }
        NativeEdit::ColumnWidths { section, para, control, widths } => {
            let at = ParaAt { section: *section, para: *para, cell: None };
            same_except(before, after, &at)?;
            let (pb, pa) = (&before.sections[*section].paras[*para], &after.sections[*section].paras[*para]);
            same_host(&{ let mut x = pb.clone(); for t in x.tables.iter_mut() { for c in t.cells.iter_mut() { c.width = 0; } } x }, &{ let mut x = pa.clone(); for t in x.tables.iter_mut() { for c in t.cells.iter_mut() { c.width = 0; } } x }, None)?;
            let ordinal = doc_after.sections[*section].paragraphs[*para].controls.iter().take(*control).filter(|x| matches!(x, Control::Table(_))).count();
            let table = pa.tables.get(ordinal).ok_or_else(|| mismatch("Resized table missing"))?;
            for c in &table.cells {
                let want: u32 = widths[c.col as usize..(c.col + c.col_span) as usize].iter().sum();
                if c.width != want { return Err(mismatch(format!("Cell ({},{}) width {} != {}", c.row, c.col, c.width, want))); }
            }
            Ok(())
        }
        NativeEdit::InsertParagraph { section, para } | NativeEdit::DeleteParagraph { section, para } => {
            let (bs, as_) = (&before.sections[*section].paras, &after.sections[*section].paras);
            let insert = matches!(edit, NativeEdit::InsertParagraph { .. });
            let expected_len = if insert { bs.len() + 1 } else { bs.len() - 1 };
            if as_.len() != expected_len { return Err(mismatch("Paragraph count did not change as requested")); }
            let (kept_breaks, new_break) = if insert { breaks_after_insert(bs, *para) } else { (bs.iter().map(|p| p.page_break).collect(), false) };
            for (k, pb) in bs.iter().enumerate() {
                let j = if insert { if k < *para { k } else { k + 1 } } else if k == *para { continue } else if k < *para { k } else { k - 1 };
                let pa = &as_[j];
                if pb.text != pa.text || pb.ctrls != pa.ctrls || pb.marks != pa.marks || pb.styles != pa.styles || pb.format != pa.format || kept_breaks[k] != pa.page_break { return Err(mismatch(format!("Paragraph {k} changed while inserting/removing paragraph {para}"))); }
            }
            if insert && (!as_[*para].text.is_empty() || !as_[*para].ctrls.is_empty()) { return Err(mismatch("The inserted paragraph is not empty")); }
            // A page break asked for on a new paragraph is its own edit (NativeEdit::PageBreak), checked after this one.
            if insert && as_[*para].page_break != new_break { return Err(mismatch("The inserted paragraph's page break is not the one of its place")); }
            Ok(())
        }
        NativeEdit::InsertTable { section, para, rows, cols } => {
            let pa = after.sections[*section].paras.get(*para).ok_or_else(|| mismatch("Table paragraph missing"))?;
            let t = pa.tables.first().ok_or_else(|| mismatch("Inserted table missing"))?;
            if (t.rows, t.cols) != (*rows, *cols) || t.cells.len() != usize::from(*rows) * usize::from(*cols) { return Err(mismatch("Inserted table has the wrong grid")); }
            let native = match doc_after.sections[*section].paragraphs[*para].controls.first() { Some(Control::Table(t)) => t, _ => return Err(mismatch("Inserted table is not the first control")) };
            for (i, c) in native.cells.iter().enumerate() {
                if usize::from(c.row) * usize::from(*cols) + usize::from(c.col) != i { return Err(mismatch("Inserted table cells are not in row-major order")); }
            }
            Ok(())
        }
        NativeEdit::InsertPicture { section, para, width, height, .. } => {
            let (bs, as_) = (&before.sections[*section].paras, &after.sections[*section].paras);
            if as_.len() != bs.len() + 1 { return Err(mismatch("Paragraph count did not grow by the picture paragraph")); }
            let (kept_breaks, new_break) = breaks_after_insert(bs, *para);
            for (k, pb) in bs.iter().enumerate() {
                let pa = &as_[if k < *para { k } else { k + 1 }];
                if pb.text != pa.text || pb.ctrls != pa.ctrls || pb.marks != pa.marks || pb.styles != pa.styles || pb.format != pa.format || kept_breaks[k] != pa.page_break || pb.pictures != pa.pictures || pb.tables != pa.tables { return Err(mismatch(format!("Paragraph {k} changed while inserting a picture"))); }
            }
            let pa = as_.get(*para).ok_or_else(|| mismatch("Picture paragraph missing"))?;
            if !pa.text.is_empty() || pa.ctrls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>() != ["gso "] || pa.pictures.len() != 1 { return Err(mismatch("The picture paragraph does not hold exactly one picture")); }
            if pa.page_break != new_break { return Err(mismatch("The picture paragraph's page break is not the one of its place")); }
            let pic = &pa.pictures[0];
            if (pic.width, pic.height) != (*width, *height) { return Err(mismatch(format!("Inserted picture size {}x{} != {width}x{height}", pic.width, pic.height))); }
            match doc_after.sections[*section].paragraphs[*para].controls.first() {
                Some(Control::Picture(p)) if p.common.treat_as_char => Ok(()),
                _ => Err(mismatch("The inserted picture is not an inline (treat-as-character) picture")),
            }
        }
        NativeEdit::SetPicture { section, para, control, width, height, .. } => {
            let at = ParaAt { section: *section, para: *para, cell: None };
            same_except(before, after, &at)?;
            let (pb, pa) = (&before.sections[*section].paras[*para], &after.sections[*section].paras[*para]);
            if pa.text != pb.text || pa.ctrls != pb.ctrls || pa.marks != pb.marks || pa.styles != pb.styles || pa.format != pb.format || pa.tables != pb.tables {
                return Err(mismatch("Replacing a picture changed the text, controls or format of its paragraph"));
            }
            let ordinal = doc_after.sections[*section].paragraphs[*para].controls.iter().take(*control).filter(|x| matches!(x, Control::Picture(_))).count();
            if pa.pictures.len() != pb.pictures.len() { return Err(mismatch("Picture count changed")); }
            for (k, (x, y)) in pb.pictures.iter().zip(&pa.pictures).enumerate() {
                if k != ordinal && x != y { return Err(mismatch(format!("Picture {k} changed while replacing picture {ordinal}"))); }
            }
            let pic = pa.pictures.get(ordinal).ok_or_else(|| mismatch("Replaced picture missing"))?;
            if (pic.width, pic.height) != (*width, *height) { return Err(mismatch(format!("Picture size {}x{} != {width}x{height}", pic.width, pic.height))); }
            Ok(())
        }
        NativeEdit::NamedStyleChar { style, props } => {
            let new = doc_after.doc_info.styles.get(*style).map(|s| u32::from(s.char_shape_id)).ok_or_else(|| mismatch("Named style missing after the edit"))?;
            if new as usize + 1 != doc_after.doc_info.char_shapes.len() { return Err(mismatch("The named style does not point to the character shape the edit added")); }
            // Every paragraph of the style in the body and table cells, edited or new ones included, left the previous shape.
            let planned = plan.named_styles.last().filter(|n| n.style == *style && n.new == new).ok_or_else(|| mismatch("The named style change is not in the plan"))?;
            if let Some(at) = still_on(doc_after, *style, planned.old) { return Err(mismatch(format!("A paragraph of the named style still uses its previous character shape ({at})"))); }
            let want = view::rhwp_char_style(doc_after, new);
            if before.sections.len() != after.sections.len() { return Err(mismatch("The section count changed")); }
            let mut changed = Vec::new();
            for (si, (a, b)) in before.sections.iter().zip(&after.sections).enumerate() {
                if a.page != b.page || a.paras.len() != b.paras.len() { return Err(mismatch(format!("Section {si} changed its page or paragraph count"))); }
                for (pi, (x, y)) in a.paras.iter().zip(&b.paras).enumerate() {
                    named_style_changes(x, y, &mut changed).map_err(|d| mismatch(format!("Section {si} paragraph {pi}: {d}")))?;
                }
            }
            // Every changed character had the same style before (the style's previous shape) and has the new one now.
            let unlinked = |s: &CharStyle| CharStyle { link: None, ..s.clone() };
            if let Some((from, _)) = changed.first() {
                let from = unlinked(from);
                if let Some((x, y)) = changed.iter().find(|(x, y)| unlinked(x) != from || unlinked(y) != want) { return Err(mismatch(format!("A character changed from {x:?} to {y:?}; the named style change gives {from:?} -> {want:?}"))); }
                let expected = patched_style(from, props, doc_after);
                if expected != want { return Err(mismatch(format!("The named style's new character shape reads {want:?}, expected {expected:?}"))); }
            }
            Ok(())
        }
    }
}

/// The explicit page breaks after a paragraph is inserted at `para` of `bs`: those of the kept paragraphs (by their index in
/// `bs`) and that of the new paragraph. At position 0 rhwp moves the break marks of the displaced first paragraph to the new
/// first paragraph (`insert_paragraph_native`: the marks belong to the start of the section). Anywhere else the new
/// paragraph has none and the kept paragraphs keep theirs. `translate` does not insert at position 0 (`native_insert_index`).
fn breaks_after_insert(bs: &[ParaView], para: usize) -> (Vec<bool>, bool) {
    let at_start = para == 0;
    let kept = bs.iter().enumerate().map(|(k, p)| p.page_break && !(at_start && k == 0)).collect();
    (kept, at_start && bs.first().is_some_and(|p| p.page_break))
}

/// `s` with the character format properties `props` applied (as rhwp applies them).
fn patched_style(mut s: CharStyle, props: &Value, doc: &rhwp::model::document::Document) -> CharStyle {
    if let Some(b) = props.get("bold").and_then(|v| v.as_bool()) { s.bold = b; }
    if let Some(it) = props.get("italic").and_then(|v| v.as_bool()) { s.italic = it; }
    if let Some(sz) = props.get("fontSize").and_then(|v| v.as_i64()) { s.base_size = sz as i32; }
    if let Some(c) = props.get("textColor").and_then(|v| v.as_str()) { s.color = color_ref(c).unwrap_or(0); }
    if let Some(f) = props.get("fontId").and_then(|v| v.as_i64()).and_then(|id| doc.doc_info.font_faces.first().and_then(|f| f.get(id as usize)).map(|f| f.name.clone())) { s.face = f; }
    s
}

/// Characters whose style differs between `b` and `a` (before, after), recursively through table cells; anything else
/// that differs is an error.
fn named_style_changes(b: &ParaView, a: &ParaView, changed: &mut Vec<(CharStyle, CharStyle)>) -> Result<(), String> {
    if b.text != a.text || b.ctrls != a.ctrls || b.marks != a.marks || b.format != a.format || b.page_break != a.page_break || b.pictures != a.pictures || b.unverifiable != a.unverifiable {
        return Err("text, controls, paragraph format or pictures changed".into());
    }
    let (sb, sa) = (per_char(b), per_char(a));
    if sb.len() != sa.len() { return Err("character count changed".into()); }
    for (x, y) in sb.iter().zip(&sa) {
        if x != y {
            if x.link != y.link { return Err("a hyperlink moved".into()); }
            changed.push((x.clone(), y.clone()));
        }
    }
    if b.tables.len() != a.tables.len() { return Err("table count changed".into()); }
    for (t, u) in b.tables.iter().zip(&a.tables) {
        if (t.rows, t.cols, t.cells.len()) != (u.rows, u.cols, u.cells.len()) { return Err("a table grid changed".into()); }
        for (c, d) in t.cells.iter().zip(&u.cells) {
            if (c.row, c.col, c.row_span, c.col_span, c.width, c.height) != (d.row, d.col, d.row_span, d.col_span, d.width, d.height) || c.paras.len() != d.paras.len() { return Err("a table cell changed".into()); }
            for (x, y) in c.paras.iter().zip(&d.paras) { named_style_changes(x, y, changed)?; }
        }
    }
    Ok(())
}

#[cfg(test)]
mod media_header_tests {
    use super::*;

    /// Task 49e input set: signature, IHDR chunk (25 bytes) and IEND chunk; the IHDR CRC is not read.
    fn png_file(width: u32, ihdr_length: u32) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        out.extend_from_slice(&ihdr_length.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]);
        out
    }

    /// The PNG gate of `translate_figure_link` on the input set.
    #[test]
    fn png_input_set_acceptance_of_the_figure_link_gate() {
        let normal = png_file(4, 13);
        let mut no_signature = normal.clone();
        no_signature[..8].fill(0);
        let no_iend = normal[..normal.len() - 12].to_vec();
        let inputs = [("P1 normal", normal), ("P2 no signature", no_signature), ("P3 IHDR length 14", png_file(4, 14)), ("P4 zero width", png_file(0, 13)), ("P5 no IEND", no_iend)];
        let table: Vec<_> = inputs.iter().map(|(name, bytes)| (*name, png_size(bytes).is_some())).collect();
        println!("49e hwp UpdateFigureLink PNG gate accepts: {table:?}");
        assert_eq!(table, [("P1 normal", true), ("P2 no signature", false), ("P3 IHDR length 14", false), ("P4 zero width", false), ("P5 no IEND", true)]);
    }
}

/// The first body or table cell paragraph of `style` (by index) that still has a run on `old` (section, paragraph).
fn still_on(doc: &rhwp::model::document::Document, style: usize, old: u32) -> Option<String> {
    fn walk(paras: &[rhwp::model::paragraph::Paragraph], style: usize, old: u32) -> bool {
        paras.iter().any(|p| (usize::from(p.style_id) == style && p.char_shapes.iter().any(|r| r.char_shape_id == old))
            || p.controls.iter().any(|c| matches!(c, Control::Table(t) if t.cells.iter().any(|cell| walk(&cell.paragraphs, style, old)))))
    }
    doc.sections.iter().enumerate().find_map(|(si, s)| s.paragraphs.iter().enumerate().find(|(_, p)| walk(std::slice::from_ref(p), style, old)).map(|(pi, _)| format!("section {si}, paragraph {pi}")))
}

/// The runs (UTF-16 start, character shape id) of every top-level paragraph, per (section, paragraph).
pub fn top_level_runs(doc: &rhwp::model::document::Document) -> std::collections::BTreeMap<(usize, usize), Vec<(u32, u32)>> {
    doc.sections.iter().enumerate().flat_map(|(si, s)| s.paragraphs.iter().enumerate().map(move |(pi, p)| ((si, pi), p.char_shapes.iter().map(|r| (r.start_pos, r.char_shape_id)).collect()))).collect()
}

/// Snapshot the model fields needed to verify saved records when other edits share a named-style history.
pub fn expect_named_style_paragraphs(plan: &mut Plan, doc: &rhwp::model::document::Document) {
    use crate::guard::CellParagraph;
    fn collect(host: &rhwp::model::paragraph::Paragraph, out: &mut Vec<CellParagraph>) {
        for control in &host.controls {
            let Control::Table(table) = control else { continue };
            for p in table.cells.iter().flat_map(|cell| &cell.paragraphs) {
                out.push(CellParagraph { text: p.text.clone(), style: p.style_id, para_shape: p.para_shape_id,
                    runs: p.char_shapes.iter().map(|r| (r.start_pos, r.char_shape_id)).collect() });
                collect(p, out);
            }
        }
    }
    if plan.named_styles.is_empty() { return; }
    plan.expected_runs = top_level_runs(doc);
    plan.expected_cells.clear();
    for (si, section) in doc.sections.iter().enumerate() {
        for (pi, host) in section.paragraphs.iter().enumerate() {
            if plan.origin.get(si).and_then(|s| s.get(pi)) == Some(&None) || plan.subtree_edited.contains(&(si, pi)) {
                let mut cells = Vec::new();
                collect(host, &mut cells);
                if !cells.is_empty() { plan.expected_cells.insert((si, pi), cells); }
            }
        }
    }
}

#[cfg(test)]
mod table_ctrl_seal_tests {
    use super::*;

    #[test]
    fn only_a_size_change_may_be_resealed() {
        let before = rhwp::model::shape::CommonObjAttr::default();
        let mut size = before.clone();
        size.width = 12_345;
        size.height = 678;
        assert!(same_except_size(&size, &before));
        let mut attr = size.clone();
        attr.treat_as_char = !attr.treat_as_char;
        assert!(!same_except_size(&attr, &before));
        let mut margin = size.clone();
        margin.margin.left += 1;
        assert!(!same_except_size(&margin, &before));
    }
}

#[cfg(test)]
mod named_style_reach_tests {
    //! Uses of a named style that the change cannot follow refuse the whole change and leave the model as it was.
    use super::*;
    use rhwp::model::document::{Document, RawRecord};

    fn hello() -> Core { open_core(&std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello_world.hwp")).unwrap()).unwrap() }
    fn props() -> Value { json!({"fontSize": 1600}) }

    /// The change on style 0 is refused naming `place`; the document keeps its shapes, styles and runs.
    fn refused(mut core: Core, place: &str) {
        let doc = core.document_mut();
        let before = (doc.doc_info.char_shapes.len(), doc.doc_info.styles[0].char_shape_id, serde_json::to_string(&doc.sections[0].paragraphs).unwrap());
        let e = set_named_style_char(doc, 0, &props()).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedOperation, "{}", e.message);
        assert!(e.message.contains(place), "{}", e.message);
        assert_eq!((doc.doc_info.char_shapes.len(), doc.doc_info.styles[0].char_shape_id, serde_json::to_string(&doc.sections[0].paragraphs).unwrap()), before);
    }
    fn old(doc: &Document) -> u32 { u32::from(doc.doc_info.styles[0].char_shape_id) }
    fn following(doc: &Document) -> rhwp::model::paragraph::Paragraph { let mut p = doc.sections[0].paragraphs[0].clone(); p.controls.clear(); p }

    fn unbatched(core: &mut Core, patch: &Value) -> Repointed {
        let (_, new, repointed) = set_named_style_char(core.document_mut(), 0, patch).unwrap();
        relayout_repointed(core, new, &repointed).unwrap();
        repointed
    }

    #[test]
    fn batched_layout_preserves_every_paragraphs_line_segment_bytes() {
        let mut source = open_core(include_bytes!("../tests/fixtures/color_fill.hwp")).unwrap();
        let table = source.document().sections[0].paragraphs[0].controls.iter().position(|c| matches!(c, Control::Table(_))).unwrap();
        source.insert_text_in_cell_native(0, 0, table, 0, 0, 0, "여러 줄에 걸쳐 배치할 표 안의 글").unwrap();
        source.insert_text_native(0, 1, 0, "표 다음 본문").unwrap();
        let bytes = rhwp::serializer::serialize_hwp_with_report(source.document()).unwrap().into_bytes();
        let line_records = |data: &[u8]| {
            let h = inspect::open(data).unwrap();
            h.section_paths().into_iter().map(|path| h.records[&path].iter().filter_map(|r| match r.tag {
                inspect::TAG_PARA_HEADER => Some((r.level, r.tag, vec![])),
                inspect::TAG_PARA_LINE_SEG => Some((r.level, r.tag, r.payload.clone())),
                _ => None,
            }).collect::<Vec<_>>()).collect::<Vec<_>>()
        };
        let mut old = open_core(&bytes).unwrap();
        let mut batched = open_core(&bytes).unwrap();
        let patch = json!({"fontSize": 3000});
        let changed = unbatched(&mut old, &patch);
        assert!(changed.len() >= 3 && changed.iter().any(|(_, _, path)| !path.is_empty()));
        apply_named_style_char(&mut batched, 0, &patch).unwrap();
        let saved_lines = |core: &Core| line_records(&rhwp::serializer::serialize_hwp_with_report(core.document()).unwrap().into_bytes());
        let reference = saved_lines(&old);
        assert_ne!(reference, line_records(&bytes));
        assert_eq!(saved_lines(&batched), reference);
    }

    #[test]
    #[ignore]
    fn time_named_style_on_largest_fixture() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let path = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "hwp"))
            .max_by_key(|p| p.metadata().unwrap().len()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let patch = props();
        for batched in [false, true] {
            let mut core = open_core(&bytes).unwrap();
            let start = std::time::Instant::now();
            if batched { apply_named_style_char(&mut core, 0, &patch).unwrap(); } else { unbatched(&mut core, &patch); }
            println!("{} ({} bytes), batched={batched}: {:?}", path.display(), bytes.len(), start.elapsed());
        }
    }

    #[test]
    fn a_character_overlap_on_the_style_shape_is_refused() {
        let mut core = hello();
        let doc = core.document_mut();
        let id = old(doc);
        doc.sections[0].paragraphs[0].controls.push(Control::CharOverlap(rhwp::model::control::CharOverlap { chars: vec!['가'], char_shape_ids: vec![id], ..Default::default() }));
        refused(core, "character overlap");
    }

    #[test]
    fn a_master_page_paragraph_of_the_style_is_refused() {
        let mut core = hello();
        let doc = core.document_mut();
        let p = following(doc);
        doc.sections[0].section_def.master_pages.push(rhwp::model::header_footer::MasterPage { paragraphs: vec![p], ..Default::default() });
        refused(core, "master page");
    }

    #[test]
    fn raw_section_records_of_the_style_are_refused() {
        for place in ["master page", "memo"] {
            let mut core = hello();
            let doc = core.document_mut();
            let mut runs = 0u32.to_le_bytes().to_vec();
            runs.extend(old(doc).to_le_bytes());
            let paragraph = [RawRecord { tag_id: inspect::TAG_PARA_HEADER, level: 2, data: vec![0; 22] }, RawRecord { tag_id: inspect::TAG_PARA_CHAR_SHAPE, level: 3, data: runs }];
            if place == "memo" {
                let tail = doc.sections[0].trailing_lists.get_or_insert_with(Default::default);
                tail.records.push(RawRecord { tag_id: 93, level: 1, data: vec![0; 4] });
                tail.records.push(RawRecord { tag_id: inspect::TAG_LIST_HEADER, level: 1, data: vec![0; 8] });
                tail.records.extend(paragraph);
            } else {
                doc.sections[0].section_def.extra_child_records.extend(paragraph);
            }
            refused(core, place);
        }
    }

    #[test]
    fn headers_footers_notes_and_captions_name_the_refused_container() {
        use rhwp::model::{footnote::{Endnote, Footnote}, header_footer::{Footer, Header}, shape::Caption, table::Table};
        let p = following(hello().document());
        for (control, place) in [
            (Control::Header(Box::new(Header { paragraphs: vec![p.clone()], ..Default::default() })), "a header"),
            (Control::Footer(Box::new(Footer { paragraphs: vec![p.clone()], ..Default::default() })), "a footer"),
            (Control::Footnote(Box::new(Footnote { paragraphs: vec![p.clone()], ..Default::default() })), "a footnote"),
            (Control::Endnote(Box::new(Endnote { paragraphs: vec![p.clone()], ..Default::default() })), "an endnote"),
            (Control::Table(Box::new(Table { caption: Some(Caption { paragraphs: vec![p], ..Default::default() }), ..Default::default() })), "a table caption"),
        ] {
            let mut core = hello();
            core.document_mut().sections[0].paragraphs[0].controls.push(control);
            refused(core, place);
        }
    }

    #[test]
    fn an_unknown_control_is_refused_even_without_a_visible_use() {
        let mut core = hello();
        core.document_mut().sections[0].paragraphs[0].controls.push(Control::Unknown(rhwp::model::control::UnknownControl { ctrl_id: 0x1234_5678 }));
        refused(core, "unknown control");
    }

    #[test]
    fn body_and_table_cell_uses_are_followed() {
        let mut core = hello();
        let doc = core.document_mut();
        let id = old(doc);
        let (o, n, repointed) = set_named_style_char(doc, 0, &props()).unwrap();
        assert_eq!((o, n as usize + 1), (id, doc.doc_info.char_shapes.len()));
        assert!(repointed.iter().any(|r| r.2.is_empty()), "{repointed:?}");
        assert!(still_on(doc, 0, id).is_none());
    }
}

#[cfg(test)]
mod inline_object_role_tests {
    //! `is_inline_object` (the text edit check) and `project::role` (the projection) for every kind of control. They
    //! agree on all kinds but `Control::Unknown`: the projection shows an unknown control as a block, and the text edit
    //! check does not count it as an inline object (backlog formats 20).
    use super::*;
    use project::Role;

    /// The answers of `is_inline_object` and `project::role` for each kind of control. The match names every variant.
    fn expected(c: &Control) -> (bool, Role) {
        match c {
            Control::Table(_) | Control::Picture(_) | Control::Shape(_) | Control::Form(_) => (true, Role::Block),
            Control::Unknown(_) => (false, Role::Block),
            Control::Equation(_) | Control::Footnote(_) | Control::Endnote(_) | Control::AutoNumber(_) | Control::Ruby(_) | Control::CharOverlap(_) => (true, Role::Atom),
            Control::SectionDef(_) | Control::ColumnDef(_) | Control::Header(_) | Control::Footer(_) | Control::NewNumber(_) | Control::PageNumberPos(_)
            | Control::Bookmark(_) | Control::IndexMark(_) | Control::PageNumCtrl(_) | Control::Hyperlink(_) | Control::PageHide(_) | Control::HiddenComment(_)
            | Control::Field(_) => (false, Role::Hidden),
        }
    }

    #[test]
    fn every_control_kind_has_the_tabled_answers() {
        let all = vec![
            Control::SectionDef(Box::default()), Control::ColumnDef(Default::default()), Control::Table(Box::default()),
            Control::Shape(Box::new(rhwp::model::shape::ShapeObject::Line(Default::default()))), Control::Picture(Box::default()),
            Control::Header(Box::default()), Control::Footer(Box::default()), Control::Footnote(Box::default()), Control::Endnote(Box::default()),
            Control::AutoNumber(Default::default()), Control::NewNumber(Default::default()), Control::PageNumberPos(Default::default()),
            Control::Bookmark(Default::default()), Control::IndexMark(Default::default()), Control::PageNumCtrl(Default::default()),
            Control::Hyperlink(Default::default()), Control::Ruby(Default::default()), Control::CharOverlap(Default::default()),
            Control::PageHide(Default::default()), Control::HiddenComment(Box::default()), Control::Equation(Box::default()),
            Control::Field(Default::default()), Control::Form(Box::default()), Control::Unknown(Default::default()),
        ];
        let kinds: std::collections::HashSet<_> = all.iter().map(std::mem::discriminant).collect();
        assert_eq!((all.len(), kinds.len()), (24, 24), "one control of each kind");
        for c in &all {
            let (inline, role) = expected(c);
            assert_eq!(is_inline_object(c), inline, "{c:?}");
            assert!(project::role(c) == role, "{c:?}");
            assert_eq!(is_inline_object(c) == (role != Role::Hidden), !matches!(c, Control::Unknown(_)), "{c:?}");
        }
    }

    /// The controls of the HWP fixtures (body and table cell paragraphs): every one gets the answers of [`expected`].
    #[test]
    fn fixture_controls_have_the_tabled_answers() {
        fn walk(paras: &[rhwp::model::paragraph::Paragraph], counts: &mut [usize; 3]) {
            for p in paras {
                for c in &p.controls {
                    let (inline, role) = expected(c);
                    assert_eq!(is_inline_object(c), inline, "{c:?}");
                    assert!(project::role(c) == role, "{c:?}");
                    counts[0] += 1;
                    if inline { counts[1] += 1; }
                    if matches!(c, Control::Unknown(_)) { counts[2] += 1; }
                    if let Control::Table(t) = c { for cell in &t.cells { walk(&cell.paragraphs, counts); } }
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<_> = [root.join("tests/fixtures"), root.join("../../../fixtures/hwp")]
            .iter()
            .flat_map(|dir| std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()))
            .filter(|p| p.extension().is_some_and(|e| e == "hwp"))
            .collect();
        files.sort();
        assert!(files.len() >= 14, "{files:?}");
        let mut counts = [0usize; 3];
        for path in &files {
            let core = open_core(&std::fs::read(path).unwrap()).unwrap_or_else(|e| panic!("{}: {}", path.display(), e.message));
            for s in &core.document().sections { walk(&s.paragraphs, &mut counts); }
        }
        println!("{} fixtures: {} controls, {} inline objects, {} unknown", files.len(), counts[0], counts[1], counts[2]);
        assert!(counts[1] > 0);
    }
}
