//! Content digest: text, inline atoms, block/table structure, image assets, opaque content, in
//! document order.
//!
//! Node ids are not hashed (v2). An id is a handle for addressing a node, not document meaning
//! (CONTRACTS.md §8: text, formula, relations, data): DOCX assigns new ids on every open, and an
//! HWP node inserted under a request id gets a path id after save and reopen. With ids in the
//! hash, identical content had different digests across sessions. Order and nesting are hashed,
//! so reordering, insertion, removal and moves between sections or cells still change the digest.
//! Relations that do name nodes (figure links) are hashed by the engines on the Artifact.
//!
//! Design (styles, fonts, paragraph format, page setup, column widths, image size) is excluded.
//! Run segmentation is excluded so a style-only run split keeps the digest.
use crate::text::LogicalParagraph;
use designllm_contracts::*;
use serde_json::{json, Value};

/// Digest version marker; bump when the hashed projection changes (2: ids no longer hashed).
pub const CONTENT_DIGEST_VERSION: u32 = 2;

pub fn content_digest(document: &FlowDocument) -> DltResult<ContentDigest> {
    let sections: Vec<Value> = document.sections.iter().map(|s| json!({"blocks": blocks_value(&s.blocks)})).collect();
    let content = json_hash(&json!({"v": CONTENT_DIGEST_VERSION, "kind": "flow", "sections": sections}))?;
    // Flow documents carry no datasets; figure links live on the Artifact (hashed by the engines).
    let data = json_hash(&json!({"v": CONTENT_DIGEST_VERSION, "kind": "flow", "data": []}))?;
    Ok(ContentDigest { content, data })
}

fn blocks_value(blocks: &[FlowBlock]) -> Vec<Value> {
    blocks.iter().map(block_value).collect()
}

