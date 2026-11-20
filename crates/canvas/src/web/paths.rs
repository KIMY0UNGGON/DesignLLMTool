//! Workspace-relative path rules for WebCode files. The rules for one path are core's `safe_relative` rules exactly
//! (crates/core/src/routes.rs); this crate also caps a whole path at 240 bytes, which is stricter than core's 512. The
//! rules for the whole project are core's `check_web_tree` rules over folded names. `Path::components()` silently drops
//! `.`, so each `/`-separated segment is tested directly.

use crate::common::invalid;
use designllm_contracts::DltResult;
use std::collections::{BTreeMap, BTreeSet};

/// This crate's own cap on a whole path, in bytes. Core allows 512.
const MAX_PATH_BYTES: usize = 240;

/// Core's `safe_relative`, for one path: 1..=512 bytes, relative, no `\`, `:`, or NUL, and for each `/`-segment: not empty,
/// `.` or `..`, not ending in `.` or space, no control character and none of `< > " | ? *`, at most 255 UTF-16 units, and
/// not a Windows device name. Returns the reason for a refusal.
pub(crate) fn check_relative(rel: &str) -> Result<(), String> {
    if rel.is_empty() || rel.len() > 512 {
        return Err("must be 1..=512 bytes long".into());
    }
    if rel.starts_with('/') {
        return Err("must be relative".into());
    }
    if rel.contains(['\\', ':', '\0']) {
        return Err("must not contain '\\', ':' or NUL; '/' is the separator".into());
    }
    for segment in rel.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err("has an empty, '.' or '..' segment".into());
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err("has a segment ending in '.' or ' ', which Windows strips".into());
        }
        if segment.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')) {
            return Err("has a control character or one of < > \" | ? *".into());
        }
        if segment.encode_utf16().count() > 255 {
            return Err("has a segment longer than 255 UTF-16 units".into());
        }
        if is_device_name(segment) {
            return Err(format!("uses the Windows device name {segment:?}"));
        }
    }
    Ok(())
}

/// Windows device names as core states them: the stem before the first dot, trailing spaces removed, ASCII uppercase.
/// CON, PRN, AUX, NUL, CONIN$, CONOUT$, and COM or LPT followed by exactly one of 0-9, U+00B9, U+00B2, U+00B3.
fn is_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|rest| {
            matches!(rest, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}")
        })
    };
    numbered("COM") || numbered("LPT")
}

/// Name as a case-insensitive Windows file system compares it: each character becomes its uppercase only when that is
/// exactly one character, and stays as it is otherwise. The same fold as core's `fold_case`.
pub(crate) fn fold_case(name: &str) -> String {
    name.chars()
        .map(|c| {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(u), None) => u,
                _ => c,
            }
        })
        .collect()
}

/// Accepts `a/b/c.html`. The rules are `check_relative`, plus this crate's 240-byte cap on the whole path.
pub(crate) fn validate_rel(rel: &str) -> DltResult<()> {
    let bad = |why: String| invalid("WEB_PATH", format!("path {rel:?} is not allowed: {why}"));
    if rel.len() > MAX_PATH_BYTES {
        return Err(bad(format!("longer than {MAX_PATH_BYTES} bytes")));
    }
    check_relative(rel).map_err(bad)
}

/// Two project paths that cannot both be kept in one Windows folder, with the path the message is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Conflict {
    pub path: String,
    pub message: String,
}

