//! Same-format save. The writer reads the original bytes, compares the target model with the model
//! those bytes produce, and renders only the elements whose model changed. All other bytes are
//! copied from the source, including unknown subtrees, comments, and untouched paragraphs.
use std::collections::{BTreeMap, HashMap, HashSet};

use designllm_contracts::{
    sha256, AssetRef, AssetStore, Diagnostic, DltResult, FlowBlock, FlowDocument, Node, Paragraph, PageSetup,
    ParagraphFormat, Section, Style, Table, TextRun, EMU_PER_HWPUNIT,
};
use designllm_package::Package;

use crate::diag;
use crate::doc::{self, Loaded};
use crate::header::{child_el, HeaderPlan, NativeLine, PCharPr, PHeader, PParaPr};
use crate::ops::{new_host_line, paragraph_line, NativeLines};
use crate::package::{self, CONTENT_HPF, HEADER, PREVIEW_IMAGE, PREVIEW_TEXT};
use crate::section::{cache_keys, leading_section_head, AtomKind, PAtom, PBlock, PHidden, PImage, PPara, PSection, PTable};
use crate::text::{LINE_BREAK_CHAR, OBJECT_CHAR, TAB_CHAR};
use crate::xmlx::{esc_attr, esc_text, render_key, render_part, InlineRun, Item, Overrides, Xml, NS_HP};

/// Row height used for generated tables, in HWPUNIT.
const GENERATED_ROW_HU: i64 = 2000;
/// Cell and table margins used for generated tables, in HWPUNIT.
const CELL_MARGIN_HU: i64 = 141;

/// Output of a save: the package bytes and the diagnostics of the plan.
pub struct Written {
    pub bytes: Vec<u8>,
    pub diagnostics: Vec<Diagnostic>,
}

enum Edit {
    Set(String, Vec<u8>),
    Remove(String),
}

/// Sequential numeric ids for generated elements. Starts above every id already in the part.
struct Ids(u64);

impl Ids {
    fn take(&mut self) -> u64 {
        let v = self.0;
        self.0 += 1;
        v
    }
}

fn max_numeric_id(xml: &Xml<'_>) -> u64 {
    let mut max = 0u64;
    for node in xml.doc.descendants() {
        for name in ["id", "instid"] {
            if let Some(v) = node.attribute(name).and_then(|v| v.parse::<u64>().ok()) {
                max = max.max(v);
            }
        }
    }
    max
}

/// Exact conversion to HWPUNIT. Values off the native grid are rejected here; the operation layer
/// rounds them first and records a `NATIVE_UNIT_ROUNDING` diagnostic.
fn to_hu(emu: i64) -> DltResult<i64> {
    if emu % EMU_PER_HWPUNIT != 0 {
        return Err(diag::unsupported(format!("{emu} EMU is not on the HWPX native grid")));
    }
    Ok(emu / EMU_PER_HWPUNIT)
}

fn block_id(block: &FlowBlock) -> &str {
    match block {
        FlowBlock::Paragraph(p) => &p.id,
        FlowBlock::Table(t) => &t.id,
        FlowBlock::Image(n) => &n.id,
        FlowBlock::Opaque { id, .. } => id,
    }
}

/// True when no other top-level block of the section shares the paragraph anchor's base (`hp:p[id=…]`),
/// so removing this paragraph does not renumber the occurrence suffix of any other anchor.
fn anchor_is_unique(orig: &PSection, host_key: &str) -> bool {
    let base = crate::verify::base_anchor(host_key);
    orig.blocks.iter().filter(|b| crate::verify::base_anchor(b.host_key()) == base).count() == 1
}

fn atom_display(kind: AtomKind) -> &'static str {
    match kind {
        AtomKind::Tab => TAB_CHAR,
        AtomKind::LineBreak => LINE_BREAK_CHAR,
        AtomKind::Object => OBJECT_CHAR,
    }
}

/// Equal in everything the paragraph shape carries, except the page break. A proportional line spacing is compared
/// by its percentage: its pitch is derived from the glyph sizes.
fn shape_eq(a: &ParagraphFormat, b: &ParagraphFormat) -> bool {
    a.align == b.align
        && a.space_before == b.space_before
        && a.space_after == b.space_after
        && designllm_flow::line_spacing::same_line_spacing(a, b)
        && a.indent == b.indent
}

pub(crate) fn format_eq(a: &ParagraphFormat, b: &ParagraphFormat) -> bool {
    shape_eq(a, b) && a.page_break_before == b.page_break_before
}

/// Value of `hp:p/@pageBreak` for a flag, in the spelling the original attribute uses: `true`/`false`, else `1`/`0`.
fn page_break_value(original: Option<&str>, on: bool) -> &'static str {
    match (original, on) {
        (Some("true" | "false"), true) => "true",
        (Some("true" | "false"), false) => "false",
        (_, true) => "1",
        (_, false) => "0",
    }
}

fn setup_eq(a: &PageSetup, b: &PageSetup) -> bool {
    a.size == b.size
        && a.margin_left == b.margin_left
        && a.margin_top == b.margin_top
        && a.margin_right == b.margin_right
        && a.margin_bottom == b.margin_bottom
}

/// Runs compared without ids: adjacent unprotected runs with equal style are one run, and empty runs
/// are dropped. Protected runs stay separate. The save planner and `ops::check_locked_runs` share it; `verify.rs`
/// `visible_runs` keeps its own copy on purpose, so the verifier rereads a saved file independently of the writer.
pub(crate) fn normalized_runs(runs: &[TextRun]) -> Vec<(String, Style, bool)> {
    let mut out: Vec<(String, Style, bool)> = Vec::new();
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if !last.2 && !run.protected && last.1 == run.style => last.0.push_str(&run.text),
            _ => out.push((run.text.clone(), run.style.clone(), run.protected)),
        }
    }
    out
}

fn group_items(items: Vec<(u32, Item)>) -> Vec<InlineRun> {
    let mut runs: Vec<InlineRun> = Vec::new();
    for (char_pr, item) in items {
        match runs.last_mut() {
            Some(run) if run.char_pr == char_pr => run.items.push(item),
            _ => runs.push(InlineRun { char_pr, items: vec![item] }),
        }
    }
    runs
}

fn hidden_item(h: &PHidden) -> Item {
    if h.in_t {
        Item::TAtom(h.key.clone())
    } else {
        Item::RAtom(h.key.clone())
    }
}

fn atom_item(a: &PAtom) -> Item {
    if a.in_t {
        Item::TAtom(a.key.clone())
    } else {
        Item::RAtom(a.key.clone())
    }
}

fn ext_for_mime(mime: &str) -> DltResult<&'static str> {
    match mime {
        "image/png" => Ok("png"),
        "image/bmp" => Ok("bmp"),
        "image/jpeg" | "image/jpg" => Ok("jpg"),
        "image/gif" => Ok("gif"),
        other => Err(diag::unsupported(format!("image type {other} cannot be embedded in HWPX"))),
    }
}

/// Planning state shared by the header, sections, and new binary items.
struct Planner<'a> {
    assets: &'a dyn AssetStore,
    loaded: &'a Loaded,
    header: &'a PHeader,
    lines: &'a NativeLines,
    hxml: &'a Xml<'a>,
    hplan: HeaderPlan,
    header_changed: bool,
    manifest_adds: Vec<Vec<u8>>,
    new_bins: Vec<(String, Vec<u8>)>,
    planned_assets: Vec<(String, String)>,
    /// Pictures that a figure link may update: block id to the render hash the link names.
    figure_targets: &'a BTreeMap<String, String>,
}

