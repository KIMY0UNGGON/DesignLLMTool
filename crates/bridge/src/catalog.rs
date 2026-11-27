//! MCP tool catalog. Each tool maps to one core RPC method (see `designllm_core` API.md for the shapes).
//!
//! Input schemas come from `designllm_contracts` (via schemars) where the contract defines the request
//! type. Methods whose parameters the contracts do not define use an object schema that names the fields
//! the core documents. Annotations are set explicitly on every tool; they are hints (MCP 2025-11-25,
//! Tool Annotations), and each value below states what the core enforces.

use designllm_contracts::{
    ChangeSet, CreateArtifactRequest, DesignSystemPublishRequest, DesignSystemSetRequest, ExportFileRequest, HistoryRequest, JobStartRequest, OpenRequest, RenderQuery,
    RenderRequest, ReservationRequest, SaveRequest, SelectionRequest, UiRunRequest, UiScreenshotRequest, UndoChangeRequest,
};
use schemars::JsonSchema;
use serde_json::{json, Map, Value};

use crate::protocol::JOB_SCOPE_METHODS;
use designllm_contracts::design_themes::{DesignThemesApplyRequest, DesignThemesRecordRequest, DesignThemesSearchRequest};

/// Tool behavior hints. Every field is always sent, so the MCP defaults never apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hints {
    /// The tool does not change any workspace state.
    pub read_only: bool,
    /// The tool may overwrite a file outside the workspace (only with overwrite=true and a target hash).
    pub destructive: bool,
    /// Repeating the same arguments has no further effect (receipts keyed by request_id, or set-style).
    pub idempotent: bool,
    /// The tool reaches outside the local workspace (starts an external agent runtime).
    pub open_world: bool,
}

const READ: Hints = Hints { read_only: true, destructive: false, idempotent: true, open_world: false };
const CHANGE: Hints = Hints { read_only: false, destructive: false, idempotent: true, open_world: false };
const CHANGE_NOT_IDEMPOTENT: Hints = Hints { read_only: false, destructive: false, idempotent: false, open_world: false };
const FILE_WRITE: Hints = Hints { read_only: false, destructive: true, idempotent: true, open_world: false };
const AGENT_START: Hints = Hints { read_only: false, destructive: false, idempotent: false, open_world: true };
/// `ui.command.run` takes any permission class, so it carries the strictest hints of them: a file-write command can
/// replace a file (destructive) and an agent-start command starts an external runtime (open world).
const RUN_ANY: Hints = Hints { read_only: false, destructive: true, idempotent: false, open_world: true };

/// How the bridge post-processes a core result before it becomes MCP content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Post {
    /// Structured result and JSON text only.
    None,
    /// `render.get`: the `preview` asset (PNG) is fetched, verified, and returned as image content.
    RenderPreview,
    /// `asset.read`: the whole asset is fetched in chunks, verified by hash, and returned as image or text content.
    AssetContent,
    /// `ui.screenshot`: the run is awaited, then its stored PNG is returned as image content.
    UiScreenshot,
    /// `templates.preview`: the `asset` PNG is fetched, verified, and returned as image content.
    TemplatePreview,
    /// `manual.guide`: answered by the bridge from the built-in manual, never sent to the core.
    Guide,
}

/// One MCP tool.
#[derive(Debug, Clone, Copy)]
pub struct ToolSpec {
    pub name: &'static str,
    pub method: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub hints: Hints,
    pub post: Post,
    pub schema: fn() -> Value,
}

/// Which tools a connection may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// External MCP attachment: every workspace tool.
    Workspace,
    /// Job-bound connection: only methods in [`JOB_SCOPE_METHODS`].
    JobBound,
}

fn schema_of<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({ "type": "object" }))
}

/// Object schema with the listed properties. `additional` = false rejects unknown keys.
fn object(properties: Value, required: &[&str], additional: bool) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": additional,
    })
}

fn string() -> Value {
    json!({ "type": "string" })
}

fn integer(min: i64) -> Value {
    json!({ "type": "integer", "minimum": min })
}

fn sha256_arg() -> Value {
    json!({ "type": "string", "pattern": "^[0-9a-f]{64}$" })
}

fn opaque_object() -> Value {
    json!({ "type": "object" })
}

fn schema_workspace_list() -> Value {
    object(json!({}), &[], false)
}
fn schema_design_themes_search() -> Value { schema_of::<DesignThemesSearchRequest>() }
fn schema_design_themes_apply() -> Value { schema_of::<DesignThemesApplyRequest>() }
fn schema_design_themes_record() -> Value {
    let mut schema = schema_of::<DesignThemesRecordRequest>();
    let colors = &mut schema["definitions"]["DesignThemeEntry"]["properties"]["colors"];
    colors["maxProperties"] = json!(32);
    colors["propertyNames"] = json!({"type":"string", "minLength":1, "maxLength":128});
    colors["additionalProperties"] = json!({"type":"string", "pattern":"^#[0-9A-Fa-f]{6}$"});
    schema["definitions"]["DesignThemeEntry"]["properties"]["recorded_at"]["maximum"] = json!(i64::MAX);
    schema
}
fn schema_create() -> Value {
    let mut schema = schema_of::<CreateArtifactRequest>();
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        if let Some(size) = properties.get_mut("size") {
            append_note(
                size,
                "Frame size in EMU (1 px = 9525 EMU, 1 mm = 36000 EMU, 1 in = 914400 EMU, 1 pt = 12700 EMU). Preset sizes: the designllm://presets resource.",
            );
        }
        if let Some(template) = properties.get_mut("template") {
            append_note(
                template,
                "Template ids depend on the registered engines. Call templates.list for the available ids; omit template when none are listed. Unsupported ids are refused.",
            );
        }
    }
    schema
}
fn schema_open() -> Value {
    schema_of::<OpenRequest>()
}
fn schema_inspect() -> Value {
    object(
        json!({
            "artifact_id": string(),
            "node_ids": { "type": "array", "items": string() },
            "revision": integer(0),
        }),
        &["artifact_id"],
        false,
    )
}
fn schema_artifact_id() -> Value {
    object(json!({ "artifact_id": string() }), &["artifact_id"], false)
}
fn schema_save() -> Value {
    schema_of::<SaveRequest>()
}
fn schema_export() -> Value {
    schema_of::<ExportFileRequest>()
}
/// Capability notes for the v1.2 items. Each note is appended after the contract's own doc comment, which stays as it is.
const V12_NOTE: &str = "v1.2: support depends on the engine. artifact_validate returns the capability report (per feature: whether this engine supports it and why not). Unsupported values are refused with UNSUPPORTED_OPERATION; do not retry the same value unchanged.";
const V12_OPEN_URL_NOTE: &str = "v1.2 (P2): Canvas supports OpenUrl for https:, mailto:, and tel: links in HTML/PDF. In mixed HTML action lists, OpenUrl runs at its stored position. The in-app WebView2 preview blocks external navigation and new windows. Give each link an accessible name; HTML export reports HTML_LINK_ACCESSIBLE_NAME_MISSING when one is absent. Other engines may refuse it with UNSUPPORTED_OPERATION; check the per-engine capability report.";
const V12_BACK_NOTE: &str = "v1.2 (P2): Canvas supports Back in exported HTML, returning to the previously shown page in that export; with no prior in-export page visit it does nothing. It does not use browser history. Other engines may refuse it with UNSUPPORTED_OPERATION; check the per-engine capability report.";
const V12_EFFECTS_NOTE: &str = "v1.2 (P0): support depends on the engine. Canvas draws most of these values and a Figure some of them; a slide deck draws only an outer shadow; document engines refuse them. A value an engine does not draw is refused with UNSUPPORTED_OPERATION. The Effects feature of the capability report (artifact_validate) says what this engine supports and why not. Do not retry the same value unchanged.";
const V12_VECTOR_NOTE: &str = "v1.2 (P0): Canvas supports this; other engines may refuse it with UNSUPPORTED_OPERATION. The VectorPaths feature of the capability report (artifact_validate) says whether this engine supports it and why not. Do not retry the same value unchanged.";
const V12_GRID_NOTE: &str = "v1.2 (P0): Canvas supports this; other engines may refuse it with UNSUPPORTED_OPERATION. The GridLayout feature of the capability report (artifact_validate) says whether this engine supports it and why not. Do not retry the same value unchanged.";
const V12_CLEAR_NOTE: &str = "v1.2 (P0): a non-empty clear is refused with UNSUPPORTED_OPERATION unless the engine supports clearing that field (Canvas clears most fields; a Figure refuses every clear). artifact_validate returns the capability report. Do not retry the same clear unchanged.";
const V12_LISTS_NOTE: &str = "v1.2 (P2): Canvas supports bullet and numbered lists on Text nodes; each hard paragraph is one item and wrapped lines continue that item. Runtime support is reported in capability_extensions.lists by artifact.validate/context.read; other engines may refuse this value with UNSUPPORTED_OPERATION.";

