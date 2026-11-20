//! Project design rules on flow documents (SPEC.md:81: project design rule > named style > direct
//! formatting). DOCX and HWPX translate a published design system into `SetNamedStyle` operations here.
//!
//! A published token named `<style name>.<field>` sets that field of the document's named style from its
//! light value: `color` takes a color token, `font_family` a font-family token and `font_size` a font-size
//! token (EMU). The style name is everything before the last `.`, so it may contain dots. A token that no
//! field takes (spacing and radius tokens, another name, a value kind that does not fit the field, a second
//! token for the same field) refuses the whole application with one diagnostic per token: nothing is
//! dropped silently. A token for a style the document does not define has nothing to apply to; it is
//! reported without blocking (`DESIGN_STYLE_ABSENT`), so one design system can serve documents of other
//! types with other style names.
use std::collections::BTreeMap;

use designllm_contracts::design_policy::DirectFormattingPolicy;
use designllm_contracts::{
    Artifact, DesignSystemRef, Diagnostic, DltError, DltResult, ErrorCode, Operation, PreparedChange, Style, StylePatch, TokenDef, TokenValues,
};
use serde_json::Value;

/// Diagnostic code of a published token that no named-style field takes.
pub const DESIGN_TOKEN_UNMAPPED: &str = "DESIGN_TOKEN_UNMAPPED";
/// Diagnostic code of a published token whose value a named style cannot take (color not `#RRGGBB`, empty
/// or control-character font family, font size outside 0 < size <= 1000 pt).
pub const DESIGN_TOKEN_INVALID: &str = "DESIGN_TOKEN_INVALID";
/// Diagnostic code (not blocking) of published tokens whose named style the document does not define.
pub const DESIGN_STYLE_ABSENT: &str = "DESIGN_STYLE_ABSENT";

/// One patch per named style, keyed by style name. Refused as a whole when any token has no field or a
/// value the field cannot take, whether or not the document defines the style.
pub fn named_style_patches(tokens: &BTreeMap<String, TokenDef>) -> DltResult<BTreeMap<String, StylePatch>> {
    let mut patches: BTreeMap<String, StylePatch> = BTreeMap::new();
    let mut refused = Vec::new();
    for (token_id, token) in tokens {
        if let Err((code, reason)) = place(&mut patches, token) {
            refused.push(refusal(code, token_id, token, &reason));
        }
    }
    if refused.is_empty() {
        return Ok(patches);
    }
    let names: Vec<&str> = tokens
        .iter()
        .filter(|(id, _)| refused.iter().any(|d| d.params.get("token_id").and_then(Value::as_str) == Some(id.as_str())))
        .map(|(_, token)| token.name.as_str())
        .collect();
    let mut error = DltError::new(
        ErrorCode::UnsupportedOperation,
        format!(
            "{} published token(s) have no named-style field or a value it cannot take, so the design system was not applied: {}",
            names.len(),
            names.join(", ")
        ),
    );
    error.diagnostics = refused;
    Err(error)
}

/// The `SetNamedStyle` operations of the published tokens for the styles the document defines (`defines`),
/// and the names of the styles it does not define. `Clear` also clears the direct formatting of the text
/// that uses each style (`clear_overrides`).
pub fn named_style_operations(
    tokens: &BTreeMap<String, TokenDef>,
    policy: DirectFormattingPolicy,
    defines: impl Fn(&str) -> bool,
) -> DltResult<(Vec<Operation>, Vec<String>)> {
    let clear_overrides = policy == DirectFormattingPolicy::Clear;
    let mut operations = Vec::new();
    let mut absent = Vec::new();
    for (name, patch) in named_style_patches(tokens)? {
        if defines(&name) {
            operations.push(Operation::SetNamedStyle { name, patch, clear_overrides });
        } else {
            absent.push(name);
        }
    }
    Ok((operations, absent))
}