/// Id-free content projection of a single block (exposed for adapters that hash partial content).
pub fn block_value(block: &FlowBlock) -> Value {
    match block {
        FlowBlock::Paragraph(p) => paragraph_value(p),
        FlowBlock::Table(t) => json!({
            "table": t.rows.iter().map(|row| row.iter().map(|cell| json!({
                "column_span": cell.column_span, "row_span": cell.row_span, "blocks": blocks_value(&cell.blocks),
            })).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }),
        FlowBlock::Image(node) => json!({"image": {"asset": node.asset.as_ref().map(|a| a.sha256.clone()), "alt": node.text}}),
        FlowBlock::Opaque { label, .. } => json!({"opaque": label}),
    }
}

/// Logical text (atoms appear as U+FFFC at their positions) and the atoms' own text.
fn paragraph_value(p: &Paragraph) -> Value {
    let lp = LogicalParagraph::new(p);
    let atoms: Vec<&str> = p.runs.iter().filter(|r| r.protected).map(|r| r.text.as_str()).collect();
    json!({"paragraph": {"text": lp.text, "atoms": atoms}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_flow_operations;

    fn doc() -> FlowDocument {
        let p = Paragraph { id: "p1".into(), runs: vec![TextRun::new("r1", "Hello 세계")], style_ref: None, format: ParagraphFormat::default() };
        FlowDocument { sections: vec![Section { id: "s1".into(), setup: PageSetup::default(), blocks: vec![FlowBlock::Paragraph(p)] }], styles: Default::default() }
    }

    #[test]
    fn design_changes_keep_digest_and_text_changes_move_it() {
        let base = doc();
        let d0 = content_digest(&base).unwrap();
        let mut styled = base.clone();
        let patch = StylePatch { bold: Some(true), font_size: Some(20 * EMU_PER_PT), ..Default::default() };
        apply_flow_operations(&mut styled, &[
            Operation::SetTextStyleRange { range: TextRange { paragraph_id: "p1".into(), start: 6, end: 8 }, patch },
            Operation::SetParagraphFormat { paragraph_id: "p1".into(), format: ParagraphFormat { align: "center".into(), ..Default::default() } },
            Operation::SetPageSetup { section_id: "s1".into(), setup: PageSetup { margin_left: 25 * EMU_PER_MM, ..Default::default() } },
        ]).unwrap();
        if let FlowBlock::Paragraph(p) = &styled.sections[0].blocks[0] { assert_eq!(p.runs.len(), 2); } else { panic!() }
        assert_eq!(content_digest(&styled).unwrap(), d0);
        let mut edited = base.clone();
        apply_flow_operations(&mut edited, &[Operation::ReplaceText { range: TextRange { paragraph_id: "p1".into(), start: 0, end: 5 }, text: "Hi".into() }]).unwrap();
        assert_ne!(content_digest(&edited).unwrap().content, d0.content);
        assert_eq!(content_digest(&edited).unwrap().data, d0.data);
    }

    fn para(id: &str, text: &str) -> FlowBlock {
        FlowBlock::Paragraph(Paragraph { id: id.into(), runs: vec![TextRun::new(format!("{id}r"), text)], style_ref: None, format: ParagraphFormat::default() })
    }
    fn atom_para(id: &str) -> FlowBlock {
        FlowBlock::Paragraph(Paragraph { id: id.into(), runs: vec![
            TextRun::new(format!("{id}a"), "x = 1"),
            TextRun { protected: true, ..TextRun::new(format!("{id}eq"), String::new()) },
        ], style_ref: None, format: ParagraphFormat::default() })
    }
    fn image(id: &str, hash: &str) -> FlowBlock {
        FlowBlock::Image(Node { id: id.into(), kind: NodeKind::Image, bounds: Rect { x: 0, y: 0, width: 100, height: 100 }, text: String::new(), style: Style::default(), asset: Some(AssetRef { sha256: hash.into(), mime: "image/png".into(), bytes: 1 }), children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: Default::default() })
    }
    fn table(id: &str, texts: [&str; 2]) -> FlowBlock {
        let cell = |k: usize| TableCell { id: format!("{id}c{k}"), blocks: vec![para(&format!("{id}p{k}"), texts[k])], column_span: 1, row_span: 1 };
        FlowBlock::Table(Table { id: id.into(), column_widths: vec![1000, 1000], rows: vec![vec![cell(0), cell(1)]], style: Style::default() })
    }
    fn rich(ids: &str) -> FlowDocument {
        let s = |k: &str, blocks| Section { id: format!("{ids}s{k}"), setup: PageSetup::default(), blocks };
        FlowDocument { sections: vec![
            s("0", vec![para(&format!("{ids}1"), "첫 문단"), atom_para(&format!("{ids}2")), image(&format!("{ids}3"), "aa"), table(&format!("{ids}4"), ["가", "나"])]),
            s("1", vec![FlowBlock::Opaque { id: format!("{ids}5"), label: "그리기 개체".into() }, para(&format!("{ids}6"), "끝")]),
        ], styles: Default::default() }
    }

    /// Same content under different ids (e.g. a DOCX reopen, or HWP request ids that become path
    /// ids) has one digest; every content change of the projection still moves it.
    #[test]
    fn digest_ignores_ids_but_tracks_order_structure_and_content() {
        let d = content_digest(&rich("a-")).unwrap();
        assert_eq!(content_digest(&rich("other-")).unwrap(), d, "ids are not content");
        let mut variants: Vec<(&str, FlowDocument)> = Vec::new();
        let mut v = rich("a-"); v.sections[0].blocks.swap(0, 1); variants.push(("reordered blocks", v));
        let mut v = rich("a-"); let b = v.sections[1].blocks.remove(1); v.sections[0].blocks.push(b); variants.push(("moved to another section", v));
        let mut v = rich("a-"); v.sections[0].blocks.insert(1, para("n", "")); variants.push(("inserted empty paragraph", v));
        let mut v = rich("a-"); v.sections[0].blocks.remove(2); variants.push(("removed image", v));
        let mut v = rich("a-"); v.sections[0].blocks[2] = image("a-3", "bb"); variants.push(("other image bytes", v));
        let mut v = rich("a-"); if let FlowBlock::Table(t) = &mut v.sections[0].blocks[3] { t.rows[0].swap(0, 1); } variants.push(("swapped cells", v));
        let mut v = rich("a-"); if let FlowBlock::Table(t) = &mut v.sections[0].blocks[3] { t.rows[0][0].column_span = 2; } variants.push(("span", v));
        let mut v = rich("a-"); if let FlowBlock::Paragraph(p) = &mut v.sections[0].blocks[1] { p.runs.swap(0, 1); } variants.push(("atom position", v));
        let mut v = rich("a-"); if let FlowBlock::Paragraph(p) = &mut v.sections[0].blocks[1] { p.runs[1].text = "y".into(); } variants.push(("atom text", v));
        let mut v = rich("a-"); v.sections[1].blocks[0] = FlowBlock::Opaque { id: "a-5".into(), label: "수식".into() }; variants.push(("opaque kind", v));
        let mut v = rich("a-"); let s = v.sections.remove(1); v.sections[0].blocks.extend(s.blocks); variants.push(("merged sections", v));
        for (name, doc) in variants { assert_ne!(content_digest(&doc).unwrap().content, d.content, "{name}"); }
        // Design-only differences still keep it.
        let mut v = rich("a-");
        if let FlowBlock::Table(t) = &mut v.sections[0].blocks[3] { t.column_widths = vec![500, 1500]; }
        if let FlowBlock::Image(n) = &mut v.sections[0].blocks[2] { n.bounds.width = 300; n.style.align = "center".into(); }
        v.sections[1].setup.margin_left += 1;
        assert_eq!(content_digest(&v).unwrap(), d);
    }
}
