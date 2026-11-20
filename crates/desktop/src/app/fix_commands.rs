//! The fixes a refusal callout offers for a lint finding (tokens `text.lint-rules`): each builds one change set for
//! the node the finding names, so a button on screen and an agent on the UI channel reach the same operation.
//!
//! | code | command | change |
//! |---|---|---|
//! | TOUCH_TARGET | `fix.grow_to_min` | the node grows (around its centre) to the minimum size |
//! | CONTRAST | `fix.darken_text` | the text takes the nearest colour of the document that meets the ratio |
//! | CLIPPED | `fix.grow_box` / `fix.shrink_text` | the box grows to its text / the font size steps down |
//! | CHILD_OUTSIDE_PARENT | `fix.grow_parent` | the parent grows to hold the child |
//! | OUT_OF_PAGE | `fix.move_into_page` | the node moves (and shrinks when larger) into its page |

use designllm_contracts::{ArtifactBody, CONTRAST_MIN, EMU_PER_CSS_PX, Node, Operation, Page, Rect as ModelRect, StylePatch, TOUCH_MIN_PX};
use serde_json::Value;

use super::{kind_commands::intents_of, DesktopApp};
use crate::commands::CommandSpec;

/// The font size never steps below 6 pt.
const MIN_FONT: i64 = 6 * designllm_contracts::EMU_PER_PT;

/// A node the fix works on, with where it is: its page, its parent and the page-absolute frames of both.
struct Located {
    page_index: usize,
    page_size: designllm_contracts::Size,
    node: Node,
    /// The node's frame on the page: where the engine drew it, else its stored bounds made absolute.
    frame: ModelRect,
    parent: Option<(Node, ModelRect)>,
    /// The fills under the node, nearest first (its own, then its ancestors').
    fills: Vec<String>,
}

fn channel(hex: &str, at: usize) -> Option<f64> {
    let value = u8::from_str_radix(hex.get(at..at + 2)?, 16).ok()?;
    let s = f64::from(value) / 255.0;
    Some(if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) })
}

fn luminance(color: &str) -> Option<f64> {
    let hex = color.strip_prefix('#').filter(|hex| hex.is_ascii() && (hex.len() == 6 || hex.len() == 8))?;
    Some(0.2126 * channel(hex, 0)? + 0.7152 * channel(hex, 2)? + 0.0722 * channel(hex, 4)?)
}