/// The non-blocking report of the styles `named_style_operations` found absent, if any.
pub fn absent_styles_diagnostic(absent: &[String]) -> Option<Diagnostic> {
    (!absent.is_empty()).then(|| Diagnostic {
        code: DESIGN_STYLE_ABSENT.into(),
        message: format!(
            "the document does not define the named style(s) {}; their published tokens have nothing to apply to",
            absent.join(", ")
        ),
        node_id: None,
        blocking: false,
        params: BTreeMap::from([("styles".to_owned(), Value::from(absent.to_vec()))]),
    })
}

/// True when `style` already holds every field of `patch`. `font_size` rounds a token size to the
/// document's grid, so a quantized stored size still matches.
pub fn style_holds(style: &Style, patch: &StylePatch, font_size: impl Fn(i64) -> i64) -> bool {
    patch.font_family.as_ref().is_none_or(|value| &style.font_family == value)
        && patch.color.as_ref().is_none_or(|value| style.color.eq_ignore_ascii_case(value))
        && patch.font_size.is_none_or(|value| style.font_size == font_size(value))
}

/// Read-only check that the named styles hold the published tokens (direct formatting is not checked: a
/// `Keep` link leaves it as it is). `style` looks a named style up by its name; a style it does not find
/// is one the document does not define, which a link reports and skips.
pub fn check_named_styles(
    tokens: &BTreeMap<String, TokenDef>,
    style: impl Fn(&str) -> Option<Style>,
    font_size: impl Fn(i64) -> i64,
) -> DltResult<()> {
    let patches = named_style_patches(tokens)?;
    let differing: Vec<&str> = patches
        .iter()
        .filter(|(name, patch)| style(name).is_some_and(|current| !style_holds(&current, patch, &font_size)))
        .map(|(name, _)| name.as_str())
        .collect();
    if differing.is_empty() {
        return Ok(());
    }
    Err(DltError::new(
        ErrorCode::IntegrityError,
        format!("named styles do not hold the published design-system values: {}", differing.join(", ")),
    ))
}

type Refusal = (&'static str, String);

fn unmapped(reason: &str) -> Refusal {
    (DESIGN_TOKEN_UNMAPPED, reason.to_owned())
}

/// True when a sync has nothing to do: a `Keep` sync at the reference the document already pins, whose
/// named styles already hold the tokens. Engines return the document unchanged then, without preparing a
/// change (every later edit of a linked document asks again).
pub fn already_synced(
    artifact: &Artifact,
    tokens: &BTreeMap<String, TokenDef>,
    reference: &DesignSystemRef,
    policy: DirectFormattingPolicy,
    style: impl Fn(&str) -> Option<Style>,
    font_size: impl Fn(i64) -> i64,
) -> bool {
    policy == DirectFormattingPolicy::Keep
        && artifact.design_system.as_ref() == Some(reference)
        && check_named_styles(tokens, style, font_size).is_ok()
}

/// The common end of a named-style sync: the absent-style report when the link or its pinned revision
/// changes (not again on every later edit), the new reference, and no changed ids when the document's
/// body, native state and figure links are what they were.
pub fn finish_sync(before: &Artifact, mut prepared: PreparedChange, reference: &DesignSystemRef, absent: &[String]) -> DltResult<PreparedChange> {
    if before.design_system.as_ref() != Some(reference) {
        prepared.diagnostics.extend(absent_styles_diagnostic(absent));
    }
    prepared.artifact.design_system = Some(reference.clone());
    let state = |artifact: &Artifact| {
        serde_json::to_value((&artifact.body, &artifact.native, &artifact.figure_links)).map_err(|error| DltError::invalid(error.to_string()))
    };
    if state(before)? == state(&prepared.artifact)? {
        prepared.changed_ids.clear();
    }
    Ok(prepared)
}

fn place(patches: &mut BTreeMap<String, StylePatch>, token: &TokenDef) -> Result<(), Refusal> {
    match &token.values {
        TokenValues::Spacing { .. } => return Err(unmapped("spacing tokens have no named-style field")),
        TokenValues::Radius { .. } => return Err(unmapped("radius tokens have no named-style field")),
        _ => {}
    }
    let Some((style, field)) = token.name.rsplit_once('.') else {
        return Err(unmapped("the token name is not <style name>.<field>"));
    };
    if style.trim().is_empty() {
        return Err(unmapped("the token name has no style name before the field"));
    }
    let single = match (field, &token.values) {
        ("color", TokenValues::Color { light, .. }) => StylePatch { color: Some(light.clone()), ..StylePatch::default() },
        ("font_family", TokenValues::FontFamily { light, .. }) => StylePatch { font_family: Some(light.clone()), ..StylePatch::default() },
        ("font_size", TokenValues::FontSize { light, .. }) => StylePatch { font_size: Some(*light), ..StylePatch::default() },
        ("color" | "font_family" | "font_size", _) => return Err(unmapped("the token kind does not fit the field")),
        _ => return Err(unmapped("the field is not color, font_family or font_size")),
    };
    check_value(&single).map_err(|reason| (DESIGN_TOKEN_INVALID, reason))?;
    let patch = patches.entry(style.to_owned()).or_default();
    let taken = |taken: bool| if taken { Err(unmapped("another published token already sets this style field")) } else { Ok(()) };
    if let Some(value) = single.color {
        taken(patch.color.is_some())?;
        patch.color = Some(value);
    }
    if let Some(value) = single.font_family {
        taken(patch.font_family.is_some())?;
        patch.font_family = Some(value);
    }
    if let Some(value) = single.font_size {
        taken(patch.font_size.is_some())?;
        patch.font_size = Some(value);
    }
    Ok(())
}

/// The flow style-patch check (`ops::validate_style_patch`: size range, empty family, color syntax), then
/// what both named-style engines need: a `#RRGGBB` color and a family without control characters.
fn check_value(patch: &StylePatch) -> Result<(), String> {
    crate::ops::validate_style_patch(patch).map_err(|error| error.message)?;
    if let Some(color) = &patch.color {
        let six_hex = color.strip_prefix('#').is_some_and(|hex| hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()));
        if !six_hex {
            return Err(format!("color must be #RRGGBB, got {color:?}"));
        }
    }
    if patch.font_family.as_ref().is_some_and(|family| family.chars().any(char::is_control)) {
        return Err("font family must not contain control characters".into());
    }
    Ok(())
}

