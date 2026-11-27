//! The window's UI language (한국어 / English).
//!
//! The Korean text in the code stays the source and the key: [`tr`] gives back the text as it is in Korean mode (so the
//! Korean window is byte for byte what it was), and in English mode the English entry of the tables under `i18n/`. A text made
//! with `format!` is found through its template: the table keeps the Korean format string (`"{name}을 열었습니다"`) with
//! its English one (`"Opened {name}"`), and a shown text that fits the Korean template is rebuilt from the English one with
//! the same parts. A text with no entry is shown as it is.
//!
//! Only what is shown is translated. Command ids, recorded control labels (`ui_record`), the MCP command list and the texts
//! of the engines stay as they are.
//!
//! Adding a string: write the Korean text in the code as before, wrap its display with `tr(..)` (house widgets already do),
//! and add `("한국어", "English")` to the table of its area. A template keeps the Korean placeholders; the English one names
//! them the same way (`{}` in order, `{0}`/`{1}` by position, `{name}` by name).

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::OnceLock,
};

mod board;
mod commands;
mod common;
mod dispatch;
mod editor;
mod engine;
mod figure;
mod home;
mod kind_commands;
mod messages;
mod properties;
mod sidebar;

/// The UI languages, in the order the selector shows them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    Korean,
    English,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::Korean, Language::English];

    /// The value kept in the settings file.
    pub fn code(self) -> &'static str {
        match self {
            Language::Korean => "ko",
            Language::English => "en",
        }
    }

    /// A settings value; anything else (an old file, an empty value) is Korean.
    pub fn from_code(code: &str) -> Language {
        match code {
            "en" => Language::English,
            _ => Language::Korean,
        }
    }

    /// The language's name in itself, as the selector shows it.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::Korean => "한국어",
            Language::English => "English",
        }
    }
}

thread_local! {
    /// The language of the window drawn on this thread. The window draws on one thread; a test bench sets its own.
    static LANGUAGE: Cell<Language> = const { Cell::new(Language::Korean) };
    /// Shown texts already looked up against the templates (English mode only).
    static MEMO: RefCell<HashMap<String, Option<String>>> = RefCell::new(HashMap::new());
}

/// The language this thread draws in.
pub fn language() -> Language {
    LANGUAGE.with(Cell::get)
}

/// Sets the language this thread draws in, from the next text on.
pub fn set_language(language: Language) {
    LANGUAGE.with(|cell| cell.set(language));
}

/// `text` in the window's language: itself in Korean mode, else its English entry (exact, or through a template), else
/// itself.
pub fn tr(text: &str) -> Cow<'_, str> {
    if language() == Language::Korean || !has_hangul(text) {
        return Cow::Borrowed(text);
    }
    if let Some(english) = tables().exact.get(text) {
        return Cow::Borrowed(english);
    }
    let found = MEMO.with(|memo| memo.borrow().get(text).cloned());
    let found = match found {
        Some(found) => found,
        None => {
            let made = from_template(text, 0);
            MEMO.with(|memo| {
                let mut memo = memo.borrow_mut();
                if memo.len() > 4096 {
                    memo.clear();
                }
                memo.insert(text.to_owned(), made.clone());
            });
            made
        }
    };
    found.map_or(Cow::Borrowed(text), Cow::Owned)
}

/// `tr` for a text that must stay borrowed (a label list): the exact entry only, no template.
pub fn tr_exact(text: &str) -> &str {
    if language() == Language::Korean || !has_hangul(text) {
        return text;
    }
    tables().exact.get(text).copied().unwrap_or(text)
}

/// `tr` of every label of a list (segmented controls, menus).
pub fn tr_all<'a>(labels: &[&'a str]) -> Vec<&'a str> {
    labels.iter().map(|label| tr_exact(label)).collect()
}

/// A page number as the window writes it: "3쪽" in Korean, "Page 3" in English. A template of one Korean letter would
/// also match people's own words, so this short label is built here instead of through `tr`.
pub fn page_label(number: impl std::fmt::Display) -> String {
    match language() {
        Language::English => format!("Page {number}"),
        _ => format!("{number}쪽"),
    }
}

/// One Korean word followed by a value, such as "표 1" / "Table 1" or "색 accent #C77A2B" / "Colors accent #C77A2B" (built
/// here for the reason of `page_label`: `tr` keeps no template with fewer than two Korean letters).
pub fn word_label(korean: &str, english: &str, value: impl std::fmt::Display) -> String {
    match language() {
        Language::English => format!("{english} {value}"),
        _ => format!("{korean} {value}"),
    }
}

