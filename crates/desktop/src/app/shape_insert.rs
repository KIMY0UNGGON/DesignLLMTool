//! Diagram shapes from the shared shape library (`designllm_contracts::shape_library`) for people: the toolbox's 다이어그램
//! 도형 tiles and the `add.shape` command insert one as a Canvas path node, the same recipe agents get over MCP
//! (`designllm://shapes`). A path holds no text, so a label is a text node placed over it.

use std::collections::BTreeMap;

use designllm_contracts::{new_id, shape_library, ArtifactBody, Intent, Node, NodeExt, NodeKind, Operation, Style, EMU_PER_CSS_PX};
use serde_json::Value;

use super::{toolbox::AddAt, DesktopApp};

/// The library shapes the toolbox offers, in tile order, with their tile names and icons.
pub(super) const TOOLBOX_SHAPES: &[(&str, &str, &str)] = &[
    ("trapezoid_narrowing_right", "인코더", "shape-encoder"),
    ("trapezoid_widening_right", "디코더", "shape-decoder"),
    ("parallelogram", "데이터", "shape-parallelogram"),
    ("cylinder", "저장소", "shape-cylinder"),
    ("circle_plus", "더하기", "shape-circle-plus"),
    ("circle_times", "곱하기", "shape-circle-times"),
    ("diamond", "분기", "shape-diamond"),
    ("hexagon", "고정 모듈", "shape-hexagon"),
    ("cuboid", "텐서", "shape-cuboid"),
];

/// The size (EMU) a library shape is inserted at: operators are small marks on a line, the others block-sized.
pub(super) fn shape_size(shape: &str) -> (i64, i64) {
    let px = EMU_PER_CSS_PX;
    match shape {
        "circle_plus" | "circle_times" => (24 * px, 24 * px),
        "cylinder" => (96 * px, 120 * px),
        "diamond" | "hexagon" => (120 * px, 96 * px),
        "cuboid" => (120 * px, 120 * px),
        _ => (160 * px, 120 * px),
    }
}

/// The style a library shape starts with: a white fill and a 1.5 px near-black outline (no stock colour, no shadow).
fn shape_style() -> Style {
    Style { fill: "#FFFFFF".into(), stroke: "#1C1C1C".into(), stroke_width: EMU_PER_CSS_PX * 3 / 2, ..Style::default() }
}

impl DesktopApp {
    /// `add.shape`: the library shape `args.shape` as a path node, centred on `args.at` (or at the current page's fixed place).
    pub(super) fn add_library_shape(&mut self, args: &Value) -> Result<(), String> {
        let id = args.get("shape").and_then(Value::as_str).unwrap_or_default();
        let recipe = shape_library::shape(id).ok_or_else(|| format!("없는 도형입니다: {id}"))?;
        let at: Option<AddAt> = self.take_add_at(args)?;
        let Some(artifact) = self.artifact.as_ref() else {
            return Err("작업물을 아직 읽는 중입니다".into());
        };
        let page_index = at.map_or(self.page_index, |at| at.page_index);
        let ArtifactBody::Canvas(document) = &artifact.body else {
            return Err("다이어그램 도형은 캔버스에만 넣을 수 있습니다".into());
        };
        let Some(page) = document.pages.get(page_index as usize) else {
            return Err("선택한 페이지를 편집할 수 없습니다".into());
        };
        let page_id = page.id.clone();
        let (width, height) = shape_size(id);
        let (x, y) = match at {
            Some(at) => (at.center.0 - width / 2, at.center.1 - height / 2),
            None => (40 * EMU_PER_CSS_PX, 40 * EMU_PER_CSS_PX),
        };
        let node = Node {
            id: new_id(),
            kind: NodeKind::Path,
            bounds: designllm_contracts::Rect { x, y, width, height },
            text: String::new(),
            style: shape_style(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: NodeExt { path: Some(recipe.path()), ..Default::default() },
        };
        self.apply_operations(vec![Operation::AddNode { page_id, parent_id: None, node }], [Intent::Content].into_iter().collect());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every toolbox shape is a library recipe with an icon that exists, and the command's schema lists every recipe.
    #[test]
    fn the_toolbox_shapes_are_library_recipes_with_icons_and_the_command_lists_them() {
        for (shape, label, icon) in TOOLBOX_SHAPES {
            assert!(shape_library::shape(shape).is_some(), "{shape}");
            assert!(!label.is_empty());
            assert!(crate::icons_table::ICONS.iter().any(|(name, _)| name == icon), "{icon} is in the icon table");
        }
        let spec = crate::commands::find("add.shape").expect("add.shape is registered");
        let schema: Value = serde_json::from_str(spec.args).expect("schema");
        let listed: Vec<&str> = schema["properties"]["shape"]["enum"].as_array().expect("enum").iter().filter_map(Value::as_str).collect();
        let library: Vec<&str> = shape_library::SHAPES.iter().map(|recipe| recipe.id).collect();
        assert_eq!(listed, library, "add.shape takes every library recipe, in library order");
        assert!(schema["properties"]["at"].is_object(), "add.shape takes a drop point like the other add commands");
    }
}