impl<'a> Planner<'a> {
    fn base_char(&self) -> DltResult<&'a PCharPr> {
        self.header.char_prs.first().ok_or_else(|| diag::malformed("header has no character shape"))
    }

    fn base_para(&self) -> DltResult<&'a PParaPr> {
        self.header.para_prs.first().ok_or_else(|| diag::malformed("header has no paragraph shape"))
    }

    fn plan_styles(&mut self, target: &FlowDocument) -> DltResult<()> {
        for name in self.loaded.flow.styles.keys() {
            if !target.styles.contains_key(name) {
                return Err(diag::unsupported(format!("removing named style {name} is not supported in HWPX")));
            }
        }
        for (name, style) in &target.styles {
            if self.loaded.flow.styles.get(name) == Some(style) {
                continue;
            }
            let st = self
                .header
                .style_by_name(name)
                .ok_or_else(|| diag::unsupported(format!("creating named style {name} is not supported in HWPX")))?;
            let base = self.header.char_pr(st.char_pr).ok_or_else(|| diag::malformed("style without charPr"))?;
            let id = self.hplan.char_pr_for(self.hxml, self.header, base, style).map_err(diag::malformed)?;
            self.hplan.set_style_char_pr(st, id);
            self.header_changed = true;
        }
        Ok(())
    }

    /// Native id of a picture's binary item for `asset`, adding a BinData part when none matches.
    fn asset_bin(&mut self, asset: &AssetRef) -> DltResult<String> {
        if let Some((id, _)) = self.loaded.bins.iter().find(|(_, b)| b.asset.sha256 == asset.sha256) {
            return Ok(id.clone());
        }
        if let Some((_, id)) = self.planned_assets.iter().find(|(sha, _)| sha == &asset.sha256) {
            return Ok(id.clone());
        }
        let bytes = self.assets.read(asset)?;
        if sha256(&bytes) != asset.sha256 || bytes.len() as u64 != asset.bytes {
            return Err(diag::invalid(diag::UNSUPPORTED_OPERATION, "image bytes do not match their asset reference"));
        }
        let ext = ext_for_mime(&asset.mime)?;
        let mut max = 0u32;
        for item in &self.loaded.manifest.items {
            if let Some(n) = item.id.strip_prefix("image").and_then(|d| d.parse::<u32>().ok()) {
                max = max.max(n);
            }
        }
        for (name, _) in &self.new_bins {
            if let Some(n) = name.strip_prefix("BinData/image").and_then(|d| d.split('.').next()).and_then(|d| d.parse::<u32>().ok()) {
                max = max.max(n);
            }
        }
        let n = max + 1;
        let id = format!("image{n}");
        let href = format!("BinData/image{n}.{ext}");
        self.manifest_adds.push(
            format!("<opf:item id=\"{id}\" href=\"{href}\" media-type=\"{}\" isEmbeded=\"1\"/>", esc_attr(&asset.mime))
                .into_bytes(),
        );
        self.new_bins.push((href, bytes));
        self.planned_assets.push((asset.sha256.clone(), id.clone()));
        Ok(id)
    }

    /// Character shape id for a run style. Reuses an original shape when one has the same style.
    fn char_for(&mut self, orig: &PPara, style: &Style, prefer: Option<u32>) -> DltResult<u32> {
        if let Some(p) = prefer {
            if self.header.char_pr(p).map(|c| &c.style) == Some(style) {
                return Ok(p);
            }
        }
        for cp in &orig.orig_cps {
            if self.header.char_pr(*cp).map(|c| &c.style) == Some(style) {
                return Ok(*cp);
            }
        }
        let base = self.header.char_pr(orig.base_char_pr).ok_or_else(|| diag::malformed("missing base charPr"))?;
        self.hplan.char_pr_for(self.hxml, self.header, base, style).map_err(diag::malformed)
    }

    fn inline_runs(&mut self, orig: &PPara, art: &Paragraph) -> DltResult<Vec<InlineRun>> {
        let protected: Vec<&TextRun> = art.runs.iter().filter(|r| r.protected).collect();
        if protected.len() != orig.atoms.len() {
            return Err(diag::unsupported("inline objects cannot be added or removed inside an existing paragraph"));
        }
        for (run, atom) in protected.iter().zip(&orig.atoms) {
            if run.text != atom_display(atom.kind) {
                return Err(diag::unsupported("inline objects cannot be changed inside a text paragraph"));
            }
        }
        let mut items: Vec<(u32, Item)> = Vec::new();
        for h in &orig.hidden_start {
            items.push((orig.base_char_pr, hidden_item(h)));
        }
        let mut atom_no = 0usize;
        for run in &art.runs {
            if run.protected {
                let atom = &orig.atoms[atom_no];
                atom_no += 1;
                let cp = self.char_for(orig, &run.style, Some(atom.char_pr))?;
                items.push((cp, atom_item(atom)));
                for h in &atom.hidden_after {
                    items.push((cp, hidden_item(h)));
                }
            } else if !run.text.is_empty() {
                if run.text.contains(TAB_CHAR) || run.text.contains(LINE_BREAK_CHAR) {
                    return Err(diag::unsupported("tab and line-break characters cannot be inserted as text in HWPX"));
                }
                let cp = self.char_for(orig, &run.style, None)?;
                items.push((cp, Item::Text(run.text.clone())));
            }
        }
        let runs = group_items(items);
        if runs.is_empty() {
            // Every character is gone. One empty run keeps the base shape, which the reader takes the size from.
            return Ok(vec![InlineRun { char_pr: orig.base_char_pr, items: Vec::new() }]);
        }
        Ok(runs)
    }

    fn plan_paragraph(&mut self, orig: &PPara, art: &Paragraph, ov: &mut Overrides) -> DltResult<bool> {
        let runs_changed = normalized_runs(&orig.runs) != normalized_runs(&art.runs);
        let style_changed = orig.style_ref != art.style_ref;
        let base = self.header.para_pr(orig.para_pr).ok_or_else(|| diag::malformed("missing paraPr"))?;
        let line = paragraph_line(art, Some((&orig.format, &base.line)), self.lines).map_err(diag::unsupported)?;
        let line_edit = (line != base.line || self.lines.get(&art.id).is_some_and(|l| !l.approximated())).then_some(&line);
        let format_changed = !format_eq(&orig.format, &art.format) || line_edit.is_some();
        if !runs_changed && !format_changed && !style_changed {
            return Ok(false);
        }
        if runs_changed && orig.regen_locked {
            return Err(diag::locked_runs(&orig.id));
        }
        // A carrier's shape is the host `hp:p`'s shape, which the picture or table after it shares. Editing it
        // would change that object's paragraph too, so only the carrier's content is left to the rules above.
        if orig.page_break_carrier && (!shape_eq(&orig.format, &art.format) || line_edit.is_some() || style_changed) {
            return Err(diag::unsupported(
                "the format and style of a paragraph that carries a picture's or table's page break cannot be edited in HWPX: its shape is shared with that picture or table",
            ));
        }
        if runs_changed {
            let runs = self.inline_runs(orig, art)?;
            ov.regen.insert(orig.key.clone(), runs);
        }
        // Only an edited format writes the shape. The file's pitch is the base, so an unchanged line
        // spacing keeps its `hh:lineSpacing`, and a glyph edit never rewrites a PERCENT or approximated one.
        if format_changed {
            let id = self.hplan.para_pr_for(self.hxml, base, &art.format, line_edit).map_err(diag::malformed)?;
            ov.set_attr(&orig.key, "paraPrIDRef", id.to_string());
        }
        // The page break is an attribute of the host `hp:p`. A picture or table paragraph's break is carried by
        // its carrier paragraph, which cannot go away, so that break cannot be cleared.
        if art.format.page_break_before != orig.format.page_break_before {
            if orig.page_break_carrier && !art.format.page_break_before {
                return Err(diag::unsupported("the page break of a picture-only or table-only paragraph cannot be removed in HWPX"));
            }
            ov.set_attr(&orig.key, "pageBreak", page_break_value(orig.page_break_attr.as_deref(), art.format.page_break_before));
        }
        if style_changed {
            let name = art.style_ref.as_ref().ok_or_else(|| diag::unsupported("removing a paragraph style is not supported"))?;
            let style = self.header.style_by_name(name).ok_or_else(|| diag::unsupported(format!("unknown style {name}")))?;
            ov.set_attr(&orig.key, "styleIDRef", style.id.to_string());
        }
        Ok(true)
    }

    /// A new displayed size changes `hp:sz` and the corners of `hp:imgRect`. A picture whose image a
    /// figure link replaces also gets the new image: `hc:img` names the new binary item, and `hp:orgSz`,
    /// `hp:imgClip`, and `hp:imgDim` describe the whole new image at the displayed size, as an inserted
    /// picture does. Any other image change is refused.
    fn plan_image(&mut self, orig: &PImage, art: &Node, ov: &mut Overrides) -> DltResult<bool> {
        let replaced = art.asset.as_ref() != Some(&orig.asset);
        let width = to_hu(art.bounds.width)?;
        let height = to_hu(art.bounds.height)?;
        let resized = width != orig.width_hu || height != orig.height_hu;
        if !replaced && !resized {
            return Ok(false);
        }
        let sz = orig.sz_key.clone().ok_or_else(|| diag::unsupported("picture without hp:sz cannot be resized"))?;
        let [pt1, pt2, pt3] = orig.rect_keys.clone().ok_or_else(|| diag::unsupported("picture without hp:imgRect cannot be resized"))?;
        if replaced {
            let new_asset = art.asset.clone().ok_or_else(|| diag::unsupported("a picture cannot lose its image"))?;
            let render = self
                .figure_targets
                .get(&art.id)
                .cloned()
                .ok_or_else(|| diag::unsupported("replacing the image of an existing picture is not supported in HWPX; only a figure link update changes it"))?;
            if new_asset.sha256 != render {
                return Err(diag::unsupported("the picture image does not match the render of its figure link"));
            }
            let [img, org, clip, dim] = orig
                .figure_keys
                .clone()
                .ok_or_else(|| diag::unsupported("picture without a replaceable image record cannot take a figure update"))?;
            let bin = self.asset_bin(&new_asset)?;
            ov.set_attr(&img, "binaryItemIDRef", bin);
            ov.set_attr(&org, "width", width.to_string());
            ov.set_attr(&org, "height", height.to_string());
            ov.set_attr(&clip, "left", "0");
            ov.set_attr(&clip, "top", "0");
            ov.set_attr(&clip, "right", width.to_string());
            ov.set_attr(&clip, "bottom", height.to_string());
            ov.set_attr(&dim, "dimwidth", width.to_string());
            ov.set_attr(&dim, "dimheight", height.to_string());
        }
        ov.set_attr(&sz, "width", width.to_string());
        ov.set_attr(&sz, "height", height.to_string());
        ov.set_attr(&pt1, "x", width.to_string());
        ov.set_attr(&pt2, "x", width.to_string());
        ov.set_attr(&pt2, "y", height.to_string());
        ov.set_attr(&pt3, "y", height.to_string());
        Ok(true)
    }

    fn plan_table(&mut self, orig: &PTable, art: &Table, ov: &mut Overrides, ids: &mut Ids) -> DltResult<bool> {
        if orig.rows.len() != art.rows.len() {
            return Err(diag::unsupported("adding or removing table rows is not supported in HWPX"));
        }
        for (orow, arow) in orig.rows.iter().zip(&art.rows) {
            if orow.len() != arow.len() {
                return Err(diag::unsupported("changing table cell structure is not supported in HWPX"));
            }
            for (ocell, acell) in orow.iter().zip(arow) {
                if ocell.id != acell.id || ocell.col_span != acell.column_span || ocell.row_span != acell.row_span {
                    return Err(diag::unsupported("changing table cell structure is not supported in HWPX"));
                }
            }
        }
        let orig_widths: Vec<i64> = orig.widths_hu.iter().map(|w| w * EMU_PER_HWPUNIT).collect();
        let mut changed = false;
        if art.column_widths != orig_widths {
            if art.column_widths.len() != orig.widths_hu.len() {
                return Err(diag::unsupported("changing the number of table columns is not supported in HWPX"));
            }
            let widths_hu: Vec<i64> = art.column_widths.iter().map(|w| to_hu(*w)).collect::<DltResult<_>>()?;
            for row in &orig.rows {
                for cell in row {
                    let start = cell.col as usize;
                    let end = (start + cell.col_span as usize).min(widths_hu.len());
                    let total: i64 = widths_hu[start..end].iter().sum();
                    ov.set_attr(&cell.width_key, "width", total.to_string());
                }
            }
            let sz_key = orig.sz_key.clone().ok_or_else(|| diag::malformed("table without hp:sz"))?;
            ov.set_attr(&sz_key, "width", widths_hu.iter().sum::<i64>().to_string());
            changed = true;
        }
        for (orow, arow) in orig.rows.iter().zip(&art.rows) {
            for (ocell, acell) in orow.iter().zip(arow) {
                changed |= self.plan_nested(&ocell.blocks, &acell.blocks, ov, ids)?;
            }
        }
        Ok(changed)
    }

    /// Blocks inside a table cell: same ids and order, changes only inside existing blocks.
    fn plan_nested(
        &mut self,
        orig: &[PBlock],
        art: &[FlowBlock],
        ov: &mut Overrides,
        ids: &mut Ids,
    ) -> DltResult<bool> {
        if orig.len() != art.len() {
            return Err(diag::unsupported("adding or removing content inside a table cell is not supported in HWPX"));
        }
        let mut changed = false;
        for (ob, ab) in orig.iter().zip(art) {
            if ob.id() != block_id(ab) {
                return Err(diag::unsupported("replacing content inside a table cell is not supported in HWPX"));
            }
            changed |= self.plan_block(ob, ab, ov, ids)?;
        }
        Ok(changed)
    }

    fn plan_block(&mut self, orig: &PBlock, art: &FlowBlock, ov: &mut Overrides, ids: &mut Ids) -> DltResult<bool> {
        match (orig, art) {
            (PBlock::Para(op), FlowBlock::Paragraph(ap)) => self.plan_paragraph(op, ap, ov),
            (PBlock::Table(ot), FlowBlock::Table(at)) => self.plan_table(ot, at, ov, ids),
            (PBlock::Image(oi), FlowBlock::Image(an)) => self.plan_image(oi, an, ov),
            (PBlock::Opaque(oo), FlowBlock::Opaque { label, .. }) if &oo.label == label => Ok(false),
            _ => Err(diag::unsupported("changing the kind of an existing block is not supported in HWPX")),
        }
    }

    /// Plans one section. Returns whether anything changed and the first changed block index.
    fn plan_section(
        &mut self,
        orig: &PSection,
        art: &Section,
        ov: &mut Overrides,
        ids: &mut Ids,
    ) -> DltResult<(bool, Option<usize>)> {
        let orig_pos: HashMap<&str, usize> = orig.blocks.iter().enumerate().map(|(i, b)| (b.id(), i)).collect();
        let art_ids: HashSet<&str> = art.blocks.iter().map(block_id).collect();
        // Only a picture that is the sole content of its paragraph can be removed, and only when its
        // paragraph anchor is unique in the section: removing it renumbers no other anchor.
        let mut first: Option<usize> = None;
        for (oi, block) in orig.blocks.iter().enumerate() {
            if art_ids.contains(block.id()) {
                continue;
            }
            match block {
                PBlock::Image(picture) if picture.sole_content && anchor_is_unique(orig, &picture.host_key) => {
                    ov.drop.insert(picture.host_key.clone());
                    first = Some(first.map_or(oi, |f| f.min(oi)));
                }
                PBlock::Image(picture) => {
                    return Err(diag::unsupported(format!(
                        "removing picture {} is not supported in HWPX: only a top-level picture that is the sole content of its paragraph, with a unique paragraph anchor and no page break, can be removed",
                        picture.id
                    )));
                }
                other => return Err(diag::unsupported(format!("removing block {} is not supported in HWPX", other.id()))),
            }
        }
        let mut changed = first.is_some();
        let mut last_orig: Option<usize> = None;
        // A new carrier folded into the host of the new object that follows it.
        let mut carried: Option<&Paragraph> = None;
        for (ai, ab) in art.blocks.iter().enumerate() {
            let id = block_id(ab);
            let carrier = carried.take();
            match orig_pos.get(id) {
                Some(&oi) => {
                    if last_orig.map(|prev| oi <= prev).unwrap_or(false) {
                        return Err(diag::unsupported("reordering blocks is not supported in HWPX"));
                    }
                    last_orig = Some(oi);
                    if self.plan_block(&orig.blocks[oi], ab, ov, ids)? {
                        changed = true;
                        first = Some(first.map_or(oi, |f| f.min(oi)));
                    }
                }
                None => {
                    // The carrier of an object's page break shares its element with the object, so nothing can go between them.
                    if let Some(prev) = last_orig {
                        if matches!(&orig.blocks[prev], PBlock::Para(p) if p.page_break_carrier) {
                            return Err(diag::unsupported("a block cannot be inserted between a page break carrier and the picture or table of its paragraph in HWPX"));
                        }
                    }
                    changed = true;
                    let boundary = last_orig.unwrap_or(0);
                    first = Some(first.map_or(boundary, |f| f.min(boundary)));
                    match carrier_fold(orig, &orig_pos, &art.blocks, ai, self.lines) {
                        Some(Fold::NewObject(p)) => {
                            carried = Some(p);
                            continue;
                        }
                        Some(Fold::ExistingHost(host_key, value)) => {
                            ov.set_attr(host_key, "pageBreak", value);
                            continue;
                        }
                        None => {}
                    }
                    let bytes = self.new_block(ab, carrier, ids)?;
                    match last_orig {
                        Some(prev) => ov.after.entry(orig.blocks[prev].host_key().to_string()).or_default().push(bytes),
                        None => match orig.blocks.iter().find(|b| art_ids.contains(b.id())) {
                            Some(head) => ov.before.entry(head.host_key().to_string()).or_default().push(bytes),
                            None => ov.append.entry(orig.key.clone()).or_default().push(bytes),
                        },
                    }
                }
            }
        }
        Ok((changed, first))
    }

    /// A new block. `carrier` is the new paragraph that carries the page break of a new picture or table.
    fn new_block(&mut self, block: &FlowBlock, carrier: Option<&Paragraph>, ids: &mut Ids) -> DltResult<Vec<u8>> {
        match block {
            FlowBlock::Paragraph(p) => self.new_paragraph(p, &paragraph_line(p, None, self.lines).map_err(diag::unsupported)?, ids),
            FlowBlock::Table(t) => self.new_table_paragraph(t, carrier, ids),
            FlowBlock::Image(n) => self.image_paragraph(n, carrier, ids),
            FlowBlock::Opaque { .. } => Err(diag::unsupported("inserting opaque blocks is not supported in HWPX")),
        }
    }

    /// The paragraph shape id and the `styleIDRef` attribute that read back as the format and style of `p`.
    fn paragraph_shape(&mut self, p: &Paragraph, line: &NativeLine) -> DltResult<(u32, String)> {
        let base_para = self.base_para()?;
        let para_id = self.hplan.para_pr_for(self.hxml, base_para, &p.format, Some(line)).map_err(diag::malformed)?;
        // Without a style reference the attribute is omitted, so a reparse reads `None` again.
        let style_attr = match &p.style_ref {
            Some(name) => {
                let id = self
                    .header
                    .style_by_name(name)
                    .ok_or_else(|| diag::unsupported(format!("unknown style {name}")))?
                    .id;
                format!(" styleIDRef=\"{id}\"")
            }
            None => String::new(),
        };
        Ok((para_id, style_attr))
    }

    /// Shape id, `styleIDRef` attribute and `pageBreak` value of the host `hp:p` of a new picture or table. With a
    /// carrier the host takes the carrier's shape and break, which the reader gives back to the carrier; without one
    /// the host starts no page.
    fn object_host(&mut self, id: &str, carrier: Option<&Paragraph>) -> DltResult<(u32, String, &'static str)> {
        let line = new_host_line(id, carrier, self.header, self.lines)?;
        match carrier {
            Some(p) => {
                let (para_id, style_attr) = self.paragraph_shape(p, &line)?;
                Ok((para_id, style_attr, page_break_value(None, true)))
            }
            None => {
                let base = self.base_para()?;
                let format = ParagraphFormat { page_break_before: false, ..base.format.clone() };
                let para_id = self.hplan.para_pr_for(self.hxml, base, &format, Some(&line)).map_err(diag::malformed)?;
                Ok((para_id, String::new(), page_break_value(None, false)))
            }
        }
    }

    fn new_paragraph(&mut self, p: &Paragraph, line: &NativeLine, ids: &mut Ids) -> DltResult<Vec<u8>> {
        let (para_id, style_attr) = self.paragraph_shape(p, line)?;
        let base_char = self.base_char()?;
        let mut items: Vec<(u32, Item)> = Vec::new();
        for run in &p.runs {
            if run.protected {
                return Err(diag::unsupported("inline objects can only be inserted as separate blocks in HWPX"));
            }
            if run.text.is_empty() {
                continue;
            }
            if run.text.contains(TAB_CHAR) || run.text.contains(LINE_BREAK_CHAR) {
                return Err(diag::unsupported("tab and line-break characters cannot be inserted as text in HWPX"));
            }
            let cp = self.hplan.char_pr_for(self.hxml, self.header, base_char, &run.style).map_err(diag::malformed)?;
            items.push((cp, Item::Text(run.text.clone())));
        }
        let runs = group_items(items);
        let body = if runs.is_empty() {
            format!("<hp:run charPrIDRef=\"{}\"/>", base_char.id)
        } else {
            runs.iter()
                .map(|run| {
                    let text: String = run
                        .items
                        .iter()
                        .filter_map(|i| match i {
                            Item::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect();
                    format!("<hp:run charPrIDRef=\"{}\"><hp:t>{}</hp:t></hp:run>", run.char_pr, esc_text(&text))
                })
                .collect()
        };
        let id = ids.take();
        let break_attr = page_break_value(None, p.format.page_break_before);
        Ok(format!(
            "<hp:p id=\"{id}\" paraPrIDRef=\"{para_id}\"{style_attr} pageBreak=\"{break_attr}\" columnBreak=\"0\" merged=\"0\">{body}</hp:p>"
        )
        .into_bytes())
    }

    fn image_paragraph(&mut self, node: &Node, carrier: Option<&Paragraph>, ids: &mut Ids) -> DltResult<Vec<u8>> {
        let asset = node.asset.as_ref().ok_or_else(|| diag::unsupported("image block without an asset"))?;
        let bin = self.asset_bin(asset)?;
        let w = to_hu(node.bounds.width)?;
        let h = to_hu(node.bounds.height)?;
        let para_id = ids.take();
        let pic_id = ids.take();
        let inst_id = ids.take();
        let base_char = self.base_char()?.id;
        let (host_para, style_attr, break_attr) = self.object_host(&node.id, carrier)?;
        let pic = format!(
            "<hp:pic id=\"{pic_id}\" zOrder=\"0\" numberingType=\"PICTURE\" textWrap=\"TOP_AND_BOTTOM\" textFlow=\"BOTH_SIDES\" lock=\"0\" dropcapstyle=\"None\" href=\"\" groupLevel=\"0\" instid=\"{inst_id}\" reverse=\"0\"><hp:offset x=\"0\" y=\"0\"/><hp:orgSz width=\"{w}\" height=\"{h}\"/><hp:curSz width=\"0\" height=\"0\"/><hp:flip horizontal=\"0\" vertical=\"0\"/><hp:rotationInfo angle=\"0\" centerX=\"{cx}\" centerY=\"{cy}\" rotateimage=\"1\"/><hp:renderingInfo><hc:transMatrix e1=\"1\" e2=\"0\" e3=\"0\" e4=\"0\" e5=\"1\" e6=\"0\"/><hc:scaMatrix e1=\"1\" e2=\"0\" e3=\"0\" e4=\"0\" e5=\"1\" e6=\"0\"/><hc:rotMatrix e1=\"1\" e2=\"0\" e3=\"0\" e4=\"0\" e5=\"1\" e6=\"0\"/></hp:renderingInfo><hc:img binaryItemIDRef=\"{bin}\" bright=\"0\" contrast=\"0\" effect=\"REAL_PIC\" alpha=\"0\"/><hp:imgRect><hc:pt0 x=\"0\" y=\"0\"/><hc:pt1 x=\"{w}\" y=\"0\"/><hc:pt2 x=\"{w}\" y=\"{h}\"/><hc:pt3 x=\"0\" y=\"{h}\"/></hp:imgRect><hp:imgClip left=\"0\" right=\"{w}\" top=\"0\" bottom=\"{h}\"/><hp:inMargin left=\"0\" right=\"0\" top=\"0\" bottom=\"0\"/><hp:imgDim dimwidth=\"{w}\" dimheight=\"{h}\"/><hp:effects/><hp:sz width=\"{w}\" widthRelTo=\"ABSOLUTE\" height=\"{h}\" heightRelTo=\"ABSOLUTE\" protect=\"0\"/><hp:pos treatAsChar=\"1\" affectLSpacing=\"0\" flowWithText=\"1\" allowOverlap=\"0\" holdAnchorAndSO=\"0\" vertRelTo=\"PARA\" horzRelTo=\"COLUMN\" vertAlign=\"TOP\" horzAlign=\"LEFT\" vertOffset=\"0\" horzOffset=\"0\"/><hp:outMargin left=\"0\" right=\"0\" top=\"0\" bottom=\"0\"/></hp:pic>",
            cx = w / 2,
            cy = h / 2,
        );
        Ok(format!(
            "<hp:p id=\"{para_id}\" paraPrIDRef=\"{host_para}\"{style_attr} pageBreak=\"{break_attr}\" columnBreak=\"0\" merged=\"0\"><hp:run charPrIDRef=\"{base_char}\">{pic}</hp:run></hp:p>"
        )
        .into_bytes())
    }

    fn new_table(&mut self, t: &Table, carrier: Option<&Paragraph>, ids: &mut Ids) -> DltResult<Vec<u8>> {
        let col_count = t.column_widths.len();
        if col_count == 0 {
            return Err(diag::unsupported("a table needs at least one column"));
        }
        let widths: Vec<i64> = t.column_widths.iter().map(|w| to_hu(*w)).collect::<DltResult<_>>()?;
        let border = self.header.border_fill_ids.first().copied().ok_or_else(|| diag::malformed("header has no borderFill"))?;

        // Grid placement with row and column spans; a cell's anchor is its top-left grid position.
        let mut occupied: Vec<Vec<bool>> = Vec::new();
        let mut placed: Vec<(usize, usize, usize, usize, usize, usize)> = Vec::new(); // (row index, cell index, col, row, cs, rs)
        for (r, row) in t.rows.iter().enumerate() {
            while occupied.len() <= r {
                occupied.push(vec![false; col_count]);
            }
            let mut c = 0usize;
            for (ci, cell) in row.iter().enumerate() {
                while c < col_count && occupied[r][c] {
                    c += 1;
                }
                let cs = cell.column_span.max(1) as usize;
                let rs = cell.row_span.max(1) as usize;
                if c + cs > col_count {
                    return Err(diag::unsupported("table cell spans beyond the table columns"));
                }
                for rr in r..r + rs {
                    while occupied.len() <= rr {
                        occupied.push(vec![false; col_count]);
                    }
                    for cc in c..c + cs {
                        occupied[rr][cc] = true;
                    }
                }
                placed.push((r, ci, c, r, cs, rs));
                c += cs;
            }
        }
        let total_rows = occupied.len().max(t.rows.len());
        let table_width: i64 = widths.iter().sum();
        let table_height = total_rows as i64 * GENERATED_ROW_HU;
        let table_id = ids.take();

        let mut rows_xml: Vec<String> = vec![String::new(); total_rows];
        for (r, ci, c, _, cs, rs) in placed {
            let cell = &t.rows[r][ci];
            let w: i64 = widths[c..c + cs].iter().sum();
            let h = rs as i64 * GENERATED_ROW_HU;
            let mut paras = String::new();
            for block in &cell.blocks {
                paras.push_str(&String::from_utf8_lossy(&self.new_block(block, None, ids)?));
            }
            if paras.is_empty() {
                // A raw snapshot lacks the insertion-time predecessor (later inserts can change its
                // neighbor). Its host plan, or style 0 for an unplanned host, seeds the missing paragraph.
                let line = new_host_line(&t.id, carrier, self.header, self.lines)?;
                paras.push_str(&String::from_utf8_lossy(&self.new_paragraph(&empty_paragraph(), &line.inherited(), ids)?));
            }
            rows_xml[r].push_str(&format!(
                "<hp:tc name=\"\" header=\"0\" hasMargin=\"0\" protect=\"0\" editable=\"0\" dirty=\"0\" borderFillIDRef=\"{border}\"><hp:subList id=\"\" textDirection=\"HORIZONTAL\" lineWrap=\"BREAK\" vertAlign=\"CENTER\" linkListIDRef=\"0\" linkListNextIDRef=\"0\" textWidth=\"0\" textHeight=\"0\" hasTextRef=\"0\" hasNumRef=\"0\">{paras}</hp:subList><hp:cellAddr colAddr=\"{c}\" rowAddr=\"{r}\"/><hp:cellSpan colSpan=\"{cs}\" rowSpan=\"{rs}\"/><hp:cellSz width=\"{w}\" height=\"{h}\"/><hp:cellMargin left=\"{CELL_MARGIN_HU}\" right=\"{CELL_MARGIN_HU}\" top=\"{CELL_MARGIN_HU}\" bottom=\"{CELL_MARGIN_HU}\"/></hp:tc>"
            ));
        }
        let trs: String = rows_xml.iter().map(|r| format!("<hp:tr>{r}</hp:tr>")).collect();
        Ok(format!(
            "<hp:tbl id=\"{table_id}\" zOrder=\"0\" numberingType=\"TABLE\" textWrap=\"TOP_AND_BOTTOM\" textFlow=\"BOTH_SIDES\" lock=\"0\" dropcapstyle=\"None\" pageBreak=\"CELL\" repeatHeader=\"0\" rowCnt=\"{total_rows}\" colCnt=\"{col_count}\" cellSpacing=\"0\" borderFillIDRef=\"{border}\" noAdjust=\"0\"><hp:sz width=\"{table_width}\" widthRelTo=\"ABSOLUTE\" height=\"{table_height}\" heightRelTo=\"ABSOLUTE\" protect=\"0\"/><hp:pos treatAsChar=\"1\" affectLSpacing=\"0\" flowWithText=\"1\" allowOverlap=\"0\" holdAnchorAndSO=\"0\" vertRelTo=\"PARA\" horzRelTo=\"COLUMN\" vertAlign=\"TOP\" horzAlign=\"LEFT\" vertOffset=\"0\" horzOffset=\"0\"/><hp:outMargin left=\"283\" right=\"283\" top=\"283\" bottom=\"283\"/><hp:inMargin left=\"{CELL_MARGIN_HU}\" right=\"{CELL_MARGIN_HU}\" top=\"{CELL_MARGIN_HU}\" bottom=\"{CELL_MARGIN_HU}\"/>{trs}</hp:tbl>"
        )
        .into_bytes())
    }

    /// A generated table inside a cell paragraph (a nested table keeps its own host paragraph).
    fn new_table_paragraph(&mut self, t: &Table, carrier: Option<&Paragraph>, ids: &mut Ids) -> DltResult<Vec<u8>> {
        let table = self.new_table(t, carrier, ids)?;
        let para_id = ids.take();
        let base_char = self.base_char()?.id;
        let (host_para, style_attr, break_attr) = self.object_host(&t.id, carrier)?;
        Ok(format!(
            "<hp:p id=\"{para_id}\" paraPrIDRef=\"{host_para}\"{style_attr} pageBreak=\"{break_attr}\" columnBreak=\"0\" merged=\"0\"><hp:run charPrIDRef=\"{base_char}\">{}</hp:run></hp:p>",
            String::from_utf8_lossy(&table)
        )
        .into_bytes())
    }
}

/// How a new page-break carrier is saved instead of an `hp:p` of its own.
enum Fold<'a> {
    /// On the host of the new picture or table after it, which the carrier gives its shape and break.
    NewObject(&'a Paragraph),
    /// As `pageBreak` (this value) on the host (this key) of the existing picture or table after it.
    ExistingHost(&'a str, &'static str),
}

/// The fold of block `index` of a target section, when it is a new page-break carrier
/// (`designllm_flow::is_page_break_carrier`, not a block of the source): the object's break, kept on the object's
/// host `hp:p`, as the reader takes it back (`section::carrier_of`). `orig_pos` gives the source index of each source
/// block id.
fn carrier_fold<'a>(orig: &'a PSection, orig_pos: &HashMap<&str, usize>, blocks: &'a [FlowBlock], index: usize, lines: &NativeLines) -> Option<Fold<'a>> {
    let FlowBlock::Paragraph(p) = blocks.get(index)? else { return None };
    if orig_pos.contains_key(p.id.as_str()) || !designllm_flow::is_page_break_carrier(blocks, index) {
        return None;
    }
    match orig_pos.get(block_id(&blocks[index + 1])) {
        None => Some(Fold::NewObject(p)),
        Some(&oi) => existing_host_break(&orig.blocks[oi], p, lines).map(|value| Fold::ExistingHost(orig.blocks[oi].host_key(), value)),
    }
}

/// Carriers that have no `hp:p` of their own in the saved file: the carriers of the source, and the new carriers that
/// the save folds into an object's host. They are not preview lines (the writer and the verification both take the
/// preview text with these), and the HWPX layout gives them no line (`designllm_flow::LayoutOptions`).
pub fn folded_carriers<'a>(sections: &'a [PSection], target: &'a FlowDocument, lines: &NativeLines) -> HashSet<&'a str> {
    let mut ids = crate::section::carrier_ids(sections);
    for (orig, section) in sections.iter().zip(&target.sections) {
        let orig_pos: HashMap<&str, usize> = orig.blocks.iter().enumerate().map(|(i, b)| (b.id(), i)).collect();
        for (index, block) in section.blocks.iter().enumerate() {
            if carrier_fold(orig, &orig_pos, &section.blocks, index, lines).is_some() {
                ids.insert(block_id(block));
            }
        }
    }
    ids
}

/// The `pageBreak` value that folds the new carrier `p` into the host of an existing picture or table, when the host
/// has no break and a break on it reads back as `p` (`section::HostShape`). Folding a carrier of another shape would
/// change the object's paragraph shape, which the model does not show, so such a carrier keeps an `hp:p` of its own.
fn existing_host_break(object: &PBlock, p: &Paragraph, lines: &NativeLines) -> Option<&'static str> {
    let shape = match object {
        PBlock::Table(t) => t.host_shape.as_ref()?,
        PBlock::Image(i) => i.host_shape.as_ref()?,
        _ => return None,
    };
    let line = paragraph_line(p, None, lines).ok()?;
    let matches = shape.uniform_line && shape.line == line && format_eq(&shape.format, &p.format)
        && shape.format.line_spacing == p.format.line_spacing && shape.style_ref == p.style_ref;
    matches.then(|| page_break_value(shape.page_break_attr.as_deref(), true))
}

