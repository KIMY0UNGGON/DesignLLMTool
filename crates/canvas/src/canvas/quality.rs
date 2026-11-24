//! Canvas glue of the design-quality warnings. The rules live in contracts (`design_quality`), shared with the slides engine. Here
//! they run on a laid-out Canvas document as non-blocking warnings (`templates::design_warnings`, so only documents with v1.2
//! values get them). The built-in templates must pass every rule: the test below pins it.

use designllm_contracts::{design_quality, CanvasDocument, Diagnostic, Rect};
use std::collections::BTreeMap;

#[cfg(test)]
#[path = "../../tests/common/mod.rs"]
mod test_support;

/// The design-quality warnings of a Canvas document. `frames` are its laid-out frames by node ID.
pub(crate) fn warnings(doc: &CanvasDocument, frames: &BTreeMap<String, Rect>) -> Vec<Diagnostic> {
    design_quality::design_quality_warnings(&doc.pages, frames)
}

#[cfg(test)]
mod tests {
    use super::super::{layout, templates};
    use super::*;
    use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, EngineAdapter, EngineContext, Gradient, GradientStop, Node, NodeExt, NodeKind, Page, RenderScene, Shadow, Size, Style, EMU_PER_CSS_PX};

    /// One text node with `text` on a page, under the id `a`.
    fn document(text: &str) -> CanvasDocument {
        let node = Node {
            id: "a".into(),
            kind: NodeKind::Text,
            bounds: Rect { x: 0, y: 0, width: 100, height: 40 },
            text: text.into(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: NodeExt::default(),
        };
        let page = Page { id: "page".into(), size: Size { width: 1_000_000, height: 1_000_000 }, nodes: vec![node], notes: String::new(), name: None, role: None };
        CanvasDocument { pages: vec![page], ..Default::default() }
    }

    #[test]
    fn the_design_quality_rules_join_the_template_rules_as_non_blocking_warnings() {
        let mut frames = BTreeMap::new();
        frames.insert("a".to_string(), Rect { x: 0, y: 0, width: 100, height: 40 });
        let scene = RenderScene { pages: Vec::new(), font_fingerprint: String::new(), diagnostics: Vec::new(), metadata: BTreeMap::new(), links: Vec::new() };
        let found = templates::design_warnings(&document("🚀 Launch"), &scene, &frames);
        let emoji = found.iter().find(|d| d.code == "ICON_EMOJI").expect("the emoji warning");
        assert!(!emoji.blocking);
        assert_eq!(emoji.node_id.as_deref(), Some("a"));
    }

    #[test]
    fn canvas_validate_checks_shape_text_and_conservative_visual_edges() {
        let fonts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
        let renderer = designllm_render::Renderer::bundled_only(&fonts).expect("bundled fonts");
        let assets = test_support::MemoryAssets::default();
        let context = EngineContext { assets: &assets, renderer: &renderer };
        let px = EMU_PER_CSS_PX;
        let validate = |nodes: Vec<Node>| {
            let doc = CanvasDocument { pages: vec![Page::new("page", Size { width: 1920 * px, height: 1080 * px })], ..Default::default() };
            let mut artifact = Artifact::new(ArtifactKind::Canvas, "test", crate::CANVAS_ENGINE_ID, ArtifactBody::Canvas(doc));
            let ArtifactBody::Canvas(document) = &mut artifact.body else { unreachable!() };
            document.pages[0].nodes = nodes;
            super::super::CanvasEngine.validate(&artifact, &context)
        };
        let make = |id: &str, kind, text: &str, x| {
            let mut node = test_support::node(id, kind, Rect { x: x * px, y: 0, width: 80 * px, height: 80 * px });
            node.text = text.into();
            node.style.opacity_milli = Some(1000);
            node
        };
        for kind in [NodeKind::Rectangle, NodeKind::Ellipse] {
            let diagnostics = validate(vec![make("shape", kind, "홍길동에게는", 0)]);
            assert!(diagnostics.iter().any(|d| d.code == "TEXT_PLACEHOLDER" && d.node_id.as_deref() == Some("shape")), "{diagnostics:?}");
            let diagnostics = validate(vec![make("shape", kind, "홍길동전", 0)]);
            assert!(!diagnostics.iter().any(|d| d.code == "TEXT_PLACEHOLDER"));
        }
        for (text, expected) in [("☀\u{FE0F} 날씨", true), ("∑\u{FE0F} 매출", false)] {
            let diagnostics = validate(vec![make("text", NodeKind::Text, text, 0)]);
            assert_eq!(diagnostics.iter().any(|d| d.code == "ICON_EMOJI"), expected, "{diagnostics:?}");
        }
        let mut background = make("bg", NodeKind::Rectangle, "", 2000);
        background.bounds.width = 1920 * px;
        background.bounds.height = 1080 * px;
        background.style.gradient = Some(Gradient { angle_millideg: 0, stops: vec![GradientStop { pos_milli: 0, color: "#7C3AED".into() }, GradientStop { pos_milli: 1000, color: "#4338CA".into() }] });
        assert!(!validate(vec![background.clone()]).iter().any(|d| d.code == "FILL_STOCK_GRADIENT"));
        background.bounds.x = 0;
        assert!(validate(vec![background]).iter().any(|d| d.code == "FILL_STOCK_GRADIENT"));
        let mut cards: Vec<Node> = (0..3).map(|i| {
            let mut card = make(&format!("card{i}"), NodeKind::Rectangle, "", i * 100);
            card.style.corner_radius = Some(16 * px);
            card.style.shadows = vec![Shadow { x: 0, y: 0, blur: 16 * px, spread: 0, color: "#00000000".into() }];
            card
        }).collect();
        assert!(!validate(cards.clone()).iter().any(|d| d.code == "LAYOUT_REPEATED_CARDS"));
        for card in &mut cards { card.style.shadows[0].color = "#00000080".into(); }
        assert!(validate(cards.clone()).iter().any(|d| d.code == "LAYOUT_REPEATED_CARDS"));
        for card in &mut cards { card.style.opacity_milli = Some(0); }
        assert!(!validate(cards.clone()).iter().any(|d| d.code == "LAYOUT_REPEATED_CARDS"));
        for (i, card) in cards.iter_mut().enumerate() {
            card.style.opacity_milli = Some(1000);
            card.bounds.y = px + px / 2;
            card.bounds.x = (i as i64 + 1) * 100 * px;
        }
        let mut outlier = cards[0].clone();
        outlier.id = "outlier".into();
        outlier.bounds.x = 0;
        outlier.bounds.y = 0;
        cards.insert(0, outlier);
        assert!(validate(cards).iter().any(|d| d.code == "LAYOUT_REPEATED_CARDS"));
    }

    /// Every built-in template, laid out at its default size with the bundled fonts, passes every rule.
    #[test]
    fn the_built_in_templates_carry_no_design_quality_warning() {
        let fonts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
        let renderer = designllm_render::Renderer::bundled_only(&fonts).expect("the bundled fonts");
        let mut flagged = Vec::new();
        for name in templates::TEMPLATES {
            let size = templates::default_size(name).expect("a default size");
            let document = templates::build(name, size).expect("the template document");
            let laid_out = layout::layout_pages(&document, &renderer, None).expect("the layout");
            let found: Vec<String> = warnings(&document, &laid_out.frames).iter().map(|d| format!("{} at {:?}", d.code, d.node_id)).collect();
            if !found.is_empty() {
                flagged.push(format!("{name}: {}", found.join(", ")));
            }
        }
        assert!(flagged.is_empty(), "built-in templates with design-quality warnings: {flagged:#?}");
    }
}
