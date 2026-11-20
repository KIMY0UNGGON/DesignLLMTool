//! Record-level preservation guard: compares the original file with saved output using the
//! independent parser. Everything not covered by the edit plan must be byte-identical; layout
//! caches (PARA_LINE_SEG) are the only general exception.
use crate::inspect::{self, Hwp5, RecNode, Record};
use designllm_contracts::{AssetRef, Diagnostic};
use std::collections::{BTreeMap, BTreeSet};

/// Expected content of a picture written by an edit (inserted or given a new image).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PicturePlan { pub asset: AssetRef, pub inserted: bool }

/// Changes an edit is allowed to make.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Number of BIN_DATA records of the original file; later positions were added by edits.
    pub original_bin_data: usize,
    /// Original pictures whose image and size may change: (section, output paragraph, control).
    pub pictures_replaced: BTreeSet<(usize, usize, usize)>,
    /// Pictures written by edits and the image they must show: (section, output paragraph, control).
    pub pictures: BTreeMap<(usize, usize, usize), PicturePlan>,
    /// Per section: output top-level paragraph index -> original index (None = new paragraph).
    pub origin: Vec<Vec<Option<usize>>>,
    /// Original paragraphs intentionally removed: (section, original index).
    pub deleted: BTreeSet<(usize, usize)>,
    /// Output paragraphs whose own records may change: (section, output index).
    pub edited: BTreeSet<(usize, usize)>,
    /// Hosts of directly edited cell paragraphs (text, character or paragraph formatting).
    pub subtree_edited: BTreeSet<(usize, usize)>,
    /// Output paragraphs whose explicit page break bit (0x04 of the PARA_HEADER break byte) may change.
    pub page_break_edited: BTreeSet<(usize, usize)>,
    /// Sections whose PAGE_DEF may change.
    pub page_edited: BTreeSet<usize>,
    /// Output paragraphs whose table CTRL_HEADER width/height may change (column widths).
    pub table_resized: BTreeSet<(usize, usize)>,
    /// DocInfo record tags that may be appended after the original records of the same tag.
    pub docinfo_append: BTreeSet<u16>,
    /// New streams allowed (inserted BinData).
    pub new_streams: BTreeSet<String>,
    /// Named style changes in edit order: the style's character shape `old` became the appended `new`.
    pub named_styles: Vec<NamedStylePlan>,
    /// CHAR_SHAPE records the edited model holds (set by every applied edit); with named style changes the saved file
    /// must hold exactly this many and ID_MAPPINGS[9] must say so.
    pub char_shapes: Option<usize>,
    /// With named style changes: the runs (UTF-16 start, shape id) of every top-level output paragraph in the edited
    /// model, per (section, output paragraph). Edited and new paragraphs must be saved with exactly these runs.
    pub expected_runs: BTreeMap<(usize, usize), Vec<(u32, u32)>>,
    /// Final cell paragraphs, in record order, for new hosts or hosts with a direct cell edit.
    pub(crate) expected_cells: BTreeMap<(usize, usize), Vec<CellParagraph>>,
}

#[derive(Debug, Clone)]
pub(crate) struct CellParagraph {
    pub text: String,
    pub style: u8,
    pub para_shape: u16,
    pub runs: Vec<(u32, u32)>,
}

/// One named style change (copy-and-repoint): STYLE `style` and the runs of its paragraphs on `old` point to `new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedStylePlan { pub style: usize, pub old: u32, pub new: u32 }

/// Per paragraph style id: original character shape id -> the id the saved file must hold.
type Remap = BTreeMap<u8, BTreeMap<u32, u32>>;

static NO_REMAP: Remap = BTreeMap::new();

impl Plan {
    /// The named style changes composed from the original file to the output.
    fn remap(&self) -> Remap {
        let mut m: Remap = BTreeMap::new();
        for n in &self.named_styles {
            let Ok(style) = u8::try_from(n.style) else { continue };
            let ids = m.entry(style).or_default();
            for v in ids.values_mut() { if *v == n.old { *v = n.new; } }
            ids.entry(n.old).or_insert(n.new);
        }
        m
    }

    pub fn identity(h: &Hwp5) -> Self {
        let origin = h.section_paths().iter().map(|p| {
            let n = h.records[p].iter().filter(|r| r.level == 0 && r.tag == inspect::TAG_PARA_HEADER).count();
            (0..n).map(Some).collect()
        }).collect();
        let original_bin_data = h.doc_info().iter().filter(|r| r.tag == inspect::TAG_BIN_DATA).count();
        Plan { origin, original_bin_data, ..Default::default() }
    }
}

