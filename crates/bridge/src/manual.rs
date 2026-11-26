//! MCP resources and prompts: the usage manual for AI clients (U11-3) and the frame presets.
//!
//! Everything here is static text or data from `designllm_contracts`, plus the tool list from [`crate::catalog`].
//! Serving it makes no engine or host call. Each page names the tools its procedure uses and shows example calls.
//! The tests check those tools and examples against the tool list and the tool input schemas, so the manual cannot
//! name a tool or an argument that does not exist.

use designllm_contracts::{ArtifactKind, NaturalUnit, EMU_PER_CSS_PX, EMU_PER_INCH, EMU_PER_MM, EMU_PER_PT, FRAME_PRESETS};
use serde_json::{json, Value};

use crate::catalog::{tools_for, Scope};

/// URI of the frame presets resource (JSON).
pub const PRESETS_URI: &str = "designllm://presets";
/// URI prefix of the manual pages (`designllm://manual/<slug>`, text/markdown).
pub const MANUAL_PREFIX: &str = "designllm://manual/";
const MARKDOWN: &str = "text/markdown";
const JSON_MIME: &str = "application/json";

/// Operation names by the intent `Operation::intent()` requires (docs/P0_CONTRACT_V1_2.md, contracts `Operation::intent`).
pub const OPERATION_INTENTS: &[(&str, &[&str])] = &[
    (
        "design",
        &[
            "set_style", "set_text_style_range", "set_named_style", "set_geometry", "set_paragraph_format",
            "set_page_setup", "set_table_dimensions", "set_canvas_table_column_widths", "arrange_panels", "set_layout", "set_axis_ticks",
            "put_token", "remove_token", "set_token_mode", "bind_token", "unbind_token",
            "set_variant_style", "set_page_size", "set_node_appearance", "set_figure_image_transform",
            "set_plot_display", "set_canvas_page_guides", "set_canvas_image_crop", "set_theme",
        ],
    ),
    (
        "content",
        &[
            "replace_text", "set_text", "add_node", "remove_node", "insert_flow_block", "insert_page", "remove_page",
            "reorder_pages", "set_page_notes", "connect_diagram", "add_diagram_node", "add_panel", "remove_panel",
            "reorder_panels", "set_caption", "put_evidence", "set_axis_label", "set_panel_label", "update_figure_link",
            "set_interaction", "set_interactions", "set_canvas_table_structure", "replace_web_source", "mapped_web_edit", "move_node", "set_z_order", "set_node_info", "set_node_list", "set_node_path", "rename_page",
            "set_page_role", "instantiate_component", "detach_instance", "set_diagram_provenance", "set_common_legend",
            "set_figure_visual_review", "replace_image_asset", "set_canvas_board_memos", "set_canvas_page_sections",
            "set_group_repeat", "set_group_condition",
        ],
    ),
    ("data", &["set_plot", "put_dataset", "set_figure_image_calibration"]),
];

/// One manual page.
pub struct Page {
    /// Last part of the URI, and the prompt name with `-` replaced by `_` (task pages only).
    pub slug: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Whether the page is a task kind, and so also a prompt.
    pub task: bool,
    /// Tools the procedure uses. Checked against the tool list by the tests.
    pub tools: &'static [&'static str],
    /// The procedure, one step per line.
    pub steps: &'static [&'static str],
    /// Example calls: (tool, arguments as JSON text). Checked against the tool input schemas by the tests.
    pub examples: &'static [(&'static str, &'static str)],
}

const INDEX: Page = Page {
    slug: "index",
    title: "DesignLLMTool: how to use this server",
    description: "Start here. Lists the task pages, the common procedure, and the rules that apply to every task.",
    task: false,
    tools: &["design_guide", "artifact_save", "history_undo",
        "workspace_list", "changes_validate", "changes_apply", "history_undo_change", "artifact_validate", "render_get", "asset_lookup", "asset_read",
        "artifact_export",
    ],
    steps: &[
        "For design work, start with the design-workflow page and the task guide. Every task prompt includes the shared design workflow. The design_guide tool returns the same guides for clients that do not read MCP resources or prompts. The first change to an artifact is refused with GUIDE_REQUIRED until its guide has been delivered on this connection; the refusal carries the guide, so read it and send the call again.",
        "Read the page for your task before you call tools. Task pages: mobile-screen, landing-page, social-post, poster, slides, document-edit, paper-diagram (model architecture, pipeline, method overview), paper-figure (data plots), web-code. Shared Canvas design systems have a procedure on the design-system page; non-rectangular shapes are on the shapes page.",
        "Call workspace_list first. It returns workspace_id and each artifact with its current revision. A workspace-scoped response may include project_folder, artifacts_folder, and workspace_notices about migration, backup, or recovery. Read notices when present.",
        "For local project work, start DesignLLMTool with --project <existing folder>; a workspace MCP client adds --mcp. Each committed revision is stored in the project's .designllm folder and can be opened again through the workspace from any MCP client. When the user wants a native file, use artifact_save without target_path to save or update the managed copy in the project's artifacts folder.",
        "When another owner holds a node, a change to it is refused with TARGET_BUSY. The collaboration page explains reservations and job scope.",
        "Every edit is one ChangeSet: request_id (a new UUID; send the same one again only when you repeat the same call), actor, workspace_id, artifact_id, base_revision (the current revision), allowed_intents, operations.",
        "The host records your actor under your connection: \"agent\" is recorded as mcp:<connection id>:agent, so you cannot act as desktop or another connection. Repeating a request with the same request_id after a reconnect returns its receipt.",
        "allowed_intents must include the intent of every operation. See the intents page.",
        "Call changes_validate, then changes_apply. Read the diagnostics. A refused change set leaves the revision unchanged.",
        "To take back one change batch by its change_id (the request_id of that change), use history_undo_change. history_undo reverts the most recent one.",
        "Before you use a v1.2 field or operation, read the refusals page, and use artifact_validate to see what this engine supports.",
        "Sizes are EMU: 1 px = 9525, 1 mm = 36000, 1 in = 914400, 1 pt = 12700. See the presets page.",
        "Check the result with render_get (a PNG of one page), then finish with artifact_export or artifact_save.",
        "asset_lookup returns the metadata of an asset by its sha256. asset_read reads its bytes in verified chunks and returns images as image content.",
    ],
    examples: &[("workspace_list", "{}")],
};