/// A row count as the window writes it: "3행" in Korean, "3 rows" in English (built here for the reason of `page_label`).
pub fn rows_label(count: usize) -> String {
    match language() {
        Language::English if count == 1 => "1 row".to_owned(),
        Language::English => format!("{count} rows"),
        _ => format!("{count}행"),
    }
}

/// Whether `text` holds a Hangul syllable or letter.
pub fn has_hangul(text: &str) -> bool {
    text.chars().any(is_hangul)
}

fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{3131}'..='\u{318E}' | '\u{1100}'..='\u{11FF}')
}

// ---------------------------------------------------------------------------------------------------------------------
// Tables

struct Tables {
    exact: HashMap<&'static str, &'static str>,
    templates: Vec<Template>,
}

/// One part of a template: literal text or a placeholder (its name: `""` for `{}`, `"0"` for `{0}`, `"name"`).
#[derive(Debug, Clone, PartialEq)]
enum Part {
    Text(String),
    Hole(String),
}

struct Template {
    korean: Vec<Part>,
    english: Vec<Part>,
    /// The longest literal part: a shown text that lacks it cannot fit.
    anchor: String,
    /// The length of all literal parts: of two templates that fit, the longer one is the closer.
    literal: usize,
}

/// Every table of every area.
fn areas() -> [&'static [(&'static str, &'static str)]; 12] {
    [
        common::ENTRIES,
        home::ENTRIES,
        sidebar::ENTRIES,
        editor::ENTRIES,
        board::ENTRIES,
        figure::ENTRIES,
        properties::ENTRIES,
        commands::ENTRIES,
        kind_commands::ENTRIES,
        dispatch::ENTRIES,
        messages::ENTRIES,
        engine::ENTRIES,
    ]
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut exact = HashMap::new();
        let mut templates = Vec::new();
        for (korean, english) in areas().into_iter().flatten().copied() {
            if english.is_empty() {
                continue;
            }
            let parts = parse(korean);
            let holes = parts.iter().any(|part| matches!(part, Part::Hole(_)));
            if !holes {
                exact.entry(korean).or_insert(english);
                continue;
            }
            let hangul: usize = parts.iter().map(|part| match part {
                Part::Text(text) => text.chars().filter(|c| is_hangul(*c)).count(),
                Part::Hole(_) => 0,
            }).sum();
            // A template with too little Korean of its own would fit text that is not the window's (a name, a title).
            if hangul < 2 {
                continue;
            }
            let anchor = parts.iter().filter_map(|part| match part {
                Part::Text(text) => Some(text.clone()),
                Part::Hole(_) => None,
            }).max_by_key(|text| text.chars().count()).unwrap_or_default();
            let literal = parts.iter().map(|part| match part {
                Part::Text(text) => text.chars().count(),
                Part::Hole(_) => 0,
            }).sum();
            templates.push(Template { korean: parts, english: parse(english), anchor, literal });
        }
        Tables { exact, templates }
    })
}

/// Splits a format string into literal parts and placeholders (`{{` and `}}` are literal braces).
fn parse(template: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                text.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                text.push('}');
            }
            '{' => {
                let mut inside = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    inside.push(c);
                }
                if !closed {
                    text.push('{');
                    text.push_str(&inside);
                    continue;
                }
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                let name = inside.split(':').next().unwrap_or_default().trim().to_owned();
                parts.push(Part::Hole(name));
            }
            c => text.push(c),
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    parts
}

/// The English of `text` through the closest template it fits, with its parts translated too.
fn from_template(text: &str, depth: usize) -> Option<String> {
    let tables = tables();
    let mut best: Option<(&Template, Vec<&str>)> = None;
    for template in &tables.templates {
        if !text.contains(template.anchor.as_str()) {
            continue;
        }
        if best.as_ref().is_some_and(|(closest, _)| closest.literal >= template.literal) {
            continue;
        }
        let mut captures = Vec::new();
        if fits(&template.korean, text, &mut captures) {
            best = Some((template, captures));
        }
    }
    match best {
        Some((template, captures)) => Some(fill(template, &captures, depth)),
        None => joined(text, depth),
    }
}

/// The separators the window joins shown parts with (`모델 선택 · 높음`, `열기  Ctrl+O`): a text with no entry of its own is
/// translated part by part.
const SEPARATORS: [&str; 3] = [" · ", "  ", " / "];

fn joined(text: &str, depth: usize) -> Option<String> {
    let separator = SEPARATORS.iter().find(|separator| text.contains(**separator))?;
    let mut changed = false;
    let parts: Vec<Cow<'_, str>> = text.split(separator).map(|part| {
        let shown = translate_part(part, depth);
        changed |= shown != part;
        shown
    }).collect();
    changed.then(|| parts.join(separator))
}