pub struct GuardReport { pub diagnostics: Vec<Diagnostic>, pub checked_parts: u32, pub checked_regions: u32 }

const DERIVED: &[&str] = &["/PrvText", "/PrvImage"];

fn block(code: &str, message: impl Into<String>) -> Diagnostic { Diagnostic { code: code.into(), message: message.into(), node_id: None, blocking: true, params: Default::default() } }

pub fn check(original: &Hwp5, output: &Hwp5, plan: &Plan) -> GuardReport {
    let mut d = Vec::new();
    let mut parts = 0u32;
    let mut regions = 0u32;
    if !output.header.signature_ok { d.push(block("hwp_invalid_output", "Saved file has no HWP signature")); }
    if let Err(e) = &output.strict_cfb { d.push(block("hwp_invalid_container", format!("Saved file fails strict CFB checks: {e}"))); }
    if output.header.version != original.header.version || output.header.flags != original.header.flags {
        d.push(block("hwp_header_changed", format!("FileHeader changed: {} flags {:#x} -> {} flags {:#x}", original.header.version_string(), original.header.flags, output.header.version_string(), output.header.flags)));
    }
    // Streams.
    for (path, s) in &original.streams {
        if DERIVED.contains(&path.as_str()) { continue; }
        let Some(o) = output.streams.get(path) else { d.push(block("hwp_stream_removed", format!("Stream {} is missing from the saved file", path.escape_debug()))); continue };
        if path == "/DocInfo" || path.starts_with("/BodyText/") { continue; }
        parts += 1;
        if o.raw != s.raw { d.push(block("hwp_stream_changed", format!("Unmodelled stream {} changed", path.escape_debug()))); }
    }
    for path in output.streams.keys() {
        if original.streams.contains_key(path) || DERIVED.contains(&path.as_str()) || plan.new_streams.contains(path) { continue; }
        d.push(block("hwp_unexpected_stream", format!("Saved file adds stream {}", path.escape_debug())));
    }
    // Structural consistency of the output.
    for p in output.doc_info_consistency() { d.push(block("hwp_docinfo_inconsistent", p)); }
    for sp in output.section_paths() {
        for p in inspect::section_consistency(&output.records[&sp]) { d.push(block("hwp_section_inconsistent", format!("{sp}: {p}"))); }
    }
    // DocInfo.
    parts += 1;
    let remap = plan.remap();
    if plan.named_styles.is_empty() {
        d.extend(check_docinfo(original.doc_info(), output.doc_info(), plan));
    } else {
        let (a, b, problems) = named_style_docinfo(original.doc_info(), output.doc_info(), plan);
        d.extend(problems);
        d.extend(check_docinfo(&a, &b, plan));
        // Exactly the character shapes the edits made: no unplanned one, and ID_MAPPINGS[9] counts them.
        let saved = output.doc_info().iter().filter(|r| r.tag == inspect::TAG_CHAR_SHAPE).count();
        match plan.char_shapes {
            Some(n) if n == saved && output.id_mappings().get(9).copied() == i32::try_from(n).ok() => {}
            n => d.push(block("hwp_named_style_shape_count", format!("Saved file holds {saved} character shapes (ID_MAPPINGS[9] {:?}); the edits made {n:?}", output.id_mappings().get(9)))),
        }
    }
    // BinData added by edits: embedded record, existing stream, payload decodes per its flags.
    for (k, entry) in output.bin_data_entries().iter().enumerate().skip(plan.original_bin_data) {
        let position = k + 1;
        match entry {
            Some(e) if e.kind() == 1 => match output.bin_data_payload(position as u16) {
                Ok(bytes) if !bytes.is_empty() => {}
                Ok(_) => d.push(block("hwp_bin_data_inconsistent", format!("Added BinData {position} ({}) is empty", e.stream_path()))),
                Err(problem) => d.push(block("hwp_bin_data_inconsistent", format!("Added BinData {position}: {problem}"))),
            },
            _ => d.push(block("hwp_bin_data_inconsistent", format!("Added BinData {position} is not an embedded item"))),
        }
    }
    // Sections.
    let (osecs, nsecs) = (original.section_paths(), output.section_paths());
    if osecs != nsecs { d.push(block("hwp_sections_changed", format!("Section streams changed: {osecs:?} -> {nsecs:?}"))); }
    for (si, sp) in osecs.iter().enumerate() {
        let (Some(a), Some(b)) = (original.records.get(sp), output.records.get(sp)) else { continue };
        parts += 1;
        let (ta, tb) = match (inspect::build_tree(a), inspect::build_tree(b)) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => { d.push(block("hwp_section_tree", format!("{sp}: {e}"))); continue; }
        };
        let empty = Vec::new();
        let origin = plan.origin.get(si).unwrap_or(&empty);
        if origin.len() != tb.len() { d.push(block("hwp_paragraphs_changed", format!("{sp}: expected {} paragraphs, saved {}", origin.len(), tb.len()))); continue; }
        let referenced: BTreeSet<usize> = origin.iter().flatten().copied().collect();
        for i in 0..ta.len() {
            if !referenced.contains(&i) && !plan.deleted.contains(&(si, i)) { d.push(block("hwp_paragraph_removed", format!("{sp}: original paragraph {i} is missing"))); }
        }
        for (j, o) in origin.iter().enumerate() {
            // With named style changes, an edited or new paragraph holds exactly the runs of the edited model.
            if !plan.named_styles.is_empty() && (o.is_none() || plan.edited.contains(&(si, j))) {
                if !plan.expected_runs.get(&(si, j)).is_some_and(|expected| has_runs(b, &tb[j], expected)) {
                    d.push(block("hwp_named_style_runs_changed", format!("{sp}: paragraph {j} saved runs differ from the edited document")));
                }
            }
            if !plan.named_styles.is_empty() && (o.is_none() || plan.subtree_edited.contains(&(si, j))) {
                let original_host = o.and_then(|i| ta.get(i));
                if let Some(problem) = check_cell_paragraphs(a, original_host, b, &tb[j], plan.expected_cells.get(&(si, j)).map(Vec::as_slice).unwrap_or_default()) {
                    d.push(block("hwp_named_style_cell_changed", format!("{sp}: paragraph {j}: {problem}")));
                }
            }
            let Some(i) = o else { continue };
            let Some(na) = ta.get(*i) else { d.push(block("hwp_plan_invalid", format!("{sp}: plan references paragraph {i}"))); continue };
            regions += 1;
            let resize = plan.table_resized.contains(&(si, j));
            let mode = Mode {
                own: plan.edited.contains(&(si, j)),
                subtree: plan.subtree_edited.contains(&(si, j)) || (resize && plan.named_styles.is_empty()),
                page: plan.page_edited.contains(&si), resize, break_bit: plan.page_break_edited.contains(&(si, j)), remap: &remap,
            };
            let pictures: BTreeSet<usize> = plan.pictures_replaced.iter().filter(|(s, p, _)| *s == si && *p == j).map(|x| x.2).collect();
            if let Some(problem) = compare_paragraph(a, na, b, &tb[j], mode, &pictures) {
                d.push(block("hwp_unrelated_record_changed", format!("{sp}: paragraph {j} (original {i}): {problem}")));
            }
        }
    }
    GuardReport { diagnostics: d, checked_parts: parts, checked_regions: regions }
}