const REFUSALS: Page = Page {
    slug: "refusals",
    title: "Refusals and capabilities",
    description: "How an engine refuses a value it does not support, and how to find out what it supports.",
    task: false,
    tools: &["artifact_validate", "changes_validate", "changes_apply"],
    steps: &[
        "An engine refuses a field or operation it does not support with UNSUPPORTED_OPERATION. The message names each refused item. Each one also has a blocking diagnostic: UNSUPPORTED_FIELD for a field, OPERATION_UNSUPPORTED for an operation.",
        "A refused change set is not applied, so the revision does not change. Send the corrected change set with a new request_id: the same request_id returns the stored refusal.",
        "TARGET_BUSY (a node another owner holds) and CONTENT_LOCKED (an operation outside a job's target scope) refuse the change set the same way. The collaboration page says what each one means.",
        "artifact_validate returns capabilities: one entry per feature (for example Effects, VectorPaths, GridLayout), each with read, render, edit, preserve, write and a reason.",
        "Use only the fields and operations whose feature this engine supports. Do not retry a refused value unchanged.",
        "Feature names used by v1.2: Effects covers corner radius, opacity, shadows, gradients, and text and stroke extras. VectorPaths covers path nodes and icons. GridLayout covers layout mode grid and the new layout fields.",
        "Canvas text lists are reported separately as the string-keyed capability_extensions.lists in artifact.validate and context.read. It is a live support report and is not part of the persisted Artifact.capabilities map.",
        "Canvas components are reported separately as capability_extensions.components. This build supports local component pages and same-document instances with text/style overrides, source refresh and detach; nested and cross-workspace component references are refused.",
        "A non-empty clear in a style patch is refused unless the engine supports clearing that field.",
        "Page names, the new operations (set_page_size, move_node, set_z_order, set_node_appearance, set_node_info, set_node_list, set_interactions, rename_page) and the new interactions (open_url, back) follow the same rule.",
        "On Canvas, set_interactions replaces the node's complete interaction list in the supplied order; an empty list clears it. The existing set_interaction operation still replaces the list with one action.",
        "When OpenUrl is combined with other interactions, Canvas HTML playback executes it in the saved action order. The in-app WebView2 preview blocks external navigation and new windows.",
        "Give each OpenUrl link an alternative text or visible label. Path and Image links may use the layer name as a fallback. A Group link does not use its layer name or guess from child text, which can be hidden, duplicated or unrelated. Without its own alternative text or visible text, HTML export reports the non-blocking HTML_LINK_ACCESSIBLE_NAME_MISSING diagnostic.",
        "Canvas HTML playback supports the back interaction within the exported pages; it returns to the previous page in that export and does nothing when no previous in-export page exists. It does not navigate browser history.",
    ],
    examples: &[("artifact_validate", r#"{"artifact_id":"<artifact_id>"}"#)],
};

const INTENTS: Page = Page {
    slug: "intents",
    title: "Intents",
    description: "Which intent each operation needs in allowed_intents.",
    task: false,
    tools: &["changes_apply"],
    steps: &[
        "allowed_intents is a set of design, content and data. A change set is refused when one of its operations needs an intent that is not in the set.",
        "For jobs, the job's allowed_intents limit what its agent may change.",
        "Canvas set_canvas_table_structure is a content operation that replaces one table's ordered row and cell ids. AddNode or RemoveNode operations for changed cells must be in the same ChangeSet; one ChangeSet may replace each table only once. This build supports equal-width columns without merged cells. Engines without Canvas table support refuse this operation.",
        "Canvas set_canvas_table_column_widths is a design operation. Provide one positive integer relative weight per column; null restores equal-width tracks. A one-column insertion uses a deterministic split and a one-column removal merges its weight with a neighbor. Custom widths are supported, but merged cells are not. Width changes on a component source refresh its instances; widths cannot be overridden on an instance, and a locked table cell refuses width propagation.",
        "Operations by intent (the names are the values of the op field):",
    ],
    examples: &[(
        "changes_apply",
        r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<artifact_id>","base_revision":3,"allowed_intents":["content"],"operations":[{"op":"set_canvas_table_structure","table_id":"<table_id>","expected":{"rows":[{"id":"<header_row_id>","cell_ids":["<cell_1>","<cell_2>"]},{"id":"<body_row_id>","cell_ids":["<cell_3>","<cell_4>"]}],"header_row_id":"<header_row_id>"},"table":{"rows":[{"id":"<header_row_id>","cell_ids":["<cell_1>","<cell_2>"]},{"id":"<body_row_id>","cell_ids":["<cell_3>","<cell_4>"]}],"header_row_id":null}}]}"##,
    ), (
        "changes_apply",
        r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<artifact_id>","base_revision":3,"allowed_intents":["design"],"operations":[{"op":"set_canvas_table_column_widths","table_id":"<table_id>","expected":null,"column_widths":[1,2]}]}"##,
    )],
};

const DESIGN_SYSTEM: Page = Page {
    slug: "design-system",
    title: "Shared Canvas design systems",
    description: "Publish a Canvas artifact's local tokens. Canvas consumers bind shared tokens; a WebCode consumer links and gets the same values in design-tokens.css; a DOCX or HWPX consumer applies them to its named styles.",
    task: false,
    tools: &[
        "artifact_validate", "design_system_list", "design_system_publish", "design_system_set", "changes_apply",
        "history_undo", "history_redo", "history_undo_change",
    ],
    steps: &[
        "Check artifact_validate for capability_extensions.design_tokens on a Canvas artifact. Canvas publishes shared token systems; Canvas, WebCode, DOCX and HWPX consumers link to them; other engines refuse these methods or fields.",
        "Call design_system.list to find published Canvas systems. It returns artifact_id, title and current revision, not token values; it is workspace-scoped and unavailable to job-bound connections.",
        "Create local token definitions on the publisher with changes_apply (PutToken), then call design_system_publish with publish=true, the current expected_revision, and allowed_intents [design]. A publisher cannot consume another system.",
        "On each Canvas consumer, call design_system_set with the publisher artifact_id and the expected_system_revision returned by design_system.list. Core rechecks this revision inside the commit transaction; a stale choice is REVISION_CONFLICT. Bind its node with changes_apply (BindToken, token.source=shared). The reference pins the publisher revision; Canvas resolves the active Light or Dark value. On a WebCode consumer, call design_system_set the same way with allowed_intents [design, content] and no BindToken: Core adds the stylesheet link to the entry file and writes design-tokens.css, so the link changes content too.",
        "On a DOCX or HWPX consumer, call design_system_set the same way with allowed_intents [design]; it needs direct_formatting keep or clear. Each published token named `<style name>.color`, `.font_family` or `.font_size` sets that named style (light value). keep leaves the document's direct formatting as it is; clear removes it from the text that uses those styles (DOCX only; HWPX refuses clear). A spacing or radius token, any other name, or a value the style cannot take refuses the whole link; a style the document lacks is reported and skipped. Publisher updates reach linked documents with keep. Canvas and WebCode take no direct_formatting (keep is accepted, clear is refused).",
        "Change a published token definition through changes_apply on the publisher. Core updates the publisher and all current consumers in one atomic group. Each consumer keeps its own Light/Dark mode. A lock, reservation, revision conflict, invalid binding, or unsupported engine refuses the entire group.",
        "When a group is committed, ChangeResult has group={group_id,members:[{artifact_id,revision,changed_ids}]}. The ordinary fields still identify the publisher result. Without propagation, group is omitted.",
        "Workspace-scoped history.undo, history.redo, and history.undo_change from any participant operate on the whole group. A later edit to any member blocks a partial revert with REVISION_CONFLICT. Job-bound calls cannot change a group outside their single-artifact scope.",
        "To detach a consumer, call design_system_set with design_system=null. Canvas keeps resolved concrete values and removes shared bindings. A WebCode consumer keeps its design-tokens.css values and loses only the link. A DOCX or HWPX detach takes no direct_formatting and keeps the document as it is. Unpublish only after all consumers detach.",
    ],
    examples: &[
        ("design_system_publish", r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<publisher_id>","expected_revision":2,"publish":true,"allowed_intents":["design"]}"##),
        ("design_system_list", "{}"),
        ("design_system_set", r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<consumer_id>","expected_revision":0,"design_system":"<publisher_id>","expected_system_revision":2,"allowed_intents":["design"]}"##),
        ("design_system_set", r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<consumer_id>","expected_revision":1,"design_system":null,"allowed_intents":["design"]}"##),
        ("design_system_set", r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<webcode_id>","expected_revision":1,"design_system":"<publisher_id>","expected_system_revision":2,"allowed_intents":["design","content"]}"##),
        ("design_system_set", r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<docx_id>","expected_revision":0,"design_system":"<publisher_id>","expected_system_revision":2,"allowed_intents":["design"],"direct_formatting":"keep"}"##),
        ("changes_apply", r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<publisher_id>","base_revision":3,"allowed_intents":["design"],"operations":[{"op":"put_token","token_id":"brand.ink","name":"Brand ink","values":{"kind":"color","light":"#112233","dark":"#445566"}}]}"##),
        ("changes_apply", r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<consumer_id>","base_revision":1,"allowed_intents":["design"],"operations":[{"op":"bind_token","node_ids":["title"],"target":{"kind":"style_color"},"token":{"source":"shared","token_id":"brand.ink"}}]}"##),
    ],
};

const PRESETS: Page = Page {
    slug: "presets",
    title: "Frame presets and templates",
    description: "Frame sizes for artifact_create and how to discover the templates in this build.",
    task: false,
    tools: &["artifact_create", "templates_list"],
    steps: &[
        "artifact_create takes size as EMU: { width, height }. 1 px = 9525 EMU, 1 mm = 36000 EMU, 1 in = 914400 EMU, 1 pt = 12700 EMU.",
        "Preset ids, natural sizes and EMU sizes are in the designllm://presets resource (JSON). Preset ids:",
        "Call templates.list for template ids available in this build. Use a returned id with artifact_create; omit template when the list is empty.",
    ],
    examples: &[("artifact_create", r##"{"request_id":"<new UUID>","actor":"agent","kind":"canvas","title":"Screen","size":{"width":3714750,"height":8039100}}"##)],
};

const AI_JOBS: Page = Page {
    slug: "ai-jobs",
    title: "AI jobs",
    description: "A job runs a Claude or Codex CLI on one artifact. The job's agent changes the artifact through MCP.",
    task: false,
    tools: &["targets_list", "changes_apply",
        "job_start", "job_get", "job_cancel", "job_resume", "events_poll", "targets_reserve", "history_undo", "figure_render_link",
        "proposal_create", "proposal_get", "proposal_preview", "proposal_apply",
    ],
    steps: &[
        "job_start queues one job for one artifact. runtime is claude, agy or codex. model and effort are the exact values you want. The runtime checks effort when the job starts: an effort it does not report fails the job with UNSUPPORTED_OPERATION, and no other model or effort is used.",
        "figure_refs is the explicit approval for Figure identity and exact revision: pass {figure_id, figure_revision} pairs when the job may render or link those Figures. source_refs approves asset hashes only. A job-bound figure_render_link and a new FigureLink accept only a pinned pair.",
        "The job's agent gets its own MCP connection with only these tools:",
        "The host checks the model the CLI reports. Claude's init line must name the requested model; a dated snapshot of the same id (claude-haiku-4-5-20251001 for claude-haiku-4-5) is accepted. Codex's model is checked when its stream reports one. A mismatch stops the run and fails the job. Effort is only passed on the command line; neither CLI reports it back.",
        "Watch job_get for the state and events_poll for job.status events. A job's changes are ordinary change sets: history_undo reverts the last one.",
        "For a race-safe cancel, pass both job_id and the current attempt_id from job_get to job_cancel. If job_resume has created a newer attempt, the old attempt_id is refused with JOB_NOT_ACTIVE; omitting it preserves the legacy job-id-only call.",
        "targets_reserve blocks changes to those nodes by other actors until you release them. A job cannot pass a reservation.",
        "Only your connection can release your reservation, targets_list shows reservation_id only for yours, and your reservations end when your MCP connection closes.",
        "job_resume starts a new attempt for a failed or conflicted job (job_id only). Read the new attempt_id from job_get afterwards.",
        "A proposal is a change draft that does not touch the artifact until it is applied. proposal_create stores it (same ChangeSet fields as changes_apply); proposal_get returns it with its base revision and diagnostics; proposal_preview renders it on its own base revision without changing the artifact (if the artifact has moved on since, a validated proposal becomes conflict with REVISION_CONFLICT); proposal_apply applies it through the normal change-set checks, so engine capability, intents and shared design-system group checks still apply and a refusal leaves the revision unchanged. A proposal written by a job passes that job's own reservations; a node another owner holds still refuses it with TARGET_BUSY (proposal_preview does not check reservations).",
        "Use a proposal when a person should look at the result before it is applied. Use changes_apply when the change should land at once.",
    ],
    examples: &[
        (
            "job_start",
            r##"{"request_id":"<new UUID>","artifact_id":"<artifact_id>","base_revision":2,"allowed_intents":["content","design"],"runtime":"claude","model":"claude-haiku-5-5","effort":"low","prompt":"Place the approved chart.","source_refs":[],"figure_refs":[{"figure_id":"<figure_id>","figure_revision":4}]}"##,
        ),
        ("job_get", r##"{"job_id":"<job_id>"}"##),
        ("job_cancel", r##"{"job_id":"<job_id>","attempt_id":"<attempt_id>"}"##),
        ("events_poll", r##"{"after_seq":0,"limit":100}"##),
    ],
};

const COLLABORATION: Page = Page {
    slug: "collaboration",
    title: "Collaboration: reservations, busy nodes and job scope",
    description: "A user and an agent can work on one artifact at the same time. Reservations keep their changes apart: a change that touches a node another owner holds is refused with TARGET_BUSY, and a job's changes outside its target nodes are refused with CONTENT_LOCKED.",
    task: false,
    tools: &["targets_list", "targets_reserve", "targets_release", "job_start", "changes_apply", "clients_list"],
    steps: &[
        "targets_list returns the reservations of one artifact: reservation_id, view_id, owner and node_ids. A user's reservation has expires_in_ms. A job's reservation has job_id instead.",
        "targets_reserve holds nodes for one owner and view while the user edits them. It needs expected_revision, the current revision, or it is refused with REVISION_CONFLICT. Reserving again with the same owner and view renews the reservation and replaces its nodes. A user's reservation expires unless it is renewed.",
        "targets_release ends a user's reservation. A job's reservation cannot be released with targets_release: it ends when the job ends.",
        "job_start with target_ids reserves those targets and what lies inside them for the job while it is queued, running or validating. A section holds its paragraphs and tables, a page holds its nodes. job_start is refused with TARGET_BUSY when another reservation holds any of them. Only that job's own changes pass its reservation.",
        "A change set that touches a node another owner holds is refused with TARGET_BUSY, including a node inside a target a job holds. The message names the owner, affected_ids lists the nodes, and nothing is applied.",
        "A job with target_ids may change only its target nodes, what lies inside them, and what it created. Any other operation, and any operation on the whole document, refuses the whole change set with CONTENT_LOCKED. affected_ids lists the outside ids, or the artifact id for a whole-document operation. Nothing is applied.",
        "A job's undo or redo of a change outside its scope is refused with CONTENT_LOCKED.",
        "A job's MCP connection has no targets_list and no targets_reserve. A job learns that a node is busy from the TARGET_BUSY answer.",
        "clients_list shows who is connected to the host: role, client name and version, the job of a job connection, and when the host last saw it. A job connection sees only the names. Use it to tell whether the GUI or a job is working on the workspace.",
    ],
    examples: &[
        ("targets_list", r#"{"artifact_id":"<artifact_id>"}"#),
        ("targets_release", r#"{"reservation_id":"<reservation_id>"}"#),
    ],
};

const TEMPLATES: Page = Page {
    slug: "templates",
    title: "Templates",
    description: "The templates engines create artifacts from. List them, preview one without creating an artifact, then create from it.",
    task: false,
    tools: &["templates_list", "templates_preview", "artifact_create"],
    steps: &[
        "templates_list lists the templates of the registered engines: id, kind, title, preset (the frame preset of the first page, when there is one) and size (the size to create the template with, when the engine gives one).",
        "templates_preview renders page 0 of one template as a PNG without creating an artifact. The PNG comes back as image content, and the answer has the asset hash and the page size. Pass kind only when two engines use the same template id.",
        "A preview is cached per template and dpi, so the same template and dpi give the same asset.",
        "artifact_create with template set to the id creates the artifact from that template. Pass size as the template's size when templates_list shows one.",
        "A template draws the same page every time it is created.",
    ],
    examples: &[
        ("templates_list", "{}"),
        ("templates_preview", r#"{"template":"<template_id>","dpi":72}"#),
    ],
};

const MOBILE_SCREEN: Page = Page {
    slug: "mobile-screen",
    title: "Mobile screen",
    description: "One phone screen as a canvas artifact, exported as PNG, PDF, SVG or HTML.",
    task: true,
    tools: &["artifact_create", "artifact_inspect", "changes_validate", "changes_apply", "render_get", "artifact_export"],
    steps: &[
        "artifact_create with kind canvas and the phone preset size: 390 x 844 px = 3714750 x 8039100 EMU. phone_large (430 x 932 px) is the other common screen size.",
        "artifact_inspect the artifact to get its page and node ids, and its revision.",
        "Build the screen: add_node (content: page_id and node), set_text (content), set_geometry (design: bounds), set_style (design: fill, color, font_size, align), set_layout (design: row or column; absolute is the default).",
        "Use corner_radius and the other Effects fields only if the capability report lists Effects as supported.",
        "changes_validate, then changes_apply with the revision you read. allowed_intents needs content and design.",
        "render_get the page. Check that text fits, that controls are at least 44 px high, and that text contrast is at least 4.5:1. These are design targets; the engine does not enforce them.",
        "artifact_export with format png (or pdf, svg, html). required_editability is the least editability you accept. The result reports the editability and any losses.",
    ],
    examples: &[
        (
            "artifact_create",
            r##"{"request_id":"<new UUID>","actor":"agent","kind":"canvas","title":"Mobile screen","size":{"width":3714750,"height":8039100}}"##,
        ),
        (
            "changes_apply",
            r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<artifact_id>","base_revision":1,"allowed_intents":["content","design"],"operations":[{"op":"set_text","node_id":"<node_id>","text":"Sign in"},{"op":"set_style","node_ids":["<node_id>"],"patch":{"fill":"#1A73E8"}}]}"##,
        ),
        (
            "artifact_export",
            r##"{"request_id":"<new UUID>","options":{"artifact_id":"<artifact_id>","expected_revision":2,"format":"png","required_editability":"none","allowed_losses":[]},"target_path":"<absolute path>.png","overwrite":false}"##,
        ),
    ],
};

const LANDING_PAGE: Page = Page {
    slug: "landing-page",
    title: "Landing page",
    description: "A desktop-width page with sections, as a canvas artifact, exported as PDF or HTML.",
    task: true,
    tools: &["artifact_create", "artifact_inspect", "changes_validate", "changes_apply", "render_get", "artifact_export"],
    steps: &[
        "artifact_create with kind canvas and the desktop preset: 1440 x 900 px = 13716000 x 8572500 EMU.",
        "artifact_inspect the artifact to get its page and node ids, and its revision.",
        "Build sections with row and column layouts (set_layout, design), then text and shapes (add_node, set_text, set_geometry, set_style).",
        "changes_validate, then changes_apply with the revision you read. allowed_intents needs content and design.",
        "render_get each page you need to check.",
        "artifact_export with format pdf or html. Read the editability and losses in the result before you deliver the file.",
    ],
    examples: &[
        (
            "artifact_create",
            r##"{"request_id":"<new UUID>","actor":"agent","kind":"canvas","title":"Landing page","size":{"width":13716000,"height":8572500}}"##,
        ),
        (
            "artifact_export",
            r##"{"request_id":"<new UUID>","options":{"artifact_id":"<artifact_id>","expected_revision":3,"format":"pdf","required_editability":"vector","allowed_losses":[]},"target_path":"<absolute path>.pdf","overwrite":false}"##,
        ),
    ],
};

const SOCIAL_POST: Page = Page {
    slug: "social-post",
    title: "Social post",
    description: "A square, portrait or story frame as a canvas artifact, exported as PNG.",
    task: true,
    tools: &["artifact_create", "artifact_inspect", "changes_validate", "changes_apply", "render_get", "artifact_export"],
    steps: &[
        "artifact_create with kind canvas and a social preset: social_square 1080 x 1080 px, social_portrait 1080 x 1350 px, or story 1080 x 1920 px. EMU sizes are in the presets resource.",
        "artifact_inspect the artifact to get its page and node ids, and its revision.",
        "Build the post with add_node, set_text, set_geometry and set_style (fill, color, font_size, align).",
        "changes_validate, then changes_apply. allowed_intents needs content and design.",
        "render_get and check that no text is cut at the frame edge.",
        "artifact_export with format png (or svg).",
    ],
    examples: &[
        (
            "artifact_create",
            r##"{"request_id":"<new UUID>","actor":"agent","kind":"canvas","title":"Social post","size":{"width":10287000,"height":10287000}}"##,
        ),
        (
            "artifact_export",
            r##"{"request_id":"<new UUID>","options":{"artifact_id":"<artifact_id>","expected_revision":2,"format":"png","required_editability":"none","allowed_losses":[]},"target_path":"<absolute path>.png","overwrite":false}"##,
        ),
    ],
};

const POSTER: Page = Page {
    slug: "poster",
    title: "Poster",
    description: "A print-size canvas artifact measured in millimetres, exported as PDF.",
    task: true,
    tools: &["artifact_create", "artifact_inspect", "changes_validate", "changes_apply", "render_get", "artifact_export"],
    steps: &[
        "artifact_create with kind canvas and a print preset: a3 297 x 420 mm or a4 210 x 297 mm. The natural unit is mm; the size is stored in EMU.",
        "artifact_inspect the artifact to get its page and node ids, and its revision.",
        "Build the poster with add_node, set_text, set_geometry, set_style and set_layout.",
        "changes_validate, then changes_apply. allowed_intents needs content and design.",
        "render_get and check the page.",
        "artifact_export with format pdf and required_editability vector. The result reports the editability and losses.",
    ],
    examples: &[
        (
            "artifact_create",
            r##"{"request_id":"<new UUID>","actor":"agent","kind":"canvas","title":"Poster","size":{"width":10692000,"height":15120000}}"##,
        ),
        (
            "artifact_export",
            r##"{"request_id":"<new UUID>","options":{"artifact_id":"<artifact_id>","expected_revision":2,"format":"pdf","required_editability":"vector","allowed_losses":[]},"target_path":"<absolute path>.pdf","overwrite":false}"##,
        ),
    ],
};

const SLIDES: Page = Page {
    slug: "slides",
    title: "Slides",
    description: "A slide deck as a slides artifact, exported as PPTX.",
    task: true,
    tools: &["artifact_create", "artifact_inspect", "artifact_validate", "changes_validate", "changes_apply", "render_get", "artifact_export"],
    steps: &[
        "artifact_create with kind slides and size slide_16_9: 12192000 x 6858000 EMU (960 x 540 pt).",
        "artifact_inspect the artifact to get its page ids and revision.",
        "Add pages with insert_page (content). Reorder with reorder_pages and remove with remove_page (content). Set speaker notes with set_page_notes (content).",
        "Add text and shapes to a page with add_node and set_text (content).",
        "A shadow on a rectangle or ellipse is read, edited and saved: one shadow, no spread, an offset on one axis. A blur is kept and reported as a loss. A shadow on another kind of node, a second shadow, and the other Effects fields (corner radius, opacity, gradients and so on) are refused with UNSUPPORTED_OPERATION. artifact_validate states the same limits under Effects.",
        "The start and end markers of a line (set_style stroke_start and stroke_end) are read, drawn, edited and saved; a line end on another kind of node is refused with UNSUPPORTED_OPERATION. artifact_validate states this under Effects.",
        "A vector path is created with its geometry and saved as custom geometry (nonzero fill rule, view box at the origin). After creation its position and style can be edited, except its shadow and text formatting; its geometry cannot be edited, it has no text, and an even-odd fill rule is refused with UNSUPPORTED_OPERATION. artifact_validate states the same limits under VectorPaths.",
        "Change the theme with set_theme (design): patch.colors maps the slots dk1, lt1, dk2, lt2, accent1 to accent6, hlink and folHlink to #RRGGBB, and major_latin, minor_latin, major_ea and minor_ea set the heading and body faces (at most 64 characters, no control character, < or &). Shapes that use a theme colour or font follow; the saved PPTX holds the new theme. A deck with more than one slide master is refused with UNSUPPORTED_OPERATION.",
        "changes_validate, then changes_apply. allowed_intents needs content, and design where you change style.",
        "render_get each slide and check that the text fits.",
        "artifact_export with format pptx.",
    ],
    examples: &[
        (
            "artifact_create",
            r##"{"request_id":"<new UUID>","actor":"agent","kind":"slides","title":"Deck","size":{"width":12192000,"height":6858000}}"##,
        ),
        (
            "artifact_export",
            r##"{"request_id":"<new UUID>","options":{"artifact_id":"<artifact_id>","expected_revision":4,"format":"pptx","required_editability":"none","allowed_losses":[]},"target_path":"<absolute path>.pptx","overwrite":false}"##,
        ),
    ],
};

const DOCUMENT_EDIT: Page = Page {
    slug: "document-edit",
    title: "Document edit (DOCX, HWPX, HWP)",
    description: "Create or edit an editable Word or Hangul document and save it in its native format.",
    task: true,
    tools: &[
        "artifact_create", "artifact_open", "artifact_inspect", "changes_validate", "changes_apply", "render_get", "artifact_save", "artifact_export",
        "flow_styles", "artifact_save_state",
    ],
    steps: &[
        "For a new document, create the requested native kind with artifact_create if the workspace does not already provide a blank target. For an existing file, artifact_open copies it into the workspace and infers docx, hwpx or hwp from its extension.",
        DOCUMENT_LAYOUT,
        "artifact_inspect to read the paragraphs and tables with their node ids and the revision.",
        "flow_styles lists the named styles of a DOCX document (read only); other engines refuse it with UNSUPPORTED_OPERATION. For HWPX, artifact_inspect's body carries the named styles.",
        "Edit with changes: replace_text (content), insert_flow_block (content), set_paragraph_format and set_text_style_range (design), set_table_dimensions (design), set_page_setup (design).",
        "changes_validate, then changes_apply. allowed_intents must cover content and design.",
        "A native save is refused with UNSAFE_ROUNDTRIP when the result would lose content the format cannot keep. Nothing is written then.",
        "render_get shows each page for a visual check.",
        "artifact_save_state returns the workspace revision, the last natively saved revision, the target hash, and whether the file changed outside the workspace. Read it before a save that replaces a file.",
        "artifact_save writes the native format to a new path. Replacing an existing file needs overwrite true and expected_target_hash. artifact_export writes pdf, svg, png, pptx or html instead, where the engine writes that format; any other format is refused with UNSUPPORTED_OPERATION. png and svg write one page, so pass page_indices when the document has several.",
    ],
    examples: &[
        (
            "artifact_open",
            r##"{"request_id":"<new UUID>","actor":"agent","path":"<absolute path to the .docx file>"}"##,
        ),
        (
            "artifact_save",
            r##"{"request_id":"<new UUID>","artifact_id":"<artifact_id>","expected_revision":3,"target_path":"<absolute path to the new file>","overwrite":false}"##,
        ),
    ],
};

/// Print-size rules shared by the two paper pages.
const FIGURE_PRINT: &str = "Size the figure for print rather than a screen, and draw it at 1:1 scale. Make single-column figures 84 to 89 mm wide (IEEE 88.9 mm, Nature 89 mm, ACL 77 mm) and double-column figures 175 to 183 mm wide, with height determined by the content. Use 7 to 9 pt for labels and 8 to 10 pt for block or panel names, never go under 6 pt, and use at most three text sizes and one sans-serif font family. If the user names a venue, follow its rules instead (Nature 5 to 7 pt figure text, IEEE about 9 to 10 pt). Set line weights to 0.5 to 1 pt. Unit conversions: 1 mm = 36000 EMU, 1 pt = 12700 EMU.";

/// What a good paper figure leaves out. Shared by the two paper pages.
const FIGURE_NO_EXTRA_TEXT: &str = "Make a paper figure understandable from its layout without auxiliary text. Do not put a title, subtitle, section headers, a source line, or explanatory sentences inside the figure; the caption below the figure in the paper provides the title and explanation. Never write labels such as Overall flow, Inside X, Step 1, Input/Output headers, or Legend when position, grouping, and arrows already show that meaning.";

/// Functional palette for diagrams and categorical palette for data, both away from stock framework colours.
const FIGURE_PALETTE: &str = "Use colour only to encode function or category, never for decoration. Use these diagram roles with a soft fill, a matching stroke, and dark #1C1C1C text: attention or mixing #FCE3C7 / #C77A2B; feed-forward or MLP #D6E6EF / #4F7CA8; normalization #F6EDB8 / #A8922E; embedding or input encoding #E5DDF0 / #7A5C99; output head or loss #D8EBD9 / #4E8A56; the paper's new component (use once) #F7D9D4 / #B4532A; plain operations and data white / #3A3A3A; frames and guides #BDBDBD. For data series, use the colour-blind-safe Okabe-Ito palette (#E69F00, #56B4E9, #009E73, #F0E442, #0072B2, #D55E00, #CC79A7, #000000), use one sequential ramp for ordered values, draw baselines in grey, and give the proposed method the strongest colour. In a Figure artifact the first series takes the panel's fill (set_style) and later series take the renderer's palette; choosing an output profile (the figure.output_profile GUI command) sets the panel fills from Okabe-Ito. Never use stock framework colours (COLOR_STOCK_DEFAULT).";

const PAPER_DIAGRAM: Page = Page {
    slug: "paper-diagram",
    title: "Paper diagram (model architecture, pipeline, method overview)",
    description: "An architecture, pipeline, or method-overview figure in the style of Transformer, ViT, ResNet, U-Net, or VAE figures: blocks, typed shapes, orthogonal arrows, residual joins, and repeated stacks, readable at print size without auxiliary text.",
    task: true,
    tools: &[
        "design_guide", "artifact_create", "artifact_inspect", "changes_validate", "changes_apply", "artifact_validate", "render_get", "artifact_export",
    ],
    steps: &[
        "Draw an architecture or pipeline figure on a Canvas artifact; Canvas provides vector paths (trapezoids, cylinders, circled operators), Line arrows with arrowheads and dashes, and groups. A Figure artifact draws only rectangles, ellipses and math formulas, with straight connectors, in its diagrams, so use it for data plots (paper-figure page).",
        FIGURE_PRINT,
        "Plan before drawing, and state the plan in your progress text: list the blocks, the data passed between them, and one reading direction. Use bottom-to-top (the Transformer) for a layer stack, and left-to-right for a pipeline. Place inputs at the start edge and outputs at the far edge along one main axis; use side branches only for skip connections, conditioning, or losses.",
        FIGURE_NO_EXTRA_TEXT,
        "Align blocks on a grid: give blocks of the same role identical width and height, keep gaps equal (8 to 16 px between blocks, 24 to 32 px between groups), place centres on the flow axis, and align parallel branches edge to edge. Make a one-line block about 1.8 times its font size tall, and add about 1.1 times the font size for each extra line. Leave at least 12 px of margin around the drawing. Do not add a page background panel or a card behind the whole figure, and nest containers at most two levels deep.",
        "Label every block with its name only, in 1 to 3 words (Multi-Head Attention, Add & Norm, Feed Forward, Patch Embedding, Encoder). Do not place sentences, parameter lists, or model sizes inside or under blocks; put those in the caption or paper text. Format math symbols (x, z, Q, K, V, h_t) in italic. Size each block's text box so its longest label fits on one line with at least 4 px padding on every side.",
        "Label an arrow only when the data on it is not obvious from its two ends. Limit the label to 1 or 2 words or a symbol. Place it beside the arrow with at least 4 px clearance, never over a line or a block, in a text box wide enough to prevent clipping.",
        FIGURE_PALETTE,
        "Use shape to indicate kind (see the recipe ids on the shapes page; their path JSON is design_guide topic shapes-json): rounded rectangle for a layer or module (corners as specified below); trapezoid for an encoder (narrowing along the flow) or a decoder (widening); parallelogram for data or a latent z; cylinder for a memory, cache, database, or codebook; circle_plus for addition and residual joins; circle_times for multiplication or gating; cuboid for a tensor or feature map; diamond for routing or a decision; hexagon for a frozen module or a loss. Add a node of kind path whose path field (on the node itself) holds the recipe's path, with bounds set to the box you want. A Path holds no text, so place a Text node with the same bounds on top of it (align center, vertical_align middle).",
        "Draw arrows as Line nodes in the ink colour, 1 px wide (0.75 to 1 pt), with stroke_end filled for the arrowhead. A Line runs from its top-left corner to its bottom-right corner (width and height are never negative), so draw an arrow pointing up or left downward or rightward and set stroke_start instead. Route orthogonally: build an elbow from two or three Lines meeting at right angles, with an arrowhead on the last segment only. Do not draw diagonal arrows or crossings; reroute around blocks instead. Start and end arrows exactly on block edges.",
        "Route residual and skip connections out of the main path, run them parallel to it at a fixed offset (12 to 20 px), and rejoin the main axis at a circle_plus (16 to 20 px, white fill, ink stroke), as in the ResNet and ViT figures, or enter the side of a combined Add & Norm block when addition and normalization are one block, as in the Transformer figure. Bring conditioning inputs in from the side with a dashed Line (stroke_dash dashed).",
        "Show a repeated stack once: draw one copy of the block group inside a frame (stroke only, #BDBDBD, corner radius 6 to 8 px, no fill) and place N× (or L×) beside the frame's left or right edge. Show a zoomed detail or variant either as a separate panel beside the overview labelled (a), (b) in bold at its top-left, or by linking a block to its detail with a dashed outline; do not nest explanations inside blocks.",
        "Use flat styling only: no shadows, gradients, glass, glow, emoji, or icons inside blocks. Use one corner radius for blocks and one corner radius for frames. Use dark text on light fills; never place text on a saturated or dark fill.",
        "Engine facts: a named block is a single rectangle node with text (align center, vertical_align middle), so no separate text node is needed. Set no fill as fill #FFFFFF00 (an empty string is rejected). Always set stroke and stroke_width on blocks, frames, and lines; the defaults produce a blue-grey 1 pt stroke. A complete worked example (a Transformer encoder as one changes_apply) is in design_guide topic paper-diagram-example; adapt its coordinates and names instead of starting from scratch.",
        "Use corners to distinguish process from data: give a module or operation rounded corners (radius about 15 to 20 % of its height, at most 8 px), and give a data block, tensor, or matrix sharp corners. Draw the main flow between modules at the regular line weight; links inside a zoomed detail may be thinner. Spend emphasis in one place only: give the strongest colour only to the component the figure is about.",
        "Keep the figure readable by using at most about 30 connections and 40 labels; if more are needed, move part of the figure into a detail panel. Label a tensor with its shape only where the shape is the point, placing channels above the block and spatial size at its lower left (as in U-Net).",
        "Review in rounds: render with render_get, write a named list of defects (for example \"label Positional Encoding touches its arrow\", \"arrow-5 enters Add & Norm from the wrong side\"), fix each defect with a small targeted changes_apply, and render again; three or four rounds are usually enough. Do not redraw the whole figure to fix one defect. Because visual review misses wrong connections, check the structure as well: list every edge as from block -> to block and confirm each Line starts and ends on the intended blocks. Last, remove one element that does not carry the message.",
        "Check the final render at 100 %: verify every label is legible at print size, nothing is clipped or overlapping, all gaps are equal, arrows touch edges, and there is one reading direction. Then call artifact_validate and fix every design-quality warning not required by the request (COLOR_STOCK_DEFAULT, TEXT_EXTRA_LABEL, TEXT_SIZE_SPREAD, SHADOW_HEAVY, TITLE_LINE_BREAKS). Export PDF or SVG (vector) for the paper.",
    ],
    examples: &[
        (
            "artifact_create",
            r##"{"request_id":"<new UUID>","actor":"agent","kind":"canvas","title":"Model architecture","size":{"width":6300000,"height":4320000}}"##,
        ),
        (
            "changes_apply",
            r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<artifact_id>","base_revision":0,"allowed_intents":["content","design"],"operations":[{"op":"add_node","page_id":"<page_id>","parent_id":null,"node":{"id":"encoder","kind":"path","bounds":{"x":457200,"y":914400,"width":1143000,"height":1371600},"style":{"fill":"#E5DDF0","stroke":"#7A5C99","stroke_width":12700},"path":{"view_box":{"x":0,"y":0,"width":1000,"height":1000},"fill_rule":"nonzero","commands":[{"type":"move_to","x":0,"y":0},{"type":"line_to","x":1000,"y":200},{"type":"line_to","x":1000,"y":800},{"type":"line_to","x":0,"y":1000},{"type":"close"}]}}},{"op":"add_node","page_id":"<page_id>","parent_id":null,"node":{"id":"encoder.label","kind":"text","bounds":{"x":457200,"y":914400,"width":1143000,"height":1371600},"text":"Encoder","style":{"font_size":114300,"color":"#1C1C1C","align":"center","vertical_align":"middle"}}},{"op":"add_node","page_id":"<page_id>","parent_id":null,"node":{"id":"arrow-1","kind":"line","bounds":{"x":1600200,"y":1600200,"width":304800,"height":0},"style":{"stroke":"#1C1C1C","stroke_width":12700,"stroke_end":"filled"}}}]}"##,
        ),
        (
            "artifact_export",
            r##"{"request_id":"<new UUID>","options":{"artifact_id":"<artifact_id>","expected_revision":1,"format":"pdf","required_editability":"vector","allowed_losses":[]},"target_path":"<absolute path>.pdf","overwrite":false}"##,
        ),
    ],
};

/// Measurable meanings of the design words the guides use, so every model reads them the same way.
const GLOSSARY: Page = Page {
    slug: "glossary",
    title: "Design glossary",
    description: "Exact meanings of the design terms used by every guide (with the Korean words users write). When a guide or a request uses one of these words, apply this definition, not a looser one.",
    task: false,
    tools: &["design_guide"],
    steps: &[
        "Auxiliary label (보조 레이블, 보조 글): text that names, introduces, or explains something the layout already shows. Examples: an eyebrow above a title, a section header such as Overview, Inputs, Step 1, or Inside X, a subtitle that repeats the title, an explanatory sentence placed below each item, a legend for a single series, a Source line inside a figure, or an arrow label that repeats the name of the block it points to. Test: remove the text. If a reader still understands what each element is and how elements connect, the text is auxiliary. Do not include auxiliary labels.",
        "Necessary label (필요한 레이블): the shortest text that identifies an element that no other cue identifies. Examples: a block or module name (1 to 3 words), an axis quantity with its unit, a symbol on an arrow (x, z, Q, K, V), a panel letter (a), or a button's action. Keep necessary labels.",
        "Eyebrow or kicker (아이브로우): a line of text smaller than the heading and placed above it, often in upper-case or coloured text. Never use an eyebrow or kicker.",
        "Heading (제목): the largest text of a page, a section, or a panel. A figure for a paper has no heading inside it.",
        "Layout explains the structure (레이아웃으로 설명): show which items belong together, their order, and their hierarchy level using four cues only. Grouping: set the gap inside a group to at most half the gap between groups. Alignment: align related items to a shared edge or centre line. Position: place items in order along the reading direction. Size: make an element at a higher hierarchy level at least 1.2 times the size of the next lower level.",
        "Reading direction or flow axis (읽는 방향, 흐름 축): the single direction in which data or attention moves through the design: left to right, top to bottom, or bottom to top for a layer stack. Place inputs at the start of this axis and outputs at the end.",
        "Main path (주 경로): the chain of blocks along the flow axis from the first input to the final output.",
        "Grid (격자): set every position, width, height, and gap to a multiple of 4 px. This rule does not apply to line widths or font sizes. Make all gaps with the same structural role equal.",
        "Generous whitespace (여백): set outer margins to at least twice the gap between neighbouring items. Ensure no element touches the frame edge.",
        "Padding and clearance (안쪽 여백, 간격): leave at least 4 px between text and any box edge, line, or other text. Size every text box so that no text is clipped.",
        "Hierarchy (위계): use three or four text sizes. Make each size level at least 1.2 times the size of the level below it. Change font weight only for names or headings.",
        "Visual direction (시각 방향): define one palette family, three or four text sizes, one alignment logic, and one corner radius before making the first edit, and keep them unchanged across the entire artifact.",
        "Neutral (무채색): a grey with chroma under 5 %. Near-black ink means colours in the range #1C1C1C to #2B2B2B.",
        "Chroma (채도 폭): (max(R, G, B) - min(R, G, B)) / 255, computed from the 0-255 channel values. 0 % is a pure grey.",
        "Navy (남색): a colour with a blue hue between 205 and 250 degrees, HSL lightness at most 32 %, HSL saturation at least 25 %, and a chroma of at least 5 %, for example #0F172A, #1E293B, #111827, or #172033. Never use navy as a surface. Use a near-black neutral instead.",
        "Stock colour (AI 기본 색): a colour within RGB distance 12 of a framework or assistant default: Tailwind blue, indigo, violet, purple, emerald, green, teal, sky, red, or rose (400 to 900 and 100 steps; not every hue has every step), Bootstrap or Material primaries, Google blue, green, or red, ChatGPT green, Claude orange, or Gemini violet. The lint check reports this as COLOR_STOCK_DEFAULT.",
        "RGB distance (RGB 거리): the Euclidean distance between two colours' 0-255 R, G, and B values: sqrt(dR² + dG² + dB²). The maximum is 441.",
        "Accent (강조색): the single saturated colour of a design, chosen to match the subject. Limit its coverage to at most 10 % of the area and use it only where the reader must look first.",
        "Saturated (채도가 높은): a colour with HSL saturation at least 40 % and HSL lightness between 25 % and 75 %.",
        "Soft fill (연한 채움): a light tint with HSL lightness at least 85 % placed behind dark text. Use soft fills to distinguish diagram blocks by their role.",
        "Dark text (진한 글자): text with HSL lightness at most 25 % and a contrast ratio of at least 4.5:1 against the fill behind it.",
        "Surface (면): any filled shape that covers at least 1 % of the page: a background, a panel, a card, or a block.",
        "Hairline (가는 선): a 1 px (0.75 pt) line drawn in a neutral grey.",
        "Card (카드): a rounded rectangle with its own fill, border, or shadow that encloses one item. A card grid is three or more cards of equal size in a row. Do not use a card grid unless the items are parallel peers at the same level.",
        "Accent bar (가장자리 색 띠, velog식 색 띠): a thin coloured stripe along the top or left edge of a card or box. It sits inside the card or box or touches it, and covers at least half of that edge. It is at most 8 px thick, is at least four times as long as it is thick, and uses a clearly chromatic colour. Do not use an accent bar unless the user asks for it. Show emphasis with type size, weight, or position instead. The lint reports it as ACCENT_BAR.",
        "Frame (프레임): a rectangle with an outline stroke (#BDBDBD, 1 px) and no fill that groups blocks; set its corner radius to 6 to 8 px.",
        "Hero panel (히어로 패널): a large filled rectangle at the top of a page that holds the title, usually dark. Never use one in a figure; avoid it elsewhere unless the user asks.",
        "Pill badge (필 배지): a small rectangle with fully rounded ends (corner radius equal to half its height) holding a 1 to 3 word tag. Do not use one.",
        "Drop shadow (그림자): any shadow drawn behind a shape. Figures use no shadows. Elsewhere, a shadow with a blur of 32 px or more at 0.25 opacity or more is heavy (SHADOW_HEAVY).",
        "Big title inside a figure (그림 안의 큰 제목): any text inside a figure that names the whole figure, or any text larger than the block labels. Never draw one; the caption carries the title.",
        "Slop or generic look (슬롭, 뻔한 AI 디자인): default patterns that generated designs fall back on. The patterns are card grids with soft drop shadows, a dark navy hero panel, purple or indigo gradients, glass or blur effects, emoji used as icons, pill badges, accent bars on box edges, eyebrows, all text centred, stock colours, filler adjectives, and a big title inside a figure. Do not use any of these patterns unless the user explicitly asks for them.",
        "Paper figure (논문 피규어): a figure placed in a paper with a caption below it. Follow the paper-diagram rules for architecture, pipeline, and method-overview figures. Follow the paper-figure rules for data plots.",
        "Caption (캡션): the text placed under a figure in a paper. Because the caption carries the figure's title and explanation, do not put a title or explanation inside the figure artifact itself.",
        "Print size, single and double column (인쇄 크기, 단): the figure's width when printed: 84 mm (3.3 in) for single column and 175 mm (6.9 in) for double column. Measure text sizes at this printed width: set labels to 7 to 9 pt, and use no text smaller than 6 pt.",
        "Block (블록): one shape representing one module, layer, or operation. Label each block with its name only.",
        "Typed shape (도형 종류): a shape whose outline indicates its component type: trapezoid for encoder or decoder, parallelogram for data or latent, cylinder for store, circle_plus for addition, circle_times for gating, cuboid for tensor, diamond for routing, and hexagon for frozen module or loss. Shape recipes are on the shapes page.",
        "Orthogonal routing (직각 연결): draw connectors using only horizontal and vertical segments that meet at right angles. Do not draw diagonal segments or crossing lines.",
        "Residual or skip connection (잔차, 스킵 연결): a line that branches from the main path, runs parallel to it at a fixed offset, and rejoins it at a circle_plus or at the side of a combined Add & Norm block.",
        "Repeat frame, N× (반복 표기): draw one copy of a repeated stack inside a frame with an outline stroke and no fill, and do not draw every copy. Place N× (or L×) outside the frame, beside its left edge, vertically centred on the frame.",
        "Panel and panel label (패널, 패널 기호): a panel is one sub-figure of a multi-part figure; its panel label is a bold marker such as (a) or (b). Place the label inside the panel's area at the panel's top-left corner: align the label's left edge with the panel's left edge and the label's top with the panel's top edge.",
        "Direct label (직접 레이블): a series name placed directly at the end of its line or beside its bars. Use direct labels instead of a legend where the tool can place them (charts drawn on a Canvas); a Figure plot with several series gets its legend from the renderer.",
        "Legend (범례): a key that maps colours or markers to series names. Use a legend only when two or more series share a panel and direct labels would overlap.",
        "Series (계열): one set of data points drawn with one colour or marker, such as one method's results.",
        "Placeholder copy and filler words (자리표시 글, 빈 수사): lorem ipsum, invented names or numbers, and promotional filler words such as seamless, cutting-edge, or 혁신적인. Use only the user's content and real data. When a value is unknown, leave a visible gap: put an em dash (—) in place of the unknown value and add a sentence in your reply that names the missing value.",
        "Artifact (작업물): the DesignLLMTool document being edited: a canvas, a slide deck, a figure, a DOCX, HWPX, or HWP document, or web code.",
    ],
    examples: &[],
};

const SHAPES_PAGE: Page = Page {
    slug: "shapes",
    title: "Shape library",
    description: "Vector shapes beyond rectangles and ellipses for Canvas diagrams: trapezoid encoders and decoders, parallelograms, cylinders, circled plus and times, diamonds, hexagons, cuboids, triangles, block arrows and documents.",
    task: false,
    tools: &["design_guide", "changes_apply", "render_get"],
    steps: &[
        "Every recipe is a Canvas path in a 1000 x 1000 view box. A Path node stretches its view box to fit its bounds, so set bounds to the box you want; the same recipe produces a tall or a wide trapezoid. Set fill, stroke, and stroke_width as on a rectangle.",
        "Add the shape using add_node (content) with kind path and the node's path field = the recipe's path (view_box, fill_rule nonzero, commands). The JSON for every recipe is in the designllm://shapes resource and in design_guide topic shapes-json.",
        "A Path holds no text. Place a Text node with the same bounds on top of it (align center, vertical_align middle), and keep both nodes in one Group so they move together.",
        "Orient each trapezoid along the reading direction: narrowing for an encoder, widening for a decoder. Size circle_plus and circle_times to 16 to 20 px; scale cuboids to show relative tensor sizes.",
        "The circle of circle_plus and circle_times spans 10 to 90 % of its box. End any arrow that meets circle_plus or circle_times 10 % of the box inside the box edge, not on the box edge. The plain circle fills its whole box.",
        "Recipes (id: use):",
    ],
    examples: &[],
};

const PAPER_FIGURE: Page = Page {
    slug: "paper-figure",
    title: "Paper figure (CSV data, plots, PDF evidence)",
    description: "A data figure for a paper: a dataset from CSV, panels with plots, evidence from a PDF, at print size and without auxiliary text. Architecture and pipeline diagrams use the paper-diagram page.",
    task: true,
    tools: &["artifact_validate",
        "design_guide", "asset_import", "source_import_csv", "source_extract_pdf", "artifact_create", "changes_validate", "changes_apply",
        "figure_dataset_edit", "figure_render_link", "render_get", "artifact_export",
    ],
    steps: &[
        "Use this page for data plots. For a model architecture, pipeline, or method overview (the Transformer kind of figure), follow the paper-diagram page on a Canvas artifact instead.",
        FIGURE_PRINT,
        FIGURE_NO_EXTRA_TEXT,
        "Show one message per panel. Give panels compared with each other the same axis ranges, tick steps, series colours, and marker styles. Label panels (a), (b) with set_panel_label, and place them on one grid with arrange_panels. The renderer draws a legend for a plot with several series. Limit each plot to the few series the message needs, and use set_common_legend when panels share series. Because a series gets its colour from its position in the plot's y columns, list shared series in the same order in every panel. A single series needs no legend.",
        "Axis labels name the quantity with its unit in parentheses (Accuracy (%), Latency (ms)); ticks are round numbers; no chart title inside a panel, the caption states the claim. Light grey gridlines only when values must be read, no 3D, shadows, gradients or background panels. Show the error statistic with set_plot_display (error_key) when error bars are drawn.",
        FIGURE_PALETTE,
        "asset_import each file (the CSV and the PDF). It returns sha256, mime and bytes.",
        "source_import_csv with the CSV sha256 and optional columns: name, data_type (category, numeric, count or text) and unit. Columns that are not declared are text. The result is a dataset with its version_hash.",
        "source_extract_pdf with the PDF sha256 returns evidence (page, region and text) for the pages you name (0-based), or for every page when pages is empty; a document over 4096 pages needs a page list. Check document_complete: it is true only when read_pages covers every page of the document. After a request for some pages it is false, so say which pages were read (read_pages) and do not describe the rest of the PDF as read. page_reports lists non-blocking diagnostics for content that was not interpreted, such as images. Use it for the figure's evidence.",
        "artifact_create with kind figure. Then add the data: put_dataset (data), add_panel (content), set_plot (data: panel_id and plot), set_axis_label (content), set_axis_ticks (design), set_caption (content), put_evidence (content).",
        "Only the user approves a review: put_evidence with review_state reviewed (or user_provided on paper_evidence), and set_figure_visual_review that approves or clears the review, are refused with CONTENT_LOCKED (REVIEW_USER_ONLY), and so are history_undo, history_redo and history_undo_change that would restore or take away such a state. Keep evidence unreviewed and a review pending or needs_revision.",
        "Diagrams: connect_diagram and add_diagram_node (content). A diagram node can reference evidence_ids. arrange_panels (design) and set_panel_label (content) place the panels.",
        "Check the capability report (artifact_validate) for Effects before you use gradients or shadows in a figure.",
        "Edit cell values with figure_dataset_edit: give the dataset_id, its version_hash and each cell's previous text. A wrong previous text is refused. The edit creates a new dataset version and updates only this figure's plots that use the old version.",
        "figure_render_link renders an immutable revision of the figure (figure_id, figure_revision, representation) for insertion into a document.",
        "render_get to check, then artifact_export with format pdf (vector), svg, png or pptx.",
    ],
    examples: &[
        (
            "asset_import",
            r##"{"path":"<absolute path to the CSV file>","mime":"text/csv"}"##,
        ),
        (
            "source_import_csv",
            r##"{"sha256":"0000000000000000000000000000000000000000000000000000000000000000","columns":[{"name":"dose","data_type":"numeric","unit":"mg"},{"name":"group","data_type":"category"}]}"##,
        ),
        (
            "figure_dataset_edit",
            r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<artifact_id>","base_revision":2,"allowed_intents":["data"],"dataset_id":"<dataset_id>","version_hash":"0000000000000000000000000000000000000000000000000000000000000000","edits":[{"type":"edit_cell","row_id":"<row_id>","column_id":"<column_id>","previous":"12.5","replacement":"13.0"}]}"##,
        ),
        (
            "figure_render_link",
            r##"{"figure_id":"<figure_id>","figure_revision":3,"representation":"print"}"##,
        ),
    ],
};

const GUI: Page = Page {
    slug: "gui",
    title: "Operating the GUI",
    description: "Read the app's view, list its commands, run one the way a person would, and check the result with a screenshot.",
    task: false,
    tools: &["changes_apply", "render_get", "artifact_save", "artifact_export", "job_start",
        "ui_state_read", "ui_command_list", "ui_command_run", "ui_command_get", "ui_screenshot", "context_read", "render_hit_test",
        "render_caret_rect", "render_selection_rects", "selection_set", "view_transform_set",
    ],
    steps: &[
        "The workspace MCP server opens the desktop on its first foreground tool call. Set background: true on calls for explicit background work, or start the MCP server with --background for an entirely background session.",
        "ui_state_read: the screen (for example home or editor), the open artifact and revision, the page, selection, zoom, viewport, open panels and tool. It also returns state_seq and the view's age.",
        "ui_command_list: the commands the view offers. Each has id, label, args_schema, intents, permission (read, change, file_write or agent_start), enabled and shortcut. Run only enabled commands.",
        "ui_command_run with command_id and args that match the command's args_schema. permission is the class you grant: read or change for most commands. Desktop file.save and file.export need file_write and an explicit target_path, with overwrite and expected_target_hash when replacing a file. Export accepts explicit allowed_losses and reports its actual revision, path and hash. The Desktop view disables job-starting and other commands without safe result tracking; check enabled in ui_command_list. Use direct artifact_save, artifact_export or job_start tools when appropriate. allowed_intents must cover the command's intents; read commands have none.",
        "ui_command_get with the run_id until status is completed, failed or timed_out. Then read its result or error.",
        "ui_screenshot to check the view. It returns the PNG as image content.",
        "context_read returns, for one artifact at one revision, the selection, properties, persisted capabilities and the render snapshot. It does not need a live GUI view.",
        "render_hit_test maps a point of a render snapshot to a node and a text position. render_caret_rect takes a text position and returns the caret rectangle; render_selection_rects takes a text range and returns its selection rectangles. They are read-only and use the same snapshot as render_get. Pass a view_id only after view_transform_set has recorded that view's transform; without it the query is refused with STALE_RENDER.",
        "selection_set records the selection of a view at one revision and view_transform_set records the transform of a preview view. These write the state the GUI shows; the GUI itself calls them. An agent does not need them to edit: change content with changes_apply, and ask the GUI to change what a person sees with ui_command_run.",
        "UNAVAILABLE means there is no live GUI view: the app is not open, or its view expired. Tell the user to open the app. Do not retry in a loop; the view stays alive while the GUI keeps publishing its state.",
    ],
    examples: &[
        ("ui_state_read", "{}"),
        ("ui_command_list", "{}"),
        (
            "ui_command_run",
            r##"{"command_id":"<command id>","args":{},"actor":"agent","permission":"change","allowed_intents":["content"],"timeout_ms":15000}"##,
        ),
        ("ui_command_get", r##"{"run_id":"<run_id>"}"##),
        ("ui_screenshot", r##"{"actor":"agent","timeout_ms":15000}"##),
    ],
};

const WEB_CODE: Page = Page {
    slug: "web-code",
    title: "Web code",
    description: "A web project kept as an artifact. Its files are written to a project folder for preview.",
    task: true,
    tools: &["artifact_create", "web_materialize", "artifact_inspect", "changes_validate", "changes_apply", "artifact_validate", "artifact_export"],
    steps: &[
        "artifact_create with kind web_code and a title.",
        "web_materialize writes the source files to a project folder in the workspace and returns the entry path. That folder is a copy. The artifact is the source: do not edit the folder directly.",
        "Change files with replace_web_source (content: path with / separators and no .. segments, expected_hash of the current file) or mapped_web_edit (content: mapping_id and property from the artifact).",
        "changes_validate, then changes_apply. allowed_intents needs content.",
        "After the changes, call web_materialize again with the new revision to refresh the folder.",
        "artifact_validate for the diagnostics of the current revision.",
        "artifact_export with format zip writes every project file into one ZIP archive (native data). Format html writes the entry page with its local files inlined. Other formats are refused with UNSUPPORTED_OPERATION, and page_indices is refused because a web project has no pages.",
    ],
    examples: &[
        ("artifact_create", r##"{"request_id":"<new UUID>","actor":"agent","kind":"web_code","title":"Landing"}"##),
        ("web_materialize", r##"{"artifact_id":"<artifact_id>","expected_revision":1}"##),
        (
            "changes_apply",
            r##"{"request_id":"<new UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<artifact_id>","base_revision":1,"allowed_intents":["content"],"operations":[{"op":"replace_web_source","path":"index.html","expected_hash":"0000000000000000000000000000000000000000000000000000000000000000","content":"<!doctype html><title>Landing</title>"}]}"##,
        ),
        (
            "artifact_export",
            r##"{"request_id":"<new UUID>","options":{"artifact_id":"<artifact_id>","expected_revision":2,"format":"zip","required_editability":"native_data","allowed_losses":[]},"target_path":"<absolute path>.zip","overwrite":false}"##,
        ),
    ],
};

const CANVAS_COMPONENTS: Page = Page {
    slug: "canvas-components",
    title: "Canvas components",
    description: "Mark a Canvas page as a local component source, materialize instances, update overrides, and detach safely.",
    task: false,
    tools: &["artifact_inspect", "changes_validate", "changes_apply", "history_undo"],
    steps: &[
        "A component source is a Canvas page in the same artifact. Use set_page_role with role=component, then instantiate_component with the target page, source page id, a new instance_id and bounds in EMU.",
        "The instance is a normal Group whose children have deterministic ids instance_id::source_node_id. Child bounds stay relative to their parent; moving the Group does not shift child coordinates.",
        "Text and style edits on an instance child create local overrides. Later source edits refresh unoverridden fields; detach_instance keeps the current materialized nodes and removes the source link.",
        "Other operations on a materialized child, nested component instances, and component references across artifacts are refused. Source or token updates that would change a locked instance or child are refused atomically.",
        "A component source page may contain tables; instances get remapped row/cell IDs, and source structure/column-width edits refresh instances while preserving cell text/style overrides. Change row/cell structure and widths on the source page. Direct structure/width edits inside an instance are refused until persisted instance table overrides exist; source width propagation also refuses when an instance table cell is locked. Keep source table structure changes separate from instance edits or detach operations in one ChangeSet. Deleting an overridden source table cell is refused until every affected instance is detached; this build has no command to clear that override. Regular-page tables can coexist with separate components.",
        "Default HTML/PDF exports omit component pages. Default PNG/SVG exports use the first regular page; page_indices explicitly selects component pages. A document with no regular page needs an explicit page_indices selection.",
        "Component pages and instances are stored with the canvas.components.v1 native/Store3 feature key. Builds without support refuse them before snapshot restore.",
    ],
    examples: &[
        ("changes_apply", r##"{"request_id":"<UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<artifact_id>","base_revision":3,"allowed_intents":["content"],"operations":[{"op":"set_page_role","page_id":"<source_page_id>","role":"component"},{"op":"instantiate_component","page_id":"<target_page_id>","parent_id":null,"instance_id":"card-1","component_page_id":"<source_page_id>","bounds":{"x":300000,"y":200000,"width":2400000,"height":1200000}}]}"##),
        ("changes_apply", r##"{"request_id":"<UUID>","actor":"agent","workspace_id":"<workspace_id>","artifact_id":"<artifact_id>","base_revision":4,"allowed_intents":["content"],"operations":[{"op":"detach_instance","instance_id":"card-1"}]}"##),
    ],
};

/// Default visual direction for new AI designs without a user-provided style.
pub const DEFAULT_DESIGN_STYLE: &str = "For a new design or a requested redesign without a specified palette, theme, or visual reference, start from neutrals: near-black ink (#1C1C1C to #2B2B2B, not navy), a white or light neutral grey surface, mid greys for secondary text and hairlines, and clear contrast. Add at most one accent suited to the subject (for example terracotta #B4532A, ochre #A8741A, moss #5B6B3A, deep teal #2F5D62, plum #6B3E5E, or steel #4A5D73) and use it only where the reader must look first. Never use stock framework or assistant colours: Tailwind blue, indigo, violet, emerald, green, teal, or red (#2563EB, #4F46E5, #7C3AED, #10B981, #22C55E, #EF4444 and similar shades), Bootstrap or Material primaries, Google blue, ChatGPT green, Claude orange, Gemini violet, or dark navy surfaces (#0F172A, #1E293B, #111827); the linter reports them as COLOR_STOCK_DEFAULT. Establish hierarchy using typography, spacing, and alignment. Follow the user's palette or visual reference when provided. Preserve the established palette for targeted edits that do not request a restyle.";

const LAYOUT_HIERARCHY: &str = "Let the layout show the structure instead of adding extra words. Build hierarchy using grouping, alignment, spacing, position, and type size. Keep auxiliary labels to a minimum: omit eyebrow or kicker text above titles, section headers that repeat what the layout already shows (Overview, Inputs, Step 1, Inside X), decorative numbering, explanatory sentences under every item, and legends for a single series. Keep only labels that readers need to identify an item, interpret the design, or act. If removing a label loses no information, remove it.";

/// The habits that make generated work read as a default template. Shared by every task prompt and job guidance.
const ANTI_SLOP: &str = "Avoid generic default patterns: rows of identical rounded cards with soft shadows, dark navy hero panels, purple or indigo gradients, glass or blur effects, emoji used as icons, pill badges, a coloured bar along the top or left edge of a card or box (the blog-callout look), centring all text, a big title inside a figure or diagram, placeholder copy, and filler adjectives. Prefer flat fills, 1 px hairlines, one uniform corner radius (0 to 8 px), aligned edges, and generous whitespace.";

const DOCUMENT_LAYOUT: &str = "Keep report text and tables editable. Use native heading hierarchy, consistent margins, table columns and deliberate page breaks to show structure. Render every page and check heading placement, wrapping, table breaks and Korean glyphs before saving.";

/// Shared authoring rules used by both MCP task prompts and the app's managed AI jobs.
const DESIGN_WORKFLOW: Page = Page {
    slug: "design-workflow",
    title: "Design workflow",
    description: "Choose and apply the built-in design guidance for a DesignLLMTool artifact.",
    task: true,
    tools: &["design_guide", "proposal_preview", "artifact_inspect", "artifact_validate", "changes_validate", "render_get"],
    steps: &[
        "Identify the artifact kind and the intended reader or use. Inspect the target and its capabilities before designing. Follow the matching task guide, plus design-quality for visual review, design-theme when a look is requested, and design-system or templates when reusing one is relevant. A paper figure is a task of its own: paper-diagram for architectures, pipelines and method overviews, paper-figure for data plots. The manual index lists the guides; the design_guide tool returns any of them.",
        DEFAULT_DESIGN_STYLE,
        LAYOUT_HIERARCHY,
        ANTI_SLOP,
        "Use editable native objects for the core content when the format supports them. Match the layout to the target size, choose one consistent visual direction, and preserve the user's requested content and constraints.",
        "Validate the changes, use the delivery mode assigned to the job, and inspect the rendered result. For proposals, use proposal_preview to review without committing. Correct avoidable layout defects and supported design-quality warnings within the request. When the user wants to follow the work live, use the GUI guide and the MCP UI commands to show the relevant view.",
    ],
    examples: &[],
};

const DESIGN_QUALITY: Page = Page {
    slug: "design-quality",
    title: "Design-quality warnings",
    description: "The visual habits that the lint reports as design-quality warnings in artifact_validate for Canvas and slide documents, and how to check a page for them. Warnings only: no edit is refused.",
    task: false,
    tools: &["artifact_validate", "artifact_inspect", "changes_validate", "changes_apply", "render_get"],
    steps: &[
        "Before you build a Canvas, Slides or Figure page, choose one visual direction and state it: one palette family, three or four text sizes, one alignment logic, and one corner radius for the cards.",
        LAYOUT_HIERARCHY,
        "Use the copy the request gives and real sources. Do not invent figures, testimonials, logos or company names. When a value is unknown, leave a visible gap and say so in your reply.",
        "The lint reports these patterns, on Canvas and slide documents, on the node they name. ICON_EMOJI: a text node starts with an emoji or is only emoji. TEXT_PLACEHOLDER: lorem ipsum, John Doe, Jane Doe, Acme, TechCorp, 홍길동 or OO회사 remains. TEXT_VAGUE_WORDING: two or more filler words on a page, or one in its title (empower, leverage, unlock, seamless, elevate, revolutionize, cutting-edge, next-generation; 혁신적인, 원활한, 차원이 다른, 새로운 차원).",
        "FILL_STOCK_GRADIENT: a rectangle or ellipse with at least a quarter of the page in its frame has a gradient whose stops are all purple or indigo (hue 230 to 300 degrees). SHADOW_HEAVY: a shape's shadow blurs 32 px or more and is at least 0.25 opaque (its colour alpha times the node opacity).",
        "BACKGROUND_CREAM: a large visible rectangle has a warm off-white fill. Avoid cream, beige and ivory backgrounds. TEXT_EXTRA_LABEL: a short, much smaller label near the title repeats title words or is a generic category above it. Keep auxiliary labels minimal; use grouping, alignment, spacing and size to show structure. TITLE_LINE_BREAKS: a short title has an explicit line break and is estimated to fit on one line. Keep titles on one line where width allows, widening the box if needed.",
        "LAYOUT_REPEATED_CARDS: three or more rectangles of the same size, with a shadow at least 0.02 opaque and a corner radius of 12 to 24 px, sit in one row without overlap. LAYOUT_ALL_CENTERED: four or more text nodes on a page are all centred and one of them is a body paragraph (three or more lines, or 120 characters or more). TEXT_SIZE_SPREAD: a page uses seven or more text node sizes, counted in whole points. The last two rules exclude text inside shapes.",
        "SLIDE_PLAIN_CLOSING (slide decks only): the text of the last slide is nothing but a closing line (Thank you, 감사합니다, Q&A or 질문).",
        "COLOR_STOCK_DEFAULT: a solid shape fill or a text colour within a small distance of a stock framework or assistant default (Tailwind blue, indigo, violet, purple, emerald, green, teal, sky, red or rose; Bootstrap and Material primaries; Google blue, green and red; ChatGPT green; Claude orange; Gemini violet), or a shape of at least 1 % of the page filled with dark navy (blue hue, lightness at most 0.32). Pick colours from a palette chosen for the subject; use near-black neutrals (#1C1C1C) instead of navy.",
        "ACCENT_BAR: a thin rectangle (at most 8 px thick and at least four times as long) or a line of 3 px or more. It uses a colour with a chroma of at least 15 %, lies along the top or left edge of a filled or outlined rectangle, sits just inside that rectangle or touches it, and covers at least half of that edge. This is the coloured stripe of a blog callout. Remove it unless the user asked for it. Show emphasis with type size, weight, or position.",
        "The warnings do not refuse an edit. Fix them where the request allows, and keep a choice that the request asks for.",
        "Check the result: after the last change, call artifact_validate on the Canvas artifact or the deck. Its diagnostics list each design-quality warning with its node_id. Keep choices explicitly requested by the user and fix the other warnings with changes_validate and changes_apply, then call artifact_validate again. Look at the page with render_get.",
        "A Canvas gets the warnings only when it uses v1.2 values (a shadow, gradient, corner radius, opacity or another v1.2 field); a Canvas with v1.1 values only gets none. A deck gets them whatever its values. Figure gets none yet, so check its look with render_get against the paper-figure rules.",
    ],
    examples: &[("artifact_validate", r#"{"artifact_id":"<artifact_id>"}"#)],
};

const DESIGN_THEME: Page = Page {
    slug: "design-theme",
    title: "Choose a design theme",
    description: "Find and apply a theme that matches the requested look.",
    task: false,
    tools: &["design_themes_search", "design_themes_record", "design_themes_apply", "artifact_inspect", "changes_validate", "changes_apply", "proposal_create", "artifact_validate"],
    steps: &[
        DEFAULT_DESIGN_STYLE,
        "When the request names or implies a theme or look, call design_themes_search with words from the request. If a returned entry fits, apply it.",
        "For a requested look with no matching saved entry, if you can reach https://getdesign.md, browse the catalog and https://getdesign.md/designs/<category>, choose the entry whose one-line description best matches the request, and read its DESIGN.md at https://getdesign.md/<slug>/design-md. Follow its colors, typography, rounded, spacing, components, do's and don'ts.",
        "For a catalog entry, call design_themes_record with its slug as id and tags and a one-line summary in your own words, plus colors, fonts, type scale, radius and spacing. Set recorded_at to the current Unix time in milliseconds.",
        "Inspect the artifact with artifact_inspect. For Canvas or Slides, call design_themes_apply with the saved theme id and the current revision; read its notes. After applying a Canvas theme, use its stored type scale, spacing and radius tokens for new elements. For proposal jobs set dry_run to true and pass the returned change_set, including its operations, to proposal_create. For other kinds, follow the saved design direction when building nodes.",
        "If a requested look has no usable guide, follow the user’s visual direction directly.",
        "Never put the brand's name, logo, imagery or marketing copy in the artifact. Describe the chosen look in your own words in progress text. After building a Canvas or deck, call artifact_validate and follow the design-quality page: preserve explicit user choices and fix the other design-quality warnings.",
    ],
    examples: &[
        ("design_themes_search", r#"{"query":"cool blue minimal","limit":10}"#),
        ("design_themes_record", r##"{"entry":{"id":"example-slug","tags":["cool","minimal"],"summary":"A crisp blue theme with restrained contrast.","colors":{"primary":"#2457A7","canvas":"#FFFFFF","ink":"#172033"},"fonts":{"heading":"Arial","body":"Arial"},"type_scale_px":[14,18,32],"radius_px":[4,8],"spacing_px":[8,16,24],"recorded_at":1732400000000}}"##),
        ("design_themes_apply", r#"{"request_id":"00000000-0000-4000-8000-000000000001","actor":"agent","workspace_id":"workspace","artifact_id":"artifact","base_revision":0,"allowed_intents":["design"],"theme_id":"example-slug"}"#),
    ],
};
/// Every manual page, in listing order.
pub const PAGES: &[Page] = &[
    INDEX, REFUSALS, INTENTS, DESIGN_SYSTEM, PRESETS, AI_JOBS, COLLABORATION, TEMPLATES, MOBILE_SCREEN, LANDING_PAGE, SOCIAL_POST, POSTER, SLIDES, DOCUMENT_EDIT,
    PAPER_DIAGRAM, PAPER_FIGURE, SHAPES_PAGE, WEB_CODE, CANVAS_COMPONENTS, DESIGN_WORKFLOW, DESIGN_QUALITY, DESIGN_THEME, GLOSSARY, GUI,
];

/// URI of the shape library resource (JSON).
pub const SHAPES_URI: &str = "designllm://shapes";

/// The design-quality warning codes (contracts `design_quality`), as the guides and the MCP server name them.
pub const DESIGN_QUALITY_CODES: &[&str] = &[
    "ICON_EMOJI", "TEXT_PLACEHOLDER", "TEXT_VAGUE_WORDING", "FILL_STOCK_GRADIENT", "SHADOW_HEAVY", "LAYOUT_REPEATED_CARDS", "LAYOUT_ALL_CENTERED",
    "TEXT_SIZE_SPREAD", "SLIDE_PLAIN_CLOSING", "BACKGROUND_CREAM", "TEXT_EXTRA_LABEL", "TITLE_LINE_BREAKS", "COLOR_STOCK_DEFAULT", "ACCENT_BAR",
];

/// English words and phrases that mark a title or a request as a paper figure (an architecture, a pipeline, a method
/// overview). They match as whole words, so "configure" does not match "figure".
const PAPER_WORDS: &[&str] = &[
    "paper", "figure", "fig", "architecture", "diagram", "pipeline", "model structure", "method overview", "overview figure", "encoder",
    "decoder", "transformer", "vae", "u-net", "unet", "diffusion model",
];
/// Korean words for the same, matched anywhere (Korean attaches particles to words).
const PAPER_WORDS_KO: &[&str] = &[
    "논문", "피규어", "구조도", "아키텍처", "모델 구조", "파이프라인", "다이어그램", "개요도", "흐름도", "모식도", "네트워크 구조", "인코더",
    "디코더", "트랜스포머", "방법론 개요",
];
/// The words among those that name a diagram rather than a plot: a Figure artifact with them also gets the diagram rules.
const DIAGRAM_WORDS: &[&str] = &["architecture", "diagram", "pipeline", "model structure", "method overview", "encoder", "decoder", "transformer"];
const DIAGRAM_WORDS_KO: &[&str] = &[
    "구조도", "아키텍처", "모델 구조", "파이프라인", "다이어그램", "개요도", "흐름도", "모식도", "네트워크 구조", "방법론 개요", "인코더", "디코더", "트랜스포머",
];

/// Whether `phrase` occurs in `lower` (already lower case) bounded by non-alphanumeric characters on both sides.
fn has_word(lower: &str, phrase: &str) -> bool {
    lower.match_indices(phrase).any(|(start, _)| {
        let before = lower[..start].chars().next_back();
        let after = lower[start + phrase.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

fn mentions(text: &str, words: &[&str], korean: &[&str]) -> bool {
    let lower = text.to_lowercase();
    words.iter().any(|word| has_word(&lower, word)) || korean.iter().any(|word| lower.contains(word))
}

/// Whether a title or a request names a paper figure (architecture, pipeline, method overview).
pub fn mentions_paper_figure(text: &str) -> bool {
    mentions(text, PAPER_WORDS, PAPER_WORDS_KO)
}

/// Whether a title or a request names a diagram (not only a plot).
fn mentions_diagram(text: &str) -> bool {
    mentions(text, DIAGRAM_WORDS, DIAGRAM_WORDS_KO)
}

/// The task page that guides work on an artifact of `kind`, given its title or the user's request. Canvas work that names a
/// paper figure follows paper-diagram; other Canvas work has no single page (the workflow names the Canvas guides).
pub fn task_page_for(kind: ArtifactKind, hint: &str) -> Option<&'static Page> {
    match kind {
        ArtifactKind::Canvas if mentions_paper_figure(hint) => Some(&PAPER_DIAGRAM),
        ArtifactKind::Canvas => None,
        ArtifactKind::Slides => Some(&SLIDES),
        ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp => Some(&DOCUMENT_EDIT),
        ArtifactKind::Figure => Some(&PAPER_FIGURE),
        ArtifactKind::WebCode => Some(&WEB_CODE),
    }
}

/// What a Figure diagram can draw, added before the paper-diagram rules in a Figure guide.
pub const FIGURE_DIAGRAM_NOTE: &str = "This Figure shows a diagram. Use only rectangle, ellipse, and math-formula text nodes in a Figure diagram; do not use path shapes or Line nodes. Each connector (edge) is one straight segment from the centre of one node towards the centre of the other, with no elbows and no bypass routes. Place connected nodes on the same centre row or column so every edge runs straight horizontally or vertically. Follow the layout, label, colour, and review rules of the paper-diagram guide below. Where that guide builds elbows from Line nodes or draws typed shapes, follow these Figure diagram limits instead. Suggest a Canvas to the user when the diagram needs elbows, residual bypasses, encoder trapezoids, cylinders, or circled operators.";

/// Key of a Figure guide that also carries the diagram rules.
const FIGURE_DIAGRAM_KEY: &str = "paper-figure+diagram";

/// The name of the guide `guide_for` gives, without building its text (the guide gate checks it on every change).
pub fn guide_key(kind: ArtifactKind, hint: &str) -> &'static str {
    match (kind, task_page_for(kind, hint).map(|page| page.slug)) {
        (ArtifactKind::Figure, _) if mentions_diagram(hint) => FIGURE_DIAGRAM_KEY,
        (_, Some(slug)) => slug,
        (ArtifactKind::Canvas, None) => "canvas",
        (_, None) => "design-workflow",
    }
}

/// The guide an agent must follow for an artifact of `kind`: the shared workflow, the task page (with the shape library for
/// a paper diagram), and the design-quality checks for the kinds the lint covers. `key` names the bundle, so a connection
/// delivers each one once.
pub struct Guide {
    pub key: &'static str,
    pub text: String,
}

pub fn guide_for(kind: ArtifactKind, hint: &str) -> Guide {
    let task = task_page_for(kind, hint);
    let key = guide_key(kind, hint);
    let mut text = String::from(
        "DesignLLMTool design guide. These rules are mandatory for this artifact: apply them to every change, and check the rendered result against them before you finish. The user's explicit instructions take precedence over a rule they contradict.\n\n",
    );
    text.push_str(&page_body(&DESIGN_WORKFLOW));
    match task {
        Some(page) => {
            text.push_str(&format!("\nTask guide ({}):\n\n", page_uri(page)));
            text.push_str(&page_body(page));
        }
        None => text.push_str("\nCanvas task guides: mobile-screen, landing-page, social-post, poster; paper-diagram for an architecture, pipeline or method figure. Call design_guide with the topic that fits before building.\n"),
    }
    if key == FIGURE_DIAGRAM_KEY {
        // A Figure's diagrams draw rectangles, ellipses and formulas only, but the layout, label, colour and review rules hold.
        text.push_str(&format!("\n{FIGURE_DIAGRAM_NOTE}\n\n"));
        text.push_str(&page_body(&PAPER_DIAGRAM));
    }
    if task.is_some_and(|page| page.slug == PAPER_DIAGRAM.slug) {
        text.push('\n');
        text.push_str(&page_body(&SHAPES_PAGE));
    }
    if matches!(kind, ArtifactKind::Canvas | ArtifactKind::Slides) {
        text.push('\n');
        text.push_str(&page_body(&DESIGN_QUALITY));
    }
    text.push('\n');
    text.push_str(&page_body(&GLOSSARY));
    Guide { key, text }
}

/// The topics `design_guide` answers: every manual page slug, `shapes-json` for the raw recipes and
/// `paper-diagram-example` for a complete worked figure.
pub fn guide_topic(topic: &str) -> Option<String> {
    match topic {
        "shapes-json" => serde_json::to_string_pretty(&shapes_json()).ok(),
        PAPER_DIAGRAM_EXAMPLE => Some(paper_diagram_example_text()),
        _ => PAGES.iter().find(|page| page.slug == topic).map(page_body),
    }
}

/// Topic of the worked paper-diagram example.
pub const PAPER_DIAGRAM_EXAMPLE: &str = "paper-diagram-example";

/// A Transformer encoder drawn by the paper-diagram rules: 84 mm single column, bottom to top, a repeat frame with N×,
/// residual connections, a circle_plus join and the functional palette. `page_id` fills every add_node.
pub fn paper_diagram_example_operations(page_id: &str) -> Value {
    // Positions in tenths of a millimetre.
    let emu = |tenths: i64| tenths * 3600;
    let bounds = |x: i64, y: i64, w: i64, h: i64| json!({ "x": emu(x), "y": emu(y), "width": emu(w), "height": emu(h) });
    // Block radius 5 px is about 17 % of a 6 to 8 mm block; the frame takes 7 px.
    let (pt8, pt9, hairline, radius_block, radius_frame) = (101_600, 114_300, 9_525, 47_625, 66_675);
    let add = |node: Value| json!({ "op": "add_node", "page_id": page_id, "parent_id": null, "node": node });
    let block = |id: &str, x, y, w, h, fill: &str, stroke: &str, text: &str| add(json!({
        "id": id, "kind": "rectangle", "bounds": bounds(x, y, w, h), "text": text,
        "style": { "fill": fill, "stroke": stroke, "stroke_width": hairline, "corner_radius": radius_block, "color": "#1C1C1C",
                   "font_size": pt8, "align": "center", "vertical_align": "middle" },
    }));
    let label = |id: &str, x, y, w, h, text: &str, align: &str, size: i64| add(json!({
        "id": id, "kind": "text", "bounds": bounds(x, y, w, h), "text": text,
        "style": { "color": "#1C1C1C", "font_size": size, "align": align, "vertical_align": "middle" },
    }));
    // An arrow pointing up is drawn downward from its tip, so its head is stroke_start.
    let line = |id: &str, x, y, w, h, head: Option<&str>| {
        let mut style = json!({ "stroke": "#1C1C1C", "stroke_width": hairline });
        if let Some(end) = head {
            style[end] = json!("filled");
        }
        add(json!({ "id": id, "kind": "line", "bounds": bounds(x, y, w, h), "style": style }))
    };
    let up = |id: &str, y, h| line(id, 420, y, 0, h, Some("stroke_start"));
    let circle_plus = designllm_contracts::shape_library::shape("circle_plus").map(|recipe| recipe.path());
    vec![
        label("inputs", 220, 1110, 400, 50, "Inputs", "center", pt8),
        up("arrow-0", 1040, 70),
        block("embedding", 240, 960, 360, 80, "#E5DDF0", "#7A5C99", "Input Embedding"),
        // A circled operator's circle spans 10 to 90 % of its box: the arrows touching it end 0.4 mm inside the 4 mm box.
        up("arrow-1", 916, 44),
        add(json!({ "id": "position.add", "kind": "path", "bounds": bounds(400, 880, 40, 40),
                    "style": { "fill": "#FFFFFF", "stroke": "#1C1C1C", "stroke_width": hairline }, "path": circle_plus })),
        label("position.label", 20, 870, 320, 60, "Positional Encoding", "right", pt8),
        line("position.arrow", 350, 900, 54, 0, Some("stroke_end")),
        up("arrow-2", 820, 64),
        add(json!({ "id": "repeat.frame", "kind": "rectangle", "bounds": bounds(180, 300, 480, 520),
                    "style": { "fill": "#FFFFFF00", "stroke": "#BDBDBD", "stroke_width": hairline, "corner_radius": radius_frame } })),
        label("repeat.count", 60, 520, 110, 80, "N×", "right", pt9),
        block("attention", 240, 680, 360, 80, "#FCE3C7", "#C77A2B", "Multi-Head Attention"),
        up("arrow-3", 760, 60),
        line("residual-1.out", 200, 790, 220, 0, None),
        line("residual-1.up", 200, 610, 0, 180, None),
        line("residual-1.in", 200, 610, 40, 0, Some("stroke_end")),
        block("norm-1", 240, 580, 360, 60, "#F6EDB8", "#A8922E", "Add & Norm"),
        up("arrow-4", 640, 40),
        block("feed-forward", 240, 440, 360, 80, "#D6E6EF", "#4F7CA8", "Feed Forward"),
        up("arrow-5", 520, 60),
        line("residual-2.out", 200, 550, 220, 0, None),
        line("residual-2.up", 200, 370, 0, 180, None),
        line("residual-2.in", 200, 370, 40, 0, Some("stroke_end")),
        block("norm-2", 240, 340, 360, 60, "#F6EDB8", "#A8922E", "Add & Norm"),
        up("arrow-6", 400, 40),
        up("arrow-7", 240, 100),
        block("linear", 240, 180, 360, 60, "#FFFFFF", "#3A3A3A", "Linear"),
        up("arrow-8", 140, 40),
        block("softmax", 240, 80, 360, 60, "#D8EBD9", "#4E8A56", "Softmax"),
        up("arrow-9", 50, 30),
        label("outputs", 140, 0, 560, 50, "Output Probabilities", "center", pt8),
    ]
    .into()
}

fn paper_diagram_example_text() -> String {
    let operations = serde_json::to_string_pretty(&paper_diagram_example_operations("<page_id>")).unwrap_or_default();
    format!(
        "# Worked example: Transformer encoder (paper-diagram)\n\nCreate a Canvas of 84 x 120 mm (size {{\"width\":3024000,\"height\":4320000}}), read its page id with artifact_inspect, then send these operations in one changes_apply with allowed_intents [\"content\", \"design\"]. Positions are multiples of 0.1 mm (3600 EMU). The figure reads bottom to top; the residual connections branch from the main arrow, run up the left side and enter each Add & Norm block; the repeated layer sits once inside a stroke-only frame with N× beside it; there is no title, no section header and no explanatory text. Adapt names, colours by role and positions to the model you draw; keep the rules.\n\n```json\n{operations}\n```\n"
    )
}

/// The shape recipes as JSON: id, use and the Canvas path that add_node takes under `path`.
pub fn shapes_json() -> Value {
    Value::Array(
        designllm_contracts::shape_library::SHAPES
            .iter()
            .map(|recipe| json!({ "id": recipe.id, "use_for": recipe.use_for, "path": recipe.path() }))
            .collect(),
    )
}

/// The app supplies this guidance directly to a managed agent, including clients that do not expose MCP resources. `prompt`
/// is the user's request: a Canvas request that names a paper figure gets the paper-diagram guide and the shape library.
pub fn job_design_guidance(kind: ArtifactKind, prompt: &str) -> String {
    let mut guidance = guide_for(kind, prompt).text;
    guidance.push_str("\nThe job's scope, intent and delivery rules take precedence over workspace-level actions described in the guides.\n");
    guidance
}

/// The prompt name of a task page: its slug with `_` for `-`.
pub fn prompt_name(page: &Page) -> String {
    page.slug.replace('-', "_")
}

/// URI of a manual page.
pub fn page_uri(page: &Page) -> String {
    format!("{MANUAL_PREFIX}{}", page.slug)
}

/// Natural size of a preset in its own unit (whole numbers stay whole).
fn natural(emu: i64, unit_emu: i64) -> Value {
    let value = emu as f64 / unit_emu as f64;
    if value.fract() == 0.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

/// The frame presets as JSON: id, family, natural size and unit, and the EMU size that artifact_create takes.
pub fn presets_json() -> Value {
    Value::Array(
        FRAME_PRESETS
            .iter()
            .map(|preset| {
                let (unit_emu, unit) = match preset.natural_unit {
                    NaturalUnit::Px => (EMU_PER_CSS_PX, "px"),
                    NaturalUnit::Mm => (EMU_PER_MM, "mm"),
                    NaturalUnit::In => (EMU_PER_INCH, "in"),
                    NaturalUnit::Pt => (EMU_PER_PT, "pt"),
                };
                json!({
                    "id": preset.id,
                    "family": serde_json::to_value(preset.family).unwrap_or(Value::Null),
                    "natural_unit": unit,
                    "natural_size": {
                        "width": natural(preset.size.width, unit_emu),
                        "height": natural(preset.size.height, unit_emu),
                    },
                    "size_emu": { "width": preset.size.width, "height": preset.size.height },
                })
            })
            .collect(),
    )
}

/// The text of a page: title, description, steps, the tools it uses, and its examples. The intents, presets, jobs and
/// index pages add their generated lists.
pub fn page_body(page: &Page) -> String {
    let mut out = format!("# {}\n\n{}\n\n", page.title, page.description);
    for (index, step) in page.steps.iter().enumerate() {
        out.push_str(&format!("{}. {step}\n", index + 1));
    }
    match page.slug {
        "intents" => {
            for (intent, ops) in OPERATION_INTENTS {
                let shown: Vec<&str> = ops.iter().copied().filter(|op| !crate::operation_exposure::is_hidden(op)).collect();
                out.push_str(&format!("- {intent}: {}\n", shown.join(", ")));
            }
        }
        "presets" => {
            let ids: Vec<&str> = FRAME_PRESETS.iter().map(|preset| preset.id).collect();
            out.push_str(&format!("   {}\n", ids.join(", ")));
        }
        "ai-jobs" => {
            let names: Vec<String> = tools_for(Scope::JobBound).iter().map(|tool| tool.name.to_string()).collect();
            out.push_str(&format!("   {}\n", names.join(", ")));
        }
        "index" => {
            out.push_str("\nTask pages:\n");
            for task in PAGES.iter().filter(|page| page.task) {
                out.push_str(&format!("- {} ({})\n", task.title, page_uri(task)));
            }
            out.push_str(&format!("\nReference pages: glossary, refusals, intents, presets, shapes, design-quality, design-theme, ai-jobs, gui. Presets JSON: {PRESETS_URI}. Shapes JSON: {SHAPES_URI}\n"));
        }
        "shapes" => {
            for recipe in designllm_contracts::shape_library::SHAPES {
                out.push_str(&format!("   - {}: {}\n", recipe.id, recipe.use_for));
            }
            out.push_str(&format!("   The path JSON of each recipe: {SHAPES_URI}, or design_guide with topic shapes-json.\n"));
        }
        _ => {}
    }
    if !page.tools.is_empty() {
        out.push_str(&format!("\nTools used: {}.\n", page.tools.join(", ")));
    }
    for (tool, arguments) in page.examples {
        out.push_str(&format!("\nExample `{tool}`:\n```json\n{arguments}\n```\n"));
    }
    out
}

/// The `resources` array of resources/list.
pub fn resources_result() -> Value {
    let mut resources: Vec<Value> = PAGES
        .iter()
        .map(|page| {
            json!({
                "uri": page_uri(page),
                "name": page.slug,
                "title": page.title,
                "description": page.description,
                "mimeType": MARKDOWN,
            })
        })
        .collect();
    resources.push(json!({
        "uri": PRESETS_URI,
        "name": "presets-json",
        "title": "Frame presets (JSON)",
        "description": "Every frame preset: id, family, natural size and unit, and the EMU size that artifact_create takes.",
        "mimeType": JSON_MIME,
    }));
    resources.push(json!({
        "uri": SHAPES_URI,
        "name": "shapes-json",
        "title": "Shape library (JSON)",
        "description": "Canvas path recipes (1000 x 1000 view box) for trapezoids, parallelograms, cylinders, circled operators, diamonds, hexagons, cuboids and more.",
        "mimeType": JSON_MIME,
    }));
    json!({ "resources": resources })
}

/// The content entry of resources/read for `uri`, or None when no resource has that URI.
pub fn read_resource(uri: &str) -> Option<Value> {
    if uri == PRESETS_URI {
        let text = serde_json::to_string_pretty(&presets_json()).ok()?;
        return Some(json!({ "uri": uri, "mimeType": JSON_MIME, "text": text }));
    }
    if uri == SHAPES_URI {
        let text = serde_json::to_string_pretty(&shapes_json()).ok()?;
        return Some(json!({ "uri": uri, "mimeType": JSON_MIME, "text": text }));
    }
    let slug = uri.strip_prefix(MANUAL_PREFIX)?;
    let page = PAGES.iter().find(|page| page.slug == slug)?;
    Some(json!({ "uri": uri, "mimeType": MARKDOWN, "text": page_body(page) }))
}

/// The `prompts` array of prompts/list: one prompt per task page.
pub fn prompts_result() -> Value {
    let prompts: Vec<Value> = PAGES
        .iter()
        .filter(|page| page.task)
        .map(|page| {
            json!({
                "name": prompt_name(page),
                "title": page.title,
                "description": page.description,
                "arguments": [{
                    "name": "artifact_id",
                    "description": "The artifact to work on, if you already have one.",
                    "required": false,
                }],
            })
        })
        .collect();
    json!({ "prompts": prompts })
}

/// The result of prompts/get for `name`, or None for an unknown prompt. A given `artifact_id` argument is named in
/// the message.
pub fn get_prompt(name: &str, arguments: Option<&Value>) -> Option<Value> {
    let page = PAGES.iter().find(|page| page.task && prompt_name(page) == name)?;
    let mut text = page_body(page);
    if page.slug != DESIGN_WORKFLOW.slug {
        text.push('\n');
        text.push_str(&page_body(&DESIGN_WORKFLOW));
    }
    text.push('\n');
    text.push_str(&page_body(&GLOSSARY));
    if let Some(artifact_id) = arguments.and_then(|a| a.get("artifact_id")).and_then(Value::as_str) {
        text.push_str(&format!("\nTarget artifact: {artifact_id}. Use it wherever the examples show <artifact_id>.\n"));
    }
    Some(json!({
        "description": page.description,
        "messages": [{ "role": "user", "content": { "type": "text", "text": text } }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_sizes_are_whole_when_they_are_and_decimal_otherwise() {
        let letter = FRAME_PRESETS.iter().find(|p| p.id == "letter").expect("letter");
        let text = serde_json::to_string(&presets_json()).expect("json");
        assert!(text.contains("\"natural_size\":{\"height\":11,\"width\":8.5}"), "letter is 8.5 x 11 in: {}", letter.id);
        assert!(text.contains("\"natural_size\":{\"height\":297,\"width\":210}"), "a4 is 210 x 297 mm");
        assert!(text.contains("\"natural_size\":{\"height\":540,\"width\":960}"), "slide_16_9 is 960 x 540 pt");
    }

    #[test]
    fn the_task_pages_have_distinct_prompt_names_and_uris() {
        let mut names: Vec<String> = PAGES.iter().filter(|p| p.task).map(prompt_name).collect();
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count);
        assert_eq!(count, 10, "nine task prompts and the general workflow");
    }

    #[test]
    fn paper_work_gets_the_paper_guides_with_the_shape_library_and_the_stock_colour_rule() {
        for hint in ["MiniMax H3 구조도", "Transformer architecture figure", "논문 피규어", "VAE encoder/decoder"] {
            let guide = guide_for(ArtifactKind::Canvas, hint);
            assert_eq!(guide.key, "paper-diagram", "{hint}");
            for fragment in [FIGURE_NO_EXTRA_TEXT, FIGURE_PRINT, FIGURE_PALETTE, ANTI_SLOP, "circle_plus", "trapezoid_narrowing_right", "COLOR_STOCK_DEFAULT", "N×"] {
                assert!(guide.text.contains(fragment), "{hint}: the guide omits {fragment}");
            }
        }
        assert_eq!(guide_key(ArtifactKind::Canvas, "Configure account settings"), "canvas", "configure is not figure");
        assert_eq!(guide_key(ArtifactKind::Canvas, "Method overview"), "paper-diagram");
        assert_eq!(guide_key(ArtifactKind::Canvas, "Fig. 2"), "paper-diagram");
        assert_eq!(guide_key(ArtifactKind::Figure, "Accuracy plot"), "paper-figure");
        let figure_diagram = guide_for(ArtifactKind::Figure, "Transformer architecture");
        assert_eq!(figure_diagram.key, "paper-figure+diagram");
        for hint in ["트랜스포머", "인코더-디코더 구조", "Transformer", "VAE decoder"] {
            assert_eq!(guide_key(ArtifactKind::Figure, hint), "paper-figure+diagram", "{hint}: English and Korean titles route alike");
        }
        assert!(figure_diagram.text.contains(FIGURE_DIAGRAM_NOTE) && FIGURE_DIAGRAM_NOTE.contains("math"), "a Figure diagram is told its own node and connector limits");
        assert!(figure_diagram.text.contains("manual/paper-figure"));
        for (kind, hint) in [(ArtifactKind::Canvas, "x"), (ArtifactKind::Canvas, "구조도"), (ArtifactKind::Figure, "pipeline"), (ArtifactKind::Slides, "")] {
            assert_eq!(guide_key(kind, hint), guide_for(kind, hint).key, "the cheap key matches the guide");
        }
        let landing = guide_for(ArtifactKind::Canvas, "Landing page");
        assert_eq!(landing.key, "canvas");
        assert!(landing.text.contains("paper-diagram") && !landing.text.contains(FIGURE_PALETTE));
        let figure = guide_for(ArtifactKind::Figure, "");
        assert_eq!(figure.key, "paper-figure");
        assert!(figure.text.contains(FIGURE_PALETTE) && figure.text.contains("Okabe-Ito"));
        assert_eq!(guide_for(ArtifactKind::Slides, "").key, "slides");
        assert!(job_design_guidance(ArtifactKind::Canvas, "논문용 모델 구조도를 그려줘").contains("paper-diagram"));
        assert!(DEFAULT_DESIGN_STYLE.contains("#2563EB") && DEFAULT_DESIGN_STYLE.contains("COLOR_STOCK_DEFAULT"));
    }

    #[test]
    fn the_shape_library_is_a_resource_a_guide_topic_and_a_page() {
        let resource = read_resource(SHAPES_URI).expect("shapes resource");
        let shapes: Value = serde_json::from_str(resource["text"].as_str().expect("text")).expect("json");
        let ids: Vec<&str> = shapes.as_array().expect("array").iter().filter_map(|s| s["id"].as_str()).collect();
        for id in ["trapezoid_narrowing_right", "parallelogram", "cylinder", "circle_plus", "circle_times", "cuboid", "diamond"] {
            assert!(ids.contains(&id), "{id}");
        }
        let path: designllm_contracts::PathSpec = serde_json::from_value(shapes[0]["path"].clone()).expect("a Canvas path");
        assert_eq!(path.view_box.width, 1000);
        assert!(resources_result()["resources"].as_array().unwrap().iter().any(|r| r["uri"] == SHAPES_URI));
        assert!(guide_topic("shapes-json").is_some() && guide_topic("shapes").unwrap().contains("cylinder"));
        assert!(guide_topic("paper-diagram").is_some() && guide_topic("nope").is_none());
    }

    #[test]
    fn the_worked_paper_diagram_example_is_a_valid_change_set_that_keeps_the_rules() {
        let operations: Vec<designllm_contracts::Operation> =
            serde_json::from_value(paper_diagram_example_operations("page")).expect("every example operation parses");
        assert!(operations.len() >= 25);
        let text = serde_json::to_string(&paper_diagram_example_operations("page")).unwrap();
        // The arrows into and out of the circle_plus meet its painted circle (10 to 90 % of the box), not its box.
        let ops = paper_diagram_example_operations("page");
        let rect = |id: &str| -> (i64, i64, i64, i64) {
            let b = &ops.as_array().unwrap().iter().find(|op| op["node"]["id"] == id).unwrap_or_else(|| panic!("{id}"))["node"]["bounds"];
            (b["x"].as_i64().unwrap(), b["y"].as_i64().unwrap(), b["width"].as_i64().unwrap(), b["height"].as_i64().unwrap())
        };
        let (cx, cy, cw, ch) = rect("position.add");
        let (_, below_y, _, _) = rect("arrow-1");
        let (_, above_y, _, above_h) = rect("arrow-2");
        let (side_x, _, side_w, _) = rect("position.arrow");
        assert_eq!(below_y, cy + ch * 9 / 10, "the arrow from below ends on the circle's bottom");
        assert_eq!(above_y + above_h, cy + ch / 10, "the arrow above starts at the circle's top");
        assert_eq!(side_x + side_w, cx + cw / 10, "the positional arrow ends on the circle's left");
        for fragment in ["\"N×\"", "\"stroke_start\":\"filled\"", "\"kind\":\"path\"", "#FFFFFF00", "Multi-Head Attention", "#FCE3C7"] {
            assert!(text.contains(fragment), "the example omits {fragment}");
        }
        for (index, _) in text.match_indices('#') {
            let hex = &text[index..(index + 7).min(text.len())];
            assert!(designllm_contracts::design_quality::stock_color(hex).is_none(), "the example uses the stock colour {hex}");
            assert!(!designllm_contracts::design_quality::navy(hex), "the example uses navy {hex}");
        }
        let topic = guide_topic(PAPER_DIAGRAM_EXAMPLE).expect("example topic");
        assert!(topic.contains("3024000") && topic.contains("<page_id>"));
        assert!(page_body(&PAPER_DIAGRAM).contains(PAPER_DIAGRAM_EXAMPLE));
    }

    #[test]
    fn every_tool_a_page_names_is_a_served_tool() {
        let names: Vec<&str> = crate::catalog::catalog().iter().map(|tool| tool.name).collect();
        for page in PAGES {
            for tool in page.tools {
                assert!(names.contains(tool), "{} names the unknown tool {tool}", page.slug);
            }
        }
    }

    #[test]
    fn every_tool_a_page_tells_the_agent_to_call_is_in_its_tool_list() {
        let names: Vec<&str> = crate::catalog::catalog().iter().map(|tool| tool.name).collect();
        let mut missing = Vec::new();
        for page in PAGES {
            let mut text = format!("{}
{}", page.description, page.steps.join("
"));
            for (tool, arguments) in page.examples {
                text.push_str(&format!("
{tool}
{arguments}"));
            }
            // Tools a page names only to say they are refused for some state.
            let refused: &[&str] = if page.slug == "paper-figure" { &["history_undo", "history_redo", "history_undo_change"] } else { &[] };
            for name in &names {
                if has_word(&text, name) && !page.tools.contains(name) && !refused.contains(name) {
                    missing.push(format!("{}: {name}", page.slug));
                }
            }
        }
        assert!(missing.is_empty(), "pages that use a tool their list omits: {missing:?}");
    }

    #[test]
    fn every_guide_and_prompt_carries_the_glossary_of_its_design_words() {
        let glossary = page_body(&GLOSSARY);
        for term in ["Auxiliary label (보조 레이블", "Navy (남색)", "Stock colour (AI 기본 색)", "Slop or generic look", "Orthogonal routing", "Repeat frame, N×", "Caption (캡션)"] {
            assert!(glossary.contains(term), "the glossary omits {term}");
        }
        for kind in [ArtifactKind::Canvas, ArtifactKind::Slides, ArtifactKind::Figure, ArtifactKind::Docx, ArtifactKind::WebCode] {
            assert!(guide_for(kind, "").text.contains(&glossary), "{kind:?}");
        }
        for page in PAGES.iter().filter(|page| page.task) {
            let prompt = get_prompt(&prompt_name(page), None).expect("prompt");
            assert!(prompt["messages"][0]["content"]["text"].as_str().unwrap().contains(&glossary), "{}", page.slug);
        }
    }

    #[test]
    fn design_workflow_reaches_managed_jobs_and_every_mcp_prompt() {
        for page in PAGES.iter().filter(|page| page.task) {
            let prompt = get_prompt(&prompt_name(page), None).expect("advertised prompt");
            let text = prompt["messages"][0]["content"]["text"].as_str().expect("prompt text");
            assert_eq!(text.matches(LAYOUT_HIERARCHY).count(), 1, "{}", page.slug);
            assert_eq!(text.matches(DEFAULT_DESIGN_STYLE).count(), 1, "{}", page.slug);
        }
        for kind in [ArtifactKind::Docx, ArtifactKind::Hwpx, ArtifactKind::Hwp] {
            let guidance = job_design_guidance(kind, "");
            assert!(guidance.contains(LAYOUT_HIERARCHY));
            assert!(guidance.contains(DEFAULT_DESIGN_STYLE));
            assert!(guidance.contains(DOCUMENT_LAYOUT));
            assert!(guidance.contains("document-edit"));
        }
        for (kind, guide) in [
            (ArtifactKind::Canvas, "poster"),
            (ArtifactKind::Slides, "slides"),
            (ArtifactKind::Figure, "paper-figure"),
            (ArtifactKind::WebCode, "web-code"),
        ] {
            let guidance = job_design_guidance(kind, "");
            assert!(guidance.contains(LAYOUT_HIERARCHY));
            assert!(guidance.contains(DEFAULT_DESIGN_STYLE));
            assert!(guidance.contains(guide));
            assert!(!guidance.contains(DOCUMENT_LAYOUT));
        }
    }

    #[test]
    fn the_design_system_and_slides_pages_state_what_the_engines_accept() {
        let design = page_body(&DESIGN_SYSTEM);
        for fragment in ["DOCX", "HWPX", "direct_formatting", "HWPX refuses clear", "`<style name>.color`", "\"direct_formatting\":\"keep\""] {
            assert!(design.contains(fragment), "the design-system page omits {fragment}");
        }
        let slides = page_body(&SLIDES);
        for fragment in ["stroke_start", "stroke_end", "line end on another kind of node is refused", "After creation its position and style can be edited"] {
            assert!(slides.contains(fragment), "the slides page omits {fragment}");
        }
    }

    #[test]
    fn the_design_quality_page_names_every_quality_code_and_is_a_reference_page() {
        let body = page_body(&DESIGN_QUALITY);
        for code in [
            "ICON_EMOJI", "TEXT_PLACEHOLDER", "TEXT_VAGUE_WORDING", "FILL_STOCK_GRADIENT", "SHADOW_HEAVY", "LAYOUT_REPEATED_CARDS",
            "LAYOUT_ALL_CENTERED", "TEXT_SIZE_SPREAD", "SLIDE_PLAIN_CLOSING", "BACKGROUND_CREAM", "TEXT_EXTRA_LABEL", "TITLE_LINE_BREAKS",
            "COLOR_STOCK_DEFAULT",
        ] {
            assert!(body.contains(code), "the design-quality page omits {code}");
        }
        assert!(body.contains("changes_apply") && body.contains("v1.2"), "the check procedure and the version gate are stated");
        for fragment in ["warm off-white fill", "repeats title words", "explicit line break", "cream, beige and ivory backgrounds", "grouping, alignment, spacing and size", "one line where width allows"] {
            assert!(body.contains(fragment), "the design-quality page omits {fragment}");
        }
        assert!(body.contains("Keep choices explicitly requested by the user and fix the other warnings"));
        assert!(!body.contains("Fix them with changes_validate"));
        assert!(!DESIGN_QUALITY.task, "design quality remains a reference page");
    }

    #[test]
    fn canvas_interaction_scope_is_documented_for_mcp_clients() {
        let resource = read_resource(&page_uri(&REFUSALS)).expect("refusals resource");
        let text = resource["text"].as_str().expect("manual body");
        assert!(text.contains("OpenUrl is combined with other interactions"));
        assert!(text.contains("executes it in the saved action order"));
        assert!(text.contains("blocks external navigation and new windows"));
        assert!(text.contains("HTML_LINK_ACCESSIBLE_NAME_MISSING"));
        assert!(text.contains("A Group link does not use its layer name or guess from child text"));
        assert!(text.contains("Canvas HTML playback supports the back interaction"));
        assert!(text.contains("does nothing when no previous in-export page exists"));
        assert!(text.contains("does not navigate browser history"));
    }
}
