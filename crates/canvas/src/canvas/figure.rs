//! Figure links on canvas image nodes (docs/FIGURE_LINKS.md).
//!
//! A figure render is an immutable PNG asset. `UpdateFigureLink` points an existing image node at that asset. The node
//! keeps its position and width, its height becomes `width * render height / render width` (EMU, half up), and the link
//! is kept in `Artifact.figure_links` under its target node. This engine supports `png` only.
//!
//! A linked picture must keep its aspect ratio. A row or column that stretches its children would change the picture's
//! size on the cross axis, so an image inside such a container is refused. The rule is checked on every candidate, so a
//! later layout edit cannot break it silently either.

use super::model::{find_node_in_doc, find_node_in_doc_mut, validate_bounds};
use crate::common::{blocking, invalid, valid_id};
use designllm_contracts::media_header::{self, png_size, AspectError, FIGURE_REPRESENTATIONS};
use designllm_contracts::{
    sha256, AssetStore, CanvasDocument, Diagnostic, DltError, DltResult, ErrorCode, FigureLink, LayoutSpec, Node, NodeKind,
};
use std::collections::BTreeSet;

const PNG_MIME: &str = "image/png";

/// `media_header::aspect_height` with the canvas diagnostics. A zero height is refused by `validate_bounds`.
pub(crate) fn aspect_height(width: i64, pixel_width: u32, pixel_height: u32) -> DltResult<i64> {
    media_header::aspect_height(width, pixel_width, pixel_height).map_err(|error| match error {
        AspectError::NonPositive => invalid("FIGURE_ASPECT", "image width and render size must be positive"),
        AspectError::OutOfRange => invalid("FIGURE_ASPECT", "image height is out of range"),
    })
}

fn supported_representation(representation: &str) -> bool {
    FIGURE_REPRESENTATIONS.contains(&representation)
}

/// `UpdateFigureLink` on the candidate document. Every refusal happens before the first write, so a refused call leaves
/// `doc` and `links` as they were.
pub(crate) fn update(
    doc: &mut CanvasDocument,
    links: &mut Vec<FigureLink>,
    assets: &dyn AssetStore,
    link: &FigureLink,
) -> DltResult<Vec<String>> {
    let target = link.target_node_id.as_str();
    if !supported_representation(&link.representation) {
        return Err(refuse(
            ErrorCode::UnsupportedOperation,
            "FIGURE_REPRESENTATION",
            format!("representation {:?} is not supported by canvas figure links; supported: {}", link.representation, FIGURE_REPRESENTATIONS.join(", ")),
            target,
        ));
    }
    let (kind, bounds) = match find_node_in_doc(doc, target) {
        Some(node) => (node.kind, node.bounds),
        None => return Err(refuse(ErrorCode::NotFound, "FIGURE_TARGET", format!("node {target} does not exist"), target)),
    };
    if kind != NodeKind::Image {
        return Err(refuse(ErrorCode::InvalidArgument, "FIGURE_TARGET", format!("node {target} is not an image"), target));
    }
    let reference = assets.lookup(&link.render_hash).map_err(|_| {
        refuse(ErrorCode::NotFound, "FIGURE_RENDER_MISSING", format!("render {} is not in the workspace store", link.render_hash), target)
    })?;
    if reference.mime != PNG_MIME {
        return Err(refuse(
            ErrorCode::InvalidArgument,
            "FIGURE_RENDER_MIME",
            format!("render {} has mime {} but {PNG_MIME} is required", link.render_hash, reference.mime),
            target,
        ));
    }
    let bytes = assets.read(&reference)?;
    if bytes.len() as u64 != reference.bytes || sha256(&bytes) != link.render_hash {
        return Err(refuse(
            ErrorCode::IntegrityError,
            "FIGURE_RENDER_HASH",
            format!("render bytes do not match sha256 {}", link.render_hash),
            target,
        ));
    }
    let Some((pixel_width, pixel_height)) = png_size(&bytes) else {
        return Err(refuse(ErrorCode::InvalidArgument, "FIGURE_RENDER_PNG", "render bytes are not a PNG image", target));
    };
    let mut next = bounds;
    next.height = aspect_height(bounds.width, pixel_width, pixel_height)?;
    validate_bounds(&next, NodeKind::Image).map_err(|mut error| {
        error.affected_ids.push(target.to_string());
        error
    })?;

    let Some(node) = find_node_in_doc_mut(doc, target) else {
        return Err(refuse(ErrorCode::NotFound, "FIGURE_TARGET", format!("node {target} does not exist"), target));
    };
    node.asset = Some(reference);
    node.bounds = next;
    links.retain(|existing| existing.target_node_id != link.target_node_id);
    links.push(link.clone());
    links.sort_by(|a, b| a.target_node_id.cmp(&b.target_node_id));
    Ok(vec![link.target_node_id.clone()])
}

