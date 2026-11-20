//! FigureLink rules for HWPX (docs/FIGURE_LINKS.md): `UpdateFigureLink` on a block picture.
//!
//! The link is part of the workspace model (`Artifact.figure_links`), not of the HWPX package. The
//! picture takes the render bytes as an ordinary embedded PNG; the package records no link.
use std::collections::BTreeMap;

use designllm_contracts::media_header::{self, png_size, AspectError, FIGURE_REPRESENTATIONS};
use designllm_contracts::{sha256, AssetRef, AssetStore, DltResult, ErrorCode, FigureLink, FlowBlock, FlowDocument, Id, Node};

use crate::diag;

/// Display height in EMU for `width_emu` at the pixel aspect of the render (`media_header::aspect_height`,
/// rounded half up). The result is then placed on the native grid by the usual rounding, which records its
/// diagnostic. A zero height is stored as it is.
pub fn height_for_width(width_emu: i64, pixel_width: u32, pixel_height: u32) -> DltResult<i64> {
    media_header::aspect_height(width_emu, pixel_width, pixel_height).map_err(|error| match error {
        AspectError::NonPositive => diag::error(ErrorCode::InvalidArgument, diag::FIGURE_LINK, "the picture width and the render size must be positive"),
        AspectError::OutOfRange => diag::error(ErrorCode::InvalidArgument, diag::FIGURE_LINK, "the picture height overflows"),
    })
}

/// Render hash of every linked picture: target node id to render hash.
pub fn figure_targets(links: &[FigureLink]) -> BTreeMap<String, String> {
    links.iter().map(|link| (link.target_node_id.clone(), link.render_hash.clone())).collect()
}

/// What an id names in the flow model.
enum Named {
    Picture,
    Other,
}