/// The project-wide rules of core's `check_web_tree`, over folded names: no two paths are equal after `fold_case`, and no
/// folded file path equals the folded directory prefix of another path. Every path involved is named in a conflict.
pub(crate) fn conflicts<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<Conflict> {
    let mut unique: Vec<&str> = paths.into_iter().collect();
    unique.sort_unstable();
    unique.dedup();
    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    // The first path seen for each folded name.
    let mut first_by_folded: BTreeMap<String, &str> = BTreeMap::new();
    // Each folded directory prefix, with every path that needs it and that path's own spelling of the folder.
    let mut folders: BTreeMap<String, Vec<(&str, String)>> = BTreeMap::new();
    let mut folded_paths: Vec<(String, &str)> = Vec::with_capacity(unique.len());
    for &path in &unique {
        let folded = fold_case(path);
        match first_by_folded.get(&folded).copied() {
            Some(first) => {
                let message = format!("{first:?} and {path:?} are the same file on a case-insensitive file system");
                found.insert((first.to_string(), message.clone()));
                found.insert((path.to_string(), message));
            }
            None => {
                first_by_folded.insert(folded.clone(), path);
            }
        }
        let original: Vec<&str> = path.split('/').collect();
        let folded_parts: Vec<String> = original.iter().map(|part| fold_case(part)).collect();
        for end in 1..original.len() {
            folders.entry(folded_parts[..end].join("/")).or_default().push((path, original[..end].join("/")));
        }
        folded_paths.push((folded, path));
    }
    for (folded, path) in &folded_paths {
        for (owner, folder) in folders.get(folded).into_iter().flatten() {
            let message = format!("{path:?} is a file, but {owner:?} needs {folder:?} to be a folder");
            found.insert(((*path).to_string(), message.clone()));
            found.insert(((*owner).to_string(), message));
        }
    }
    found.into_iter().map(|(path, message)| Conflict { path, message }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_relative_paths() {
        assert!(validate_rel("web/index.html").is_ok());
        assert!(validate_rel("styles.css").is_ok());
    }

    #[test]
    fn rejects_traversal_absolute_drive_reserved_and_dot_segments() {
        for bad in ["../x", "a/../../b", "/abs", "C:/x", "a\\b", "CON.html", "", "a/./b", "a/b.", "a//b", "aux/x.js", "a/b "] {
            assert!(validate_rel(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn the_per_path_rules_are_core_safe_relative_examples() {
        // The examples of core's own tests (crates/core/src/routes.rs, safe_relative), accepted and refused alike.
        for ok in ["index.html", "css/site.css", "a/b/c.js", "img/logo.v2.png", "console.js", "com10.js", "lpt.js", " lead.css", "\u{d55c}\u{ae00}.html"] {
            assert!(validate_rel(ok).is_ok(), "should accept {ok:?}");
        }
        for bad in [
            "", "/abs.html", "..", "../x", "a/../b", "a//b", "a/", "a\\b", "C:/x", "c:x", "con", "CON.txt", "nul.js", "nul .js",
            "com1", "com0.js", "lpt0/app.js", "lpt9.html", "com\u{b9}.txt", "conin$", "CONOUT$.log", "a./b", "a /b", "x?.js",
            "./a", "tab\t.js",
        ] {
            assert!(validate_rel(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn device_names_are_refused_with_or_without_extension_trailing_spaces_and_superscript_digits() {
        for name in [
            "CON", "prn.txt", "Aux.x", "NUL", "nul .js", "nul   .js", "CON .txt", "conin$", "CONOUT$.log", "com1", "COM9.js",
            "LPT0", "lpt9 .log", "com\u{b9}", "COM\u{b2}.js", "lpt\u{b3}/x.js", "COM0",
        ] {
            assert!(validate_rel(name).is_err(), "should refuse the device name {name:?}");
        }
        // Not device names: a longer stem, a letter after the name, or a leading space.
        for name in ["COM10.js", "LPT10/x.js", "COMX.js", "COM", "LPT", "CONX.js", "AUX1.txt", "NUL1", "COM\u{b2}0.js", " nul.js", "CON1"] {
            assert!(validate_rel(name).is_ok(), "should accept {name:?}");
        }
    }

    #[test]
    fn control_characters_and_forbidden_punctuation_are_refused() {
        for bad in ["a\u{7f}.css", "a\u{85}b.css", "tab\t.js", "a<b", "a>b", "a\"b", "a|b", "a?b", "a*b", "a\u{0}b", "a:b", "a\\b", "/x"] {
            assert!(validate_rel(bad).is_err(), "should refuse {bad:?}");
        }
        for ok in ["my file-2.css", "\u{d55c}\u{ae00}.html", "a+b=c.js", "x~y.js", "[a].js", "a(1).js"] {
            assert!(validate_rel(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn segments_hold_at_most_255_utf16_units_and_a_path_at_most_512_bytes_by_core_rule() {
        // Core's limits are checked with `check_relative`. This crate's own 240-byte cap is tested with `validate_rel`.
        assert!(check_relative(&"a".repeat(255)).is_ok());
        assert!(check_relative(&"a".repeat(256)).is_err());
        // Counted in UTF-16 units, not bytes: 127 emoji are 508 bytes and 254 units; 128 are 512 bytes and 256 units.
        let emoji = "\u{1F600}";
        assert!(check_relative(&emoji.repeat(127)).is_ok());
        assert!(check_relative(&emoji.repeat(128)).is_err());
        // 512 bytes is the limit: 255 + 1 + 254 + 2 = 512 is fine, one more byte is not.
        let at_limit = format!("{}/{}/c", "a".repeat(255), "b".repeat(254));
        assert_eq!(at_limit.len(), 512);
        assert!(check_relative(&at_limit).is_ok());
        assert!(check_relative(&format!("{at_limit}d")).is_err());
        assert!(validate_rel(&"a".repeat(240)).is_ok());
        assert!(validate_rel(&"a".repeat(241)).is_err());
    }

    #[test]
    fn names_are_compared_after_the_windows_fold_not_after_lowercasing() {
        // The Kelvin sign is its own uppercase, so it does not fold to k. Lowercasing used to make them equal.
        assert!(conflicts(["\u{212A}.css", "k.css"]).is_empty());
        assert!(conflicts(["\u{212A}.css", "K.css"]).is_empty());
        // Dotless i uppercases to I, so it collides with i.
        assert_eq!(conflicts(["\u{131}.css", "i.css"]).len(), 2);
        // The same collision with a folder: a file named with dotless i, and a folder named i.
        let found = conflicts(["\u{131}", "i/x.css"]);
        let named: BTreeSet<&str> = found.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(named, BTreeSet::from(["\u{131}", "i/x.css"]));
        // A letter whose uppercase is two characters stays as it is: sharp s does not fold to SS.
        assert!(conflicts(["\u{DF}.css", "SS.css"]).is_empty());
        assert_eq!(conflicts(["\u{DF}.css", "\u{DF}.CSS"]).len(), 2);
        // The titlecase digraph and the lowercase digraph both uppercase to the same character.
        assert_eq!(conflicts(["\u{1C5}.css", "\u{1C6}.css"]).len(), 2);
    }

    #[test]
    fn names_that_differ_only_in_case_conflict_and_both_are_named() {
        let found = conflicts(["index.html", "a.css", "A.css"]);
        let named: BTreeSet<&str> = found.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(named, BTreeSet::from(["A.css", "a.css"]));
        assert!(found.iter().all(|c| c.message.contains("\"a.css\"") && c.message.contains("\"A.css\"")), "{found:?}");
        // Letters outside ASCII fold too.
        assert_eq!(conflicts(["\u{C4}.css", "\u{E4}.css"]).len(), 2);
        // Three spellings of one name are reported, each naming the others.
        assert_eq!(conflicts(["a.css", "A.css", "a.CSS"]).iter().map(|c| c.path.as_str()).collect::<BTreeSet<_>>().len(), 3);
    }

    #[test]
    fn a_file_cannot_share_a_name_with_a_folder_of_the_project_even_in_other_case() {
        for (file, child) in [("a", "a/b.html"), ("A", "a/b.html"), ("a/b", "a/b/c.html")] {
            let found = conflicts([file, child]);
            let named: BTreeSet<&str> = found.iter().map(|c| c.path.as_str()).collect();
            assert_eq!(named, BTreeSet::from([file, child]), "{file} and {child}");
            assert!(found.iter().all(|c| c.message.contains(&format!("{file:?}")) && c.message.contains(&format!("{child:?}"))), "{found:?}");
        }
    }

    #[test]
    fn names_that_only_look_alike_and_repeated_names_do_not_conflict() {
        assert!(conflicts(["a.b", "a/b"]).is_empty());
        assert!(conflicts(["a", "ab/c.html"]).is_empty());
        assert!(conflicts(["index.html", "css/site.css", "css/app.js", "a/bb.css"]).is_empty());
        assert!(conflicts(["a.css", "a.css"]).is_empty());
    }

    #[test]
    fn the_tree_examples_of_core_hold_for_these_rules() {
        // Core's `the_whole_tree_has_no_case_twins_or_file_folder_clashes` (crates/core/src/routes.rs), example by example.
        // Canonically equivalent spellings (precomposed and combining accent) are different names: nothing is normalized.
        for ok in [vec!["index.html", "a/b.html", "a/c.html", "ab", "A b/x.js"], vec!["\u{e9}.css", "e\u{301}.css"]] {
            assert!(conflicts(ok.iter().copied()).is_empty(), "{ok:?}");
        }
        for (bad, named) in [
            (vec!["index.html", "a.css", "A.css"], vec!["A.css", "a.css"]),
            (vec!["\u{131}.css", "i.css"], vec!["i.css", "\u{131}.css"]),
            (vec!["index.html", "a", "a/b.html"], vec!["a", "a/b.html"]),
            (vec!["A", "a/b/c.html"], vec!["A", "a/b/c.html"]),
            (vec!["x/Y", "x/y/z.js"], vec!["x/Y", "x/y/z.js"]),
        ] {
            let found = conflicts(bad.iter().copied());
            let involved: BTreeSet<&str> = found.iter().map(|c| c.path.as_str()).collect();
            assert_eq!(involved, named.iter().copied().collect::<BTreeSet<&str>>(), "{bad:?}");
        }
    }
}