/// Style fields added in v1.2. Style and StylePatch use the same names.
const V12_STYLE_FIELDS: &[&str] = &[
    "corner_radius", "opacity_milli", "shadows", "gradient", "line_height_milli", "letter_spacing", "vertical_align",
    "font_weight", "stroke_dash", "stroke_cap", "stroke_join", "underline", "strikethrough",
];
const V12_LAYOUT_FIELDS: &[&str] = &["justify", "padding_sides", "grid_columns", "row_gap", "align_self", "grid_span", "mode"];
const V12_BREAKPOINT_FIELDS: &[&str] = &["grid_columns", "justify"];

/// (definition, properties, note): properties added in v1.2 to a contract struct. Node carries the NodeExt fields.
const V12_PROPERTY_GROUPS: &[(&str, &[&str], &str)] = &[
    ("Style", V12_STYLE_FIELDS, V12_EFFECTS_NOTE),
    ("StylePatch", V12_STYLE_FIELDS, V12_EFFECTS_NOTE),
    ("StylePatch", &["clear"], V12_CLEAR_NOTE),
    ("LayoutSpec", V12_LAYOUT_FIELDS, V12_GRID_NOTE),
    ("Breakpoint", V12_BREAKPOINT_FIELDS, V12_GRID_NOTE),
    ("Node", &["path", "icon"], V12_VECTOR_NOTE),
    ("Node", &["image_fit", "alt", "name", "hidden", "locked"], V12_NOTE),
    ("Node", &["list"], V12_LISTS_NOTE),
    ("Page", &["name"], V12_NOTE),
];

/// (definition, variant, note): oneOf options added in v1.2, named by their tag (NodeKind string, Interaction action,
/// Operation op).
const V12_VARIANTS: &[(&str, &str, &str)] = &[
    ("NodeKind", "path", V12_VECTOR_NOTE),
    ("Interaction", "open_url", V12_OPEN_URL_NOTE),
    ("Interaction", "back", V12_BACK_NOTE),
    ("Operation", "set_page_size", V12_NOTE),
    ("Operation", "move_node", V12_NOTE),
    ("Operation", "set_z_order", V12_NOTE),
    ("Operation", "set_node_appearance", V12_NOTE),
    ("Operation", "set_node_info", V12_NOTE),
    ("Operation", "set_node_list", V12_LISTS_NOTE),
    ("Operation", "set_interactions", V12_NOTE),
    ("Operation", "rename_page", V12_NOTE),
];

/// Appends the v1.2 notes to the definitions of a change set schema.
fn annotate_v12(schema: &mut Value) {
    let Some(defs) = schema.get_mut("definitions").and_then(Value::as_object_mut) else {
        return;
    };
    for (definition, properties, note) in V12_PROPERTY_GROUPS {
        let Some(fields) = defs.get_mut(*definition).and_then(|d| d.get_mut("properties")).and_then(Value::as_object_mut)
        else {
            continue;
        };
        for field in *properties {
            if let Some(property) = fields.get_mut(*field) {
                append_note(property, note);
            }
        }
    }
    for (definition, variant, note) in V12_VARIANTS {
        let Some(options) = defs.get_mut(*definition).and_then(|d| d.get_mut("oneOf")).and_then(Value::as_array_mut) else {
            continue;
        };
        for option in options.iter_mut() {
            let hit = variant_names(option).contains(variant);
            if hit {
                append_note(option, note);
            }
        }
    }
}

