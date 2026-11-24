//! Anti-slop design warnings (tokens `text.lint-rules`, codes `SLOP_*`) for Canvas pages and slides: habits that make a page
//! read as a default generated layout. The rules are pure functions of the page model (`Page`, `Node`, `Style`) and of each
//! node's frame, so the Canvas engine and the slides engine share one set. They are non-blocking warnings: no edit is
//! refused by them. Each rule fires only on a clear pattern (conservative thresholds). Nothing here is serialized, so the
//! schemas do not change. The built-in Canvas templates must pass every rule (`canvas::quality` pins it).

use crate::{Diagnostic, Gradient, Node, NodeKind, Page, Rect, Shadow, Style, EMU_PER_CSS_PX, EMU_PER_PT};
use std::collections::{BTreeMap, BTreeSet};

/// Codes of the anti-slop warnings. The desktop words them in `strings::lint_label`.
pub const SLOP_EMOJI_ICON: &str = "SLOP_EMOJI_ICON";
pub const SLOP_PLACEHOLDER_TEXT: &str = "SLOP_PLACEHOLDER_TEXT";
pub const SLOP_FILLER_COPY: &str = "SLOP_FILLER_COPY";
pub const SLOP_DEFAULT_GRADIENT: &str = "SLOP_DEFAULT_GRADIENT";
pub const SLOP_HEAVY_SHADOW: &str = "SLOP_HEAVY_SHADOW";
pub const SLOP_UNIFORM_CARDS: &str = "SLOP_UNIFORM_CARDS";
pub const SLOP_ALL_CENTERED: &str = "SLOP_ALL_CENTERED";
pub const SLOP_TYPE_SCALE: &str = "SLOP_TYPE_SCALE";
/// Slides only: the last page's text is nothing but a closing line.
pub const SLOP_GENERIC_CLOSING: &str = "SLOP_GENERIC_CLOSING";

/// A shadow is heavy from 32 CSS px of blur (EMU) at 0.25 opacity or more.
const HEAVY_BLUR: i64 = 32 * EMU_PER_CSS_PX;
const HEAVY_OPACITY: f64 = 0.25;
/// Purple and indigo hues in degrees. A stop counts only with at least `MIN_CHROMA` (0..=1): a greyer stop has no hue to speak of.
const PURPLE_HUE_MIN: f64 = 230.0;
const PURPLE_HUE_MAX: f64 = 300.0;
const MIN_CHROMA: f64 = 0.2;
/// A large shape covers at least this share of its page.
const LARGE_SHARE: f64 = 0.25;
/// Corner radius of a generic card (EMU): 12 to 24 CSS px.
const CARD_RADIUS_MIN: i64 = 12 * EMU_PER_CSS_PX;
const CARD_RADIUS_MAX: i64 = 24 * EMU_PER_CSS_PX;
/// Cards in one row. Two cards are a pair, not a pattern.
const CARD_ROW: usize = 3;
/// Edges and sizes that differ by at most this (EMU, one CSS px) count as the same.
const SAME_EMU: i64 = EMU_PER_CSS_PX;
/// Text nodes on one page, from this count, that are all centred while one of them is a body paragraph.
const CENTRED_TEXTS: usize = 4;
/// A body paragraph has at least this many non-empty lines, or at least `BODY_CHARS` characters.
const BODY_LINES: usize = 3;
const BODY_CHARS: usize = 120;
/// Text sizes on one page, counted in whole points, from this count.
const TEXT_SIZES: usize = 7;

/// Filler words, matched as whole words in lower case. A hyphen or space inside an entry is a word break.
const FILLER_WORDS: &[&str] =
    &["empower", "leverage", "unlock", "seamless", "seamlessly", "elevate", "revolutionize", "cutting-edge", "next-generation"];
/// Filler phrases of Korean copy, matched as substrings.
const FILLER_PHRASES: &[&str] = &["혁신적인", "원활한", "차원이 다른", "새로운 차원"];
/// Placeholder copy, matched as whole words or Korean names with particles.
const PLACEHOLDER_WORDS: &[&str] = &["lorem ipsum", "john doe", "jane doe", "acme", "techcorp"];
const PLACEHOLDER_PHRASES: &[&str] = &["홍길동", "oo회사"];

/// The colour channels of `#RRGGBB` or `#RRGGBBAA` (alpha 255 when absent). None for anything else, named colours included.
pub fn rgba(color: &str) -> Option<(u8, u8, u8, u8)> {
    let hex = color.strip_prefix('#').filter(|h| h.is_ascii() && (h.len() == 6 || h.len() == 8))?;
    let at = |i: usize| -> Option<u8> { u8::from_str_radix(hex.get(i..i + 2)?, 16).ok() };
    Some((at(0)?, at(2)?, at(4)?, if hex.len() == 8 { at(6)? } else { 255 }))
}

/// The warnings of a list of pages. `frames` gives the drawn frame of each node by ID (a laid-out Canvas document's).
pub fn slop_warnings(pages: &[Page], frames: &BTreeMap<String, Rect>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for page in pages {
        let nodes = flatten(&page.nodes);
        for node in nodes.iter().copied() {
            out.extend(node_warnings(node, page, frames));
        }
        out.extend(page_warnings(&nodes, frames));
    }
    out
}

/// The warnings of a deck: the page rules over the nodes' own bounds (slide nodes are placed in absolute coordinates), and
/// SLOP_GENERIC_CLOSING on the last page.
pub fn slop_deck_warnings(pages: &[Page]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for page in pages {
        let bounds: BTreeMap<String, Rect> = flatten(&page.nodes).into_iter().map(|node| (node.id.clone(), node.bounds)).collect();
        out.extend(slop_warnings(std::slice::from_ref(page), &bounds));
    }
    out.extend(closing_warning(pages));
    out
}

fn warning(code: &str, node_id: &str, message: String) -> Diagnostic {
    Diagnostic { code: code.into(), message, node_id: Some(node_id.into()), blocking: false, params: Default::default() }
}

/// A visible node whose kind can draw text.
fn live_text(node: &Node) -> bool {
    matches!(node.kind, NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse) && node.ext.hidden != Some(true) && !node.text.trim().is_empty()
}

/// The node tree in draw order, parents before their children.
fn flatten(nodes: &[Node]) -> Vec<&Node> {
    let mut out = Vec::new();
    for node in nodes {
        if node.ext.hidden != Some(true) {
            out.push(node);
            out.extend(flatten(&node.children));
        }
    }
    out
}

