//! Words the window shows for codes that come from the engines. Only facts the screen cannot show otherwise are put
//! into words (GUI_DESIGN §1 rule 2): an engine's refusal, a lint finding.

/// The short label of a lint or template rule code (tokens `text.lint-rules.*.label`), for the refusal callout.
/// Codes match without regard to case (`table_overflow` = `TABLE_OVERFLOW`), and the engines' own names map to the
/// rule they report. `None` for a code the table does not know: the callout then uses the engine's Korean reason.
#[cfg_attr(not(test), allow(dead_code))]
pub fn lint_label(code: &str) -> Option<&'static str> {
    Some(match code.to_ascii_uppercase().as_str() {
        // The canvas template lint reports TOUCH_TARGET_SMALL, CONTRAST_LOW and TEXT_CLIPPED (templates.rs).
        "TOUCH_TARGET" | "TOUCH_TARGET_SMALL" => "누르는 영역이 작음",
        "CONTRAST" | "CONTRAST_LOW" => "글자 대비 부족",
        // The flow engine's text_overflow and content_overflow are clipped text too.
        "CLIPPED" | "TEXT_CLIPPED" | "TEXT_OVERFLOW" | "CONTENT_OVERFLOW" => "글이 잘림",
        "CHILD_OUTSIDE_PARENT" => "부모 밖으로 나감",
        "OUT_OF_PAGE" => "페이지 밖",
        "TEMPLATE_FIT" => "템플릿 크기 맞지 않음",
        "TABLE_OVERFLOW" => "표가 본문보다 넓음",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The labels are the ones tokens.json defines.
    #[test]
    fn every_lint_code_has_the_tokens_label_and_unknown_codes_have_none() {
        let tokens: serde_json::Value =
            serde_json::from_str(include_str!("../../../design/tokens.json")).expect("tokens.json parses");
        let rules = tokens.pointer("/text/lint-rules").expect("text.lint-rules");
        for code in ["TOUCH_TARGET", "CONTRAST", "CLIPPED", "CHILD_OUTSIDE_PARENT", "OUT_OF_PAGE", "TEMPLATE_FIT", "TABLE_OVERFLOW"] {
            assert_eq!(lint_label(code), rules[code]["label"].as_str(), "{code}");
        }
        assert_eq!(lint_label("table_overflow"), lint_label("TABLE_OVERFLOW"), "case does not matter");
        assert_eq!(lint_label("text_overflow"), lint_label("CLIPPED"));
        assert_eq!(lint_label("TOUCH_TARGET_SMALL"), lint_label("TOUCH_TARGET"));
        assert_eq!(lint_label("SOMETHING_ELSE"), None);
    }
}