/// Whether `text` fits `parts` whole; the placeholders' texts are pushed to `captures` in order.
fn fits<'t>(parts: &[Part], text: &'t str, captures: &mut Vec<&'t str>) -> bool {
    match parts.split_first() {
        None => text.is_empty(),
        Some((Part::Text(literal), rest)) => {
            text.strip_prefix(literal.as_str()).is_some_and(|after| fits(rest, after, captures))
        }
        Some((Part::Hole(_), rest)) => {
            let mut ends: Vec<usize> = match rest.first() {
                None => vec![text.len()],
                Some(Part::Text(literal)) => text.match_indices(literal.as_str()).map(|(at, _)| at).collect(),
                Some(Part::Hole(_)) => text.char_indices().map(|(at, _)| at).skip(1).chain([text.len()]).collect(),
            };
            ends.dedup();
            for end in ends {
                captures.push(&text[..end]);
                if fits(rest, &text[end..], captures) {
                    return true;
                }
                captures.pop();
            }
            false
        }
    }
}

/// The placeholders that hold the person's own text (a title, a name, a file): what fills them is never translated, so a
/// document called `저장` stays `저장` inside an English sentence.
const VERBATIM_HOLES: [&str; 12] = ["title", "name", "text", "words", "heading", "note", "file_name", "file", "path", "prompt", "parent", "section"];

/// Opening quotes: a placeholder right after one quotes the person's text (`‘{}’`, `“{}”`).
const QUOTES: [char; 4] = ['‘', '“', '\'', '"'];

/// Whether the placeholder at `index` of the Korean template holds the person's text: it is named so, or it is quoted.
fn verbatim_hole(korean: &[Part], index: usize) -> bool {
    let mut seen = 0;
    for (position, part) in korean.iter().enumerate() {
        if let Part::Hole(name) = part {
            if seen == index {
                let quoted = position.checked_sub(1).and_then(|before| match &korean[before] {
                    Part::Text(text) => text.chars().last(),
                    Part::Hole(_) => None,
                }).is_some_and(|last| QUOTES.contains(&last));
                return quoted || VERBATIM_HOLES.contains(&name.as_str());
            }
            seen += 1;
        }
    }
    false
}