/// The names a oneOf option carries: a string enum, or the `op` / `action` tag enum.
fn variant_names(option: &Value) -> Vec<&str> {
    let tag = option.get("properties").and_then(|p| p.get("op").or_else(|| p.get("action")));
    let names = option.get("enum").or_else(|| tag.and_then(|t| t.get("enum")));
    names
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Appends `note` to the schema's description, after the existing text.
fn append_note(value: &mut Value, note: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let text = match object.get("description").and_then(Value::as_str) {
        Some(existing) => format!("{existing} {note}"),
        None => note.to_string(),
    };
    object.insert("description".into(), Value::String(text));
}

fn schema_change_set() -> Value {
    let mut schema = schema_of::<ChangeSet>();
    remove_hidden_operations(&mut schema);
    annotate_v12(&mut schema);
    schema
}

/// Drops the `Operation` options that `operation_exposure` hides from MCP sessions.
fn remove_hidden_operations(schema: &mut Value) {
    let Some(options) = schema.pointer_mut("/definitions/Operation/oneOf").and_then(Value::as_array_mut) else {
        return;
    };
    options.retain(|option| !variant_names(option).into_iter().any(crate::operation_exposure::is_hidden));
}
fn schema_dataset_edit() -> Value {
    let mut schema = schema_of::<ChangeSet>();
    let properties = schema["properties"].as_object_mut().expect("ChangeSet object schema");
    properties.remove("operations");
    properties.remove("read_dependencies");
    properties.insert("dataset_id".into(), string());
    properties.insert("version_hash".into(), sha256_arg());
    properties.insert("edits".into(), json!({"type":"array","minItems":1,"items":{
        "type":"object","additionalProperties":false,
        "required":["type","row_id","column_id","previous","replacement"],
        "properties":{"type":{"const":"edit_cell"},"row_id":string(),"column_id":string(),"previous":string(),"replacement":string()}
    }}));
    let required = schema["required"].as_array_mut().expect("ChangeSet required fields");
    required.retain(|v| v != "operations");
    required.extend([json!("dataset_id"), json!("version_hash"), json!("edits")]);
    schema["additionalProperties"] = json!(false);
    schema
}
fn schema_history() -> Value {
    schema_of::<HistoryRequest>()
}
fn schema_design_system_publish() -> Value {
    schema_of::<DesignSystemPublishRequest>()
}
fn schema_design_system_set() -> Value {
    schema_of::<DesignSystemSetRequest>()
}
fn schema_render_request() -> Value {
    schema_of::<RenderRequest>()
}
fn schema_render_query() -> Value {
    schema_of::<RenderQuery>()
}
fn schema_asset_import() -> Value {
    object(json!({ "path": string(), "mime": string() }), &["path"], false)
}
fn schema_asset_read() -> Value {
    object(
        json!({
            "sha256": sha256_arg(),
            "max_bytes": { "type": "integer", "minimum": 1, "maximum": 8_388_608u64 },
        }),
        &["sha256"],
        false,
    )
}
fn schema_asset_lookup() -> Value {
    object(json!({ "sha256": sha256_arg() }), &["sha256"], false)
}
fn schema_selection() -> Value {
    schema_of::<SelectionRequest>()
}
fn schema_reservation() -> Value {
    let mut schema = schema_of::<ReservationRequest>();
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert("ttl_ms".into(), json!({ "type": "integer", "minimum": 1000, "maximum": 300_000 }));
    }
    schema
}
fn schema_reservation_id() -> Value {
    object(json!({ "reservation_id": string() }), &["reservation_id"], false)
}
fn schema_design_guide() -> Value {
    let topics: Vec<&str> = crate::manual::PAGES.iter().map(|page| page.slug).chain(["shapes-json", crate::manual::PAPER_DIAGRAM_EXAMPLE]).collect();
    object(
        json!({
            "artifact_id": { "type": "string", "description": "Return the mandatory guide for this artifact (its kind and title choose the task guide)." },
            "topic": { "type": "string", "enum": topics, "description": "Return one manual page: a task guide (paper-diagram, paper-figure, slides ...), glossary, shapes, shapes-json or another reference page." },
        }),
        &[],
        false,
    )
}
fn schema_no_params() -> Value {
    object(json!({}), &[], false)
}
fn schema_templates_preview() -> Value {
    object(
        json!({
            "kind": string(),
            "template": string(),
            "dpi": { "type": "integer", "minimum": 1, "maximum": 9600 },
            "preset": {
                "type": "string",
                "description": "One preset the template lists in templates.list. Not together with size.",
            },
            "size": {
                "type": "object",
                "description": "Page size in EMU (1 px = 9525 EMU), positive, at most 10 m per side. Not together with preset.",
                "properties": { "width": { "type": "integer" }, "height": { "type": "integer" } },
                "required": ["width", "height"],
                "additionalProperties": false,
            },
        }),
        &["template", "dpi"],
        false,
    )
}
fn schema_view_transform() -> Value {
    object(
        json!({
            "view_id": string(),
            "page_index": integer(0),
            "artifact_id": string(),
            "transform": {
                "type": "object",
                "properties": {
                    "id": string(),
                    "dpi": { "type": "integer", "minimum": 1, "maximum": 9600 },
                    "zoom_milli": { "type": "integer", "minimum": 1 },
                    "offset_x_micropx": { "type": "integer" },
                    "offset_y_micropx": { "type": "integer" },
                    "crop": { "type": ["object", "null"] },
                },
                "required": ["id", "dpi", "zoom_milli", "offset_x_micropx", "offset_y_micropx"],
                "additionalProperties": false,
            },
        }),
        &["view_id", "page_index", "transform"],
        false,
    )
}
fn schema_context_read() -> Value {
    object(
        json!({
            "artifact_id": string(),
            "view_id": string(),
            "include_render": { "type": "boolean" },
            "profile": opaque_object(),
        }),
        &[],
        false,
    )
}
fn schema_job_start() -> Value {
    schema_of::<JobStartRequest>()
}
fn schema_job_id_only() -> Value {
    object(json!({ "job_id": string() }), &["job_id"], false)
}
fn schema_job_cancel() -> Value {
    object(json!({ "job_id": string(), "attempt_id": string() }), &["job_id"], false)
}
fn schema_events_poll() -> Value {
    object(
        json!({
            "after_seq": integer(0),
            "limit": { "type": "integer", "minimum": 1, "maximum": 1000 },
        }),
        &[],
        false,
    )
}
fn schema_proposal_id() -> Value {
    object(json!({ "proposal_id": string() }), &["proposal_id"], false)
}
fn schema_proposal_preview() -> Value {
    object(
        json!({
            "proposal_id": string(),
            "page_index": integer(0),
            "profile": opaque_object(),
        }),
        &["proposal_id"],
        false,
    )
}
/// `{ change_set: ChangeSet }`. The ChangeSet's `#/definitions/...` references are lifted to the root so
/// they still resolve after the ChangeSet is nested under a property.
fn schema_proposal_create() -> Value {
    let mut change_set = schema_change_set();
    let mut definitions = Map::new();
    if let Value::Object(map) = &mut change_set {
        map.remove("$schema");
        if let Some(Value::Object(defs)) = map.remove("definitions") {
            definitions = defs;
        }
    }
    let mut root = object(json!({ "change_set": change_set }), &["change_set"], false);
    if let Value::Object(map) = &mut root {
        if !definitions.is_empty() {
            map.insert("definitions".into(), Value::Object(definitions));
        }
    }
    root
}
fn schema_proposal_apply() -> Value {
    schema_proposal_id()
}
fn schema_figure_render_link() -> Value {
    object(
        json!({
            "figure_id": string(),
            "figure_revision": integer(0),
            "representation": string(),
            "profile": opaque_object(),
        }),
        &["figure_id", "figure_revision", "representation"],
        false,
    )
}
fn schema_source_extract_pdf() -> Value {
    object(
        json!({
            "sha256": sha256_arg(),
            "pages": { "type": "array", "items": integer(0) },
        }),
        &["sha256"],
        false,
    )
}
/// `source.import_csv({sha256, columns?})`. `columns` declares the type and unit of named columns. An `id` may be
/// sent; the core ignores it. The bridge forwards `columns` unchanged, and the core checks the values.
fn schema_source_import_csv() -> Value {
    let column = json!({
        "type": "object",
        "properties": {
            "id": string(),
            "name": { "type": "string", "description": "Header text of the CSV column." },
            "data_type": { "type": "string", "description": "Column type: category, numeric, count, or text." },
            "unit": { "type": ["string", "null"] },
        },
        "required": ["name", "data_type"],
        "additionalProperties": false,
    });
    object(
        json!({
            "sha256": sha256_arg(),
            "columns": {
                "type": "array",
                "minItems": 1,
                "description": "Optional. Declares data_type and unit for the named columns. Undeclared columns are text.",
                "items": column,
            },
        }),
        &["sha256"],
        false,
    )
}
/// `ui.state.read` and `ui.command.list`: an optional view id (the most recently published live view by default).
fn schema_ui_view() -> Value {
    object(json!({ "view_id": string() }), &[], false)
}
/// `ui.command.get`: one run id.
fn schema_ui_run_id() -> Value {
    object(json!({ "run_id": string() }), &["run_id"], false)
}
/// `ui.command.run`: the contract request. `permission` is the class the call grants (read, change, file_write or
/// agent_start); the core refuses a command whose class is higher.
fn schema_ui_run() -> Value {
    schema_of::<UiRunRequest>()
}
fn schema_ui_screenshot() -> Value {
    schema_of::<UiScreenshotRequest>()
}
fn schema_history_undo_change() -> Value {
    schema_of::<UndoChangeRequest>()
}
fn schema_web_materialize() -> Value {
    object(
        json!({
            "artifact_id": string(),
            "expected_revision": integer(0),
        }),
        &["artifact_id", "expected_revision"],
        false,
    )
}

fn entry(
    name: &'static str,
    method: &'static str,
    title: &'static str,
    description: &'static str,
    hints: Hints,
    post: Post,
    schema: fn() -> Value,
) -> ToolSpec {
    ToolSpec { name, method, title, description, hints, post, schema }
}

/// Method of the `design_guide` tool. The bridge answers it from [`crate::manual`]; the core has no such method.
pub const GUIDE_METHOD: &str = "manual.guide";
/// Methods the bridge answers itself.
pub const LOCAL_METHODS: &[&str] = &[GUIDE_METHOD];

