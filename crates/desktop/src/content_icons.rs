//! The content icons a user can put into a canvas or slide (`design/icons/content`, 43 one-path line icons on a 24
//! grid). The picker finds them by their Korean or English keywords (`index.json`); the inserted node is a Path with
//! the icon's own path data and an `IconRef`, the same form the canvas templates use.

use std::sync::OnceLock;

use designllm_contracts::{IconRef, PathSpec, Rect};
use serde::Deserialize;

/// Name and SVG source of every content icon, in the order of `index.json`.
pub const CONTENT_ICONS: &[(&str, &str)] = &[
    ("home", include_str!("../../../design/icons/content/home.svg")),
    ("search", include_str!("../../../design/icons/content/search.svg")),
    ("user", include_str!("../../../design/icons/content/user.svg")),
    ("settings", include_str!("../../../design/icons/content/settings.svg")),
    ("bell", include_str!("../../../design/icons/content/bell.svg")),
    ("heart", include_str!("../../../design/icons/content/heart.svg")),
    ("star", include_str!("../../../design/icons/content/star.svg")),
    ("cart", include_str!("../../../design/icons/content/cart.svg")),
    ("mail", include_str!("../../../design/icons/content/mail.svg")),
    ("phone", include_str!("../../../design/icons/content/phone.svg")),
    ("calendar", include_str!("../../../design/icons/content/calendar.svg")),
    ("clock", include_str!("../../../design/icons/content/clock.svg")),
    ("map-pin", include_str!("../../../design/icons/content/map-pin.svg")),
    ("camera", include_str!("../../../design/icons/content/camera.svg")),
    ("chart", include_str!("../../../design/icons/content/chart.svg")),
    ("file", include_str!("../../../design/icons/content/file.svg")),
    ("folder", include_str!("../../../design/icons/content/folder.svg")),
    ("download", include_str!("../../../design/icons/content/download.svg")),
    ("upload", include_str!("../../../design/icons/content/upload.svg")),
    ("share", include_str!("../../../design/icons/content/share.svg")),
    ("check", include_str!("../../../design/icons/content/check.svg")),
    ("x", include_str!("../../../design/icons/content/x.svg")),
    ("plus", include_str!("../../../design/icons/content/plus.svg")),
    ("minus", include_str!("../../../design/icons/content/minus.svg")),
    ("menu", include_str!("../../../design/icons/content/menu.svg")),
    ("arrow-up", include_str!("../../../design/icons/content/arrow-up.svg")),
    ("arrow-down", include_str!("../../../design/icons/content/arrow-down.svg")),
    ("arrow-left", include_str!("../../../design/icons/content/arrow-left.svg")),
    ("arrow-right", include_str!("../../../design/icons/content/arrow-right.svg")),
    ("chevron-up", include_str!("../../../design/icons/content/chevron-up.svg")),
    ("chevron-down", include_str!("../../../design/icons/content/chevron-down.svg")),
    ("chevron-left", include_str!("../../../design/icons/content/chevron-left.svg")),
    ("chevron-right", include_str!("../../../design/icons/content/chevron-right.svg")),
    ("lock", include_str!("../../../design/icons/content/lock.svg")),
    ("eye", include_str!("../../../design/icons/content/eye.svg")),
    ("edit", include_str!("../../../design/icons/content/edit.svg")),
    ("trash", include_str!("../../../design/icons/content/trash.svg")),
    ("filter", include_str!("../../../design/icons/content/filter.svg")),
    ("sort", include_str!("../../../design/icons/content/sort.svg")),
    ("refresh", include_str!("../../../design/icons/content/refresh.svg")),
    ("info", include_str!("../../../design/icons/content/info.svg")),
    ("warning", include_str!("../../../design/icons/content/warning.svg")),
    ("play", include_str!("../../../design/icons/content/play.svg")),
];

const INDEX: &str = include_str!("../../../design/icons/content/index.json");

#[derive(Deserialize)]
struct Index {
    version: u32,
    icons: Vec<Entry>,
}

#[derive(Clone, Deserialize)]
pub struct Entry {
    pub name: String,
    pub purpose: String,
    pub keywords: Vec<String>,
    #[serde(default)]
    pub keywords_en: Vec<String>,
}

fn index() -> &'static (u32, Vec<Entry>) {
    static INDEX_CELL: OnceLock<(u32, Vec<Entry>)> = OnceLock::new();
    INDEX_CELL.get_or_init(|| {
        let parsed: Index = serde_json::from_str(INDEX).expect("design/icons/content/index.json is valid");
        (parsed.version, parsed.icons)
    })
}

/// The SVG of a content icon.
pub fn source(name: &str) -> Option<&'static str> {
    CONTENT_ICONS.iter().find(|(icon, _)| *icon == name).map(|(_, svg)| *svg)
}

/// The icons whose name, purpose or keywords contain `query` (case-insensitive, spaces ignored), in index order.
/// An empty query lists every icon.
pub fn search(query: &str) -> Vec<&'static Entry> {
    let wanted: String = query.split_whitespace().collect::<String>().to_lowercase();
    index()
        .1
        .iter()
        .filter(|entry| {
            wanted.is_empty()
                || entry.name.contains(&wanted)
                || entry.purpose.replace(' ', "").to_lowercase().contains(&wanted)
                || entry.keywords.iter().chain(&entry.keywords_en).any(|keyword| keyword.replace(' ', "").to_lowercase().contains(&wanted))
        })
        .collect()
}

/// The path of a content icon on its 24 grid and the reference the node keeps.
pub fn path_of(name: &str) -> Option<(PathSpec, IconRef)> {
    let svg = source(name)?;
    let start = svg.find(" d=\"")? + 4;
    let end = start + svg[start..].find('"')?;
    let path = PathSpec::from_svg(&svg[start..end], Rect { x: 0, y: 0, width: 24_000, height: 24_000 }).ok()?;
    Some((path, IconRef { set: "content".into(), name: name.into(), version: index().0.to_string() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_indexed_icon_has_a_source_and_a_path() {
        assert_eq!(index().1.len(), CONTENT_ICONS.len());
        for entry in &index().1 {
            assert!(path_of(&entry.name).is_some(), "{} has a usable path", entry.name);
        }
    }

    #[test]
    fn korean_and_english_keywords_find_icons() {
        let names = |query: &str| search(query).iter().map(|entry| entry.name.clone()).collect::<Vec<_>>();
        assert!(names("집").contains(&"home".to_owned()));
        assert!(names("돋보기").contains(&"search".to_owned()));
        assert!(names("Cart").contains(&"cart".to_owned()));
        assert_eq!(search("").len(), CONTENT_ICONS.len());
        assert!(search("없는말").is_empty());
    }
}
