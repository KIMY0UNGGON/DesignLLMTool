//! Native records (small JSON per model node), canonical forms, and the flattened block list.
//!
//! A canonical form is a `serde_json::Value` built only from model content. Two blocks with equal
//! canonical forms produce identical WordprocessingML, so the writer can copy the original bytes.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{DltError, DltResult, FlowBlock, FlowDocument, Id, Paragraph, Section, Style, Table};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Native record of one model node. Stored in `NativeSource.mappings` under the node id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Rec {
    /// A section. `sect` is the path of its `w:sectPr` (body-level or inside a paragraph `w:pPr`).
    Section { sect: Option<Vec<usize>> },
    /// A paragraph. `path` is its original element path (`None` for new paragraphs). `tail` lists the
    /// original child indices of zero-width markers that follow the last run.
    Paragraph {
        path: Option<Vec<usize>>,
        direct: BTreeSet<String>,
        #[serde(default)]
        tail: Vec<usize>,
    },
    /// A run or a protected inline atom. `origin` is the child index inside the original paragraph.
    /// `lead` lists the original child indices of zero-width markers written before this run.
    Run {
        origin: Option<usize>,
        atom: bool,
        direct: BTreeSet<String>,
        rstyle: Option<String>,
        #[serde(default)]
        lead: Vec<usize>,
    },
    /// A table. `path` is its original `w:tbl` path. `hidden` is true when the original table has
    /// vertically merged continuation cells, which are kept in the XML but not modelled.
    Table {
        path: Option<Vec<usize>>,
        #[serde(default)]
        hidden: bool,
    },
    /// A block-level picture. `path` is the original paragraph path.
    Image { path: Option<Vec<usize>> },
    /// A block kept byte-for-byte.
    Opaque { path: Vec<usize> },
}

pub type Records = BTreeMap<Id, Rec>;

pub fn records_to_mappings(records: &Records) -> DltResult<BTreeMap<Id, Value>> {
    records
        .iter()
        .map(|(id, rec)| Ok((id.clone(), serde_json::to_value(rec).map_err(|e| DltError::invalid(e.to_string()))?)))
        .collect()
}

pub fn records_from_mappings(mappings: &BTreeMap<Id, Value>) -> DltResult<Records> {
    mappings
        .iter()
        .map(|(id, value)| {
            let rec: Rec = serde_json::from_value(value.clone())
                .map_err(|e| DltError::new(designllm_contracts::ErrorCode::IntegrityError, format!("native record {id}: {e}")))?;
            Ok((id.clone(), rec))
        })
        .collect()
}

/// Canonical form of a run's visible content and modelled style.
fn canon_style(style: &Style) -> Value {
    json!([style.font_family, style.font_size, style.color, style.bold, style.italic])
}

pub fn canon_paragraph(p: &Paragraph) -> Value {
    let f = &p.format;
    json!({
        "k": "p",
        "st": p.style_ref,
        "f": [f.align, f.space_before, f.space_after, f.line_spacing, f.indent, f.page_break_before],
        "r": p.runs.iter().map(|r| json!({"t": r.text, "a": r.protected, "s": canon_style(&r.style)})).collect::<Vec<_>>(),
    })
}

/// Shallow canonical form: a table lists how many blocks each cell holds, not their content.
pub fn canon_block(block: &FlowBlock, deep: bool) -> Value {
    match block {
        FlowBlock::Paragraph(p) => canon_paragraph(p),
        FlowBlock::Table(t) => canon_table(t, deep),
        FlowBlock::Image(n) => {
            let (sha, mime, bytes) = match &n.asset {
                Some(a) => (a.sha256.clone(), a.mime.clone(), a.bytes),
                None => (String::new(), String::new(), 0),
            };
            json!({"k": "i", "sha": sha, "mime": mime, "n": bytes, "w": n.bounds.width, "h": n.bounds.height})
        }
        FlowBlock::Opaque { label, .. } => json!({"k": "o", "label": label}),
    }
}