fn runs(payload: &[u8]) -> impl Iterator<Item = (u32, u32)> + '_ {
    payload.chunks_exact(8).map(|r| (u32::from_le_bytes(r[..4].try_into().unwrap()), u32::from_le_bytes(r[4..].try_into().unwrap())))
}

fn has_runs(records: &[Record], p: &RecNode, expected: &[(u32, u32)]) -> bool {
    let mut shapes = p.children.iter().map(|c| &records[c.index]).filter(|r| r.tag == inspect::TAG_PARA_CHAR_SHAPE);
    let payload = shapes.next().map(|r| r.payload.as_slice()).unwrap_or_default();
    shapes.next().is_none() && payload.len() % 8 == 0 && runs(payload).eq(expected.iter().copied())
}

fn cell_paragraphs<'a>(records: &[Record], host: &'a RecNode, out: &mut Vec<&'a RecNode>) {
    for ctrl in &host.children {
        let r = &records[ctrl.index];
        if r.tag != inspect::TAG_CTRL_HEADER || !r.payload.starts_with(b" lbt") { continue; }
        // A table caption precedes TABLE; only the paragraph lists after TABLE belong to cells.
        for p in ctrl.children.iter().skip_while(|c| records[c.index].tag != inspect::TAG_TABLE)
            .filter(|c| records[c.index].tag == inspect::TAG_PARA_HEADER) {
            out.push(p);
            cell_paragraphs(records, p, out);
        }
    }
}