fn empty_paragraph() -> Paragraph {
    Paragraph {
        id: String::new(),
        runs: Vec::new(),
        style_ref: None,
        format: ParagraphFormat::default(),
    }
}

/// Preview text policy: one line per top-level paragraph, CRLF after each line, atoms omitted. The carrier
/// paragraphs (`carriers`, the ids from `folded_carriers`) are not lines: they hold no text of their own.
pub fn preview_text(doc: &FlowDocument, carriers: &HashSet<&str>) -> String {
    let mut out = String::new();
    for section in &doc.sections {
        for block in &section.blocks {
            if let FlowBlock::Paragraph(p) = block {
                if carriers.contains(p.id.as_str()) {
                    continue;
                }
                out.push_str(&crate::text::plain_text(&p.runs));
                out.push_str("\r\n");
            }
        }
    }
    out
}

fn inject_into_first_run(para_bytes: &mut Vec<u8>, lead: &[u8]) -> DltResult<()> {
    let s = std::str::from_utf8(para_bytes).map_err(|e| diag::malformed(e.to_string()))?;
    let start = s
        .find("<hp:run ")
        .or_else(|| s.find("<hp:run>"))
        .ok_or_else(|| diag::malformed("generated block has no hp:run"))?;
    let gt = start + s[start..].find('>').ok_or_else(|| diag::malformed("unclosed hp:run start tag"))?;
    let lead_str = std::str::from_utf8(lead).map_err(|e| diag::malformed(e.to_string()))?;
    let mut out = String::with_capacity(s.len() + lead_str.len() + 9);
    if s.as_bytes()[gt - 1] == b'/' {
        out.push_str(&s[..gt - 1]);
        out.push('>');
        out.push_str(lead_str);
        out.push_str("</hp:run>");
        out.push_str(&s[gt + 1..]);
    } else {
        out.push_str(&s[..gt + 1]);
        out.push_str(lead_str);
        out.push_str(&s[gt + 1..]);
    }
    *para_bytes = out.into_bytes();
    Ok(())
}