/// The full catalog. Names are `[a-z_]` only, which every model API accepts as a tool name.
pub fn catalog() -> Vec<ToolSpec> {
    let t = entry;
    vec![
        t("design_guide", GUIDE_METHOD, "Design guide", "Returns the mandatory design guide: with artifact_id, the guide for that artifact (shared workflow, the task guide its kind and title call for, design-quality checks and the design glossary); with topic, one manual page (paper-diagram, paper-figure, glossary, shapes, shapes-json ...); with both, the artifact guide followed by that page; with neither, the index and the shared workflow. Call it with the artifact_id before the first change to an artifact: the artifact-bound guide (from this tool, an artifact_create, artifact_open or artifact_inspect result, or a GUIDE_REQUIRED refusal) unlocks changes on this connection to that artifact and to every artifact that takes the same guide (for example every deck); a topic page alone is reference reading and unlocks nothing. The first change to an artifact whose guide this connection has not received is refused with GUIDE_REQUIRED and the guide.", READ, Post::Guide, schema_design_guide),
        t("workspace_list", "workspace.list", "List artifacts", "Lists the artifacts in the open workspace with revision and dirty state. A workspace-scoped response may also include workspace_notices (Diagnostic entries) about migration, backup, or recovery; job-bound responses may omit this workspace-level field.", READ, Post::None, schema_workspace_list),
        t("artifact_create", "artifact.create", "Create artifact", "Creates a new artifact. Repeating the same request_id returns the stored receipt. Size is in EMU; the frame presets are in the designllm://presets resource.", CHANGE, Post::None, schema_create),
        t("artifact_open", "artifact.open", "Open file copy", "Imports a supported file as an artifact. Omit kind to infer Office files or native .designllm files. PDF opens as an editable DOCX copy using Microsoft Word on Windows. Recognized tables remain editable, and conversion may change the layout. The original file is not modified.", CHANGE, Post::None, schema_open),
        t("artifact_inspect", "artifact.inspect", "Inspect artifact", "Returns the public view of an artifact, or the requested node ids.", READ, Post::None, schema_inspect),
        t("artifact_validate", "artifact.validate", "Validate artifact", "Returns diagnostics for the current revision and capabilities: the persisted Feature map plus optional string-keyed runtime capability_extensions such as lists and design_tokens. Use the live extension report to discover Canvas support. Does not change the artifact.", READ, Post::None, schema_artifact_id),
        t("artifact_save_state", "artifact.save_state", "Save state", "Returns the workspace revision, the last native-saved revision, the target hash, and whether the file changed outside the workspace.", READ, Post::None, schema_artifact_id),
        t("artifact_save", "artifact.save", "Save to file", "Writes the artifact in its native format. In a project opened with --project, omit target_path to save in the project's artifacts folder and update that artifact's managed file on later saves. External changes and collisions are refused. For an explicit target_path, replacing a file requires overwrite=true and expected_target_hash.", FILE_WRITE, Post::None, schema_save),
        t("artifact_export", "artifact.export", "Export file", "Exports PDF, SVG, PNG, PPTX, or HTML and reports editability and losses. A WebCode artifact exports only html (the entry page with its local files inlined) or zip (every project file in one archive), and refuses page_indices. page_indices selects pages by 0-based index for PDF, PNG, and SVG; PPTX, DOCX, HWPX, and HWP exports write the whole file and refuse page_indices with UNSUPPORTED_OPERATION. PNG and SVG write one file per page, so they export exactly one page: pass page_indices when the artifact has several. An engine refuses a format it does not write with UNSUPPORTED_OPERATION. Replaces a file only with overwrite=true and expected_target_hash.", FILE_WRITE, Post::None, schema_export),
        t("changes_validate", "changes.validate", "Validate change set", "Checks a ChangeSet against the current revision, its intents, and the format rules. Does not change the artifact. Support depends on the engine; artifact_validate reports the live capability_extensions and feature-specific support. Unsupported values are refused with UNSUPPORTED_OPERATION.", READ, Post::None, schema_change_set),
        t("changes_apply", "changes.apply", "Apply change set", "Applies a ChangeSet atomically as one undoable transaction. Repeating the same request_id returns the stored receipt. Support depends on the engine; unsupported values are refused with UNSUPPORTED_OPERATION and the revision does not change. On Canvas, set_interactions replaces the node's complete interaction list in supplied order; an empty list clears it. The existing set_interaction operation still replaces the list with one action. A token definition update on a published design system synchronizes its consumers in one atomic group and returns optional group details. Read artifact_validate's capability report first, and do not retry a refused value unchanged.", CHANGE, Post::None, schema_change_set),
        t("design_system_list", "design_system.list", "List design systems", "Lists published Canvas design systems by artifact_id, title, and current revision. It returns no token values and is workspace-scoped; job-bound connections cannot enumerate workspace systems.", READ, Post::None, schema_no_params),
        t("design_themes_search", "design_themes.search", "Search design themes", "Ranks themes by the count of distinct query words found in tags or summary, then newest recorded_at and id. An empty query lists recent entries.", READ, Post::None, schema_design_themes_search),
        t("design_themes_record", "design_themes.record", "Record design theme", "Stores or updates one workspace design theme. The oldest entries are removed when the limit is reached.", CHANGE, Post::None, schema_design_themes_record),
        t("design_themes_apply", "design_themes.apply", "Apply design theme", "Applies a saved theme to a Canvas or Slides artifact as one validated change. Set dry_run to true to return operations and a change_set without applying them. The result includes structured notes.", CHANGE, Post::None, schema_design_themes_apply),
        t("design_system_publish", "design_system.publish", "Publish design system", "Marks a Canvas artifact as a shared design-system publisher or unpublishes it. Requires design intent and the current expected_revision. A publisher cannot consume another system; unpublishing is refused while any consumer still references it. Define or change token values with changes_apply. Other engines refuse shared design tokens.", CHANGE, Post::None, schema_design_system_publish),
        t("design_system_set", "design_system.set", "Set design system", "Links a Canvas, WebCode, DOCX or HWPX consumer to a published Canvas artifact, or detaches it with design_system null. Requires design intent and the current consumer expected_revision; a WebCode link also needs content intent, since it adds the stylesheet link to the entry file. Pass optional expected_system_revision from design_system_list to reject a publisher changed since selection; Core checks it again at commit. Canvas resolves shared bindings for the consumer's Light/Dark mode; detaching keeps their concrete values. WebCode writes design-tokens.css, and detaching keeps its values and removes only the link. DOCX and HWPX apply each published token named `<style name>.color`, `.font_family` or `.font_size` to that named style (light value) and need direct_formatting: keep leaves direct formatting as it is, clear removes it from the text using those styles (DOCX only; HWPX refuses clear). A spacing or radius token, any other name, or a value the style cannot take (color not #RRGGBB, empty font family, size outside 0-1000 pt) refuses the whole link; a style the document lacks is reported and skipped, and on a publisher update the report names the consumer in params.artifact_id. Publisher updates reach linked documents with keep. Canvas and WebCode need no direct_formatting (keep is accepted, clear is refused); a detach takes none. Other engines refuse shared design tokens.", CHANGE, Post::None, schema_design_system_set),
        t("history_undo", "history.undo", "Undo", "Reverts the last committed transaction of the artifact.", CHANGE_NOT_IDEMPOTENT, Post::None, schema_history),
        t("history_redo", "history.redo", "Redo", "Re-applies the transaction most recently undone.", CHANGE_NOT_IDEMPOTENT, Post::None, schema_history),
        t("history_undo_change", "history.undo_change", "Undo change", "Takes back one committed change batch, named by change_id (the request_id of that change), even when later changes exist, if no later change still in effect changed the same ids. Repeating the same request_id returns the stored receipt.", CHANGE_NOT_IDEMPOTENT, Post::None, schema_history_undo_change),
        t("render_get", "render.get", "Render page", "Renders one page of an artifact at the given revision. The PNG preview is returned as image content.", READ, Post::RenderPreview, schema_render_request),
        t("render_hit_test", "render.hit_test", "Hit test", "Maps a point in a render snapshot to a node and a text position.", READ, Post::None, schema_render_query),
        t("render_caret_rect", "render.caret_rect", "Caret rectangle", "Returns the caret rectangle for a text position in a render snapshot.", READ, Post::None, schema_render_query),
        t("render_selection_rects", "render.selection_rects", "Selection rectangles", "Returns the rectangles covering a text range in a render snapshot.", READ, Post::None, schema_render_query),
        t("asset_import", "asset.import", "Import asset", "Copies a local file into the content-addressed asset store and returns its hash. The path must be absolute, without . or .. components, and name a regular file of at most 1 GiB; any other path is refused.", CHANGE, Post::None, schema_asset_import),
        t("asset_read", "asset.read", "Read asset", "Reads an asset by hash. Chunks are verified against the hash. Images are returned as image content.", READ, Post::AssetContent, schema_asset_read),
        t("asset_lookup", "asset.lookup", "Look up asset", "Returns the metadata of an asset by hash.", READ, Post::None, schema_asset_lookup),
        t("selection_set", "selection.set", "Set selection", "Records the selection of a view at one revision.", CHANGE, Post::None, schema_selection),
        t("targets_reserve", "targets.reserve", "Reserve targets", "Marks nodes as busy so agent changes are refused while the user edits them.", CHANGE_NOT_IDEMPOTENT, Post::None, schema_reservation),
        t("targets_release", "targets.release", "Release targets", "Releases a reservation.", CHANGE, Post::None, schema_reservation_id),
        t("targets_list", "targets.list", "List reservations", "Lists the reservations of one artifact: owner, view_id and node_ids, with expires_in_ms for a user's reservation or job_id for a job's. A change that touches a node another owner holds is refused with TARGET_BUSY.", READ, Post::None, schema_artifact_id),
        t("clients_list", "clients.list", "List clients", "Lists the connections to the workspace host: role, client name and version, the job of a job connection, and when the host saw it. A job connection sees only the names.", READ, Post::None, schema_no_params),
        t("templates_list", "templates.list", "List templates", "Lists the templates of the registered engines: id, kind, title, preset and size. A template id is what artifact_create takes as template.", READ, Post::None, schema_no_params),
        t("templates_preview", "templates.preview", "Preview template", "Renders page 0 of a template as a PNG without creating an artifact. The PNG is returned as image content. Give preset (one that templates.list names for the template) or size, not both; neither uses the engine's default size. kind is needed only when two engines use the same template id.", READ, Post::TemplatePreview, schema_templates_preview),
        t("flow_styles", "flow.styles", "List flow styles", "Lists the named styles of a DOCX document, in document order: id, kind, display name, and whether a style is quick, hidden or built in. Read-only. Only DOCX supports it; other engines refuse it with UNSUPPORTED_OPERATION.", READ, Post::None, schema_artifact_id),
        t("view_transform_set", "view.transform.set", "Set view transform", "Records the transform of a preview view.", CHANGE, Post::None, schema_view_transform),
        t("context_read", "context.read", "Read context", "Reads the selection, properties, persisted capabilities, optional string-keyed runtime capability_extensions such as lists, and render snapshot of one artifact at one revision.", READ, Post::None, schema_context_read),
        t("figure_render_link", "figure.render_link", "Render figure link", "Renders a Figure revision as an immutable asset. A job-bound connection may render only exact figure_id/figure_revision pairs declared in job.start.figure_refs; source_refs authorizes asset hashes only.", CHANGE, Post::None, schema_figure_render_link),
        t("source_extract_pdf", "source.extract_pdf", "Extract PDF evidence", "Reads the requested pages of a PDF asset and returns evidence with page, region, and text. Pages are 0-based; an empty pages list reads every page. The result also has read_pages (every page whose content was read, with or without evidence), page_count (the count the reader verified against the page tree), and document_complete, which is true only when read_pages covers every page of the document. A page that cannot be read fails the whole request.", READ, Post::None, schema_source_extract_pdf),
        t("source_import_csv", "source.import_csv", "Import CSV dataset", "Parses a CSV asset into a dataset. Optional columns declare each column's data_type and unit. Malformed and ambiguous cells are reported, not guessed.", CHANGE, Post::None, schema_source_import_csv),
        t("figure_dataset_edit", "figure.dataset.edit", "Edit dataset cells", "Checks previous raw values, creates a derived immutable dataset version and atomically updates plots in this figure. Requires data intent.", CHANGE, Post::None, schema_dataset_edit),
        t("web_materialize", "web.materialize", "Materialize web source", "Writes the web artifact's source files to a project folder in the workspace.", CHANGE, Post::None, schema_web_materialize),
        t("job_start", "job.start", "Start AI job", "Queues an AI job for an artifact. Optional figure_refs explicitly pins Figure artifact IDs and exact revisions that the job may render; source_refs authorizes asset hashes only. Optional deliver_as proposal makes the job deliver a proposal_create draft that the user previews and applies; core refuses every change the job would commit itself (changes_apply, history undo/redo, figure_dataset_edit) with CONTENT_LOCKED. The host starts the runtime with a job-bound MCP connection.", AGENT_START, Post::None, schema_job_start),
        t("job_get", "job.get", "Get job", "Returns the state of a job and its current attempt.", READ, Post::None, schema_job_id_only),
        t("job_cancel", "job.cancel", "Cancel job", "Cancels a job. If attempt_id is supplied, it must still be the current attempt or the cancellation is refused with JOB_NOT_ACTIVE; omitting it preserves job-id-only behavior. Changes committed before cancellation remain, and later changes from the canceled attempt are refused.", CHANGE, Post::None, schema_job_cancel),
        t("job_resume", "job.resume", "Resume job", "Starts a new attempt for a failed or conflicted job.", AGENT_START, Post::None, schema_job_id_only),
        t("proposal_create", "proposal.create", "Create proposal", "Stores a change draft for preview and later apply. Its fields and operations are checked against the target engine's capability report when validated or applied; artifact_validate reports that support. Unsupported values are refused with UNSUPPORTED_OPERATION.", CHANGE_NOT_IDEMPOTENT, Post::None, schema_proposal_create),
        t("proposal_get", "proposal.get", "Get proposal", "Returns a stored proposal with its base revision and diagnostics.", READ, Post::None, schema_proposal_id),
        t("proposal_preview", "proposal.preview", "Preview proposal", "Renders a stored proposal without changing the artifact.", READ, Post::None, schema_proposal_preview),
        t("proposal_apply", "proposal.apply", "Apply proposal", "Applies a stored proposal through the normal change-set checks, including engine capability and shared design-system group checks.", CHANGE, Post::None, schema_proposal_apply),
        t("events_poll", "events.poll", "Poll events", "Returns the events after a sequence number. resync_required means re-read the context.", READ, Post::None, schema_events_poll),
        t("ui_state_read", "ui.state.read", "Read GUI state", "Reads what the live GUI view shows: screen, artifact, revision, page, selection, zoom, viewport, panels, tool and detail, with its state_seq and age. Omit view_id for the most recently published live view. UNAVAILABLE means no live GUI view (the app is not open, or its view has expired).", READ, Post::None, schema_ui_view),
        t("ui_command_list", "ui.command.list", "List GUI commands", "Lists the command registry of the live GUI view: each button, menu item and shortcut as a command with id, label, args_schema, intents, permission (read, change, file_write or agent_start), enabled and shortcut. A command with enabled false is refused when run. UNAVAILABLE means no live GUI view.", READ, Post::None, schema_ui_view),
        t("ui_command_get", "ui.command.get", "Get GUI run", "Reads a GUI run by run_id: status (queued, delivered, completed, failed or timed_out) and its result or error. A run is pending while it is queued or delivered. Only the most recent finished runs stay readable.", READ, Post::None, schema_ui_run_id),
        t("ui_command_run", "ui.command.run", "Run GUI command", "Queues a GUI command as the given actor and returns the run with status queued; read its outcome with ui_command_get. permission is the class this call grants: read, change, file_write or agent_start. A command that needs a higher class is refused with INVALID_ARGUMENT. Desktop file.save and file.export require file_write and an explicit target_path; they forward overwrite and expected_target_hash. Export accepts only explicitly allowed losses and returns its actual revision, path and hash. The Desktop view disables job-starting and other commands that lack safe result tracking; inspect enabled in ui_command_list. Direct artifact_save, artifact_export and job_start remain separate tools. A window started for UI tests (DESIGNLLM_UI_TEST_INPUT) also lists ui.input (synthetic pointer, key and IME input into its next frames; agent_start with content, design and data) and ui.layout (read: screen rectangles of panels, pages, nodes, table cells, memos, layer rows and controls); other windows do not list them. This tool is marked destructive and open world because views can publish commands with broader permissions. Refused at once: UNAVAILABLE when there is no live GUI view, CONTENT_LOCKED when the command's intents are not in allowed_intents, REVISION_CONFLICT when expected_state_seq is stale, and any disabled command.", RUN_ANY, Post::None, schema_ui_run),
        t("ui_screenshot", "ui.screenshot", "Screenshot GUI", "Captures the live GUI window as a PNG and returns it as image content. It queues a ui.screenshot run and waits up to timeout_ms (default 15000, at most 120000) for the GUI to finish. If the run is still pending when the wait ends, the result is an error with the run's status; read the run later with ui_command_get. UNAVAILABLE means no live GUI view.", CHANGE, Post::UiScreenshot, schema_ui_screenshot),
    ]
}