/// WCAG contrast of two `#RRGGBB` colours (the same formula as the canvas lint).
pub(super) fn contrast(a: &str, b: &str) -> Option<f64> {
    let (la, lb) = (luminance(a)?, luminance(b)?);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

fn rgb(color: &str) -> Option<(i32, i32, i32)> {
    let hex = color.strip_prefix('#')?;
    let part = |at: usize| hex.get(at..at + 2).and_then(|pair| i32::from_str_radix(pair, 16).ok());
    Some((part(0)?, part(2)?, part(4)?))
}

/// The candidate nearest to `current` (RGB distance) whose contrast with `background` is at least `min_ratio`.
/// Black and white are always candidates, so a background always has an answer.
pub(super) fn nearest_passing(current: &str, background: &str, min_ratio: f64, candidates: &[String]) -> Option<String> {
    let from = rgb(current).unwrap_or((0, 0, 0));
    let distance = |color: &str| {
        rgb(color).map(|(r, g, b)| (r - from.0).pow(2) + (g - from.1).pow(2) + (b - from.2).pow(2))
    };
    candidates
        .iter()
        .map(String::as_str)
        .chain(["#000000", "#FFFFFF"])
        .filter(|color| contrast(color, background).is_some_and(|ratio| ratio >= min_ratio))
        .filter_map(|color| distance(color).map(|d| (d, color)))
        .min_by_key(|(d, _)| *d)
        .map(|(_, color)| color.to_owned())
}

/// `rect` grown around its centre to at least `min_w` × `min_h`, never to a negative origin.
pub(super) fn grown_to_min(rect: ModelRect, min_w: i64, min_h: i64) -> ModelRect {
    let width = rect.width.max(min_w);
    let height = rect.height.max(min_h);
    ModelRect {
        x: (rect.x - (width - rect.width) / 2).max(0),
        y: (rect.y - (height - rect.height) / 2).max(0),
        width,
        height,
    }
}

/// The move (dx, dy) and the size that put `frame` inside a `page` of that size; a frame larger than the page
/// takes the page's size.
pub(super) fn into_page(frame: ModelRect, page: designllm_contracts::Size) -> (i64, i64, i64, i64) {
    let width = frame.width.min(page.width);
    let height = frame.height.min(page.height);
    let x = frame.x.clamp(0, page.width - width);
    let y = frame.y.clamp(0, page.height - height);
    (x - frame.x, y - frame.y, width, height)
}

fn visible_fill(fill: &str) -> bool {
    !fill.is_empty() && fill != "transparent" && fill.starts_with('#')
}

/// Walks `nodes` for `id`; `origin` is the page-absolute origin of `nodes`' parent.
fn walk(nodes: &[Node], id: &str, origin: (i64, i64), parent: Option<(&Node, ModelRect)>, fills: &[String]) -> Option<(Node, ModelRect, Option<(Node, ModelRect)>, Vec<String>)> {
    for node in nodes {
        let frame = ModelRect { x: origin.0 + node.bounds.x, y: origin.1 + node.bounds.y, ..node.bounds };
        let mut under = Vec::with_capacity(fills.len() + 1);
        if visible_fill(&node.style.fill) {
            under.push(node.style.fill.clone());
        }
        under.extend_from_slice(fills);
        if node.id == id {
            return Some((node.clone(), frame, parent.map(|(node, rect)| (node.clone(), rect)), under));
        }
        if let Some(found) = walk(&node.children, id, (frame.x, frame.y), Some((node, frame)), &under) {
            return Some(found);
        }
    }
    None
}

/// Every text colour the pages and the named styles use: the document's own palette.
fn document_colours(pages: &[Page], styles: &std::collections::BTreeMap<String, designllm_contracts::Style>) -> Vec<String> {
    fn gather(nodes: &[Node], out: &mut Vec<String>) {
        for node in nodes {
            if node.style.color.starts_with('#') && !out.contains(&node.style.color) {
                out.push(node.style.color.clone());
            }
            gather(&node.children, out);
        }
    }
    let mut out: Vec<String> = styles.values().map(|style| style.color.clone()).filter(|color| color.starts_with('#')).collect();
    for page in pages {
        gather(&page.nodes, &mut out);
    }
    out
}

impl DesktopApp {
    /// Runs the lint fixes; `None` for an id this module does not serve.
    pub(super) fn dispatch_fixes(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        if !spec.id.starts_with("fix.") {
            return None;
        }
        let result = self.locate_for_fix(args).and_then(|located| {
            let operations = match spec.id {
                "fix.grow_to_min" => self.fix_grow_to_min(&located, args),
                "fix.darken_text" => self.fix_darken_text(&located, args),
                "fix.grow_box" => self.fix_grow_box(&located),
                "fix.shrink_text" => fix_shrink_text(&located),
                "fix.grow_parent" => self.fix_grow_parent(&located),
                "fix.move_into_page" => Ok(fix_move_into_page(&located)),
                _ => return Err(format!("{}: 아직 연결되지 않았습니다", spec.label)),
            }?;
            if operations.is_empty() {
                return Err("이미 맞습니다".into());
            }
            if self.context.is_none() {
                return Err("작업물을 아직 읽는 중입니다".into());
            }
            let intents = intents_of(&operations);
            self.apply_operations(operations, intents);
            Ok(())
        });
        Some(result)
    }

    fn locate_for_fix(&self, args: &Value) -> Result<Located, String> {
        let id = args
            .get("node_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.selected_nodes.first().cloned())
            .ok_or("고칠 대상을 고르세요")?;
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let pages = match &artifact.body {
            ArtifactBody::Canvas(document) => &document.pages,
            ArtifactBody::Slides(document) => &document.pages,
            _ => return Err("이 작업물에서는 이 고침을 쓸 수 없습니다".into()),
        };
        for (page_index, page) in pages.iter().enumerate() {
            let Some((node, stored, parent, fills)) = walk(&page.nodes, &id, (0, 0), None, &[]) else {
                continue;
            };
            let drawn = self.page_textures.get(&(page_index as u32)).map(|texture| texture.frames.clone()).unwrap_or_default();
            let frame = drawn.get(&node.id).copied().unwrap_or(stored);
            let parent = parent.map(|(parent, rect)| {
                let rect = drawn.get(&parent.id).copied().unwrap_or(rect);
                (parent, rect)
            });
            return Ok(Located { page_index, page_size: page.size, node, frame, parent, fills });
        }
        Err("대상을 찾지 못했습니다".into())
    }

    /// Length arguments are in px on canvases and slides.
    fn fix_grow_to_min(&self, located: &Located, args: &Value) -> Result<Vec<Operation>, String> {
        let length = |key: &str| (args.get(key).and_then(Value::as_f64).unwrap_or(TOUCH_MIN_PX as f64).max(1.0) * EMU_PER_CSS_PX as f64).round() as i64;
        let grown = grown_to_min(located.node.bounds, length("min_w"), length("min_h"));
        Ok(if grown == located.node.bounds { Vec::new() } else { vec![Operation::SetGeometry { node_id: located.node.id.clone(), bounds: grown }] })
    }

    fn fix_darken_text(&self, located: &Located, args: &Value) -> Result<Vec<Operation>, String> {
        let min_ratio = args.get("min_ratio").and_then(Value::as_f64).unwrap_or(CONTRAST_MIN);
        let background = located.fills.first().cloned().unwrap_or_else(|| "#FFFFFF".into());
        let current = located.node.style.color.clone();
        if contrast(&current, &background).is_some_and(|ratio| ratio >= min_ratio) {
            return Ok(Vec::new());
        }
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let colours = match &artifact.body {
            ArtifactBody::Canvas(document) => document_colours(&document.pages, &document.styles),
            ArtifactBody::Slides(document) => document_colours(&document.pages, &document.styles),
            _ => Vec::new(),
        };
        let color = nearest_passing(&current, &background, min_ratio, &colours).ok_or("맞는 색을 찾지 못했습니다")?;
        Ok(vec![Operation::SetStyle { node_ids: vec![located.node.id.clone()], patch: StylePatch { color: Some(color), ..Default::default() } }])
    }

    /// The box grows to the bottom of its last drawn text line; when no line reaches past it, by a quarter.
    fn fix_grow_box(&self, located: &Located) -> Result<Vec<Operation>, String> {
        let lines_bottom = self
            .page_textures
            .get(&(located.page_index as u32))
            .map(|texture| {
                texture
                    .page
                    .source_map
                    .iter()
                    .filter(|source| source.node_id == located.node.id && source.logical_range.is_some())
                    .map(|source| source.bounds.y + source.bounds.height)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let needed = lines_bottom - located.frame.y;
        let bounds = located.node.bounds;
        let height = if needed > bounds.height { needed } else { bounds.height + bounds.height / 4 };
        Ok(vec![Operation::SetGeometry { node_id: located.node.id.clone(), bounds: ModelRect { height, ..bounds } }])
    }

    /// The parent grows to hold the child's frame plus its padding; a child left or above the padding moves in.
    fn fix_grow_parent(&self, located: &Located) -> Result<Vec<Operation>, String> {
        let (parent, outer) = located.parent.clone().ok_or("상위 프레임이 없습니다")?;
        let [top, right, bottom, left] = parent.layout.as_ref().map_or([0; 4], |layout| layout.padding_sides.unwrap_or([layout.padding; 4]));
        let child = located.frame;
        let mut operations = Vec::new();
        // A child before the padding moves in (stored bounds are relative to the parent).
        let (shift_x, shift_y) = ((outer.x + left - child.x).max(0), (outer.y + top - child.y).max(0));
        if shift_x > 0 || shift_y > 0 {
            let bounds = located.node.bounds;
            operations.push(Operation::SetGeometry {
                node_id: located.node.id.clone(),
                bounds: ModelRect { x: bounds.x + shift_x, y: bounds.y + shift_y, ..bounds },
            });
        }
        let width = outer.width.max(child.x + shift_x + child.width + right - outer.x);
        let height = outer.height.max(child.y + shift_y + child.height + bottom - outer.y);
        if width != outer.width || height != outer.height {
            operations.push(Operation::SetGeometry { node_id: parent.id.clone(), bounds: ModelRect { width, height, ..parent.bounds } });
        }
        Ok(operations)
    }
}

/// The font size one step (a tenth) down, never below 6 pt.
fn fix_shrink_text(located: &Located) -> Result<Vec<Operation>, String> {
    let size = located.node.style.font_size;
    if size <= MIN_FONT {
        return Err("글자를 더 줄일 수 없습니다".into());
    }
    let smaller = (size - size / 10).max(MIN_FONT);
    Ok(vec![Operation::SetStyle { node_ids: vec![located.node.id.clone()], patch: StylePatch { font_size: Some(smaller), ..Default::default() } }])
}

fn fix_move_into_page(located: &Located) -> Vec<Operation> {
    let (dx, dy, width, height) = into_page(located.frame, located.page_size);
    let bounds = located.node.bounds;
    let moved = ModelRect { x: bounds.x + dx, y: bounds.y + dy, width, height };
    if moved == bounds {
        Vec::new()
    } else {
        vec![Operation::SetGeometry { node_id: located.node.id.clone(), bounds: moved }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::Size;

    fn rect(x: i64, y: i64, width: i64, height: i64) -> ModelRect {
        ModelRect { x, y, width, height }
    }

    #[test]
    fn a_small_target_grows_around_its_centre_and_never_past_the_page_origin() {
        assert_eq!(grown_to_min(rect(100, 100, 20, 20), 44, 44), rect(88, 88, 44, 44));
        assert_eq!(grown_to_min(rect(0, 0, 20, 50), 44, 44), rect(0, 0, 44, 50), "a larger height stays");
    }

    #[test]
    fn a_frame_outside_the_page_moves_in_and_a_larger_one_takes_the_page_size() {
        let page = Size { width: 1000, height: 800 };
        assert_eq!(into_page(rect(-50, 700, 200, 200), page), (50, -100, 200, 200));
        assert_eq!(into_page(rect(10, 10, 2000, 100), page), (-10, 0, 1000, 100));
        assert_eq!(into_page(rect(10, 10, 100, 100), page), (0, 0, 100, 100), "a frame inside stays");
    }

    #[test]
    fn the_nearest_document_colour_that_meets_the_ratio_is_chosen() {
        let colours = vec!["#999999".to_owned(), "#555555".to_owned(), "#1F2937".to_owned()];
        // #999999 on white is 2.85:1; #555555 is 7.46:1 and nearer than #1F2937 and black.
        let chosen = nearest_passing("#AAAAAA", "#FFFFFF", 4.5, &colours).expect("a colour");
        assert_eq!(chosen, "#555555");
        assert!(contrast(&chosen, "#FFFFFF").expect("a ratio") >= 4.5);
        // On a dark fill only light colours pass: #999999 (6.6:1) is nearer than white.
        assert_eq!(nearest_passing("#333333", "#111111", 4.5, &colours).as_deref(), Some("#999999"));
    }
}
