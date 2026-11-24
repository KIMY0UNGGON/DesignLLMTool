use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn is_false(value: &bool) -> bool { !value }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesignThemeFonts {
    #[schemars(length(min = 1, max = 64))]
    pub heading: String,
    #[schemars(length(min = 1, max = 64))]
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesignThemeEntry {
    #[schemars(length(min = 1, max = 128), regex(pattern = "^[a-z0-9._-]+$"))]
    pub id: String,
    #[schemars(length(max = 16), inner(length(min = 1, max = 64)))]
    pub tags: Vec<String>,
    #[schemars(length(min = 1, max = 300))]
    pub summary: String,
    pub colors: BTreeMap<String, String>,
    pub fonts: DesignThemeFonts,
    #[schemars(length(max = 12), inner(range(min = 0.0, max = 10000.0)))]
    pub type_scale_px: Vec<f64>,
    #[schemars(length(max = 12), inner(range(min = 0.0, max = 10000.0)))]
    pub radius_px: Vec<f64>,
    #[schemars(length(max = 12), inner(range(min = 0.0, max = 10000.0)))]
    pub spacing_px: Vec<f64>,
    /// Unix time in milliseconds.
    pub recorded_at: u64,
}

impl DesignThemeEntry {
    pub fn is_valid(&self) -> bool {
        fn text_ok(value: &str, max: usize) -> bool {
            !value.trim().is_empty() && value.chars().count() <= max && !value.chars().any(char::is_control)
        }
        !self.id.is_empty() && self.id.len() <= 128
            && self.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            && self.tags.len() <= 16 && self.tags.iter().all(|v| text_ok(v, 64))
            && text_ok(&self.summary, 300) && self.colors.len() <= 32
            && self.colors.iter().all(|(name, color)| text_ok(name, 128) && color.len() == 7 && color.starts_with('#') && color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit))
            && text_ok(&self.fonts.heading, 64) && text_ok(&self.fonts.body, 64)
            && [&self.type_scale_px, &self.radius_px, &self.spacing_px].iter().all(|values| values.len() <= 12 && values.iter().all(|n| n.is_finite() && *n >= 0.0 && *n <= 10_000.0))
            && self.recorded_at <= i64::MAX as u64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesignThemesSearchRequest {
    #[schemars(length(max = 300))]
    pub query: String,
    #[schemars(range(min = 1, max = 20))]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesignThemesRecordRequest {
    pub entry: DesignThemeEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesignThemesApplyRequest {
    pub request_id: String,
    pub actor: String,
    pub workspace_id: String,
    pub artifact_id: String,
    pub base_revision: u64,
    pub allowed_intents: std::collections::BTreeSet<crate::Intent>,
    #[serde(default)] pub job_ref: Option<crate::JobRef>,
    #[serde(default)] pub read_dependencies: Vec<crate::DependencyRef>,
    pub theme_id: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesignThemesSearchResult {
    pub entries: Vec<DesignThemeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesignThemesRecordResult {
    pub entry: DesignThemeEntry,
}