/// The English template with the captured texts in its placeholders. A capture is translated when it is the window's own
/// word (a kind, a reason, a nested message), never when the placeholder holds the person's text (`verbatim_hole`).
fn fill(template: &Template, captures: &[&str], depth: usize) -> String {
    let names: Vec<&str> = template.korean.iter().filter_map(|part| match part {
        Part::Hole(name) => Some(name.as_str()),
        Part::Text(_) => None,
    }).collect();
    let mut out = String::new();
    let mut next = 0;
    for part in &template.english {
        match part {
            Part::Text(text) => out.push_str(text),
            Part::Hole(name) => {
                let index = if name.is_empty() {
                    let index = next;
                    next += 1;
                    Some(index)
                } else if let Ok(index) = name.parse::<usize>() {
                    Some(index)
                } else {
                    names.iter().position(|korean| korean == name)
                };
                match index.and_then(|index| captures.get(index).map(|capture| (index, capture))) {
                    Some((index, capture)) if verbatim_hole(&template.korean, index) => out.push_str(capture),
                    Some((_, capture)) => out.push_str(&translate_part(capture, depth)),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
            }
        }
    }
    out
}

/// A placeholder's text: its own entry when it has one (a tool or kind name inside a sentence).
fn translate_part(text: &str, depth: usize) -> Cow<'_, str> {
    if !has_hangul(text) {
        return Cow::Borrowed(text);
    }
    if let Some(english) = tables().exact.get(text) {
        return Cow::Borrowed(english);
    }
    if depth < 2 {
        if let Some(made) = from_template(text, depth + 1) {
            return Cow::Owned(made);
        }
    }
    Cow::Borrowed(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct English;
    impl English {
        fn on() -> Self {
            set_language(Language::English);
            English
        }
    }
    impl Drop for English {
        fn drop(&mut self) {
            set_language(Language::Korean);
        }
    }

    #[test]
    fn korean_mode_gives_the_text_back_unchanged() {
        set_language(Language::Korean);
        for (korean, _) in areas().into_iter().flatten() {
            assert_eq!(tr(korean), *korean);
            assert!(matches!(tr(korean), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn english_mode_finds_exact_entries_and_templates() {
        let _english = English::on();
        assert_eq!(tr("저장"), "Save");
        assert_eq!(tr("no korean here"), "no korean here");
        // A text with no entry is shown as it is.
        assert_eq!(tr("표에 없는 사용자 글"), "표에 없는 사용자 글");
        // Parts joined for display are translated one by one; a part with no entry stays as it is.
        assert_eq!(tr("저장 · 표에 없는 글"), "Save · 표에 없는 글");
        assert_eq!(tr("Claude · 저장"), "Claude · Save");
    }

    #[test]
    fn templates_fit_by_position_and_by_name() {
        let template = |korean: &str, english: &str| {
            let korean = parse(korean);
            let anchor = String::new();
            Template { korean, english: parse(english), anchor, literal: 0 }
        };
        let named = template("{name}을 {count}번 열었습니다", "Opened {name} {count} times");
        let mut captures = Vec::new();
        assert!(fits(&named.korean, "보고서을 3번 열었습니다", &mut captures));
        assert_eq!(fill(&named, &captures, 0), "Opened 보고서 3 times");
        let swapped = template("{}에서 {}로", "to {1} from {0}");
        let mut captures = Vec::new();
        assert!(fits(&swapped.korean, "A에서 B로", &mut captures));
        assert_eq!(fill(&swapped, &captures, 0), "to B from A");
        // The person's text in a named or quoted placeholder is kept as written, even when it is a word of the tables.
        let titled = template("{title} 버전 {count}", "{title} version {count}");
        let mut captures = Vec::new();
        assert!(fits(&titled.korean, "저장 버전 저장", &mut captures));
        set_language(Language::English);
        assert_eq!(fill(&titled, &captures, 0), "저장 version Save");
        let quoted = template("‘{}’ 밖으로 나갑니다", "Goes outside ‘{}’");
        let mut captures = Vec::new();
        assert!(fits(&quoted.korean, "‘저장’ 밖으로 나갑니다", &mut captures));
        assert_eq!(fill(&quoted, &captures, 0), "Goes outside ‘저장’");
        set_language(Language::Korean);
        let braces = template("{{x}} {}개", "{} items {{x}}");
        let mut captures = Vec::new();
        assert!(fits(&braces.korean, "{x} 4개", &mut captures));
        assert_eq!(fill(&braces, &captures, 0), "4 items {x}");
        assert!(!fits(&braces.korean, "{x} 4", &mut Vec::new()));
    }

    #[test]
    fn every_entry_is_english_and_keeps_the_korean_placeholders() {
        let mut problems = Vec::new();
        for (korean, english) in areas().into_iter().flatten() {
            if english.is_empty() {
                continue;
            }
            if has_hangul(english) {
                problems.push(format!("Korean left in the English of {korean:?}: {english:?}"));
            }
            let holes = |parts: &[Part]| parts.iter().filter(|part| matches!(part, Part::Hole(_))).count();
            let korean_parts = parse(korean);
            let english_parts = parse(english);
            let names: Vec<&String> = korean_parts.iter().filter_map(|part| match part { Part::Hole(name) => Some(name), _ => None }).collect();
            for part in &english_parts {
                if let Part::Hole(name) = part {
                    let known = name.is_empty() || name.parse::<usize>().is_ok_and(|index| index < names.len()) || names.contains(&name);
                    if !known {
                        problems.push(format!("{english:?} names {{{name}}}, which {korean:?} does not have"));
                    }
                }
            }
            if holes(&english_parts) > holes(&korean_parts) && english_parts.iter().any(|part| part == &Part::Hole(String::new())) {
                problems.push(format!("{english:?} has more placeholders than {korean:?}"));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn every_template_entry_has_enough_korean_to_be_kept() {
        // `tables` drops a template with fewer than two Korean letters of its own, so such an entry would never be shown:
        // build that label with `word_label`, `page_label` or `rows_label` instead.
        let dropped: Vec<&str> = areas().into_iter().flatten().filter(|(korean, english)| {
            let parts = parse(korean);
            let hangul: usize = parts.iter().map(|part| match part {
                Part::Text(text) => text.chars().filter(|c| is_hangul(*c)).count(),
                Part::Hole(_) => 0,
            }).sum();
            !english.is_empty() && parts.iter().any(|part| matches!(part, Part::Hole(_))) && hangul < 2
        }).map(|(korean, _)| *korean).collect();
        assert!(dropped.is_empty(), "templates the lookup drops: {dropped:?}");
    }

    #[test]
    fn one_korean_text_has_one_english_text() {
        let mut seen: HashMap<&str, &str> = HashMap::new();
        let mut problems = Vec::new();
        for (korean, english) in areas().into_iter().flatten().copied() {
            if english.is_empty() {
                continue;
            }
            if let Some(before) = seen.insert(korean, english) {
                if before != english {
                    problems.push(format!("{korean:?}: {before:?} / {english:?}"));
                }
            }
        }
        assert!(problems.is_empty(), "the tables disagree:\n{}", problems.join("\n"));
    }
}