fn refusal(code: &str, token_id: &str, token: &TokenDef, reason: &str) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        message: format!("published token {} ({token_id}): {reason}", token.name),
        node_id: None,
        blocking: true,
        params: BTreeMap::from([
            ("token_id".to_owned(), Value::from(token_id)),
            ("token".to_owned(), Value::from(token.name.as_str())),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(name: &str, values: TokenValues) -> TokenDef {
        TokenDef { name: name.into(), values }
    }

    fn color(light: &str) -> TokenValues {
        TokenValues::Color { light: light.into(), dark: "#000000".into() }
    }

    #[test]
    fn style_field_names_map_to_one_patch_per_style_with_light_values() {
        let tokens = BTreeMap::from([
            ("t1".to_owned(), token("Heading 1.color", color("#FF0000"))),
            ("t2".to_owned(), token("Heading 1.font_size", TokenValues::FontSize { light: 228_600, dark: 1 })),
            ("t3".to_owned(), token("Body.v2.font_family", TokenValues::FontFamily { light: "Pretendard".into(), dark: "X".into() })),
        ]);
        let patches = named_style_patches(&tokens).unwrap();
        assert_eq!(patches.len(), 2);
        assert_eq!(patches["Heading 1"], StylePatch { color: Some("#FF0000".into()), font_size: Some(228_600), ..Default::default() });
        assert_eq!(patches["Body.v2"], StylePatch { font_family: Some("Pretendard".into()), ..Default::default() });

        let (keep, absent) = named_style_operations(&tokens, DirectFormattingPolicy::Keep, |_| true).unwrap();
        let (clear, _) = named_style_operations(&tokens, DirectFormattingPolicy::Clear, |_| true).unwrap();
        assert_eq!(keep.len(), 2);
        assert!(absent.is_empty() && absent_styles_diagnostic(&absent).is_none());
        assert!(keep.iter().all(|op| matches!(op, Operation::SetNamedStyle { clear_overrides: false, .. })));
        assert!(clear.iter().all(|op| matches!(op, Operation::SetNamedStyle { clear_overrides: true, .. })));
        assert!(named_style_operations(&BTreeMap::new(), DirectFormattingPolicy::Keep, |_| true).unwrap().0.is_empty());
    }

    #[test]
    fn a_style_the_document_does_not_define_is_skipped_and_reported_without_blocking() {
        let tokens = BTreeMap::from([
            ("a".to_owned(), token("Heading 1.color", color("#FF0000"))),
            ("b".to_owned(), token("개요 1.color", color("#FF0000"))),
        ]);
        let (operations, absent) = named_style_operations(&tokens, DirectFormattingPolicy::Keep, |name| name == "Heading 1").unwrap();
        assert!(matches!(operations.as_slice(), [Operation::SetNamedStyle { name, .. }] if name == "Heading 1"));
        assert_eq!(absent, ["개요 1"]);
        let diagnostic = absent_styles_diagnostic(&absent).expect("reported");
        assert_eq!(diagnostic.code, DESIGN_STYLE_ABSENT);
        assert!(!diagnostic.blocking && diagnostic.message.contains("개요 1"));
    }

    #[test]
    fn spacing_radius_and_unmapped_names_refuse_everything_with_one_diagnostic_each() {
        let tokens = BTreeMap::from([
            ("ok".to_owned(), token("Heading 1.color", color("#FF0000"))),
            ("space".to_owned(), token("Heading 1.spacing", TokenValues::Spacing { light: 10, dark: 10 })),
            ("radius".to_owned(), token("Card.radius", TokenValues::Radius { light: 4, dark: 4 })),
            ("plain".to_owned(), token("brand", color("#00FF00"))),
            ("field".to_owned(), token("Heading 1.fill", color("#00FF00"))),
            ("kind".to_owned(), token("Heading 1.font_size", color("#00FF00"))),
            ("empty".to_owned(), token(" .color", color("#00FF00"))),
        ]);
        let error = named_style_patches(&tokens).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedOperation);
        assert_eq!(error.diagnostics.len(), 6, "{error:?}");
        assert!(error.diagnostics.iter().all(|d| d.code == DESIGN_TOKEN_UNMAPPED && d.blocking));
        for name in ["Heading 1.spacing", "Card.radius", "brand", "Heading 1.fill", "Heading 1.font_size"] {
            assert!(error.message.contains(name), "{}", error.message);
        }
        assert!(!error.message.contains("Heading 1.color"), "{}", error.message);
        assert!(named_style_operations(&tokens, DirectFormattingPolicy::Keep, |_| true).is_err());
    }

    #[test]
    fn invalid_values_refuse_even_for_a_style_the_document_lacks() {
        let size = |light: i64| TokenValues::FontSize { light, dark: light };
        let family = |light: &str| TokenValues::FontFamily { light: light.into(), dark: "X".into() };
        let tokens = BTreeMap::from([
            ("neg".to_owned(), token("Absent.font_size", size(-500))),
            ("huge".to_owned(), token("Absent2.font_size", size(1_001 * 12_700))),
            ("css".to_owned(), token("Absent.color", color("red;font-size:0"))),
            ("short".to_owned(), token("Absent2.color", color("#FFF"))),
            ("blank".to_owned(), token("Absent.font_family", family("  "))),
            ("bell".to_owned(), token("Absent2.font_family", family("Pre\u{7}tendard"))),
        ]);
        let error = named_style_operations(&tokens, DirectFormattingPolicy::Keep, |_| false).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedOperation);
        assert_eq!(error.diagnostics.len(), 6, "{error:?}");
        assert!(error.diagnostics.iter().all(|d| d.code == DESIGN_TOKEN_INVALID && d.blocking), "{error:?}");
        let ok = BTreeMap::from([
            ("a".to_owned(), token("Title.font_size", size(1_000 * 12_700))),
            ("b".to_owned(), token("Title.color", color("#a0B1c2"))),
        ]);
        assert!(named_style_patches(&ok).is_ok());
    }

    #[test]
    fn already_synced_needs_keep_the_pinned_reference_and_held_values_and_finish_clears_no_op_ids() {
        let tokens = BTreeMap::from([("c".to_owned(), token("Title.color", color("#AA0000")))]);
        let reference = DesignSystemRef { artifact_id: "p".into(), revision: 2 };
        let held = Style { color: "#AA0000".into(), ..Style::default() };
        let mut artifact = Artifact::new(
            designllm_contracts::ArtifactKind::Docx, "d", "e", designllm_contracts::ArtifactBody::Flow(designllm_contracts::FlowDocument::default()),
        );
        let lookup = |_: &str| Some(held.clone());
        assert!(!already_synced(&artifact, &tokens, &reference, DirectFormattingPolicy::Keep, lookup, |v| v), "not linked yet");
        artifact.design_system = Some(reference.clone());
        assert!(already_synced(&artifact, &tokens, &reference, DirectFormattingPolicy::Keep, lookup, |v| v));
        assert!(!already_synced(&artifact, &tokens, &reference, DirectFormattingPolicy::Clear, lookup, |v| v));
        let newer = DesignSystemRef { revision: 3, ..reference.clone() };
        assert!(!already_synced(&artifact, &tokens, &newer, DirectFormattingPolicy::Keep, lookup, |v| v));
        let other = Style { color: "#000000".into(), ..Style::default() };
        assert!(!already_synced(&artifact, &tokens, &reference, DirectFormattingPolicy::Keep, |_| Some(other.clone()), |v| v));

        let prepared = PreparedChange { artifact: artifact.clone(), changed_ids: vec!["Title".into()], diagnostics: Vec::new() };
        let same = finish_sync(&artifact, prepared, &reference, &["Absent".into()]).unwrap();
        assert!(same.changed_ids.is_empty() && same.diagnostics.is_empty(), "same reference: no report, no ids");
        let prepared = PreparedChange { artifact: artifact.clone(), changed_ids: vec!["Title".into()], diagnostics: Vec::new() };
        let relinked = finish_sync(&artifact, prepared, &newer, &["Absent".into()]).unwrap();
        assert_eq!(relinked.artifact.design_system, Some(newer));
        assert_eq!(relinked.diagnostics.len(), 1, "a new pinned revision reports the absent style");
    }

    #[test]
    fn a_second_token_for_the_same_style_field_is_refused() {
        let tokens = BTreeMap::from([
            ("a".to_owned(), token("Title.color", color("#111111"))),
            ("b".to_owned(), token("Title.color", color("#222222"))),
        ]);
        let error = named_style_patches(&tokens).unwrap_err();
        assert_eq!(error.diagnostics.len(), 1);
        assert_eq!(error.diagnostics[0].params["token_id"], "b");
    }

    #[test]
    fn named_style_check_compares_style_values_with_the_grid_rounding() {
        let tokens = BTreeMap::from([
            ("c".to_owned(), token("Title.color", color("#aa0000"))),
            ("s".to_owned(), token("Title.font_size", TokenValues::FontSize { light: 100, dark: 100 })),
        ]);
        let held = Style { color: "#AA0000".into(), font_size: 127, ..Style::default() };
        let round = |v: i64| (v + 63) / 127 * 127;
        assert!(check_named_styles(&tokens, |name| (name == "Title").then(|| held.clone()), round).is_ok());
        let other = Style { color: "#BB0000".into(), ..held.clone() };
        let error = check_named_styles(&tokens, |_| Some(other.clone()), round).unwrap_err();
        assert_eq!(error.code, ErrorCode::IntegrityError);
        assert!(error.message.contains("Title"));
        assert!(check_named_styles(&tokens, |_| None, round).is_ok(), "a style the document does not define is skipped");
        let unmapped = BTreeMap::from([("r".to_owned(), token("Title.radius", TokenValues::Radius { light: 1, dark: 1 }))]);
        assert!(check_named_styles(&unmapped, |_| Some(held.clone()), round).is_err());
    }
}