fn node_warnings(node: &Node, page: &Page, frames: &BTreeMap<String, Rect>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if live_text(node) && emoji_as_icon(&node.text) {
        out.push(warning(SLOP_EMOJI_ICON, &node.id, format!("text {} starts with an emoji or is only emoji, so it reads as an icon", node.id)));
    }
    if live_text(node) && placeholder(&node.text) {
        out.push(warning(SLOP_PLACEHOLDER_TEXT, &node.id, format!("text {} is placeholder copy", node.id)));
    }
    // shortcut: a path frame does not establish its filled area; inspect path geometry before warning on it.
    if let Some(gradient) = node.style.gradient.as_ref().filter(|_| matches!(node.kind, NodeKind::Rectangle | NodeKind::Ellipse)) {
        let large = frames.get(&node.id).is_some_and(|frame| covers_page(*frame, page));
        if large && purple_gradient(gradient, &node.style) {
            out.push(warning(SLOP_DEFAULT_GRADIENT, &node.id, format!("large shape {} is filled with a purple or indigo gradient", node.id)));
        }
    }
    if matches!(node.kind, NodeKind::Rectangle | NodeKind::Ellipse) && heavy_shadow(&node.style) {
        out.push(warning(SLOP_HEAVY_SHADOW, &node.id, format!("the shadow on {} blurs at least 32 px and is at least 0.25 opaque", node.id)));
    }
    out
}

fn page_warnings(nodes: &[&Node], frames: &BTreeMap<String, Rect>) -> Vec<Diagnostic> {
    let texts: Vec<&Node> = nodes.iter().copied().filter(|node| live_text(node)).collect();
    let mut out = Vec::new();

    // A title must be short and uniquely larger than the other text on the page.
    let hits: Vec<usize> = texts.iter().map(|node| filler_count(&node.text)).collect();
    let total: usize = hits.iter().sum();
    let title = (0..texts.len()).find(|&i| {
        !is_body(&texts[i].text) && texts.len() > 1
            && texts.iter().enumerate().all(|(j, other)| i == j || texts[i].style.font_size > other.style.font_size)
    });
    let title_hit = title.is_some_and(|i| hits[i] > 0);
    if title_hit || total >= 2 {
        let anchor = match title {
            Some(i) if title_hit => i,
            _ => hits.iter().position(|&n| n > 0).unwrap_or(0),
        };
        out.push(warning(SLOP_FILLER_COPY, &texts[anchor].id, format!("{total} filler words on this page, or one in its title")).with_param("count", total));
    }

    if let Some(card) = card_row(nodes, frames) {
        out.push(warning(SLOP_UNIFORM_CARDS, &card.id, format!("a row of identical shadowed cards starts at {}", card.id)));
    }

    let text_nodes: Vec<&Node> = texts.iter().copied().filter(|node| node.kind == NodeKind::Text).collect();
    if text_nodes.len() >= CENTRED_TEXTS && text_nodes.iter().all(|node| node.style.align == "center") && text_nodes.iter().any(|node| is_body(&node.text)) {
        out.push(
            warning(SLOP_ALL_CENTERED, &text_nodes[0].id, format!("all {} text nodes on this page are centred, body copy included", text_nodes.len()))
                .with_param("count", text_nodes.len()),
        );
    }

    let sizes: BTreeSet<i64> = text_nodes.iter().map(|node| node.style.font_size.saturating_add(EMU_PER_PT / 2) / EMU_PER_PT).collect();
    if sizes.len() >= TEXT_SIZES {
        out.push(warning(SLOP_TYPE_SCALE, &text_nodes[0].id, format!("this page uses {} text sizes", sizes.len())).with_param("count", sizes.len()));
    }
    out
}

/// SLOP_GENERIC_CLOSING: the last page has text, and all of it is a closing line.
fn closing_warning(pages: &[Page]) -> Option<Diagnostic> {
    let last = pages.last()?;
    let texts: Vec<&Node> = flatten(&last.nodes).into_iter().filter(|node| live_text(node)).collect();
    let first = texts.first()?;
    if texts.iter().all(|node| closing_line(&node.text)) {
        Some(warning(SLOP_GENERIC_CLOSING, &first.id, "the last page holds only a closing line".to_owned()))
    } else {
        None
    }
}

/// Whether a text is a closing line. Case, spacing and trailing punctuation do not count.
fn closing_line(text: &str) -> bool {
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    matches!(words.trim_end_matches(|c: char| matches!(c, '.' | '!' | '?' | '~')), "thank you" | "감사합니다" | "q&a" | "질문")
}

/// A body paragraph: at least `BODY_LINES` non-empty lines, or at least `BODY_CHARS` characters.
fn is_body(text: &str) -> bool {
    text.lines().filter(|line| !line.trim().is_empty()).count() >= BODY_LINES || text.chars().count() >= BODY_CHARS
}

/// Whether a text node starts with an emoji, or is made of emoji and spaces only: an emoji in place of an icon.
fn emoji_as_icon(text: &str) -> bool {
    let mut chars = text.trim_start().chars();
    let Some(first) = chars.next() else { return false };
    let second = chars.next();
    let third = chars.next();
    let regional = |c: char| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c);
    let flag = regional(first) && second.is_some_and(regional);
    let keycap = (first.is_ascii_digit() || matches!(first, '#' | '*'))
        && (second == Some('\u{20E3}') || (second == Some('\u{FE0F}') && third == Some('\u{20E3}')));
    let emoji_variant = second == Some('\u{FE0F}') && matches!(u32::from(first),
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x2194..=0x2199 | 0x21A9..=0x21AA | 0x2328 | 0x23CF | 0x23F8..=0x23FA
        | 0x24C2 | 0x25AA..=0x25AB | 0x25B6 | 0x25C0 | 0x25FB..=0x25FC | 0x2600..=0x2604 | 0x260E | 0x2611 | 0x2618 | 0x261D
        | 0x2620 | 0x2622..=0x2623 | 0x2626 | 0x262A | 0x262E..=0x262F | 0x2638..=0x263A | 0x2640 | 0x2642 | 0x265F..=0x2660
        | 0x2663 | 0x2665..=0x2666 | 0x2668 | 0x267B | 0x267E | 0x2692..=0x2697 | 0x2699 | 0x269B..=0x269C | 0x26A0 | 0x26A7
        | 0x26B0..=0x26B1 | 0x26C8 | 0x26CF | 0x26D1 | 0x26D3 | 0x26E9 | 0x26F0..=0x26F1 | 0x26F4 | 0x26F7..=0x26F9
        | 0x2702 | 0x2708..=0x2709 | 0x270C..=0x270D | 0x270F | 0x2712 | 0x2714 | 0x2716 | 0x271D | 0x2721 | 0x2733..=0x2734
        | 0x2744 | 0x2747 | 0x2763..=0x2764 | 0x27A1 | 0x2934..=0x2935 | 0x2B05..=0x2B07 | 0x1F5A5 | 0x1F6E0 | 0x1F5D3);
    flag || keycap || emoji_variant || (is_emoji(first) && second != Some('\u{FE0E}'))
}