/// Tools visible in `scope`.
pub fn tools_for(scope: Scope) -> Vec<ToolSpec> {
    catalog()
        .into_iter()
        .filter(|tool| match scope {
            Scope::Workspace => true,
            Scope::JobBound => JOB_SCOPE_METHODS.contains(&tool.method) || LOCAL_METHODS.contains(&tool.method),
        })
        .collect()
}

/// Looks up a tool by its MCP name.
pub fn find(tools: &[ToolSpec], name: &str) -> Option<ToolSpec> {
    tools.iter().find(|tool| tool.name == name).copied()
}

impl ToolSpec {
    /// `tools/list` entry for this tool.
    pub fn to_mcp(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": (self.schema)(),
            "annotations": {
                "title": self.title,
                "readOnlyHint": self.hints.read_only,
                "destructiveHint": self.hints.destructive,
                "idempotentHint": self.hints.idempotent,
                "openWorldHint": self.hints.open_world,
            },
        })
    }
}

/// Checks `arguments` against the tool's object schema: required keys and, when the schema forbids
/// extra keys, unknown keys. Value types are checked by the core, which owns the contract types.
pub fn validate_arguments(schema: &Value, arguments: &Value) -> Result<(), String> {
    let object: &Map<String, Value> = arguments
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_string())?;
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(key) {
                return Err(format!("missing required argument: {key}"));
            }
        }
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        let known = schema.get("properties").and_then(Value::as_object);
        for key in object.keys() {
            if !known.is_some_and(|props| props.contains_key(key)) {
                return Err(format!("unknown argument: {key}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CORE_METHODS;
    use std::collections::BTreeSet;

    #[test]
    fn design_theme_tools_are_available_to_job_agents() {
        let tools = tools_for(Scope::JobBound);
        let search = find(&tools, "design_themes_search").unwrap();
        let record = find(&tools, "design_themes_record").unwrap();
        let apply = find(&tools, "design_themes_apply").unwrap();
        let schema = (search.schema)();
        assert_eq!(schema["properties"]["limit"]["maximum"].as_f64(), Some(20.0));
        assert!(validate_arguments(&schema, &json!({"query": "blue", "limit": 10})).is_ok());
        assert!(validate_arguments(&schema, &json!({"query": "blue", "limit": 10, "extra": true})).is_err());
        assert!(validate_arguments(&(record.schema)(), &json!({})).is_err());
        assert_eq!((record.schema)()["definitions"]["DesignThemeEntry"]["properties"]["colors"]["maxProperties"], 32);
        assert!(!record.hints.read_only);
        assert!(!apply.hints.read_only);
        let args = json!({"request_id":"00000000-0000-4000-8000-000000000000", "actor":"agent", "workspace_id":"w", "artifact_id":"a", "base_revision":0, "allowed_intents":["design"], "theme_id":"theme"});
        assert!(validate_arguments(&(apply.schema)(), &args).is_ok());
        let mut preview = args.clone();
        preview["dry_run"] = json!(true);
        assert!(validate_arguments(&(apply.schema)(), &preview).is_ok());
        let mut extra = args;
        extra["extra"] = json!(true);
        assert!(validate_arguments(&(apply.schema)(), &extra).is_err());
        assert!(search.hints.read_only);
    }

    #[test]
    fn names_are_unique_and_use_the_model_safe_alphabet() {
        let tools = catalog();
        let names: BTreeSet<&str> = tools.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), tools.len());
        for name in names {
            assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'), "{name}");
            assert!(name.len() <= 64, "{name}");
        }
    }

    #[test]
    fn every_core_method_except_host_hello_has_exactly_one_tool_and_no_tool_invents_a_method() {
        let tool_methods: Vec<&str> = catalog().iter().map(|t| t.method).collect();
        let distinct: BTreeSet<&str> = tool_methods.iter().copied().collect();
        assert_eq!(distinct.len(), tool_methods.len(), "one tool per method");
        for &method in CORE_METHODS {
            if method == "system.hello" {
                assert!(!distinct.contains(method), "system.hello is not a tool");
                continue;
            }
            assert!(distinct.contains(method), "no MCP tool for core method {method}");
        }
        for method in distinct.iter().filter(|method| !LOCAL_METHODS.contains(method)) {
            assert!(CORE_METHODS.contains(method), "tool maps to a method the core does not have: {method}");
        }
    }

    #[test]
    fn job_bound_scope_exposes_only_scoped_methods_and_never_job_start() {
        let bound = tools_for(Scope::JobBound);
        assert!(bound.iter().any(|t| t.method == "changes.apply"));
        assert!(bound.iter().all(|t| JOB_SCOPE_METHODS.contains(&t.method) || LOCAL_METHODS.contains(&t.method)));
        assert!(find(&bound, "design_guide").is_some(), "job agents can read the guides too");
        assert!(bound.iter().all(|t| t.method != "job.start" && t.method != "host.stop"));
        assert!(find(&bound, "job_start").is_none());
        assert!(find(&bound, "figure_render_link").is_some(), "job-bound agents need the explicitly pinned Figure render route");
        assert!(find(&bound, "changes_apply").is_some());
        assert!(find(&bound, "source_extract_pdf").is_some(), "source methods are in the core's bound list");
        assert!(find(&bound, "design_system_publish").is_none(), "a bound job cannot publish across artifact scope");
        assert!(find(&bound, "design_system_set").is_none(), "a bound job cannot link another artifact");
        assert!(find(&bound, "design_system_list").is_none(), "a bound job cannot enumerate workspace design systems");
    }

    #[test]
    fn job_figure_refs_are_optional_in_mcp_and_documented_as_identity_approval() {
        let tools = tools_for(Scope::Workspace);
        let start = find(&tools, "job_start").expect("job_start");
        let schema = (start.schema)();
        assert_eq!(schema["properties"]["figure_refs"]["maxItems"], json!(128));
        assert!(!schema["required"].as_array().unwrap().iter().any(|field| field == "figure_refs"));
        let request = json!({
            "request_id": "00000000-0000-4000-8000-000000000001", "artifact_id": "document", "base_revision": 0,
            "allowed_intents": ["content"], "runtime": "mock", "model": "m", "effort": "e", "prompt": "place it",
            "figure_refs": [{ "figure_id": "figure-1", "figure_revision": 3 }],
        });
        assert!(validate_arguments(&schema, &request).is_ok());
        assert_eq!(schema["definitions"]["FigureRevisionRef"]["required"], json!(["figure_id", "figure_revision"]));
        assert_eq!(schema["definitions"]["FigureRevisionRef"]["additionalProperties"], false);
        let bound = tools_for(Scope::JobBound);
        assert!(find(&bound, "figure_render_link").is_some(), "pinned jobs can render approved Figures");

        let page = crate::manual::PAGES.iter().find(|page| page.slug == "ai-jobs").expect("AI jobs manual");
        let body = crate::manual::page_body(page);
        assert!(page.tools.contains(&"figure_render_link"));
        assert!(body.contains("figure_refs") && body.contains("source_refs approves asset hashes only"));
    }

    #[test]
    fn job_cancel_schema_accepts_an_optional_attempt_guard_and_keeps_id_only_calls() {
        let tools = tools_for(Scope::Workspace);
        let cancel = find(&tools, "job_cancel").expect("job_cancel");
        let schema = (cancel.schema)();
        assert_eq!(schema["required"], json!(["job_id"]));
        assert_eq!(schema["properties"]["attempt_id"]["type"], "string");
        assert!(validate_arguments(&schema, &json!({ "job_id": "job-1" })).is_ok(), "legacy ID-only calls stay valid");
        assert!(validate_arguments(&schema, &json!({ "job_id": "job-1", "attempt_id": "attempt-1" })).is_ok());
        assert!(validate_arguments(&schema, &json!({ "job_id": "job-1", "extra": true })).is_err());

        let get = find(&tools, "job_get").expect("job_get");
        assert!(validate_arguments(&(get.schema)(), &json!({ "job_id": "job-1", "attempt_id": "attempt-1" })).is_err());
        assert!(cancel.description.contains("JOB_NOT_ACTIVE") && cancel.description.contains("omitting it preserves job-id-only behavior"));
    }

    #[test]
    fn contract_backed_schemas_name_contract_fields() {
        let change = find(&tools_for(Scope::Workspace), "changes_apply").unwrap();
        let schema = (change.schema)().to_string();
        assert!(schema.contains("allowed_intents"), "{schema}");
        assert!(schema.contains("base_revision"), "{schema}");
        assert!(schema.contains("request_id"), "{schema}");

        let publish = find(&tools_for(Scope::Workspace), "design_system_publish").unwrap();
        let publish_schema = (publish.schema)();
        assert!(validate_arguments(&publish_schema, &json!({
            "request_id":"<uuid>","actor":"agent","workspace_id":"<workspace>","artifact_id":"<artifact>",
            "expected_revision":1,"publish":true,"allowed_intents":["design"]
        })).is_ok(), "publish uses its public contract schema");
        assert!(publish_schema["properties"].get("expected_revision").is_some());

        let set = find(&tools_for(Scope::Workspace), "design_system_set").unwrap();
        let set_schema = (set.schema)();
        let base = json!({
            "request_id":"<uuid>","actor":"agent","workspace_id":"<workspace>","artifact_id":"<artifact>",
            "expected_revision":1,"allowed_intents":["design"]
        });
        assert!(validate_arguments(&set_schema, &base).is_ok(), "missing optional design_system detaches");
        let mut detach = base.clone();
        detach["design_system"] = Value::Null;
        assert!(validate_arguments(&set_schema, &detach).is_ok(), "null design_system detaches");
        assert!(set_schema["properties"].get("design_system").is_some());
        let expected_system_revision = &set_schema["properties"]["expected_system_revision"];
        assert!(expected_system_revision["type"] == "integer"
            || expected_system_revision["type"].as_array().is_some_and(|types| types.iter().any(|kind| kind == "integer")));
        assert!(set_schema["required"].as_array().unwrap().iter().all(|field| field != "expected_system_revision"), "the publisher revision guard is optional for older callers");
        assert!(set_schema["required"].as_array().unwrap().iter().all(|field| field != "direct_formatting"), "the policy is optional for Canvas and WebCode");
        let mut keep = base.clone();
        keep["design_system"] = json!("<publisher>");
        keep["direct_formatting"] = json!("keep");
        assert!(validate_arguments(&set_schema, &keep).is_ok(), "a DOCX or HWPX link states its direct-formatting policy");
        keep["direct_formatting"] = json!("clear");
        assert!(validate_arguments(&set_schema, &keep).is_ok());
        assert!(set_schema["properties"].get("direct_formatting").is_some());
        let policy = set_schema.to_string();
        assert!(policy.contains("\"keep\"") && policy.contains("\"clear\""), "the schema names both policies: {policy}");
        assert!(set.description.contains("direct_formatting"));

        let list = find(&tools_for(Scope::Workspace), "design_system_list").unwrap();
        assert!(validate_arguments(&(list.schema)(), &json!({})).is_ok());
        assert!(validate_arguments(&(list.schema)(), &json!({"extra":true})).is_err());
    }

    #[test]
    fn proposal_schema_keeps_contract_references_resolvable_at_the_root() {
        let schema = schema_proposal_create();
        assert!(schema.get("definitions").is_some(), "definitions moved to the root");
        assert!(schema["properties"].get("definitions").is_none());
        assert_eq!(schema["required"], json!(["change_set"]));
    }

    #[test]
    fn every_tool_declares_all_four_hints_explicitly() {
        for tool in catalog() {
            let mcp = tool.to_mcp();
            let annotations = &mcp["annotations"];
            for key in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
                assert!(annotations.get(key).is_some_and(Value::is_boolean), "{} lacks {key}", tool.name);
            }
            if tool.hints.read_only {
                assert!(!tool.hints.destructive, "{} is read-only and destructive", tool.name);
            }
        }
    }

    #[test]
    fn argument_validation_rejects_missing_and_unknown_keys() {
        let schema = schema_asset_read();
        assert!(validate_arguments(&schema, &json!({"sha256": "a".repeat(64)})).is_ok());
        assert!(validate_arguments(&schema, &json!({})).unwrap_err().contains("sha256"));
        assert!(validate_arguments(&schema, &json!({"sha256": "x", "extra": 1})).unwrap_err().contains("extra"));
        assert!(validate_arguments(&schema, &json!([])).is_err());
    }

    #[test]
    fn events_poll_matches_the_documented_optional_cursor() {
        let schema = schema_events_poll();
        assert!(validate_arguments(&schema, &json!({})).is_ok(), "after_seq defaults to 0");
        assert_eq!(schema["properties"]["limit"]["maximum"], 1000);
    }

    #[test]
    fn source_import_csv_schema_declares_the_optional_columns() {
        let schema = schema_source_import_csv();
        assert_eq!(schema["required"], json!(["sha256"]), "columns stays optional");
        let columns = &schema["properties"]["columns"];
        assert_eq!(columns["type"], "array");
        assert_eq!(columns["minItems"], 1);
        let item = &columns["items"];
        assert_eq!(item["type"], "object");
        assert_eq!(item["additionalProperties"], false);
        assert_eq!(item["required"], json!(["name", "data_type"]));
        for key in ["id", "name", "data_type", "unit"] {
            assert!(item["properties"].get(key).is_some(), "column property {key}");
        }
        assert_eq!(item["properties"]["unit"]["type"], json!(["string", "null"]));
    }

    #[test]
    fn source_import_csv_accepts_columns_and_keeps_refusing_unknown_top_level_keys() {
        let schema = schema_source_import_csv();
        let with_columns = json!({"sha256": "a".repeat(64), "columns": [{"name": "x", "data_type": "numeric", "unit": "mm"}]});
        assert!(validate_arguments(&schema, &with_columns).is_ok());
        assert!(validate_arguments(&schema, &json!({"sha256": "a".repeat(64)})).is_ok(), "absent columns stay valid");
        let unknown = validate_arguments(&schema, &json!({"sha256": "a".repeat(64), "delimiter": ","}));
        assert!(unknown.unwrap_err().contains("delimiter"));
    }

    #[test]
    fn every_required_key_of_every_tool_is_a_declared_property() {
        for tool in catalog() {
            let schema = (tool.schema)();
            let Some(required) = schema.get("required").and_then(Value::as_array) else {
                continue;
            };
            let properties = schema.get("properties").and_then(Value::as_object);
            for key in required.iter().filter_map(Value::as_str) {
                assert!(properties.is_some_and(|p| p.contains_key(key)), "{} requires the undeclared key {key}", tool.name);
            }
        }
    }

    #[test]
    fn figure_dataset_edit_schema_matches_the_core_request() {
        let schema = schema_dataset_edit();
        let properties = schema["properties"].as_object().expect("properties");
        assert!(!properties.contains_key("operations"), "figure.dataset.edit takes edits, not operations");
        assert!(!properties.contains_key("read_dependencies"));
        let required: Vec<&str> = schema["required"].as_array().expect("required").iter().filter_map(Value::as_str).collect();
        for key in ["request_id", "actor", "workspace_id", "artifact_id", "base_revision", "allowed_intents", "dataset_id", "version_hash", "edits"] {
            assert!(required.contains(&key), "{key} is required");
        }
        assert!(!required.contains(&"operations") && !required.contains(&"read_dependencies"));
        assert_eq!(schema["properties"]["edits"]["items"]["properties"]["type"]["const"], "edit_cell");
    }

    #[test]
    fn the_tool_names_are_fixed_for_this_build() {
        let names: Vec<&str> = catalog().iter().map(|tool| tool.name).collect();
        assert_eq!(
            names,
            [
                "design_guide", "workspace_list", "artifact_create", "artifact_open", "artifact_inspect", "artifact_validate",
                "artifact_save_state", "artifact_save", "artifact_export", "changes_validate", "changes_apply",
                "design_system_list", "design_themes_search", "design_themes_record", "design_themes_apply", "design_system_publish", "design_system_set",
                "history_undo", "history_redo", "history_undo_change", "render_get", "render_hit_test", "render_caret_rect",
                "render_selection_rects", "asset_import", "asset_read", "asset_lookup", "selection_set",
                "targets_reserve", "targets_release", "targets_list", "clients_list", "templates_list", "templates_preview", "flow_styles", "view_transform_set", "context_read", "figure_render_link",
                "source_extract_pdf", "source_import_csv", "figure_dataset_edit", "web_materialize", "job_start",
                "job_get", "job_cancel", "job_resume", "proposal_create", "proposal_get", "proposal_preview",
                "proposal_apply", "events_poll", "ui_state_read", "ui_command_list", "ui_command_get", "ui_command_run",
                "ui_screenshot",
            ],
            "a tool name changed; update this list deliberately"
        );
    }

    #[test]
    fn the_change_schemas_mark_v12_items_with_capability_notes() {
        for tool in ["changes_validate", "changes_apply", "proposal_create"] {
            let schema = (find(&tools_for(Scope::Workspace), tool).expect("tool").schema)();
            let defs = &schema["definitions"];
            for (definition, properties, _) in V12_PROPERTY_GROUPS {
                for field in *properties {
                    let text = defs[*definition]["properties"][*field]["description"].as_str().unwrap_or_default();
                    assert!(
                        text.contains("UNSUPPORTED_OPERATION") && text.contains("capability"),
                        "{tool}: {definition}.{field} is not marked: {text:?}"
                    );
                }
            }
            for (definition, variant, _) in V12_VARIANTS {
                let option = defs[*definition]["oneOf"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|option| variant_names(option).contains(variant))
                    .unwrap_or_else(|| panic!("{tool}: {definition}.{variant} is missing"));
                let text = option["description"].as_str().unwrap_or_default();
                assert!(text.contains("UNSUPPORTED_OPERATION"), "{tool}: {definition}.{variant} is not marked: {text:?}");
                if *definition == "Interaction" && *variant == "open_url" {
                    assert!(text.contains("Canvas supports OpenUrl") && text.contains("https:") && text.contains("mailto:") && text.contains("tel:"), "{tool}: OpenUrl support note is incomplete: {text:?}");
                }
                if *definition == "Interaction" && *variant == "back" {
                    assert!(text.contains("Canvas supports Back") && text.contains("exported HTML") && text.contains("does nothing") && text.contains("browser history"), "{tool}: Back support note is incomplete: {text:?}");
                }
            }
        }
    }

    #[test]
    fn the_change_tools_say_how_a_refusal_looks_and_where_the_capability_report_is() {
        for tool in ["changes_validate", "changes_apply", "proposal_create"] {
            let spec = find(&tools_for(Scope::Workspace), tool).expect("tool");
            assert!(spec.description.contains("UNSUPPORTED_OPERATION"), "{tool} must name the refusal");
            assert!(spec.description.contains("artifact_validate"), "{tool} must name where the capability report is");
        }
        let apply = find(&tools_for(Scope::Workspace), "changes_apply").expect("tool");
        assert!(apply.description.contains("revision does not change"), "a refused change set leaves the revision unchanged");
    }

    #[test]
    fn v11_change_sets_keep_their_required_keys_and_still_pass_the_argument_check() {
        let schema = (find(&tools_for(Scope::Workspace), "changes_apply").expect("tool").schema)();
        let mut required: Vec<&str> =
            schema["required"].as_array().expect("required").iter().filter_map(Value::as_str).collect();
        required.sort_unstable();
        assert_eq!(required, ["actor", "allowed_intents", "artifact_id", "base_revision", "operations", "request_id", "workspace_id"]);
        let v11 = json!({
            "request_id": "00000000-0000-4000-8000-000000000001",
            "actor": "agent",
            "workspace_id": "w",
            "artifact_id": "a",
            "base_revision": 1,
            "allowed_intents": ["content"],
            "operations": [{"op": "set_text", "node_id": "n", "text": "t"}],
        });
        assert!(validate_arguments(&schema, &v11).is_ok());
    }
}