/// Keeps `hp:secPr` (and its adjacent `hp:ctrl`) at the start of the section's first `hp:p/hp:run`
/// when a block is inserted before `orig.blocks[0]` or `orig.blocks[0]` is removed.
fn relocate_section_head(
    xml: &Xml<'_>,
    orig: &PSection,
    art: &Section,
    ov: &mut Overrides,
    default_char_pr: u32,
    lines: &NativeLines,
) -> DltResult<bool> {
    let Some(orig_first) = orig.blocks.first() else { return Ok(false) };
    // Refused whether or not the section leads with its head: a section part needs a paragraph.
    let Some(art_first) = art.blocks.first() else {
        return Err(diag::unsupported("removing every block of a section is not supported in HWPX"));
    };
    let Some(orig_node) = xml.node(orig_first.host_key()) else { return Ok(false) };
    let Some((sec_pr, opt_ctrl)) = leading_section_head(orig_node) else { return Ok(false) };
    // A new carrier folded into the existing object after it writes no `hp:p`: that object's host leads the section.
    let orig_pos: HashMap<&str, usize> = orig.blocks.iter().enumerate().map(|(i, b)| (b.id(), i)).collect();
    let art_first = match carrier_fold(orig, &orig_pos, &art.blocks, 0, lines) {
        Some(Fold::ExistingHost(..)) => &art.blocks[1],
        _ => art_first,
    };
    if block_id(art_first) == orig_first.id() {
        return Ok(false);
    }

    let sec_pr_key = xml.key(sec_pr).ok_or_else(|| diag::malformed("secPr without anchor"))?.to_string();
    let mut sec_pr_ov = Overrides::default();
    let page_keys: Vec<(String, String)> = ov
        .attrs
        .keys()
        .filter(|(k, _)| k == &orig.page_key || Some(k.as_str()) == orig.margin_key.as_deref())
        .cloned()
        .collect();
    for key in page_keys {
        if let Some(val) = ov.attrs.remove(&key) {
            sec_pr_ov.attrs.insert(key, val);
        }
    }
    let mut head_bytes = render_key(xml, &sec_pr_key, &sec_pr_ov).map_err(diag::malformed)?;
    ov.drop.insert(sec_pr_key);
    if let Some(ctrl) = opt_ctrl {
        let ctrl_key = xml.key(ctrl).ok_or_else(|| diag::malformed("ctrl without anchor"))?.to_string();
        head_bytes.extend_from_slice(xml.raw(ctrl).as_bytes());
        ov.drop.insert(ctrl_key);
    }

    if let Some(target_orig) = orig.blocks.iter().find(|b| b.id() == block_id(art_first)) {
        if matches!(target_orig, PBlock::Opaque(_)) {
            return Err(diag::unsupported("a non-paragraph block cannot become the first block of a section with section properties"));
        }
        let target_key = target_orig.host_key();
        if let Some(runs) = ov.regen.get_mut(target_key) {
            if let Some(first_run) = runs.first_mut() {
                first_run.items.insert(0, Item::Raw(head_bytes));
            } else {
                let char_pr = match target_orig {
                    PBlock::Para(p) => p.base_char_pr,
                    _ => default_char_pr,
                };
                runs.push(InlineRun { char_pr, items: vec![Item::Raw(head_bytes)] });
            }
        } else {
            let target_node = xml.node(target_key).ok_or_else(|| diag::malformed("target block anchor lost"))?;
            if let Some(run_node) = child_el(target_node, NS_HP, "run") {
                let run_key = xml.key(run_node).ok_or_else(|| diag::malformed("run without anchor"))?.to_string();
                ov.prepend.entry(run_key).or_default().insert(0, head_bytes);
            } else {
                let char_pr = match target_orig {
                    PBlock::Para(p) => p.base_char_pr,
                    _ => default_char_pr,
                };
                let mut wrapped = format!("<hp:run charPrIDRef=\"{char_pr}\">").into_bytes();
                wrapped.extend_from_slice(&head_bytes);
                wrapped.extend_from_slice(b"</hp:run>");
                ov.prepend.entry(target_key.to_string()).or_default().insert(0, wrapped);
            }
        }
    } else {
        let art_ids: HashSet<&str> = art.blocks.iter().map(block_id).collect();
        let slot = match orig.blocks.iter().find(|b| art_ids.contains(b.id())) {
            Some(surviving) => ov.before.get_mut(surviving.host_key()).and_then(|v| v.first_mut()),
            None => ov.append.get_mut(&orig.key).and_then(|v| v.first_mut()),
        };
        let target_bytes = slot.ok_or_else(|| diag::malformed("inserted leading block missing from plan"))?;
        inject_into_first_run(target_bytes, &head_bytes)?;
    }
    Ok(true)
}