fn cell_text(record: Option<&Record>) -> String {
    use inspect::TextUnit::*;
    record.into_iter().flat_map(|r| inspect::text_units(&r.payload)).filter_map(|unit| match unit {
        Char(c) => Some(c), Ctrl { code: 9, .. } => Some('\t'), Mark(10) => Some('\n'), Mark(24) => Some('-'),
        Mark(30) => Some('\u{00A0}'), Mark(31) => Some('\u{2007}'), _ => None,
    }).collect()
}

fn text_records<'a>(records: &'a [Record], p: &'a RecNode) -> impl Iterator<Item = (usize, &'a Record)> + 'a {
    p.children.iter().map(|c| &records[c.index]).filter(|r| r.tag != inspect::TAG_PARA_LINE_SEG)
        .enumerate().filter(|(_, r)| r.tag == inspect::TAG_PARA_TEXT)
}

/// Structure edits can also change cell text or formatting. Compare those fields with the final model;
/// when the model kept the original text, preserve even its nonprinting control bytes and record presence.
fn check_cell_paragraphs(a: &[Record], host: Option<&RecNode>, b: &[Record], saved_host: &RecNode, model: &[CellParagraph]) -> Option<String> {
    let mut original = Vec::new();
    if let Some(host) = host { cell_paragraphs(a, host, &mut original); }
    let mut saved = Vec::new();
    cell_paragraphs(b, saved_host, &mut saved);
    if saved.len() != model.len() || (host.is_some() && original.len() != saved.len()) {
        return Some(format!("cell paragraph count: original {}, saved {}, model {}", original.len(), saved.len(), model.len()));
    }
    for (i, (p, expected)) in saved.into_iter().zip(model).enumerate() {
        let Some(h) = inspect::para_header(&b[p.index].payload) else { return Some(format!("cell paragraph {i}: short PARA_HEADER")) };
        if (h.style_id, h.para_shape_id, usize::from(h.char_shape_count)) != (expected.style, expected.para_shape, expected.runs.len()) {
            return Some(format!("cell paragraph {i}: PARA_HEADER differs from the edited document"));
        }
        if !has_runs(b, p, &expected.runs) {
            return Some(format!("cell paragraph {i}: PARA_CHAR_SHAPE differs from the edited document"));
        }
        let mut texts = text_records(b, p);
        let text = texts.next();
        if texts.next().is_some() || cell_text(text.map(|(_, r)| r)) != expected.text {
            return Some(format!("cell paragraph {i}: PARA_TEXT differs from the edited document"));
        }
        if let Some(old) = original.get(i) {
            let old_text = text_records(a, old).next();
            let count_changed = inspect::para_header(&a[old.index].payload).map(|v| v.nchars) != Some(h.nchars);
            if (old_text != text || count_changed) && cell_text(old_text.map(|(_, r)| r)) == expected.text {
                return Some(format!("cell paragraph {i}: original PARA_TEXT or PARA_HEADER character count changed"));
            }
        }
    }
    None
}