/// Drops the links of removed nodes, so no link names a node that is gone.
pub(crate) fn prune(links: &mut Vec<FigureLink>, removed: &BTreeSet<String>) {
    links.retain(|link| !removed.contains(&link.target_node_id));
}

/// Checks the links against the document: each names an image whose picture is the linked render, the representation is
/// `png`, the figure id is well formed, and no image sits where its parent stretches it.
pub(crate) fn diagnostics(doc: &CanvasDocument, links: &[FigureLink]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for link in links {
        let target = Some(link.target_node_id.clone());
        if !valid_id(&link.figure_id) {
            out.push(blocking("FIGURE_LINK_ID", "figure id is empty or contains invalid characters", target.clone()));
        }
        if !supported_representation(&link.representation) {
            out.push(blocking("FIGURE_LINK_REPRESENTATION", format!("representation {:?} is not supported", link.representation), target.clone()));
        }
        if !seen.insert(link.target_node_id.as_str()) {
            out.push(blocking("FIGURE_LINK_DUPLICATE", format!("node {} has more than one figure link", link.target_node_id), target));
            continue;
        }
        match find_node_in_doc(doc, &link.target_node_id) {
            None => out.push(blocking("FIGURE_LINK_TARGET", format!("linked node {} does not exist", link.target_node_id), target)),
            Some(node) if node.kind != NodeKind::Image => {
                out.push(blocking("FIGURE_LINK_TARGET", format!("linked node {} is not an image", link.target_node_id), target))
            }
            Some(node) => {
                if node.asset.as_ref().map(|asset| asset.sha256.as_str()) != Some(link.render_hash.as_str()) {
                    out.push(blocking("FIGURE_LINK_PICTURE", format!("node {} does not show render {}", link.target_node_id, link.render_hash), target.clone()));
                }
                if stretched_by_parent(doc, &link.target_node_id) {
                    out.push(blocking(
                        "FIGURE_LINK_LAYOUT",
                        format!("node {} is inside a container that stretches its children; the linked picture would change shape", link.target_node_id),
                        target,
                    ));
                }
            }
        }
    }
    out
}

fn refuse(code: ErrorCode, diagnostic: &str, message: impl Into<String>, target: &str) -> DltError {
    let message = message.into();
    let mut error = DltError::new(code, message.clone());
    error.diagnostics.push(blocking(diagnostic, message, Some(target.to_string())));
    error.affected_ids.push(target.to_string());
    error
}

fn parent_of<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
    for node in nodes {
        if node.children.iter().any(|child| child.id == id) {
            return Some(node);
        }
        if let Some(found) = parent_of(&node.children, id) {
            return Some(found);
        }
    }
    None
}

/// True when the node's parent is a row or column whose layout stretches children at the base or at a breakpoint.
fn stretched_by_parent(doc: &CanvasDocument, id: &str) -> bool {
    let parent = doc.pages.iter().find_map(|page| parent_of(&page.nodes, id));
    parent.and_then(|node| node.layout.as_ref()).is_some_and(may_stretch_children)
}