/// Pictographs that show as emoji by default, in the common Unicode ranges.
fn is_emoji(c: char) -> bool {
    !matches!(u32::from(c), 0x1F3FB..=0x1F3FF | 0x1F9B0..=0x1F9B3) && matches!(
        u32::from(c),
        0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615 | 0x2648..=0x2653 | 0x267F | 0x2693
            | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE | 0x26C4..=0x26C5 | 0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA
            | 0x26FD | 0x2705 | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF
            | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A | 0x1F201 | 0x1F21A | 0x1F22F
            | 0x1F232..=0x1F236 | 0x1F238..=0x1F23A | 0x1F250..=0x1F251 | 0x1F300..=0x1F320 | 0x1F32D..=0x1F335 | 0x1F337..=0x1F37C
            | 0x1F37E..=0x1F393 | 0x1F3A0..=0x1F3CA | 0x1F3CF..=0x1F3D3 | 0x1F3E0..=0x1F3F0 | 0x1F3F4 | 0x1F3F8..=0x1F43E
            | 0x1F440 | 0x1F442..=0x1F4FC | 0x1F4FF..=0x1F53D | 0x1F54B..=0x1F54E | 0x1F550..=0x1F567 | 0x1F57A
            | 0x1F595..=0x1F596 | 0x1F5A4 | 0x1F5FB..=0x1F64F | 0x1F680..=0x1F6C5 | 0x1F6CC | 0x1F6D0..=0x1F6D2
            | 0x1F6D5..=0x1F6D7 | 0x1F6DC..=0x1F6DF | 0x1F6EB..=0x1F6EC | 0x1F6F4..=0x1F6FC | 0x1F7E0..=0x1F7EB | 0x1F7F0
            | 0x1F90C..=0x1F93A | 0x1F93C..=0x1F945 | 0x1F947..=0x1F9FF | 0x1FA70..=0x1FA7C | 0x1FA80..=0x1FA89
            | 0x1FA8F..=0x1FAC6 | 0x1FACE..=0x1FADC | 0x1FADF..=0x1FAE9 | 0x1FAF0..=0x1FAF8
    )
}

/// The words of `text` in lower case. Anything that is not a letter or a digit ends a word.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).map(str::to_owned).collect()
}

/// How often `phrase` occurs in `words` as whole words. The phrase's own hyphens and spaces are word breaks too.
fn count_phrase(words: &[String], phrase: &str) -> usize {
    let wanted: Vec<&str> = phrase.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).collect();
    if wanted.is_empty() {
        return 0;
    }
    words.windows(wanted.len()).filter(|window| window.iter().zip(&wanted).all(|(word, want)| word.as_str() == *want)).count()
}

fn filler_count(text: &str) -> usize {
    let words = words(text);
    let english: usize = FILLER_WORDS.iter().map(|phrase| count_phrase(&words, *phrase)).sum();
    let korean: usize = FILLER_PHRASES.iter().map(|phrase| text.matches(*phrase).count()).sum();
    english + korean
}

fn placeholder(text: &str) -> bool {
    let words = words(text);
    let particles = ["", "은", "는", "이", "가", "을", "를", "의", "과", "와", "도", "만", "에게", "께", "께서", "에서", "으로", "로", "입니다", "이다"];
    PLACEHOLDER_WORDS.iter().any(|phrase| count_phrase(&words, *phrase) > 0)
        || words.iter().any(|word| PLACEHOLDER_PHRASES.iter().any(|phrase| {
            word.strip_prefix(*phrase).is_some_and(|suffix| {
                let suffix = suffix.strip_prefix("님").or_else(|| suffix.strip_prefix("씨")).unwrap_or(suffix);
                particles.contains(&suffix) || ["에게", "께", "께서", "에서", "으로", "로"].iter().any(|base| {
                    suffix.strip_prefix(*base).is_some_and(|rest| matches!(rest, "는" | "도" | "만"))
                })
            })
        }))
}

/// Hue in degrees (0..360) and chroma (0..=1) of a colour. A grey has chroma 0.
fn hue_and_chroma(color: &str) -> Option<(f64, f64)> {
    let (r, g, b, _) = rgba(color)?;
    let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let delta = max - min;
    if delta == 0.0 {
        return Some((0.0, 0.0));
    }
    let hue = if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    Some((hue, delta / 255.0))
}

/// Whether every stop of a gradient is a saturated purple or indigo.
fn purple_gradient(gradient: &Gradient, style: &Style) -> bool {
    let opacity = style.opacity_milli.unwrap_or(1000).min(1000);
    let mut visible = false;
    let purple = !gradient.stops.is_empty() && gradient.stops.iter().all(|stop| {
        let Some((_, _, _, alpha)) = rgba(&stop.color) else { return false };
        visible |= (u32::from(alpha) * opacity + 500) / 1000 > 0;
        hue_and_chroma(&stop.color).is_some_and(|(hue, chroma)| chroma >= MIN_CHROMA && (PURPLE_HUE_MIN..=PURPLE_HUE_MAX).contains(&hue))
    });
    purple && visible
}

/// Whether a frame covers at least `LARGE_SHARE` of its page.
fn covers_page(frame: Rect, page: &Page) -> bool {
    if frame.width <= 0 || frame.height <= 0 || page.size.width <= 0 || page.size.height <= 0 { return false; }
    let visible = |start: i64, length: i64, page_length: i64| {
        (i128::from(start) + i128::from(length)).min(i128::from(page_length)).saturating_sub(i128::from(start).max(0)).max(0) as f64
    };
    let page_area = page.size.width as f64 * page.size.height as f64;
    visible(frame.x, frame.width, page.size.width) * visible(frame.y, frame.height, page.size.height) >= LARGE_SHARE * page_area
}

/// Opacity a shadow paints with: its colour's alpha times the opacity of the node.
fn shadow_opacity(shadow: &Shadow, style: &Style) -> Option<f64> {
    let (_, _, _, alpha) = rgba(&shadow.color)?;
    let node = style.opacity_milli.map_or(1.0, |milli| f64::from(milli.min(1000)) / 1000.0);
    Some(f64::from(alpha) / 255.0 * node)
}

fn heavy_shadow(style: &Style) -> bool {
    style.shadows.iter().any(|shadow| shadow.blur >= HEAVY_BLUR && shadow_opacity(shadow, style).is_some_and(|opacity| opacity >= HEAVY_OPACITY))
}