fn check_docinfo(a: &[Record], b: &[Record], plan: &Plan) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    let mutable = [inspect::TAG_DOCUMENT_PROPERTIES, inspect::TAG_ID_MAPPINGS];
    let mut by_tag_a: BTreeMap<u16, Vec<&Record>> = BTreeMap::new();
    let mut by_tag_b: BTreeMap<u16, Vec<&Record>> = BTreeMap::new();
    for r in a { by_tag_a.entry(r.tag).or_default().push(r); }
    for r in b { by_tag_b.entry(r.tag).or_default().push(r); }
    let tags: BTreeSet<u16> = by_tag_a.keys().chain(by_tag_b.keys()).copied().collect();
    for tag in tags {
        let ra = by_tag_a.get(&tag).cloned().unwrap_or_default();
        let rb = by_tag_b.get(&tag).cloned().unwrap_or_default();
        if mutable.contains(&tag) {
            if ra.len() != rb.len() { d.push(block("hwp_docinfo_changed", format!("DocInfo {} count {} -> {}", inspect::tag_name(tag), ra.len(), rb.len()))); }
            if tag == inspect::TAG_DOCUMENT_PROPERTIES {
                for (x, y) in ra.iter().zip(&rb) {
                    // Section count and starting numbers (first 14 bytes) must stay; caret position may move.
                    if x.payload.get(..14) != y.payload.get(..14) { d.push(block("hwp_docinfo_changed", "DOCUMENT_PROPERTIES numbering or section count changed")); }
                }
            }
            continue;
        }
        if rb.len() < ra.len() { d.push(block("hwp_docinfo_record_removed", format!("DocInfo lost {} {} records", ra.len() - rb.len(), inspect::tag_name(tag)))); continue; }
        for (k, (x, y)) in ra.iter().zip(&rb).enumerate() {
            if x.payload != y.payload || x.level != y.level { d.push(block("hwp_docinfo_record_changed", format!("DocInfo {} #{k} changed", inspect::tag_name(tag)))); break; }
        }
        if rb.len() > ra.len() && !plan.docinfo_append.contains(&tag) {
            d.push(block("hwp_docinfo_record_added", format!("DocInfo adds {} {} records", rb.len() - ra.len(), inspect::tag_name(tag))));
        }
    }
    // Record order: the original sequence must be a subsequence of the output.
    let mut it = b.iter();
    for r in a {
        if mutable.contains(&r.tag) { continue; }
        if !it.any(|x| x.tag == r.tag && x.level == r.level && x.payload == r.payload) {
            d.push(block("hwp_docinfo_order_changed", format!("DocInfo record {} moved or changed", inspect::tag_name(r.tag))));
            break;
        }
    }
    d
}

/// The DocInfo changes of named style edits, checked before the general comparison: a planned STYLE record changes
/// only its character shape id, to the composed target; the shape an edit appended is its source shape with only the
/// face ids, base size, bold/italic bits and text colour changed; ID_MAPPINGS changes only the counts of tags that edits
/// append. Returns both record lists with the planned STYLE ids blanked, for `check_docinfo`, and the problems.
fn named_style_docinfo(a: &[Record], b: &[Record], plan: &Plan) -> (Vec<Record>, Vec<Record>, Vec<Diagnostic>) {
    let mut d = Vec::new();
    let remap = plan.remap();
    let (mut a2, mut b2) = (a.to_vec(), b.to_vec());
    let positions = |recs: &[Record], tag: u16| recs.iter().enumerate().filter(|(_, r)| r.tag == tag).map(|(i, _)| i).collect::<Vec<_>>();
    let (sa, sb) = (positions(a, inspect::TAG_STYLE), positions(b, inspect::TAG_STYLE));
    let planned: BTreeSet<usize> = plan.named_styles.iter().map(|n| n.style).collect();
    for k in planned {
        let (Some(&ia), Some(&ib)) = (sa.get(k), sb.get(k)) else { d.push(block("hwp_named_style_record_changed", format!("DocInfo STYLE #{k} is missing"))); continue };
        let (Some(x), Some(y)) = (inspect::parse_style(&a[ia].payload), inspect::parse_style(&b[ib].payload)) else { d.push(block("hwp_named_style_record_changed", format!("DocInfo STYLE #{k} cannot be read"))); continue };
        let want = u8::try_from(k).ok().and_then(|s| remap.get(&s)).and_then(|m| m.get(&u32::from(x.char_shape_id))).copied();
        blank(&mut a2[ia].payload, x.char_shape_at..x.char_shape_at + 2);
        blank(&mut b2[ib].payload, x.char_shape_at..x.char_shape_at + 2);
        if a2[ia].payload != b2[ib].payload || a[ia].level != b[ib].level { d.push(block("hwp_named_style_record_changed", format!("DocInfo STYLE #{k} changed outside its character shape id"))); }
        if Some(u32::from(y.char_shape_id)) != want { d.push(block("hwp_named_style_record_changed", format!("DocInfo STYLE #{k} points to character shape {}, expected {want:?}", y.char_shape_id))); }
    }
    let (ca, cb) = (positions(a, inspect::TAG_CHAR_SHAPE), positions(b, inspect::TAG_CHAR_SHAPE));
    for n in &plan.named_styles {
        let (Some(&src), Some(&made)) = (cb.get(n.old as usize), cb.get(n.new as usize)) else { d.push(block("hwp_named_style_shape_changed", format!("Character shape {} or {} of a named style change is missing", n.old, n.new))); continue };
        if (n.new as usize) < ca.len() { d.push(block("hwp_named_style_shape_changed", format!("Character shape {} of a named style change is not an appended one", n.new))); }
        let masked = |p: &[u8]| { let mut v = p.to_vec(); for r in [0..14, 42..46, 52..56] { blank(&mut v, r); } if let Some(x) = v.get_mut(46) { *x &= !0x03; } v };
        if b[src].payload.len() != b[made].payload.len() || masked(&b[src].payload) != masked(&b[made].payload) {
            d.push(block("hwp_named_style_shape_changed", format!("Character shape {} differs from its source {} outside face, size, bold, italic and colour", n.new, n.old)));
        }
    }
    let ids = |recs: &[Record]| recs.iter().find(|r| r.tag == inspect::TAG_ID_MAPPINGS).map(|r| r.payload.chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect::<Vec<_>>()).unwrap_or_default();
    let (ia, ib) = (ids(a), ids(b));
    let tag_of = |i: usize| match i {
        0 => Some(inspect::TAG_BIN_DATA), 1..=7 => Some(inspect::TAG_FACE_NAME), 8 => Some(inspect::TAG_BORDER_FILL), 9 => Some(inspect::TAG_CHAR_SHAPE),
        10 => Some(inspect::TAG_TAB_DEF), 11 => Some(inspect::TAG_NUMBERING), 12 => Some(inspect::TAG_BULLET), 13 => Some(inspect::TAG_PARA_SHAPE), _ => None,
    };
    if ia.len() != ib.len() { d.push(block("hwp_named_style_ids_changed", format!("ID_MAPPINGS has {} entries, the original {}", ib.len(), ia.len()))); }
    for (i, (x, y)) in ia.iter().zip(&ib).enumerate() {
        if x != y && !tag_of(i).map(|t| plan.docinfo_append.contains(&t)).unwrap_or(false) { d.push(block("hwp_named_style_ids_changed", format!("ID_MAPPINGS[{i}] changed {x} -> {y}"))); }
    }
    (a2, b2, d)
}

