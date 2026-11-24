//! Design-quality warnings (tokens `text.lint-rules`) for Canvas pages and slides: patterns that can make a page
//! read as a default generated layout. The rules are pure functions of the page model (`Page`, `Node`, `Style`) and of each
//! node's frame, so the Canvas engine and the slides engine share one set. They are non-blocking warnings: no edit is
//! refused by them. Each rule fires only on a clear pattern (conservative thresholds). Nothing here is serialized, so the
//! schemas do not change. The built-in Canvas templates must pass every rule (`canvas::quality` pins it).

use crate::{Diagnostic, Gradient, Node, NodeKind, Page, Rect, Shadow, Style, EMU_PER_CSS_PX, EMU_PER_PT};
use std::collections::{BTreeMap, BTreeSet};

/// Codes of the design-quality warnings. The desktop words them in `strings::lint_label`.
pub const ICON_EMOJI: &str = "ICON_EMOJI";
pub const TEXT_PLACEHOLDER: &str = "TEXT_PLACEHOLDER";
pub const TEXT_VAGUE_WORDING: &str = "TEXT_VAGUE_WORDING";
pub const FILL_STOCK_GRADIENT: &str = "FILL_STOCK_GRADIENT";
pub const SHADOW_HEAVY: &str = "SHADOW_HEAVY";
pub const LAYOUT_REPEATED_CARDS: &str = "LAYOUT_REPEATED_CARDS";
pub const LAYOUT_ALL_CENTERED: &str = "LAYOUT_ALL_CENTERED";
pub const TEXT_SIZE_SPREAD: &str = "TEXT_SIZE_SPREAD";
pub const BACKGROUND_CREAM: &str = "BACKGROUND_CREAM";
pub const TEXT_EXTRA_LABEL: &str = "TEXT_EXTRA_LABEL";
pub const TITLE_LINE_BREAKS: &str = "TITLE_LINE_BREAKS";
/// Slides only: the last page's text is nothing but a closing line.
pub const SLIDE_PLAIN_CLOSING: &str = "SLIDE_PLAIN_CLOSING";

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
pub fn design_quality_warnings(pages: &[Page], frames: &BTreeMap<String, Rect>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for page in pages {
        let nodes = flatten(&page.nodes);
        for (index, node) in nodes.iter().copied().enumerate() {
            out.extend(node_warnings(node, page, frames, &nodes[index + 1..]));
        }
        out.extend(page_warnings(&nodes, frames));
    }
    out
}