/// SHA-256 and MIME type of the picture in a canonical image block; `None` for any other block.
pub fn picture_asset(canon: &Value) -> Option<(&str, &str)> {
    if canon.get("k").and_then(Value::as_str) != Some("i") {
        return None;
    }
    Some((canon.get("sha")?.as_str()?, canon.get("mime")?.as_str()?))
}

fn canon_table(t: &Table, deep: bool) -> Value {
    let rows: Vec<Value> = t
        .rows
        .iter()
        .map(|row| {
            Value::Array(
                row.iter()
                    .map(|cell| {
                        if deep {
                            json!({
                                "cs": cell.column_span,
                                "rs": cell.row_span,
                                "b": cell.blocks.iter().map(|b| canon_block(b, true)).collect::<Vec<_>>(),
                            })
                        } else {
                            json!({"cs": cell.column_span, "rs": cell.row_span, "nb": cell.blocks.len()})
                        }
                    })
                    .collect(),
            )
        })
        .collect();
    json!({"k": "t", "g": t.column_widths, "rows": rows})
}

pub fn canon_section(section: &Section) -> Value {
    let s = &section.setup;
    json!({
        "k": "s",
        "size": [s.size.width, s.size.height],
        "m": [s.margin_left, s.margin_top, s.margin_right, s.margin_bottom],
    })
}

pub fn block_id(block: &FlowBlock) -> Option<&Id> {
    match block {
        FlowBlock::Paragraph(p) => Some(&p.id),
        FlowBlock::Table(t) => Some(&t.id),
        FlowBlock::Image(n) => Some(&n.id),
        FlowBlock::Opaque { id, .. } => Some(id),
    }
}

/// One entry of the flattened document: sections and blocks (including nested cell blocks) in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Flat {
    pub id: Option<Id>,
    pub canon: Value,
}

pub fn flatten(document: &FlowDocument) -> Vec<Flat> {
    let mut out = Vec::new();
    for section in &document.sections {
        out.push(Flat { id: None, canon: canon_section(section) });
        for block in &section.blocks {
            push_block(block, &mut out);
        }
    }
    out
}

fn push_block(block: &FlowBlock, out: &mut Vec<Flat>) {
    out.push(Flat { id: block_id(block).cloned(), canon: canon_block(block, false) });
    if let FlowBlock::Table(table) = block {
        for row in &table.rows {
            for cell in row {
                for child in &cell.blocks {
                    push_block(child, out);
                }
            }
        }
    }
}

/// Every block id in the document (sections excluded), checking uniqueness of all ids.
pub fn collect_ids(document: &FlowDocument) -> DltResult<BTreeSet<Id>> {
    let mut ids = BTreeSet::new();
    let mut add = |id: &Id| -> DltResult<()> {
        if id.is_empty() || !ids.insert(id.clone()) {
            return Err(DltError::invalid(format!("duplicate or empty node id {id:?}")));
        }
        Ok(())
    };
    fn walk(block: &FlowBlock, add: &mut dyn FnMut(&Id) -> DltResult<()>) -> DltResult<()> {
        match block {
            FlowBlock::Paragraph(p) => {
                add(&p.id)?;
                for run in &p.runs {
                    add(&run.id)?;
                }
            }
            FlowBlock::Table(t) => {
                add(&t.id)?;
                for row in &t.rows {
                    for cell in row {
                        add(&cell.id)?;
                        for child in &cell.blocks {
                            walk(child, add)?;
                        }
                    }
                }
            }
            FlowBlock::Image(n) => add(&n.id)?,
            FlowBlock::Opaque { id, .. } => add(id)?,
        }
        Ok(())
    }
    for section in &document.sections {
        add(&section.id)?;
        for block in &section.blocks {
            walk(block, &mut add)?;
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_through_mapping_values() {
        let mut records = Records::new();
        records.insert("p1".into(), Rec::Paragraph { path: Some(vec![0, 3]), direct: ["align".to_string()].into_iter().collect(), tail: vec![4] });
        records.insert("r1".into(), Rec::Run { origin: Some(2), atom: true, direct: BTreeSet::new(), rstyle: None, lead: vec![1] });
        let mappings = records_to_mappings(&records).unwrap();
        let back = records_from_mappings(&mappings).unwrap();
        assert_eq!(back, records);
    }
}