#[derive(Clone, Copy)]
/// `remap`: named style changes (per paragraph style: original character shape id -> saved id) that PARA_CHAR_SHAPE records may show.
struct Mode<'a> { own: bool, subtree: bool, page: bool, resize: bool, break_bit: bool, remap: &'a Remap }

/// A top-level paragraph. Controls listed in `pictures` (CTRL_HEADER ordinals) are pictures whose
/// image and size were replaced: only those fields may differ (see `compare_picture`).
fn compare_paragraph(a: &[Record], na: &RecNode, b: &[Record], nb: &RecNode, mode: Mode<'_>, pictures: &BTreeSet<usize>) -> Option<String> {
    if pictures.is_empty() { return compare_node(a, na, b, nb, mode, true); }
    let ctrl = |recs: &[Record], n: &RecNode| -> Vec<usize> { n.children.iter().filter(|c| recs[c.index].tag == inspect::TAG_CTRL_HEADER).map(|c| c.index).collect() };
    let (ca, cb) = (ctrl(a, na), ctrl(b, nb));
    if ca.len() != cb.len() { return Some(format!("control count {} -> {}", ca.len(), cb.len())); }
    for &k in pictures {
        let (Some(&ia), Some(&ib)) = (ca.get(k), cb.get(k)) else { return Some(format!("replaced picture control {k} is missing")) };
        let (xa, xb) = (na.children.iter().find(|c| c.index == ia).unwrap(), nb.children.iter().find(|c| c.index == ib).unwrap());
        if let Some(p) = compare_picture(a, xa, b, xb) { return Some(format!("picture control {k}: {p}")); }
    }
    // Everything else as usual, with the replaced pictures taken out of both sides.
    let strip = |n: &RecNode, keep: &[usize]| RecNode { index: n.index, children: n.children.iter().filter(|c| !pictures.iter().any(|k| keep.get(*k) == Some(&c.index))).cloned().collect() };
    compare_node(a, &strip(na, &ca), b, &strip(nb, &cb), mode, true)
}