fn named_in(blocks: &[FlowBlock], id: &str) -> Option<Named> {
    for block in blocks {
        let found = match block {
            FlowBlock::Paragraph(p) if p.id == id || p.runs.iter().any(|r| r.id == id) => Some(Named::Other),
            FlowBlock::Image(node) if node.id == id => Some(Named::Picture),
            FlowBlock::Opaque { id: block_id, .. } if block_id.as_str() == id => Some(Named::Other),
            FlowBlock::Table(table) if table.id == id => Some(Named::Other),
            FlowBlock::Table(table) => table.rows.iter().flatten().find_map(|cell| {
                if cell.id == id {
                    Some(Named::Other)
                } else {
                    named_in(&cell.blocks, id)
                }
            }),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// The picture with this id, top level or inside a table cell.
fn picture_mut<'a>(blocks: &'a mut [FlowBlock], id: &str) -> Option<&'a mut Node> {
    for block in blocks.iter_mut() {
        match block {
            FlowBlock::Image(node) if node.id == id => return Some(node),
            FlowBlock::Table(table) => {
                for cell in table.rows.iter_mut().flatten() {
                    if let Some(found) = picture_mut(&mut cell.blocks, id) {
                        return Some(found);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// The render asset of a link: present in the store, a PNG by mime type.
fn render_asset(assets: &dyn AssetStore, link: &FigureLink) -> DltResult<AssetRef> {
    let asset = assets.lookup(&link.render_hash).map_err(|e| {
        if e.code == ErrorCode::NotFound {
            diag::error(ErrorCode::NotFound, diag::FIGURE_LINK, format!("render asset {} is not in the workspace store", link.render_hash))
        } else {
            e
        }
    })?;
    if asset.mime != "image/png" {
        return Err(diag::error(
            ErrorCode::InvalidArgument,
            diag::FIGURE_LINK,
            format!("render asset mime {} does not match representation png (image/png)", asset.mime),
        ));
    }
    Ok(asset)
}

/// `UpdateFigureLink` on the candidate `doc`. The target picture takes the render bytes, keeps its
/// width, and takes the height of the render's aspect ratio. The link replaces the entry with the same
/// target id, and the list stays sorted by target id. Every refusal happens before `doc` or `links`
/// changes, so a refused change leaves the candidate state as it was.
pub fn apply_link(
    doc: &mut FlowDocument,
    links: &mut Vec<FigureLink>,
    link: &FigureLink,
    assets: &dyn AssetStore,
    changed: &mut Vec<Id>,
) -> DltResult<()> {
    if !FIGURE_REPRESENTATIONS.contains(&link.representation.as_str()) {
        return Err(diag::error(
            ErrorCode::UnsupportedOperation,
            diag::FIGURE_LINK,
            format!("figure link representation {} is not supported in HWPX; supported: {}", link.representation, FIGURE_REPRESENTATIONS.join(", ")),
        ));
    }
    let target = link.target_node_id.as_str();
    match doc.sections.iter().find_map(|section| named_in(&section.blocks, target)) {
        None => {
            return Err(diag::error(ErrorCode::NotFound, diag::FIGURE_LINK, format!("figure link target {target} was not found")));
        }
        Some(Named::Other) => {
            return Err(diag::error(ErrorCode::InvalidArgument, diag::FIGURE_LINK, format!("figure link target {target} is not a picture")));
        }
        Some(Named::Picture) => {}
    }
    let asset = render_asset(assets, link)?;
    let bytes = assets.read(&asset)?;
    if sha256(&bytes) != link.render_hash {
        return Err(diag::error(
            ErrorCode::IntegrityError,
            diag::FIGURE_LINK,
            format!("render asset {} does not hash to its render_hash", link.render_hash),
        ));
    }
    let Some((pixel_width, pixel_height)) = png_size(&bytes) else {
        return Err(diag::error(ErrorCode::InvalidArgument, diag::FIGURE_LINK, format!("render asset {} is not a readable PNG", link.render_hash)));
    };
    let node = doc
        .sections
        .iter_mut()
        .find_map(|section| picture_mut(&mut section.blocks, target))
        .ok_or_else(|| diag::error(ErrorCode::NotFound, diag::FIGURE_LINK, format!("figure link target {target} was not found")))?;
    let height = height_for_width(node.bounds.width, pixel_width, pixel_height)?;
    node.asset = Some(asset);
    node.bounds.height = height;
    links.retain(|existing| existing.target_node_id != target);
    links.push(link.clone());
    links.sort_by(|a, b| a.target_node_id.cmp(&b.target_node_id));
    changed.push(link.target_node_id.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn height_is_the_aspect_ratio_rounded_half_up_in_emu() {
        // 800 HWPUNIT wide, a 16 x 12 render: exactly 600 HWPUNIT high.
        assert_eq!(height_for_width(800 * 127, 16, 12).expect("height"), 600 * 127);
        // 1 EMU wide, a 2 x 1 render: 0.5 rounds up.
        assert_eq!(height_for_width(1, 2, 1).expect("height"), 1);
        // 3 EMU wide, a 2 x 1 render: 1.5 rounds up to 2.
        assert_eq!(height_for_width(3, 2, 1).expect("height"), 2);
        assert!(height_for_width(0, 2, 1).is_err());
        assert!(height_for_width(10, 0, 1).is_err());
    }

    #[test]
    fn png_size_reads_the_ihdr_chunk_and_refuses_other_bytes() {
        let mut png = media_header::PNG_SIGNATURE.to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&16u32.to_be_bytes());
        png.extend_from_slice(&12u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        assert_eq!(png_size(&png), Some((16, 12)));
        assert_eq!(png_size(b"not a png, but long enough to pass the length check"), None);
        assert_eq!(png_size(&png[..20]), None);
    }

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

    /// The PNG gate of `apply_link` on the input set.
    #[test]
    fn png_input_set_acceptance_of_the_figure_link_gate() {
        let normal = png_file(4, 13);
        let mut no_signature = normal.clone();
        no_signature[..8].fill(0);
        let no_iend = normal[..normal.len() - 12].to_vec();
        let inputs = [("P1 normal", normal), ("P2 no signature", no_signature), ("P3 IHDR length 14", png_file(4, 14)), ("P4 zero width", png_file(0, 13)), ("P5 no IEND", no_iend)];
        let table: Vec<_> = inputs.iter().map(|(name, bytes)| (*name, png_size(bytes).is_some())).collect();
        println!("49e hwpx UpdateFigureLink PNG gate accepts: {table:?}");
        assert_eq!(table, [("P1 normal", true), ("P2 no signature", false), ("P3 IHDR length 14", false), ("P4 zero width", false), ("P5 no IEND", true)]);
    }
}