fn may_stretch_children(spec: &LayoutSpec) -> bool {
    let flow = |mode: &str| matches!(mode, "row" | "column");
    let flows = flow(spec.mode.as_str()) || spec.breakpoints.iter().any(|bp| bp.mode.as_deref().is_some_and(flow));
    let stretches = spec.align == "stretch" || spec.breakpoints.iter().any(|bp| bp.align.as_deref() == Some("stretch"));
    flows && stretches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut out = media_header::PNG_SIGNATURE.to_vec();
        out.extend_from_slice(&[0, 0, 0, 13]);
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0]);
        out
    }

    #[test]
    fn png_size_is_read_from_ihdr_and_other_bytes_are_refused() {
        assert_eq!(png_size(&png_header(640, 480)), Some((640, 480)));
        assert_eq!(png_size(&png_header(0, 480)), None);
        let mut jpeg_like = png_header(4, 4);
        jpeg_like[0] = 0xFF;
        assert_eq!(png_size(&jpeg_like), None);
        assert_eq!(png_size(b"not a png at all, padded to 24 bytes"), None);
    }

    #[test]
    fn aspect_height_rounds_half_up_in_emu() {
        // 1000 * 2 / 3 = 666.67 -> 667
        assert_eq!(aspect_height(1000, 3, 2).unwrap(), 667);
        // 5 * 1 / 2 = 2.5 -> 3 (ties go up)
        assert_eq!(aspect_height(5, 2, 1).unwrap(), 3);
        // 1000 * 2 / 6 = 333.33 -> 333 (render twice as wide)
        assert_eq!(aspect_height(1000, 6, 2).unwrap(), 333);
        assert_eq!(aspect_height(3_810_000, 300, 150).unwrap(), 1_905_000);
        assert!(aspect_height(0, 1, 1).is_err());
        assert!(aspect_height(10, 0, 1).is_err());
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

    fn png_input_set() -> [(&'static str, Vec<u8>); 5] {
        let normal = png_file(4, 13);
        let mut no_signature = normal.clone();
        no_signature[..8].fill(0);
        let no_iend = normal[..normal.len() - 12].to_vec();
        [
            ("P1 normal", normal),
            ("P2 no signature", no_signature),
            ("P3 IHDR length 14", png_file(4, 14)),
            ("P4 zero width", png_file(0, 13)),
            ("P5 no IEND", no_iend),
        ]
    }

    use designllm_contracts::AssetRef;

    struct OneAsset(AssetRef, Vec<u8>);
    impl AssetStore for OneAsset {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> {
            Err(DltError::unsupported("no asset writes"))
        }
        fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> {
            Ok(self.1.clone())
        }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
            if hash == self.0.sha256 { Ok(self.0.clone()) } else { Err(DltError::new(ErrorCode::NotFound, hash)) }
        }
    }

    #[test]
    fn png_input_set_acceptance_through_update_figure_link() {
        let mut table = Vec::new();
        for (name, bytes) in png_input_set() {
            let reference = AssetRef { sha256: sha256(&bytes), mime: PNG_MIME.into(), bytes: bytes.len() as u64 };
            let image = Node {
                id: "img".into(), kind: NodeKind::Image, bounds: designllm_contracts::Rect { x: 0, y: 0, width: 3_810_000, height: 1_000_000 },
                text: String::new(), style: Default::default(), asset: None, children: Vec::new(), layout: None,
                interactions: Vec::new(), variants: Default::default(), ext: Default::default(),
            };
            let page = designllm_contracts::Page { nodes: vec![image], ..designllm_contracts::Page::new("page", designllm_contracts::Size::slide()) };
            let mut doc = CanvasDocument { pages: vec![page], ..Default::default() };
            let link = FigureLink {
                figure_id: "figure-1".into(), figure_revision: 1, target_node_id: "img".into(),
                representation: "png".into(), render_hash: reference.sha256.clone(),
            };
            let assets = OneAsset(reference, bytes);
            table.push((name, update(&mut doc, &mut Vec::new(), &assets, &link).is_ok()));
        }
        println!("49e canvas UpdateFigureLink accepts: {table:?}");
        assert_eq!(
            table,
            [("P1 normal", true), ("P2 no signature", false), ("P3 IHDR length 14", false), ("P4 zero width", false), ("P5 no IEND", true)]
        );
    }

    #[test]
    fn stretch_is_detected_at_the_base_or_at_any_breakpoint_of_a_flow_container() {
        let row_stretch = LayoutSpec { mode: "row".into(), align: "stretch".into(), ..Default::default() };
        assert!(may_stretch_children(&row_stretch));
        let column_center = LayoutSpec { mode: "column".into(), align: "center".into(), ..Default::default() };
        assert!(!may_stretch_children(&column_center));
        let absolute_stretch = LayoutSpec { mode: "absolute".into(), align: "stretch".into(), ..Default::default() };
        assert!(!may_stretch_children(&absolute_stretch), "absolute children are placed at their bounds");
        let becomes_flow = LayoutSpec {
            mode: "absolute".into(),
            breakpoints: vec![designllm_contracts::Breakpoint {
                min_width_css_px: 900,
                mode: Some("row".into()),
                gap: None,
                padding: None,
                align: Some("stretch".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(may_stretch_children(&becomes_flow));
    }
}