/// A picture whose image was replaced: the frame size, SHAPE_COMPONENT sizes, rotation centre and
/// rendering matrices, and the picture's corner points, crop, BinData id and image extent may
/// change. Every other byte (anchor, wrap, offsets, margins, ids, description, border, padding,
/// effects, caption, CTRL_DATA) must be identical.
fn compare_picture(a: &[Record], na: &RecNode, b: &[Record], nb: &RecNode) -> Option<String> {
    let (ra, rb) = (&a[na.index], &b[nb.index]);
    if ra.payload.get(0..4) != Some(&b" osg"[..]) || rb.payload.get(0..4) != Some(&b" osg"[..]) || ra.level != rb.level { return Some("the control is not the same picture object".into()); }
    let masked = |p: &[u8], ranges: &[std::ops::Range<usize>]| { let mut v = p.to_vec(); for r in ranges { blank(&mut v, r.clone()); } v };
    if ra.payload.len() != rb.payload.len() || masked(&ra.payload, &[16..24]) != masked(&rb.payload, &[16..24]) { return Some("picture CTRL_HEADER changed outside its size".into()); }
    if na.children.len() != nb.children.len() { return Some(format!("picture child count {} -> {}", na.children.len(), nb.children.len())); }
    for (x, y) in na.children.iter().zip(&nb.children) {
        let (rx, ry) = (&a[x.index], &b[y.index]);
        if rx.tag != ry.tag || rx.level != ry.level { return Some(format!("picture record {} became {}", inspect::tag_name(rx.tag), inspect::tag_name(ry.tag))); }
        if rx.tag != inspect::TAG_SHAPE_COMPONENT {
            if let Some(p) = compare_node(a, x, b, y, Mode { own: false, subtree: false, page: false, resize: false, break_bit: false, remap: &NO_REMAP }, false) { return Some(p); }
            continue;
        }
        let base = if rx.payload.len() >= 8 && rx.payload[0..4] == rx.payload[4..8] { 8 } else { 4 };
        let comp = [base + 12..base + 28, base + 34..rx.payload.len().max(ry.payload.len())];
        if rx.payload.len() != ry.payload.len() || masked(&rx.payload, &comp) != masked(&ry.payload, &comp) { return Some("picture SHAPE_COMPONENT changed outside sizes and rendering".into()); }
        if x.children.len() != y.children.len() { return Some("picture SHAPE_COMPONENT child count changed".into()); }
        for (u, v) in x.children.iter().zip(&y.children) {
            let (ru, rv) = (&a[u.index], &b[v.index]);
            if ru.tag != rv.tag || ru.level != rv.level { return Some(format!("picture record {} became {}", inspect::tag_name(ru.tag), inspect::tag_name(rv.tag))); }
            if ru.tag == inspect::TAG_SHAPE_COMPONENT_PICTURE {
                let extra = ru.payload.len().saturating_sub(73);
                let mut pic = vec![12..60, 71..73];
                if extra == 17 || extra == 18 { pic.push(82..90); }
                if ru.payload.len() != rv.payload.len() || masked(&ru.payload, &pic) != masked(&rv.payload, &pic) { return Some("SHAPE_COMPONENT_PICTURE changed outside points, crop, image id and extent".into()); }
                if !u.children.is_empty() || !v.children.is_empty() { return Some("SHAPE_COMPONENT_PICTURE has child records".into()); }
            } else if let Some(p) = compare_node(a, u, b, v, Mode { own: false, subtree: false, page: false, resize: false, break_bit: false, remap: &NO_REMAP }, false) { return Some(p); }
        }
    }
    None
}