/// The warnings of a deck: the page rules over the nodes' own bounds (slide nodes are placed in absolute coordinates), and
/// SLIDE_PLAIN_CLOSING on the last page.
pub fn deck_quality_warnings(pages: &[Page]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for page in pages {
        let bounds: BTreeMap<String, Rect> = flatten(&page.nodes).into_iter().map(|node| (node.id.clone(), node.bounds)).collect();
        out.extend(design_quality_warnings(std::slice::from_ref(page), &bounds));
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

fn node_warnings(node: &Node, page: &Page, frames: &BTreeMap<String, Rect>, above: &[&Node]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if live_text(node) && emoji_as_icon(&node.text) {
        out.push(warning(ICON_EMOJI, &node.id, format!("text {} starts with an emoji or is only emoji, so it reads as an icon", node.id)));
    }
    if live_text(node) && placeholder(&node.text) {
        out.push(warning(TEXT_PLACEHOLDER, &node.id, format!("text {} is placeholder copy", node.id)));
    }
    // shortcut: a path frame does not establish its filled area; inspect path geometry before warning on it.
    if let Some(gradient) = node.style.gradient.as_ref().filter(|_| matches!(node.kind, NodeKind::Rectangle | NodeKind::Ellipse)) {
        let large = frames.get(&node.id).is_some_and(|frame| covers_page(*frame, page));
        if large && purple_gradient(gradient, &node.style) {
            out.push(warning(FILL_STOCK_GRADIENT, &node.id, format!("large shape {} is filled with a purple or indigo gradient", node.id)));
        }
    }
    if node.kind == NodeKind::Rectangle && node.style.gradient.is_none()
        && frames.get(&node.id).is_some_and(|frame| covers_share(*frame, page, 0.5))
        && cream_fill(&node.style)
        && !above.iter().any(|other| covers_background(node, other, page, frames))
    {
        out.push(warning(BACKGROUND_CREAM, &node.id, format!("large background {} has a warm off-white fill", node.id)));
    }
    if matches!(node.kind, NodeKind::Rectangle | NodeKind::Ellipse) && heavy_shadow(&node.style) {
        out.push(warning(SHADOW_HEAVY, &node.id, format!("the shadow on {} blurs at least 32 px and is at least 0.25 opaque", node.id)));
    }
    out
}

fn page_warnings(nodes: &[&Node], frames: &BTreeMap<String, Rect>) -> Vec<Diagnostic> {
    let texts: Vec<&Node> = nodes.iter().copied().filter(|node| live_text(node)).collect();
    let mut out = Vec::new();

    // Use a uniquely largest short text as the title; compare measured frames and font sizes for labels.
    let heading = texts.iter().copied().find(|node| node.kind == NodeKind::Text && node.style.font_size > 0
        && node.text.chars().count() <= 60 && !is_body(&node.text)
        && texts.iter().all(|other| std::ptr::eq(*other, *node) || node.style.font_size > other.style.font_size));
    if let Some(heading) = heading {
        if title_fits_one_line(heading, frames) {
            out.push(warning(TITLE_LINE_BREAKS, &heading.id, "short title contains an explicit line break".into()));
        }
        if let Some(label) = texts.iter().copied().find(|node| extra_label(node, heading, frames)) {
            out.push(warning(TEXT_EXTRA_LABEL, &label.id, "short auxiliary label is close to the title".into()));
        }
    }

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
        out.push(warning(TEXT_VAGUE_WORDING, &texts[anchor].id, format!("{total} filler words on this page, or one in its title")).with_param("count", total));
    }

    if let Some(card) = card_row(nodes, frames) {
        out.push(warning(LAYOUT_REPEATED_CARDS, &card.id, format!("a row of identical shadowed cards starts at {}", card.id)));
    }

    let text_nodes: Vec<&Node> = texts.iter().copied().filter(|node| node.kind == NodeKind::Text).collect();
    if text_nodes.len() >= CENTRED_TEXTS && text_nodes.iter().all(|node| node.style.align == "center") && text_nodes.iter().any(|node| is_body(&node.text)) {
        out.push(
            warning(LAYOUT_ALL_CENTERED, &text_nodes[0].id, format!("all {} text nodes on this page are centred, body copy included", text_nodes.len()))
                .with_param("count", text_nodes.len()),
        );
    }

    let sizes: BTreeSet<i64> = text_nodes.iter().map(|node| node.style.font_size.saturating_add(EMU_PER_PT / 2) / EMU_PER_PT).collect();
    if sizes.len() >= TEXT_SIZES {
        out.push(warning(TEXT_SIZE_SPREAD, &text_nodes[0].id, format!("this page uses {} text sizes", sizes.len())).with_param("count", sizes.len()));
    }
    out
}

/// SLIDE_PLAIN_CLOSING: the last page has text, and all of it is a closing line.
fn closing_warning(pages: &[Page]) -> Option<Diagnostic> {
    let last = pages.last()?;
    let texts: Vec<&Node> = flatten(&last.nodes).into_iter().filter(|node| live_text(node)).collect();
    let first = texts.first()?;
    if texts.iter().all(|node| closing_line(&node.text)) {
        Some(warning(SLIDE_PLAIN_CLOSING, &first.id, "the last page holds only a closing line".to_owned()))
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

fn cream_fill(style: &Style) -> bool {
    let Some((r, g, b, alpha)) = rgba(&style.fill) else { return false };
    if u32::from(alpha) * style.opacity_milli.unwrap_or(1000).min(1000) < 255 * 950 { return false; }
    let Some((hue, _)) = hue_and_chroma(&style.fill) else { return false };
    let max = f64::from(r.max(g).max(b)) / 255.0;
    let min = f64::from(r.min(g).min(b)) / 255.0;
    let lightness = (max + min) / 2.0;
    let saturation = if lightness == 1.0 { 0.0 } else { (max - min) / (1.0 - (2.0 * lightness - 1.0).abs()) };
    (30.0..=60.0).contains(&hue) && (0.15..=0.6).contains(&saturation) && lightness >= 0.88
}

// shortcut: image pixels and fitted bounds are unavailable; treat a full-opacity image frame as potentially covering its background.
fn covers_background(background: &Node, other: &Node, page: &Page, frames: &BTreeMap<String, Rect>) -> bool {
    let painted = other.style.opacity_milli.unwrap_or(1000) >= 1000 && !other.style.corner_radius.is_some_and(|r| r > 0)
        && match other.kind {
            NodeKind::Image => other.asset.is_some(),
            NodeKind::Rectangle => other.style.gradient.as_ref().map_or_else(
                || rgba(&other.style.fill).is_some_and(|(_, _, _, alpha)| alpha == 255),
                |gradient| !gradient.stops.is_empty() && gradient.stops.iter().all(|s| rgba(&s.color).is_some_and(|(_, _, _, a)| a == 255)),
            ),
            _ => false,
        };
    if !painted { return false; }
    let (Some(a), Some(b)) = (frames.get(&background.id), frames.get(&other.id)) else { return false };
    if a.width <= 0 || a.height <= 0 || b.width <= 0 || b.height <= 0 || page.size.width <= 0 || page.size.height <= 0 { return false; }
    let interval = |start: i64, length: i64, limit: i64| (i128::from(start).max(0), (i128::from(start) + i128::from(length)).min(i128::from(limit)));
    let (ax1, ax2) = interval(a.x, a.width, page.size.width);
    let (ay1, ay2) = interval(a.y, a.height, page.size.height);
    let (bx1, bx2) = interval(b.x, b.width, page.size.width);
    let (by1, by2) = interval(b.y, b.height, page.size.height);
    let area = (ax2 - ax1).max(0) as f64 * (ay2 - ay1).max(0) as f64;
    let overlap = (ax2.min(bx2) - ax1.max(bx1)).max(0) as f64 * (ay2.min(by2) - ay1.max(by1)).max(0) as f64;
    area > 0.0 && overlap >= area * 0.5
}

// shortcut: glyph widths are unavailable; use generous character estimates until shaped text metrics are provided.
fn title_fits_one_line(title: &Node, frames: &BTreeMap<String, Rect>) -> bool {
    if !title.text.contains('\n') && !title.text.contains('\r') { return false; }
    let Some(frame) = frames.get(&title.id).filter(|frame| frame.width > 0) else { return false };
    let units: f64 = title.text.chars().map(|c| if c == '\t' { 5.0 } else if c.is_ascii() { 1.25 } else { 1.5 }).sum();
    let spacing = title.style.letter_spacing.unwrap_or(0).max(0) as f64 * title.text.chars().count().saturating_sub(1) as f64;
    units * title.style.font_size as f64 + spacing <= frame.width as f64
}

// At most four words/24 characters, <= 60% of the title size, with overlapping frames and a gap <= one title font size.
// Require a repeated title phrase or an uppercase/spaced generic category directly above the title.
fn extra_label(node: &Node, title: &Node, frames: &BTreeMap<String, Rect>) -> bool {
    if std::ptr::eq(node, title) || node.style.font_size <= 0
        || i128::from(node.style.font_size) * 5 > i128::from(title.style.font_size) * 3
        || node.text.chars().count() > 24 || node.text.split_whitespace().count() > 4
        || node.text.chars().any(|c| c.is_numeric()) { return false; }
    let (Some(a), Some(b)) = (frames.get(&node.id), frames.get(&title.id)) else { return false };
    if a.width <= 0 || a.height <= 0 || b.width <= 0 || b.height <= 0 { return false; }
    let (ax, ay, aw, ah) = (i128::from(a.x), i128::from(a.y), i128::from(a.width), i128::from(a.height));
    let (bx, by, bw, bh) = (i128::from(b.x), i128::from(b.y), i128::from(b.width), i128::from(b.height));
    let gap = i128::from(title.style.font_size);
    let above = ax < bx + bw && bx < ax + aw && ay + ah <= by && by - (ay + ah) <= gap;
    let beside = ay < by + bh && by < ay + ah && ((ax + aw <= bx && bx - (ax + aw) <= gap) || (bx + bw <= ax && ax - (bx + bw) <= gap));
    if !(above || beside) { return false; }
    let label_words = words(&node.text);
    let title_words = words(&title.text);
    let repeated = label_words.len() >= 2 && title_words.windows(label_words.len()).any(|window| window == label_words.as_slice());
    let category = matches!(node.text.trim().to_ascii_lowercase().as_str(), "overview" | "introduction" | "section" | "chapter" | "category" | "summary")
        && (node.text.chars().any(|c| c.is_ascii_uppercase()) && !node.text.chars().any(|c| c.is_ascii_lowercase())
            || node.style.letter_spacing.is_some_and(|spacing| spacing > 0));
    repeated || (above && category)
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
    covers_share(frame, page, LARGE_SHARE)
}

fn covers_share(frame: Rect, page: &Page, share: f64) -> bool {
    if frame.width <= 0 || frame.height <= 0 || page.size.width <= 0 || page.size.height <= 0 { return false; }
    let visible = |start: i64, length: i64, page_length: i64| {
        (i128::from(start) + i128::from(length)).min(i128::from(page_length)).saturating_sub(i128::from(start).max(0)).max(0) as f64
    };
    let page_area = page.size.width as f64 * page.size.height as f64;
    visible(frame.x, frame.width, page.size.width) * visible(frame.y, frame.height, page.size.height) >= share * page_area
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
        let found = design_quality_warnings(pages, frames);
        assert!(found.iter().all(|d| !d.blocking), "design-quality warnings never block: {found:?}");
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
    fn warm_background_requires_a_large_visible_opaque_fill() {
        let frames = BTreeMap::from([("bg".into(), rect(0, 0, 1920, 1080))]);
        let make = |kind, fill: &str| node("bg", kind, "", Style { fill: fill.into(), ..Style::default() });
        assert!(codes(&pages(vec![make(NodeKind::Rectangle, "#F2EADB")]), &frames).contains(&BACKGROUND_CREAM.to_owned()));
        for color in ["#FFFFFF", "#EEEEEE", "#EEF3F6", "#F2EADB00", "#F4DAB0"] {
            assert!(!codes(&pages(vec![make(NodeKind::Rectangle, color)]), &frames).contains(&BACKGROUND_CREAM.to_owned()), "{color}");
        }
        assert!(!codes(&pages(vec![make(NodeKind::Image, "#F2EADB")]), &frames).contains(&BACKGROUND_CREAM.to_owned()));
        let faded = node("bg", NodeKind::Rectangle, "", Style { fill: "#F2EADB".into(), opacity_milli: Some(500), ..Style::default() });
        assert!(!codes(&pages(vec![faded]), &frames).contains(&BACKGROUND_CREAM.to_owned()));
        let small = BTreeMap::from([("bg".into(), rect(0, 0, 400, 300))]);
        assert!(!codes(&pages(vec![make(NodeKind::Rectangle, "#F2EADB")]), &small).contains(&BACKGROUND_CREAM.to_owned()));
    }

    #[test]
    fn covering_layers_hide_a_warm_background_only_when_above_it() {
        let background = node("bg", NodeKind::Rectangle, "", Style { fill: "#F2EADB".into(), ..Style::default() });
        let mut image = node("photo", NodeKind::Image, "", Style::default());
        image.asset = Some(crate::AssetRef { sha256: "a".repeat(64), mime: "image/jpeg".into(), bytes: 1 });
        let mut frames = BTreeMap::from([("bg".into(), rect(0, 0, 1920, 1080)), ("photo".into(), rect(0, 0, 1920, 1080))]);
        assert!(!codes(&pages(vec![background.clone(), image.clone()]), &frames).contains(&BACKGROUND_CREAM.to_owned()));
        assert!(codes(&pages(vec![image.clone(), background.clone()]), &frames).contains(&BACKGROUND_CREAM.to_owned()));
        image.style.opacity_milli = Some(500);
        assert!(codes(&pages(vec![background.clone(), image.clone()]), &frames).contains(&BACKGROUND_CREAM.to_owned()));
        image.style.opacity_milli = None;
        frames.insert("photo".into(), rect(0, 0, 400, 300));
        assert!(codes(&pages(vec![background.clone(), image]), &frames).contains(&BACKGROUND_CREAM.to_owned()));
        let cover = node("cover", NodeKind::Rectangle, "", Style { fill: "#FFFFFF".into(), ..Style::default() });
        frames.insert("cover".into(), rect(0, 0, 1920, 1080));
        assert!(!codes(&pages(vec![background, cover]), &frames).contains(&BACKGROUND_CREAM.to_owned()));
    }

    #[test]
    fn extra_labels_require_a_short_small_text_close_to_the_title() {
        let frames = BTreeMap::from([("title".into(), rect(100, 120, 400, 60)), ("label".into(), rect(100, 90, 100, 20))]);
        let title = text("title", "Annual report", 30, "left");
        let label = text("label", "OVERVIEW", 12, "left");
        assert!(codes(&pages(vec![title.clone(), label.clone()]), &frames).contains(&TEXT_EXTRA_LABEL.to_owned()));
        let mut large = label.clone(); large.style.font_size = 20 * EMU_PER_PT;
        assert!(!codes(&pages(vec![title.clone(), large]), &frames).contains(&TEXT_EXTRA_LABEL.to_owned()));
        let distant = BTreeMap::from([("title".into(), rect(100, 120, 400, 60)), ("label".into(), rect(100, 0, 100, 20))]);
        assert!(!codes(&pages(vec![title.clone(), label.clone()]), &distant).contains(&TEXT_EXTRA_LABEL.to_owned()));
        let beside = BTreeMap::from([("title".into(), rect(100, 120, 400, 60)), ("label".into(), rect(0, 130, 90, 20))]);
        assert!(!codes(&pages(vec![title.clone(), label.clone()]), &beside).contains(&TEXT_EXTRA_LABEL.to_owned()));
        let repeated = text("label", "Annual report", 12, "left");
        assert!(codes(&pages(vec![title.clone(), repeated]), &beside).contains(&TEXT_EXTRA_LABEL.to_owned()));
        for copy in ["Seoul", "20 June 2026", "Overview"] {
            assert!(!codes(&pages(vec![title.clone(), text("label", copy, 12, "left")]), &frames).contains(&TEXT_EXTRA_LABEL.to_owned()), "{copy}");
        }
        let equal = text("other", "Another heading", 30, "left");
        assert!(!codes(&pages(vec![title.clone(), label.clone(), equal]), &frames).contains(&TEXT_EXTRA_LABEL.to_owned()));
        let mut badge = label; badge.kind = NodeKind::Rectangle; badge.text = "Annual report".into();
        assert!(codes(&pages(vec![title.clone(), badge.clone()]), &frames).contains(&TEXT_EXTRA_LABEL.to_owned()));
        badge.text = "New".into();
        assert!(!codes(&pages(vec![title, badge]), &frames).contains(&TEXT_EXTRA_LABEL.to_owned()));
    }

    #[test]
    fn short_title_explicit_break_warns_but_long_title_does_not() {
        let frames = BTreeMap::from([("title".into(), rect(0, 0, 1000, 100))]);
        assert!(codes(&pages(vec![text("title", "Annual\nreport", 30, "left")]), &frames).contains(&TITLE_LINE_BREAKS.to_owned()));
        let narrow = BTreeMap::from([("title".into(), rect(0, 0, 120, 100))]);
        assert!(!codes(&pages(vec![text("title", "Annual\nreport", 30, "left")]), &narrow).contains(&TITLE_LINE_BREAKS.to_owned()));
        assert!(!codes(&pages(vec![text("title", "Annual\nreport", 30, "left")]), &BTreeMap::new()).contains(&TITLE_LINE_BREAKS.to_owned()));
        let cjk = text("title", "한글\n제목", 30, "left");
        assert!(codes(&pages(vec![cjk.clone()]), &frames).contains(&TITLE_LINE_BREAKS.to_owned()));
        assert!(!codes(&pages(vec![cjk]), &narrow).contains(&TITLE_LINE_BREAKS.to_owned()));
        assert!(!codes(&pages(vec![text("title", "Annual report", 30, "left")]), &frames).contains(&TITLE_LINE_BREAKS.to_owned()));
        assert!(!codes(&pages(vec![text("body", "First line\nSecond line\nThird line", 30, "left"), text("other", "Copy", 12, "left")]), &frames).contains(&TITLE_LINE_BREAKS.to_owned()));
        let long = format!("{}\n{}", "A".repeat(35), "B".repeat(35));
        assert!(!codes(&pages(vec![text("title", &long, 30, "left")]), &frames).contains(&TITLE_LINE_BREAKS.to_owned()));
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
        assert_eq!(codes(&pages(vec![text("a", "🚀 Launch", 12, "left")]), &frames), [ICON_EMOJI]);
        assert_eq!(codes(&pages(vec![text("a", "🚀", 12, "left")]), &frames), [ICON_EMOJI]);
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
            assert_eq!(codes(&pages(vec![text("a", copy, 12, "left")]), &frames), [TEXT_PLACEHOLDER], "{copy}");
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
            assert!(codes(&pages(vec![shape.clone()]), &BTreeMap::new()).contains(&TEXT_PLACEHOLDER.to_owned()));
            shape.text = "🚀 Launch".into();
            assert!(codes(&pages(vec![shape.clone()]), &BTreeMap::new()).contains(&ICON_EMOJI.to_owned()));
            shape.text = "혁신적인 서비스".into();
            assert!(codes(&pages(vec![shape.clone(), text("body", "내용", 12, "left")]), &BTreeMap::new()).contains(&TEXT_VAGUE_WORDING.to_owned()));
            shape.text = "매출 10% 증가".into();
            let deck = pages(vec![text("end", "Thank you", 40, "left"), shape.clone()]);
            assert!(!deck_quality_warnings(&deck).iter().any(|d| d.code == SLIDE_PLAIN_CLOSING));
            shape.text = "홍길동".into();
        }
        let body = "First line\nSecond line\nThird line";
        let mut centered: Vec<Node> = (0..4).map(|i| text(&format!("t{i}"), if i == 0 { body } else { "Heading" }, 12, "center")).collect();
        for node in &mut centered { node.kind = NodeKind::Rectangle; }
        assert!(!codes(&pages(centered), &BTreeMap::new()).contains(&LAYOUT_ALL_CENTERED.to_owned()));
        let scaled: Vec<Node> = (10..17).map(|size| { let mut n = text(&format!("t{size}"), "Copy", size, "left"); n.kind = NodeKind::Ellipse; n }).collect();
        assert!(!codes(&pages(scaled), &BTreeMap::new()).contains(&TEXT_SIZE_SPREAD.to_owned()));
        shape.kind = NodeKind::Group;
        assert!(!codes(&pages(vec![shape]), &BTreeMap::new()).contains(&TEXT_PLACEHOLDER.to_owned()));
    }

    #[test]
    fn hidden_ancestors_exclude_descendants_from_page_and_closing_warnings() {
        let mut group = node("group", NodeKind::Group, "", Style::default());
        let mut inner = node("inner", NodeKind::Group, "", Style::default());
        inner.children = vec![text("placeholder", "홍길동", 12, "left"), text("closing", "감사합니다", 12, "left")];
        group.children.push(inner);
        group.ext.hidden = Some(true);
        assert!(deck_quality_warnings(&pages(vec![group.clone()])).is_empty());
        group.ext.hidden = Some(false);
        let found = deck_quality_warnings(&pages(vec![group.clone()]));
        assert!(found.iter().any(|d| d.code == TEXT_PLACEHOLDER && d.node_id.as_deref() == Some("placeholder")));
        group.children[0].children.remove(0);
        assert!(deck_quality_warnings(&pages(vec![group])).iter().any(|d| d.code == SLIDE_PLAIN_CLOSING));
    }

    #[test]
    fn group_opacity_does_not_change_a_childs_own_opacity() {
        let mut group = node("group", NodeKind::Group, "", Style { opacity_milli: Some(0), ..Style::default() });
        group.children.push(shadowed("child", 32, "#000000"));
        let found = design_quality_warnings(&pages(vec![group.clone()]), &BTreeMap::new());
        assert!(found.iter().any(|d| d.code == SHADOW_HEAVY && d.node_id.as_deref() == Some("child")));
        group.children[0].style.opacity_milli = Some(0);
        assert!(!design_quality_warnings(&pages(vec![group]), &BTreeMap::new()).iter().any(|d| d.code == SHADOW_HEAVY));
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
        assert_eq!(codes(&pages(vec![text("title", "Unlock growth", 30, "left"), text("body", "Copy", 12, "left")]), &frames), [TEXT_VAGUE_WORDING]);
        assert_eq!(codes(&pages(vec![text("first", "leverage", 12, "left"), text("second", "empower", 12, "left")]), &frames), [TEXT_VAGUE_WORDING]);
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
        assert!(!codes(&pages(nodes.clone()), &BTreeMap::new()).contains(&LAYOUT_ALL_CENTERED.to_owned()));
        for i in 0..3 { nodes.push(text(&format!("heading{i}"), "Heading", 12, "center")); }
        assert!(codes(&pages(nodes), &BTreeMap::new()).contains(&LAYOUT_ALL_CENTERED.to_owned()));
    }

    #[test]
    fn button_sizes_do_not_count_toward_type_scale() {
        let mut nodes: Vec<Node> = (10..16).map(|size| text(&format!("t{size}"), "Copy", size, "left")).collect();
        let mut button = text("button", "Open", 32, "center");
        button.kind = NodeKind::Rectangle;
        nodes.push(button);
        assert!(!codes(&pages(nodes.clone()), &BTreeMap::new()).contains(&TEXT_SIZE_SPREAD.to_owned()));
        nodes.push(text("seventh", "Copy", 32, "left"));
        assert!(codes(&pages(nodes), &BTreeMap::new()).contains(&TEXT_SIZE_SPREAD.to_owned()));
    }

    #[test]
    fn type_scale_rounding_handles_maximum_font_size() {
        let mut nodes: Vec<Node> = (10..16).map(|size| text(&format!("t{size}"), "Copy", size, "left")).collect();
        assert!(!codes(&pages(nodes.clone()), &BTreeMap::new()).contains(&TEXT_SIZE_SPREAD.to_owned()));
        let mut extreme = text("extreme", "Copy", 12, "left");
        extreme.style.font_size = i64::MAX;
        nodes.push(extreme);
        assert!(codes(&pages(nodes), &BTreeMap::new()).contains(&TEXT_SIZE_SPREAD.to_owned()));
    }

    #[test]
    fn filler_words_warn_from_two_on_a_page_or_from_one_in_the_title() {
        let frames = BTreeMap::new();
        let two_in_body = pages(vec![text("title", "Quarterly results", 28, "left"), text("a", "We leverage data and empower teams", 12, "left")]);
        assert_eq!(codes(&two_in_body, &frames), [TEXT_VAGUE_WORDING]);
        let one_in_body = pages(vec![text("title", "Quarterly results", 28, "left"), text("a", "We leverage data", 12, "left")]);
        assert!(codes(&one_in_body, &frames).is_empty(), "one filler word in the body is left alone");
        let one_in_title = pages(vec![text("title", "Unlock growth", 28, "left"), text("a", "Plain body copy", 12, "left")]);
        let found = design_quality_warnings(&one_in_title, &frames);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].node_id.as_deref(), Some("title"), "the title takes the warning");
        assert_eq!(codes(&pages(vec![text("a", "Our cutting edge product", 12, "left"), text("b", "혁신적인 원활한 서비스", 12, "left")]), &frames), [TEXT_VAGUE_WORDING]);
    }

    #[test]
    fn a_large_purple_or_indigo_gradient_is_the_default_look() {
        let mut frames = BTreeMap::new();
        let big = node("bg", NodeKind::Rectangle, "", purple("#7C3AED", "#4338CA"));
        frames.insert("bg".to_string(), rect(0, 0, 1920, 1080));
        assert_eq!(codes(&pages(vec![big]), &frames), [FILL_STOCK_GRADIENT]);

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
            assert_eq!(codes(&pages(vec![node("bg", NodeKind::Rectangle, "", style)]), &frames), [FILL_STOCK_GRADIENT]);
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
        assert_eq!(deck_quality_warnings(&deck).iter().filter(|d| d.code == FILL_STOCK_GRADIENT).count(), 1);
        deck.reverse();
        assert_eq!(deck_quality_warnings(&deck).iter().filter(|d| d.code == FILL_STOCK_GRADIENT).count(), 1);

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
        assert_eq!(deck_quality_warnings(&deck).iter().filter(|d| d.code == LAYOUT_REPEATED_CARDS).count(), 1);
        deck.reverse();
        assert_eq!(deck_quality_warnings(&deck).iter().filter(|d| d.code == LAYOUT_REPEATED_CARDS).count(), 1);
    }

    #[test]
    fn only_drawable_shapes_get_gradient_and_shadow_warnings() {
        use crate::{FillRule, PathCommand, PathSpec};
        let frames = BTreeMap::from([("item".to_owned(), rect(0, 0, 1920, 1080))]);
        let mut style = purple("#7C3AED", "#4338CA");
        style.shadows.push(Shadow { x: 0, y: 0, blur: px(40), spread: 0, color: "#000000".into() });
        for kind in [NodeKind::Rectangle, NodeKind::Ellipse] {
            let found = codes(&pages(vec![node("item", kind, "", style.clone())]), &frames);
            assert!(found.contains(&FILL_STOCK_GRADIENT.to_owned()));
            assert!(found.contains(&SHADOW_HEAVY.to_owned()));
        }
        for kind in [NodeKind::Group, NodeKind::Image, NodeKind::Video, NodeKind::Line, NodeKind::Text] {
            let found = codes(&pages(vec![node("item", kind, "", style.clone())]), &frames);
            assert!(!found.contains(&FILL_STOCK_GRADIENT.to_owned()), "{kind:?}");
            assert!(!found.contains(&SHADOW_HEAVY.to_owned()), "{kind:?}");
        }
        let mut path = node("item", NodeKind::Path, "", style);
        assert!(codes(&pages(vec![path.clone()]), &frames).is_empty());
        path.ext.path = Some(PathSpec { view_box: rect(0, 0, 1, 1), fill_rule: FillRule::Nonzero, commands: vec![PathCommand::MoveTo { x: 0, y: 0 }, PathCommand::LineTo { x: px(1), y: 0 }, PathCommand::Close] });
        let found = codes(&pages(vec![path]), &frames);
        assert!(!found.contains(&FILL_STOCK_GRADIENT.to_owned()));
        assert!(!found.contains(&SHADOW_HEAVY.to_owned()));
    }

    #[test]
    fn a_heavy_shadow_is_wide_and_strong() {
        let frames = BTreeMap::new();
        assert_eq!(codes(&pages(vec![shadowed("a", 32, "#000000")]), &frames), [SHADOW_HEAVY]);
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
        let found = design_quality_warnings(&pages(cards.clone()), &frames);
        assert_eq!(found.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(), [LAYOUT_REPEATED_CARDS]);
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
        assert_eq!(codes(&pages(cards), &frames), [LAYOUT_REPEATED_CARDS]);
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
        assert_eq!(codes(&pages(cards), &frames), [LAYOUT_REPEATED_CARDS]);
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
        assert!(!codes(&pages(cards.clone()), &frames).contains(&LAYOUT_REPEATED_CARDS.to_owned()));
        for card in &mut cards { card.style.opacity_milli = Some(20); }
        assert!(codes(&pages(cards.clone()), &frames).contains(&LAYOUT_REPEATED_CARDS.to_owned()));
        for card in &mut cards { card.style.opacity_milli = Some(1000); card.style.shadows[0].color = "#00000005".into(); }
        assert!(!codes(&pages(cards), &frames).contains(&LAYOUT_REPEATED_CARDS.to_owned()));
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
        assert_eq!(codes(&pages(cards.clone()), &frames), [LAYOUT_REPEATED_CARDS]);
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
        assert_eq!(codes(&pages(std::mem::take(&mut cards)), &frames), [LAYOUT_REPEATED_CARDS]);
    }

    #[test]
    fn four_centred_text_nodes_with_a_body_paragraph_are_the_all_centred_pattern() {
        let frames = BTreeMap::new();
        let body = "Our team builds tools for designers who need to ship pages every week without losing the details that make a layout their own, and this paragraph is long enough to count as body copy.";
        let headings: Vec<Node> = (0..3).map(|i| text(&format!("h{i}"), "Heading", 12, "center")).collect();
        assert!(codes(&pages(headings.clone()), &frames).is_empty(), "short centred headings alone are not the pattern");
        let mut with_body = headings.clone();
        with_body.push(text("body", body, 12, "center"));
        assert_eq!(codes(&pages(with_body.clone()), &frames), [LAYOUT_ALL_CENTERED]);
        let mut one_left = with_body.clone();
        one_left[0].style.align = "left".into();
        assert!(codes(&pages(one_left), &frames).is_empty(), "one node that is not centred breaks the pattern");
        let three_lines = text("lines", "First line\nSecond line\nThird line", 12, "center");
        let mut with_lines = headings.clone();
        with_lines.push(three_lines);
        assert_eq!(codes(&pages(with_lines), &frames), [LAYOUT_ALL_CENTERED], "three lines is a body paragraph");
        let mut with_empty = with_body;
        with_empty.push(text("empty", "  ", 12, "left"));
        assert_eq!(codes(&pages(with_empty), &frames), [LAYOUT_ALL_CENTERED], "an empty text box does not count");
    }

    #[test]
    fn seven_text_sizes_on_a_page_are_too_many_and_sizes_round_to_whole_points() {
        let frames = BTreeMap::new();
        let sizes = [10, 12, 14, 16, 18, 24, 32];
        let seven: Vec<Node> = sizes.iter().enumerate().map(|(i, size)| text(&format!("t{i}"), "Copy", *size, "left")).collect();
        assert_eq!(codes(&pages(seven.clone()), &frames), [TEXT_SIZE_SPREAD]);
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
        let found = deck_quality_warnings(&deck);
        let closing = found.iter().find(|d| d.code == SLIDE_PLAIN_CLOSING).expect("the closing warning");
        assert_eq!(closing.node_id.as_deref(), Some("end"));
        assert!(!closing.blocking);

        for line in ["감사합니다", "Q&A", "질문", "thank you"] {
            deck[1].nodes = vec![text("end", line, 40, "center")];
            assert!(deck_quality_warnings(&deck).iter().any(|d| d.code == SLIDE_PLAIN_CLOSING), "{line}");
        }
        deck[1].nodes = vec![text("end", "Thank you, Ana Kim", 40, "center")];
        assert!(!deck_quality_warnings(&deck).iter().any(|d| d.code == SLIDE_PLAIN_CLOSING), "a name after the line is content");
        deck[1].nodes = vec![text("end", "Thank you", 40, "center"), text("more", "Ask us about the Q3 plan", 20, "left")];
        assert!(!deck_quality_warnings(&deck).iter().any(|d| d.code == SLIDE_PLAIN_CLOSING), "other text on the last page is content");
        deck[1].nodes = vec![node("picture", NodeKind::Image, "", Style::default())];
        assert!(!deck_quality_warnings(&deck).iter().any(|d| d.code == SLIDE_PLAIN_CLOSING), "a last page with no text is not a closing line");
        deck[1].nodes = vec![text("end", "Thank you", 40, "center")];
        deck[0].nodes = vec![text("only", "Thank you", 40, "center")];
        assert_eq!(deck_quality_warnings(&deck).iter().filter(|d| d.code == SLIDE_PLAIN_CLOSING).count(), 1, "only the last page is judged");
    }

    #[test]
    fn a_deck_reads_the_node_bounds_for_its_card_rows() {
        let mut cards: Vec<Node> = (0..3).map(|i| shadowed(&format!("card{i}"), 16, "#0F172A1A")).collect();
        for (i, card) in cards.iter_mut().enumerate() {
            card.bounds = rect(100 + i as i64 * 250, 300, 200, 120);
            card.style.corner_radius = Some(px(16));
        }
        let found = deck_quality_warnings(&pages(cards));
        assert_eq!(found.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(), [LAYOUT_REPEATED_CARDS]);
    }
}