/// Encodes `target` as a same-format HWPX package derived from `original`. `figure_targets` maps the
/// pictures that a figure link may update to the render hash the link names. When no part changes, the
/// original bytes are the output, so an unedited save is byte-identical to its source.
pub fn encode(
    original: Vec<u8>,
    target: &FlowDocument,
    lines: &NativeLines,
    assets: &dyn AssetStore,
    figure_targets: &BTreeMap<String, String>,
) -> DltResult<Written> {
    let mut pkg = package::open(&original)?;
    let (edits, diagnostics) = {
        let loaded = doc::load_package(&pkg, assets)?;
        plan_and_render(&pkg, &loaded, target, lines, assets, figure_targets)?
    };
    let mut changed = false;
    for edit in edits {
        match edit {
            Edit::Set(name, bytes) => {
                changed |= !pkg.has_part(&name) || pkg.part(&name)? != bytes.as_slice();
                pkg.set_part(&name, bytes)?;
            }
            Edit::Remove(name) => {
                changed = true;
                pkg.remove_part(&name)?;
            }
        }
    }
    if !changed {
        return Ok(Written { bytes: original, diagnostics });
    }
    let bytes = pkg.encode()?;
    Ok(Written { bytes, diagnostics })
}

fn plan_and_render(
    pkg: &Package,
    loaded: &Loaded,
    target: &FlowDocument,
    lines: &NativeLines,
    assets: &dyn AssetStore,
    figure_targets: &BTreeMap<String, String>,
) -> DltResult<(Vec<Edit>, Vec<Diagnostic>)> {
    if target.sections.len() != loaded.sections.len() {
        return Err(diag::unsupported("adding or removing sections is not supported in HWPX"));
    }
    let header_src = std::str::from_utf8(pkg.part(HEADER)?).map_err(|e| diag::malformed(e.to_string()))?;
    let hxml = Xml::parse(header_src).map_err(diag::malformed)?;
    let hplan = HeaderPlan::new(&hxml, &loaded.header).map_err(diag::malformed)?;
    let mut planner = Planner {
        assets,
        loaded,
        header: &loaded.header,
        lines,
        hxml: &hxml,
        hplan,
        header_changed: false,
        manifest_adds: Vec::new(),
        new_bins: Vec::new(),
        planned_assets: Vec::new(),
        figure_targets,
    };
    planner.plan_styles(target)?;

    struct SectionWork {
        part: String,
        ov: Overrides,
        first: Option<usize>,
        changed: bool,
    }
    let mut works: Vec<SectionWork> = Vec::new();
    let mut diagnostics = Vec::new();
    for (orig, art) in loaded.sections.iter().zip(&target.sections) {
        if orig.id != art.id {
            return Err(diag::unsupported("section identity changed"));
        }
        let src = std::str::from_utf8(pkg.part(&orig.part)?).map_err(|e| diag::malformed(e.to_string()))?;
        let xml = Xml::parse(src).map_err(diag::malformed)?;
        let mut ids = Ids(max_numeric_id(&xml) + 1);
        let mut ov = Overrides::default();
        let (mut changed, mut first) = planner.plan_section(orig, art, &mut ov, &mut ids)?;
        if !setup_eq(&orig.setup, &art.setup) {
            let page_key = orig.page_key.clone();
            let margin_key = orig.margin_key.clone().ok_or_else(|| diag::malformed("section without hp:margin"))?;
            ov.set_attr(&page_key, "width", to_hu(art.setup.size.width)?.to_string());
            ov.set_attr(&page_key, "height", to_hu(art.setup.size.height)?.to_string());
            ov.set_attr(&margin_key, "left", to_hu(art.setup.margin_left)?.to_string());
            ov.set_attr(&margin_key, "right", to_hu(art.setup.margin_right)?.to_string());
            ov.set_attr(&margin_key, "top", to_hu(art.setup.margin_top)?.to_string());
            ov.set_attr(&margin_key, "bottom", to_hu(art.setup.margin_bottom)?.to_string());
            changed = true;
            first = Some(0);
        }
        let default_char_pr = planner.base_char()?.id;
        if relocate_section_head(&xml, orig, art, &mut ov, default_char_pr, lines)? {
            changed = true;
            first = Some(0);
        }
        works.push(SectionWork { part: orig.part.clone(), ov, first, changed });
    }

    // Cache policy: an edited paragraph and everything after it in its section lose their
    // line-segment cache. Later sections lose theirs too, because page flow can move.
    let header_changed = planner.header_changed;
    let mut downstream = false;
    for work in works.iter_mut() {
        if header_changed || downstream {
            work.first = Some(0);
        }
        if work.first.is_some() {
            downstream = true;
        }
    }
    let mut dropped = 0usize;
    for (work, orig) in works.iter_mut().zip(&loaded.sections) {
        if let Some(from) = work.first {
            for block in orig.blocks.iter().skip(from) {
                for key in cache_keys(block) {
                    work.ov.drop.insert(key);
                    dropped += 1;
                }
            }
        }
    }
    if dropped > 0 {
        diagnostics.push(diag::info(
            diag::LINESEG_INVALIDATED,
            format!("{dropped} line-segment cache entries removed for edited and downstream paragraphs; layout recomputes on open"),
            None,
        ));
    }

    let mut edits: Vec<Edit> = Vec::new();
    let header_ov = if planner.hplan.has_changes() {
        Some(planner.hplan.finish(&hxml, &loaded.header).map_err(diag::malformed)?)
    } else {
        None
    };
    let header_edited = header_ov.is_some();
    if let Some(ov) = header_ov {
        let bytes = render_part(&hxml, &ov).map_err(diag::malformed)?;
        edits.push(Edit::Set(HEADER.to_string(), bytes));
    }

    let mut any_change = header_edited || !planner.new_bins.is_empty();
    for work in &works {
        if work.changed || !work.ov.is_empty() {
            any_change = true;
            let src = std::str::from_utf8(pkg.part(&work.part)?).map_err(|e| diag::malformed(e.to_string()))?;
            let xml = Xml::parse(src).map_err(diag::malformed)?;
            let bytes = render_part(&xml, &work.ov).map_err(diag::malformed)?;
            edits.push(Edit::Set(work.part.clone(), bytes));
        }
    }

    if !planner.manifest_adds.is_empty() {
        let src = std::str::from_utf8(pkg.part(CONTENT_HPF)?).map_err(|e| diag::malformed(e.to_string()))?;
        let xml = Xml::parse(src).map_err(diag::malformed)?;
        let mut ov = Overrides::default();
        ov.append.insert(loaded.manifest.key.clone(), std::mem::take(&mut planner.manifest_adds));
        let bytes = render_part(&xml, &ov).map_err(diag::malformed)?;
        edits.push(Edit::Set(CONTENT_HPF.to_string(), bytes));
    }
    for (name, bytes) in std::mem::take(&mut planner.new_bins) {
        edits.push(Edit::Set(name, bytes));
    }

    if any_change {
        if pkg.has_part(PREVIEW_TEXT) {
            let carriers = folded_carriers(&loaded.sections, target, lines);
            edits.push(Edit::Set(PREVIEW_TEXT.to_string(), preview_text(target, &carriers).into_bytes()));
            diagnostics.push(diag::info(diag::PREVIEW_TEXT_REGENERATED, "PrvText.txt regenerated from the edited text", None));
        }
        if pkg.has_part(PREVIEW_IMAGE) {
            // Removal is allowed only when no manifest item and no container rootfile names the part.
            let container = std::str::from_utf8(pkg.part(package::CONTAINER)?).map_err(|e| diag::malformed(e.to_string()))?;
            let referenced = loaded.manifest.items.iter().any(|i| i.href == PREVIEW_IMAGE) || container.contains(PREVIEW_IMAGE);
            if referenced {
                return Err(diag::unsupported("PrvImage.png is referenced by the package and cannot be removed safely"));
            }
            edits.push(Edit::Remove(PREVIEW_IMAGE.to_string()));
            diagnostics.push(diag::info(
                diag::PREVIEW_IMAGE_REMOVED,
                "PrvImage.png removed because it no longer describes the edited document",
                None,
            ));
        }
    }
    Ok((edits, diagnostics))
}