/// Compare two record subtrees. `top` marks the top-level paragraph.
fn compare_node(a: &[Record], na: &RecNode, b: &[Record], nb: &RecNode, mode: Mode<'_>, top: bool) -> Option<String> {
    let (ra, rb) = (&a[na.index], &b[nb.index]);
    if ra.tag != rb.tag { return Some(format!("record {} became {}", inspect::tag_name(ra.tag), inspect::tag_name(rb.tag))); }
    let own = if top { mode.own } else { mode.subtree };
    match ra.tag {
        inspect::TAG_PARA_HEADER => {
            if ra.payload != rb.payload {
                // Fields that follow text/format edits: nchars, para shape id, char shape count, line seg count.
                let mut x = ra.payload.clone();
                let mut y = rb.payload.clone();
                // Line seg count follows layout; bit 31 of nchars marks the last paragraph of a list
                // (position-derived, verified by section_consistency).
                for range in [16..18usize] { blank(&mut x, range.clone()); blank(&mut y, range); }
                if let (Some(a3), Some(b3)) = (x.get_mut(3), y.get_mut(3)) { *a3 &= 0x7F; *b3 &= 0x7F; }
                if own { for range in [0..4usize, 8..10, 12..14] { blank(&mut x, range.clone()); blank(&mut y, range); } }
                // The explicit page break bit (0x04) may change only in a paragraph that a page break edit names.
                if mode.break_bit {
                    if let Some(v) = x.get_mut(11) { *v &= !0x04; }
                    if let Some(v) = y.get_mut(11) { *v &= !0x04; }
                }
                if x != y {
                    let diff: Vec<usize> = (0..x.len().max(y.len())).filter(|i| x.get(*i) != y.get(*i)).collect();
                    return Some(format!("PARA_HEADER changed outside the edited fields (bytes {diff:?}: {:02X?} -> {:02X?})", ra.payload, rb.payload));
                }
            }
        }
        inspect::TAG_PARA_LINE_SEG => {}
        inspect::TAG_PARA_TEXT | inspect::TAG_PARA_CHAR_SHAPE if own => {}
        inspect::TAG_PARA_RANGE_TAG if own => {
            let tags = |p: &[u8]| -> Vec<u32> { p.chunks_exact(12).map(|c| u32::from_le_bytes(c[8..12].try_into().unwrap())).collect() };
            if tags(&ra.payload) != tags(&rb.payload) { return Some("range tags changed".into()); }
        }
        inspect::TAG_PAGE_DEF if mode.page => {}
        inspect::TAG_TABLE | inspect::TAG_LIST_HEADER if mode.subtree || mode.resize => {}
        inspect::TAG_CTRL_HEADER if mode.resize && ra.payload.get(0..4) == Some(&b" lbt"[..]) => {
            let (mut x, mut y) = (ra.payload.clone(), rb.payload.clone());
            blank(&mut x, 16..24);
            blank(&mut y, 16..24);
            if x != y || ra.level != rb.level { return Some("table CTRL_HEADER changed outside its size".into()); }
        }
        _ => { if ra.payload != rb.payload || ra.level != rb.level { return Some(format!("{} changed", inspect::tag_name(ra.tag))); } }
    }
    // Children: same sequence except optional PARA_TEXT / PARA_LINE_SEG presence in edited paragraphs.
    let optional = |r: &Record| matches!(r.tag, inspect::TAG_PARA_LINE_SEG) || (own && r.tag == inspect::TAG_PARA_TEXT);
    let ca: Vec<&RecNode> = na.children.iter().filter(|c| !optional(&a[c.index])).collect();
    let cb: Vec<&RecNode> = nb.children.iter().filter(|c| !optional(&b[c.index])).collect();
    if ca.len() != cb.len() { return Some(format!("{} child count {} -> {}", inspect::tag_name(ra.tag), ca.len(), cb.len())); }
    for (x, y) in ca.iter().zip(&cb) {
        let child_mode = Mode { own: false, break_bit: false, ..mode };
        // A paragraph of a changed named style: its runs on the style's previous shape show the new one, nothing else.
        if !own && ra.tag == inspect::TAG_PARA_HEADER && a[x.index].tag == inspect::TAG_PARA_CHAR_SHAPE {
            if let Some(m) = inspect::para_header(&ra.payload).and_then(|h| mode.remap.get(&h.style_id)) {
                let (rx, ry) = (&a[x.index], &b[y.index]);
                let want = runs(&rx.payload).map(|(pos, id)| (pos, m.get(&id).copied().unwrap_or(id)));
                if ry.tag != rx.tag || ry.level != rx.level || rx.payload.len() % 8 != 0 || ry.payload.len() != rx.payload.len()
                    || !want.eq(runs(&ry.payload)) || !x.children.is_empty() || !y.children.is_empty() {
                    return Some("PARA_CHAR_SHAPE differs from the named style remap".into());
                }
                continue;
            }
        }
        // Paragraph records directly under an edited top-level paragraph belong to it.
        if top && matches!(a[x.index].tag, inspect::TAG_PARA_TEXT | inspect::TAG_PARA_CHAR_SHAPE | inspect::TAG_PARA_RANGE_TAG) {
            if let Some(p) = compare_node(a, x, b, y, Mode { own: mode.own, ..child_mode }, true) { return Some(p); }
            continue;
        }
        if let Some(p) = compare_node(a, x, b, y, child_mode, false) { return Some(p); }
    }
    // Optional records that remain must still be consistent (LINE_SEG is a cache; PARA_TEXT is checked semantically).
    None
}

fn blank(v: &mut [u8], range: std::ops::Range<usize>) {
    if let Some(s) = v.get_mut(range) { s.fill(0); }
}