/// The first card of a row of `CARD_ROW` or more identical shadowed rectangles: the same size and corner radius (12 to 24 px),
/// the same top edge, and non-overlapping positions.
fn card_row<'a>(nodes: &[&'a Node], frames: &BTreeMap<String, Rect>) -> Option<&'a Node> {
    let cards: Vec<(&'a Node, Rect)> = nodes
        .iter()
        .copied()
        .filter(|node| node.kind == NodeKind::Rectangle && node.style.shadows.iter().any(|shadow| shadow_opacity(shadow, &node.style).is_some_and(|opacity| opacity >= 0.02)))
        .filter(|node| node.style.corner_radius.is_some_and(|radius| (CARD_RADIUS_MIN..=CARD_RADIUS_MAX).contains(&radius)))
        .filter_map(|node| frames.get(&node.id).filter(|frame| frame.width > 0 && frame.height > 0).map(|frame| (node, *frame)))
        .collect();
    let tolerance = i128::from(SAME_EMU);
    let close = |a: i64, b: i64| (i128::from(a) - i128::from(b)).abs() <= tolerance;
    // shortcut: two initial outliers may mask a row; use a multidimensional range index if missed warnings matter.
    for shifts in 0..8 {
        let bucket = |value: i64, bit: u32| (i128::from(value) + if shifts & (1 << bit) != 0 { 2 * tolerance } else { 0 }).div_euclid(4 * tolerance);
        let mut groups: BTreeMap<(i64, i128, i128, i128), Vec<(&Node, Rect)>> = BTreeMap::new();
        for &(card, frame) in &cards {
            let key = (card.style.corner_radius.unwrap_or_default(), bucket(frame.y, 0), bucket(frame.width, 1), bucket(frame.height, 2));
            groups.entry(key).or_default().push((card, frame));
        }
        for group in groups.values_mut() {
            if group.len() < CARD_ROW { continue; }
            group.sort_by_key(|(_, frame)| frame.x);
            for start in 0..group.len().min(2) {
                let (first, anchor) = group[start];
                let mut right = i128::from(anchor.x) + i128::from(anchor.width);
                let mut count = 1;
                for &(_, frame) in &group[start + 1..] {
                    if close(frame.y, anchor.y) && close(frame.width, anchor.width) && close(frame.height, anchor.height)
                        && i128::from(frame.x) >= right
                    {
                        count += 1;
                        if count >= CARD_ROW { return Some(first); }
                        right = i128::from(frame.x) + i128::from(frame.width);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GradientStop, NodeExt, Size};

    fn px(value: i64) -> i64 {
        value * EMU_PER_CSS_PX
    }

    fn rect(x: i64, y: i64, width: i64, height: i64) -> Rect {
        Rect { x: px(x), y: px(y), width: px(width), height: px(height) }
    }

    fn node(id: &str, kind: NodeKind, text: &str, style: Style) -> Node {
        Node {
            id: id.into(),
            kind,
            bounds: rect(0, 0, 0, 0),
            text: text.into(),
            style,
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: NodeExt::default(),
        }
    }

    fn text(id: &str, copy: &str, size_pt: i64, align: &str) -> Node {
        node(id, NodeKind::Text, copy, Style { font_size: size_pt * EMU_PER_PT, align: align.into(), ..Style::default() })
    }

    fn shadowed(id: &str, blur_px: i64, color: &str) -> Node {
        node(id, NodeKind::Rectangle, "", Style { shadows: vec![Shadow { x: 0, y: 0, blur: px(blur_px), spread: 0, color: color.into() }], ..Style::default() })
    }

    /// One 1920 x 1080 px page with the given nodes.
    fn pages(nodes: Vec<Node>) -> Vec<Page> {
        vec![Page { id: "page".into(), size: Size { width: px(1920), height: px(1080) }, nodes, notes: String::new(), name: None, role: None }]
    }

    fn codes(pages: &[Page], frames: &BTreeMap<String, Rect>) -> Vec<String> {
        let found = slop_warnings(pages, frames);
        assert!(found.iter().all(|d| !d.blocking), "SLOP warnings never block: {found:?}");
        found.into_iter().map(|d| d.code).collect()
    }

    fn purple(from: &str, to: &str) -> Style {
        Style {
            gradient: Some(Gradient {
                angle_millideg: 135_000,
                stops: vec![GradientStop { pos_milli: 0, color: from.into() }, GradientStop { pos_milli: 1000, color: to.into() }],
            }),
            ..Style::default()
        }
    }

    #[test]
    fn colour_channels_read_hex_with_or_without_alpha_and_refuse_the_rest() {
        assert_eq!(rgba("#0F172A"), Some((15, 23, 42, 255)));
        assert_eq!(rgba("#0F172A80"), Some((15, 23, 42, 128)));
        assert_eq!(rgba("#GGGGGG"), None);
        assert_eq!(rgba("red"), None);
        assert_eq!(rgba("#FFF"), None);
    }

    #[test]
    fn an_emoji_in_front_of_the_copy_or_alone_reads_as_an_icon() {
        let frames = BTreeMap::new();
        assert_eq!(codes(&pages(vec![text("a", "🚀 Launch", 12, "left")]), &frames), [SLOP_EMOJI_ICON]);
        assert_eq!(codes(&pages(vec![text("a", "🚀", 12, "left")]), &frames), [SLOP_EMOJI_ICON]);
        assert!(codes(&pages(vec![text("a", "Launch 🚀", 12, "left")]), &frames).is_empty(), "an emoji at the end is not an icon");
        assert!(codes(&pages(vec![text("a", "☀ sunny", 12, "left")]), &frames).is_empty(), "a text-default symbol is not an emoji");
        assert!(codes(&pages(vec![text("a", "Launch", 12, "left")]), &frames).is_empty());
    }

    #[test]
    fn emoji_sequences_and_text_variants_are_distinguished() {
        for copy in ["\u{1F1F0}\u{1F1F7} 대한민국", "1\u{FE0F}\u{20E3}", "☀\u{FE0F} 날씨", "#\u{20E3} 번호"] {
            assert!(emoji_as_icon(copy), "{copy}");
        }
        for copy in ["대한민국 \u{1F1F0}\u{1F1F7}", "1번", "☀ 날씨", "☀\u{FE0E} 날씨", "A\u{FE0F} title", "∑\u{FE0F} 매출", "\u{1F321} 날씨", "1\u{FE0F}번", "\u{1F1F0} alone"] {
            assert!(!emoji_as_icon(copy), "{copy}");
        }
    }

    #[test]
    fn text_default_technical_symbols_need_emoji_presentation_selector() {
        for copy in ["\u{1F5A5}\u{FE0F} 컴퓨터", "\u{1F6E0}\u{FE0F} 도구", "\u{1F5D3}\u{FE0F} 일정"] {
            assert!(emoji_as_icon(copy), "{copy}");
        }
        for copy in ["\u{1F5A5} 컴퓨터", "\u{1F6E0} 도구", "\u{1F5D3} 일정", "∑\u{FE0F} 매출"] {
            assert!(!emoji_as_icon(copy), "{copy}");
        }
    }

    #[test]
    fn placeholder_copy_is_named_and_similar_words_are_not() {
        let frames = BTreeMap::new();
        for copy in ["Lorem ipsum dolor", "John Doe", "Acme", "TechCorp launch", "홍길동", "OO회사 소개"] {
            assert_eq!(codes(&pages(vec![text("a", copy, 12, "left")]), &frames), [SLOP_PLACEHOLDER_TEXT], "{copy}");
        }
        assert!(codes(&pages(vec![text("a", "Acmeware", 12, "left")]), &frames).is_empty(), "a longer word is not the name");
    }

    #[test]
    fn korean_placeholder_requires_a_word_boundary_or_known_particle() {
        for copy in ["홍길동", "홍길동은 발표자", "홍길동의 소개", "홍길동에게는", "홍길동님", "홍길동씨가", "홍길동께서", "OO회사에서도", "OO회사의 제품", "(홍길동)"] {
            assert!(placeholder(copy), "{copy}");
        }
        for copy in ["홍길동전", "홍길동전은 소설", "김홍길동", "OO회사원", "홍길동123"] {
            assert!(!placeholder(copy), "{copy}");
        }
    }

    #[test]
    fn text_on_shapes_uses_every_text_rule_and_counts_as_closing_content() {
        let mut shape = text("shape", "홍길동", 40, "left");
        for kind in [NodeKind::Rectangle, NodeKind::Ellipse] {
            shape.kind = kind;
            assert!(codes(&pages(vec![shape.clone()]), &BTreeMap::new()).contains(&SLOP_PLACEHOLDER_TEXT.to_owned()));
            shape.text = "🚀 Launch".into();
            assert!(codes(&pages(vec![shape.clone()]), &BTreeMap::new()).contains(&SLOP_EMOJI_ICON.to_owned()));
            shape.text = "혁신적인 서비스".into();
            assert!(codes(&pages(vec![shape.clone(), text("body", "내용", 12, "left")]), &BTreeMap::new()).contains(&SLOP_FILLER_COPY.to_owned()));
            shape.text = "매출 10% 증가".into();
            let deck = pages(vec![text("end", "Thank you", 40, "left"), shape.clone()]);
            assert!(!slop_deck_warnings(&deck).iter().any(|d| d.code == SLOP_GENERIC_CLOSING));
            shape.text = "홍길동".into();
        }
        let body = "First line\nSecond line\nThird line";
        let mut centered: Vec<Node> = (0..4).map(|i| text(&format!("t{i}"), if i == 0 { body } else { "Heading" }, 12, "center")).collect();
        for node in &mut centered { node.kind = NodeKind::Rectangle; }
        assert!(!codes(&pages(centered), &BTreeMap::new()).contains(&SLOP_ALL_CENTERED.to_owned()));
        let scaled: Vec<Node> = (10..17).map(|size| { let mut n = text(&format!("t{size}"), "Copy", size, "left"); n.kind = NodeKind::Ellipse; n }).collect();
        assert!(!codes(&pages(scaled), &BTreeMap::new()).contains(&SLOP_TYPE_SCALE.to_owned()));
        shape.kind = NodeKind::Group;
        assert!(!codes(&pages(vec![shape]), &BTreeMap::new()).contains(&SLOP_PLACEHOLDER_TEXT.to_owned()));
    }

    #[test]
    fn hidden_ancestors_exclude_descendants_from_page_and_closing_warnings() {
        let mut group = node("group", NodeKind::Group, "", Style::default());
        let mut inner = node("inner", NodeKind::Group, "", Style::default());
        inner.children = vec![text("placeholder", "홍길동", 12, "left"), text("closing", "감사합니다", 12, "left")];
        group.children.push(inner);
        group.ext.hidden = Some(true);
        assert!(slop_deck_warnings(&pages(vec![group.clone()])).is_empty());
        group.ext.hidden = Some(false);
        let found = slop_deck_warnings(&pages(vec![group.clone()]));
        assert!(found.iter().any(|d| d.code == SLOP_PLACEHOLDER_TEXT && d.node_id.as_deref() == Some("placeholder")));
        group.children[0].children.remove(0);
        assert!(slop_deck_warnings(&pages(vec![group])).iter().any(|d| d.code == SLOP_GENERIC_CLOSING));
    }

    #[test]
    fn group_opacity_does_not_change_a_childs_own_opacity() {
        let mut group = node("group", NodeKind::Group, "", Style { opacity_milli: Some(0), ..Style::default() });
        group.children.push(shadowed("child", 32, "#000000"));
        let found = slop_warnings(&pages(vec![group.clone()]), &BTreeMap::new());
        assert!(found.iter().any(|d| d.code == SLOP_HEAVY_SHADOW && d.node_id.as_deref() == Some("child")));
        group.children[0].style.opacity_milli = Some(0);
        assert!(!slop_warnings(&pages(vec![group]), &BTreeMap::new()).iter().any(|d| d.code == SLOP_HEAVY_SHADOW));
    }

    #[test]
    fn filler_title_must_be_short_and_uniquely_larger() {
        let frames = BTreeMap::new();
        let body = "We leverage data\nSecond line\nThird line";
        let mut same = vec![text("first", body, 12, "left"), text("second", "Ordinary paragraph", 12, "left")];
        assert!(codes(&pages(same.clone()), &frames).is_empty());
        same.reverse();
        assert!(codes(&pages(same), &frames).is_empty());
        assert!(codes(&pages(vec![text("body", body, 30, "left"), text("other", "Copy", 12, "left")]), &frames).is_empty());
        assert!(codes(&pages(vec![text("only", "Unlock growth", 30, "left")]), &frames).is_empty());
        assert_eq!(codes(&pages(vec![text("title", "Unlock growth", 30, "left"), text("body", "Copy", 12, "left")]), &frames), [SLOP_FILLER_COPY]);
        assert_eq!(codes(&pages(vec![text("first", "leverage", 12, "left"), text("second", "empower", 12, "left")]), &frames), [SLOP_FILLER_COPY]);
    }

    #[test]
    fn button_labels_do_not_count_toward_centered_body_warning() {
        let body = "First line\nSecond line\nThird line";
        let mut nodes = vec![text("hero", body, 20, "center")];
        for i in 0..3 {
            let mut button = text(&format!("button{i}"), "Open", 12, "center");
            button.kind = if i == 0 { NodeKind::Ellipse } else { NodeKind::Rectangle };
            nodes.push(button);
        }
        assert!(!codes(&pages(nodes.clone()), &BTreeMap::new()).contains(&SLOP_ALL_CENTERED.to_owned()));
        for i in 0..3 { nodes.push(text(&format!("heading{i}"), "Heading", 12, "center")); }
        assert!(codes(&pages(nodes), &BTreeMap::new()).contains(&SLOP_ALL_CENTERED.to_owned()));
    }

    #[test]
    fn button_sizes_do_not_count_toward_type_scale() {
        let mut nodes: Vec<Node> = (10..16).map(|size| text(&format!("t{size}"), "Copy", size, "left")).collect();
        let mut button = text("button", "Open", 32, "center");
        button.kind = NodeKind::Rectangle;
        nodes.push(button);
        assert!(!codes(&pages(nodes.clone()), &BTreeMap::new()).contains(&SLOP_TYPE_SCALE.to_owned()));
        nodes.push(text("seventh", "Copy", 32, "left"));
        assert!(codes(&pages(nodes), &BTreeMap::new()).contains(&SLOP_TYPE_SCALE.to_owned()));
    }

    #[test]
    fn type_scale_rounding_handles_maximum_font_size() {
        let mut nodes: Vec<Node> = (10..16).map(|size| text(&format!("t{size}"), "Copy", size, "left")).collect();
        assert!(!codes(&pages(nodes.clone()), &BTreeMap::new()).contains(&SLOP_TYPE_SCALE.to_owned()));
        let mut extreme = text("extreme", "Copy", 12, "left");
        extreme.style.font_size = i64::MAX;
        nodes.push(extreme);
        assert!(codes(&pages(nodes), &BTreeMap::new()).contains(&SLOP_TYPE_SCALE.to_owned()));
    }

    #[test]
    fn filler_words_warn_from_two_on_a_page_or_from_one_in_the_title() {
        let frames = BTreeMap::new();
        let two_in_body = pages(vec![text("title", "Quarterly results", 28, "left"), text("a", "We leverage data and empower teams", 12, "left")]);
        assert_eq!(codes(&two_in_body, &frames), [SLOP_FILLER_COPY]);
        let one_in_body = pages(vec![text("title", "Quarterly results", 28, "left"), text("a", "We leverage data", 12, "left")]);
        assert!(codes(&one_in_body, &frames).is_empty(), "one filler word in the body is left alone");
        let one_in_title = pages(vec![text("title", "Unlock growth", 28, "left"), text("a", "Plain body copy", 12, "left")]);
        let found = slop_warnings(&one_in_title, &frames);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].node_id.as_deref(), Some("title"), "the title takes the warning");
        assert_eq!(codes(&pages(vec![text("a", "Our cutting edge product", 12, "left"), text("b", "혁신적인 원활한 서비스", 12, "left")]), &frames), [SLOP_FILLER_COPY]);
    }

    #[test]
    fn a_large_purple_or_indigo_gradient_is_the_default_look() {
        let mut frames = BTreeMap::new();
        let big = node("bg", NodeKind::Rectangle, "", purple("#7C3AED", "#4338CA"));
        frames.insert("bg".to_string(), rect(0, 0, 1920, 1080));
        assert_eq!(codes(&pages(vec![big]), &frames), [SLOP_DEFAULT_GRADIENT]);

        let small = node("chip", NodeKind::Rectangle, "", purple("#7C3AED", "#4338CA"));
        frames.insert("chip".to_string(), rect(0, 0, 300, 200));
        assert!(codes(&pages(vec![small]), &frames).is_empty(), "a small shape is not the background");

        let blue = node("bg", NodeKind::Rectangle, "", purple("#2563EB", "#1E3A8A"));
        frames.insert("bg".to_string(), rect(0, 0, 1920, 1080));
        assert!(codes(&pages(vec![blue]), &frames).is_empty(), "blue stops are not purple");

        let grey = node("bg", NodeKind::Rectangle, "", purple("#6B6B80", "#707070"));
        assert!(codes(&pages(vec![grey]), &frames).is_empty(), "greys have no purple hue");
    }

    #[test]
    fn gradient_requires_visible_purple_stops_after_node_opacity() {
        let frames = BTreeMap::from([("bg".to_owned(), rect(0, 0, 1920, 1080))]);
        for style in [purple("#8000FF00", "#4000FF00"), Style { opacity_milli: Some(0), ..purple("#7C3AED", "#4338CA") }, Style { opacity_milli: Some(1), ..purple("#8000FF01", "#4000FF01") }] {
            assert!(codes(&pages(vec![node("bg", NodeKind::Rectangle, "", style)]), &frames).is_empty());
        }
        for style in [purple("#8000FF80", "#4000FF00"), Style { opacity_milli: Some(500), ..purple("#7C3AED", "#4338CA") }] {
            assert_eq!(codes(&pages(vec![node("bg", NodeKind::Rectangle, "", style)]), &frames), [SLOP_DEFAULT_GRADIENT]);
        }
        assert!(codes(&pages(vec![node("bg", NodeKind::Rectangle, "", purple("#00FF0000", "#4338CA"))]), &frames).is_empty());
    }

    #[test]
    fn gradient_area_is_clipped_to_the_page() {
        let page = pages(Vec::new()).remove(0);
        assert!(!covers_page(rect(2000, 0, 1920, 1080), &page));
        assert!(!covers_page(rect(-1800, 0, 1920, 1080), &page));
        assert!(!covers_page(rect(0, 0, -1920, 1080), &page));
        assert!(covers_page(rect(-1440, 0, 1920, 1080), &page));
        assert!(!covers_page(rect(-1441, 0, 1920, 1080), &page));
        assert!(!covers_page(Rect { x: i64::MAX, y: 0, width: i64::MAX, height: i64::MAX }, &page));
    }

    #[test]
    fn deck_bounds_are_isolated_per_page_even_with_reused_node_ids() {
        let mut background = node("bg", NodeKind::Rectangle, "", purple("#7C3AED", "#4338CA"));
        background.bounds = rect(0, 0, 1920, 1080);
        let mut small = background.clone();
        small.bounds = rect(0, 0, 20, 20);
        let mut deck = pages(vec![background]);
        let mut other = pages(vec![small]).remove(0);
        other.id = "other".into();
        deck.push(other);
        assert_eq!(slop_deck_warnings(&deck).iter().filter(|d| d.code == SLOP_DEFAULT_GRADIENT).count(), 1);
        deck.reverse();
        assert_eq!(slop_deck_warnings(&deck).iter().filter(|d| d.code == SLOP_DEFAULT_GRADIENT).count(), 1);

        let mut row: Vec<Node> = (0..3).map(|i| {
            let mut card = shadowed(&format!("card{i}"), 16, "#000000");
            card.style.corner_radius = Some(px(16));
            card.bounds = rect(i * 100, 0, 80, 100);
            card
        }).collect();
        let mut stacked = row.clone();
        for card in &mut stacked { card.bounds.x = 0; }
        let mut deck = pages(std::mem::take(&mut row));
        let mut other = pages(stacked).remove(0);
        other.id = "other".into();
        deck.push(other);
        assert_eq!(slop_deck_warnings(&deck).iter().filter(|d| d.code == SLOP_UNIFORM_CARDS).count(), 1);
        deck.reverse();
        assert_eq!(slop_deck_warnings(&deck).iter().filter(|d| d.code == SLOP_UNIFORM_CARDS).count(), 1);
    }

    #[test]
    fn only_drawable_shapes_get_gradient_and_shadow_warnings() {
        use crate::{FillRule, PathCommand, PathSpec};
        let frames = BTreeMap::from([("item".to_owned(), rect(0, 0, 1920, 1080))]);
        let mut style = purple("#7C3AED", "#4338CA");
        style.shadows.push(Shadow { x: 0, y: 0, blur: px(40), spread: 0, color: "#000000".into() });
        for kind in [NodeKind::Rectangle, NodeKind::Ellipse] {
            let found = codes(&pages(vec![node("item", kind, "", style.clone())]), &frames);
            assert!(found.contains(&SLOP_DEFAULT_GRADIENT.to_owned()));
            assert!(found.contains(&SLOP_HEAVY_SHADOW.to_owned()));
        }
        for kind in [NodeKind::Group, NodeKind::Image, NodeKind::Video, NodeKind::Line, NodeKind::Text] {
            let found = codes(&pages(vec![node("item", kind, "", style.clone())]), &frames);
            assert!(!found.contains(&SLOP_DEFAULT_GRADIENT.to_owned()), "{kind:?}");
            assert!(!found.contains(&SLOP_HEAVY_SHADOW.to_owned()), "{kind:?}");
        }
        let mut path = node("item", NodeKind::Path, "", style);
        assert!(codes(&pages(vec![path.clone()]), &frames).is_empty());
        path.ext.path = Some(PathSpec { view_box: rect(0, 0, 1, 1), fill_rule: FillRule::Nonzero, commands: vec![PathCommand::MoveTo { x: 0, y: 0 }, PathCommand::LineTo { x: px(1), y: 0 }, PathCommand::Close] });
        let found = codes(&pages(vec![path]), &frames);
        assert!(!found.contains(&SLOP_DEFAULT_GRADIENT.to_owned()));
        assert!(!found.contains(&SLOP_HEAVY_SHADOW.to_owned()));
    }

    #[test]
    fn a_heavy_shadow_is_wide_and_strong() {
        let frames = BTreeMap::new();
        assert_eq!(codes(&pages(vec![shadowed("a", 32, "#000000")]), &frames), [SLOP_HEAVY_SHADOW]);
        assert!(codes(&pages(vec![shadowed("a", 31, "#000000")]), &frames).is_empty(), "31 px is not heavy");
        assert!(codes(&pages(vec![shadowed("a", 40, "#0F172A1A")]), &frames).is_empty(), "a 0.10 alpha shadow is light");
        let faded = node(
            "a",
            NodeKind::Rectangle,
            "",
            Style {
                shadows: vec![Shadow { x: 0, y: 0, blur: px(40), spread: 0, color: "#00000080".into() }],
                opacity_milli: Some(400),
                ..Style::default()
            },
        );
        assert!(codes(&pages(vec![faded]), &frames).is_empty(), "alpha 0.5 at node opacity 0.4 is 0.2, below 0.25");
    }

    #[test]
    fn a_row_of_three_identical_shadowed_cards_is_the_uniform_card_pattern() {
        let mut frames = BTreeMap::new();
        let cards: Vec<Node> = (0..3)
            .map(|i| {
                let mut card = shadowed(&format!("card{i}"), 16, "#0F172A1A");
                card.style.corner_radius = Some(px(16));
                frames.insert(card.id.clone(), rect(100 + i * 250, 300, 200, 120));
                card
            })
            .collect();
        let found = slop_warnings(&pages(cards.clone()), &frames);
        assert_eq!(found.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(), [SLOP_UNIFORM_CARDS]);
        assert_eq!(found[0].node_id.as_deref(), Some("card0"));

        assert!(codes(&pages(cards[..2].to_vec()), &frames).is_empty(), "two cards are not a pattern");

        let mut round = cards.clone();
        for card in round.iter_mut() {
            card.style.corner_radius = Some(px(40));
        }
        assert!(codes(&pages(round), &frames).is_empty(), "a radius outside 12 to 24 px is not a card");

        let stacked = cards.clone();
        for (i, card) in stacked.iter().enumerate() {
            frames.insert(card.id.clone(), rect(100 + i as i64 * 250, 300 + i as i64 * 200, 200, 120));
        }
        assert!(codes(&pages(stacked), &frames).is_empty(), "cards on different rows are not one row");
    }

    #[test]
    fn card_rows_require_three_non_overlapping_positions() {
        let mut frames = BTreeMap::new();
        let cards: Vec<Node> = (0..3).map(|i| {
            let mut card = shadowed(&format!("card{i}"), 16, "#000000");
            card.style.corner_radius = Some(px(16));
            frames.insert(card.id.clone(), rect([0, 100, 100][i], 0, 80, 120));
            card
        }).collect();
        assert!(codes(&pages(cards.clone()), &frames).is_empty());
        frames.insert("card2".into(), rect(150, 0, 80, 120));
        assert!(codes(&pages(cards.clone()), &frames).is_empty());
        frames.insert("card2".into(), rect(200, 0, 80, 120));
        assert_eq!(codes(&pages(cards), &frames), [SLOP_UNIFORM_CARDS]);
    }

    #[test]
    fn invisible_shadows_do_not_form_a_card_row() {
        let mut frames = BTreeMap::new();
        let mut cards: Vec<Node> = (0..3).map(|i| {
            let mut card = shadowed(&format!("card{i}"), 16, "#00000000");
            card.style.corner_radius = Some(px(16));
            frames.insert(card.id.clone(), rect(i * 100, 0, 80, 100));
            card
        }).collect();
        assert!(codes(&pages(cards.clone()), &frames).is_empty());
        for card in &mut cards { card.style.shadows[0].color = "#000000".into(); card.style.opacity_milli = Some(0); }
        assert!(codes(&pages(cards.clone()), &frames).is_empty());
        for card in &mut cards { card.style.opacity_milli = Some(1000); }
        assert_eq!(codes(&pages(cards), &frames), [SLOP_UNIFORM_CARDS]);
    }

    #[test]
    fn card_shadows_require_effective_opacity_at_least_two_percent() {
        let mut frames = BTreeMap::new();
        let mut cards: Vec<Node> = (0..3).map(|i| {
            let mut card = shadowed(&format!("card{i}"), 16, "#000000");
            card.style.corner_radius = Some(px(16));
            card.style.opacity_milli = Some(19);
            frames.insert(card.id.clone(), rect(i * 100, 0, 80, 100));
            card
        }).collect();
        assert!(!codes(&pages(cards.clone()), &frames).contains(&SLOP_UNIFORM_CARDS.to_owned()));
        for card in &mut cards { card.style.opacity_milli = Some(20); }
        assert!(codes(&pages(cards.clone()), &frames).contains(&SLOP_UNIFORM_CARDS.to_owned()));
        for card in &mut cards { card.style.opacity_milli = Some(1000); card.style.shadows[0].color = "#00000005".into(); }
        assert!(!codes(&pages(cards), &frames).contains(&SLOP_UNIFORM_CARDS.to_owned()));
    }

    #[test]
    fn one_outlier_before_a_card_row_does_not_hide_it() {
        let mut frames = BTreeMap::new();
        let cards: Vec<Node> = (0..4).map(|i| {
            let mut card = shadowed(&format!("card{i}"), 16, "#000000");
            card.style.corner_radius = Some(px(16));
            let mut frame = rect(i * 100, 0, 50, 100);
            if i > 0 { frame.y = px(1) + px(1) / 2; }
            frames.insert(card.id.clone(), frame);
            card
        }).collect();
        assert_eq!(codes(&pages(cards.clone()), &frames), [SLOP_UNIFORM_CARDS]);
        assert!(codes(&pages(cards[..3].to_vec()), &frames).is_empty());
    }

    #[test]
    fn five_thousand_cards_on_separate_rows_and_a_large_page_with_a_row() {
        let mut frames = BTreeMap::new();
        let mut cards: Vec<Node> = (0..5000).map(|i| {
            let mut card = shadowed(&format!("card{i}"), 16, "#000000");
            card.style.corner_radius = Some(px(16));
            frames.insert(card.id.clone(), rect(0, i * 10, 80, 100));
            card
        }).collect();
        assert!(codes(&pages(cards.clone()), &frames).is_empty());
        for i in 0..3 {
            frames.insert(cards[i].id.clone(), rect(i as i64 * 100, 0, 80, 100));
        }
        assert_eq!(codes(&pages(std::mem::take(&mut cards)), &frames), [SLOP_UNIFORM_CARDS]);
    }

    #[test]
    fn four_centred_text_nodes_with_a_body_paragraph_are_the_all_centred_pattern() {
        let frames = BTreeMap::new();
        let body = "Our team builds tools for designers who need to ship pages every week without losing the details that make a layout their own, and this paragraph is long enough to count as body copy.";
        let headings: Vec<Node> = (0..3).map(|i| text(&format!("h{i}"), "Heading", 12, "center")).collect();
        assert!(codes(&pages(headings.clone()), &frames).is_empty(), "short centred headings alone are not the pattern");
        let mut with_body = headings.clone();
        with_body.push(text("body", body, 12, "center"));
        assert_eq!(codes(&pages(with_body.clone()), &frames), [SLOP_ALL_CENTERED]);
        let mut one_left = with_body.clone();
        one_left[0].style.align = "left".into();
        assert!(codes(&pages(one_left), &frames).is_empty(), "one node that is not centred breaks the pattern");
        let three_lines = text("lines", "First line\nSecond line\nThird line", 12, "center");
        let mut with_lines = headings.clone();
        with_lines.push(three_lines);
        assert_eq!(codes(&pages(with_lines), &frames), [SLOP_ALL_CENTERED], "three lines is a body paragraph");
        let mut with_empty = with_body;
        with_empty.push(text("empty", "  ", 12, "left"));
        assert_eq!(codes(&pages(with_empty), &frames), [SLOP_ALL_CENTERED], "an empty text box does not count");
    }

    #[test]
    fn seven_text_sizes_on_a_page_are_too_many_and_sizes_round_to_whole_points() {
        let frames = BTreeMap::new();
        let sizes = [10, 12, 14, 16, 18, 24, 32];
        let seven: Vec<Node> = sizes.iter().enumerate().map(|(i, size)| text(&format!("t{i}"), "Copy", *size, "left")).collect();
        assert_eq!(codes(&pages(seven.clone()), &frames), [SLOP_TYPE_SCALE]);
        assert!(codes(&pages(seven[..6].to_vec()), &frames).is_empty(), "six sizes are not too many");
        let mut near = seven[..6].to_vec();
        // 0.2 pt is 2 540 EMU: 12.2 pt and 11.8 pt round to the same whole point as 12 pt.
        near.push(Node { style: Style { font_size: 12 * EMU_PER_PT + 2_540, ..Style::default() }, ..text("a", "Copy", 12, "left") });
        near.push(Node { style: Style { font_size: 12 * EMU_PER_PT - 2_540, ..Style::default() }, ..text("b", "Copy", 12, "left") });
        assert!(codes(&pages(near), &frames).is_empty(), "12.2 pt and 11.8 pt round to 12 pt, so the page keeps six sizes");
    }

    #[test]
    fn a_closing_last_page_is_the_generic_closing_of_a_deck() {
        let mut deck = pages(vec![text("hero", "Quarterly results", 40, "left")]);
        deck.push(Page { id: "last".into(), size: Size { width: px(1920), height: px(1080) }, nodes: vec![text("end", "Thank you!", 40, "center")], notes: String::new(), name: None, role: None });
        let found = slop_deck_warnings(&deck);
        let closing = found.iter().find(|d| d.code == SLOP_GENERIC_CLOSING).expect("the closing warning");
        assert_eq!(closing.node_id.as_deref(), Some("end"));
        assert!(!closing.blocking);

        for line in ["감사합니다", "Q&A", "질문", "thank you"] {
            deck[1].nodes = vec![text("end", line, 40, "center")];
            assert!(slop_deck_warnings(&deck).iter().any(|d| d.code == SLOP_GENERIC_CLOSING), "{line}");
        }
        deck[1].nodes = vec![text("end", "Thank you, Ana Kim", 40, "center")];
        assert!(!slop_deck_warnings(&deck).iter().any(|d| d.code == SLOP_GENERIC_CLOSING), "a name after the line is content");
        deck[1].nodes = vec![text("end", "Thank you", 40, "center"), text("more", "Ask us about the Q3 plan", 20, "left")];
        assert!(!slop_deck_warnings(&deck).iter().any(|d| d.code == SLOP_GENERIC_CLOSING), "other text on the last page is content");
        deck[1].nodes = vec![node("picture", NodeKind::Image, "", Style::default())];
        assert!(!slop_deck_warnings(&deck).iter().any(|d| d.code == SLOP_GENERIC_CLOSING), "a last page with no text is not a closing line");
        deck[1].nodes = vec![text("end", "Thank you", 40, "center")];
        deck[0].nodes = vec![text("only", "Thank you", 40, "center")];
        assert_eq!(slop_deck_warnings(&deck).iter().filter(|d| d.code == SLOP_GENERIC_CLOSING).count(), 1, "only the last page is judged");
    }

    #[test]
    fn a_deck_reads_the_node_bounds_for_its_card_rows() {
        let mut cards: Vec<Node> = (0..3).map(|i| shadowed(&format!("card{i}"), 16, "#0F172A1A")).collect();
        for (i, card) in cards.iter_mut().enumerate() {
            card.bounds = rect(100 + i as i64 * 250, 300, 200, 120);
            card.style.corner_radius = Some(px(16));
        }
        let found = slop_deck_warnings(&pages(cards));
        assert_eq!(found.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(), [SLOP_UNIFORM_CARDS]);
    }
}
