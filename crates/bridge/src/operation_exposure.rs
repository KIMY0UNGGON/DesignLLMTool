//! Which ChangeSet operations an MCP session may send (`md/03-tasks/open/80-mcp-phase.md` policy 3). A new `Operation`
//! variant is not offered to agents by accident: the match in [`exposure`] has no wildcard, so the bridge does not compile
//! until the variant is classified here.
//!
//! A hidden operation stays a GUI-only edit. It is left out of the `operations` of the `changes_validate`, `changes_apply`
//! and `proposal_create` tool schemas (`catalog.rs` `schema_change_set`), and [`refusal`] refuses every one of those calls
//! whose change set holds it, before the core is asked (`mcp.rs` `run`). The schema alone would not stop a client that sends
//! the operation anyway. The host refuses the same calls, and `proposal.apply` of a stored change set that holds it, for every
//! connection without the gui role (`designllm-host` `conn.rs` `review_refusal`), so a raw bridge or cli connection cannot
//! send it either.
//!
//! This build hides nothing: every operation was reachable through those tools before this list, and no document or code
//! names an operation as GUI-only. The MCP-only allowlist of `tests/integration/tests/all/gui_mcp_parity.rs` (`MCP_ONLY`,
//! `fixtures/mcp-parity/allowlist.json`) is the opposite direction: `set_interaction` has no GUI command, so hiding it would
//! leave no path to it.
//!
//! Only change sets an agent sends are judged. Other methods make operations themselves, and none of them is judged yet; they
//! are listed in [`INDIRECT_PATHS`]. Before an operation is marked hidden, each of them must refuse it too, or the operation
//! still reaches the document. The test `hiding_an_operation_needs_the_indirect_paths_closed` fails as soon as one is hidden,
//! to make that happen.

use designllm_contracts::{Diagnostic, DltError, Operation};
use serde_json::Value;

/// (method, how it makes operations): the paths that produce or restore operations an agent did not send as a change set, so
/// [`refusal`] does not see them. Found by searching every product construction of `Operation::` (2026-11-20).
pub const INDIRECT_PATHS: &[(&str, &str)] = &[
    ("ui.command.run", "the desktop window runs a GUI command for the agent and builds its change set itself"),
    ("figure.dataset.edit", "core builds PutDataset and SetPlot from the edits (core/src/dataset.rs)"),
    (
        "design_system.set, design_system.publish, changes.apply of a published token update",
        "engines build SetNamedStyle and token sync operations (sync_design_system_in_context, flow/src/design_rules.rs)",
    ),
    ("artifact.create, job.start with create", "the HWP engine applies SetPageSetup for a requested size (formats/hwp/src/lib.rs)"),
    ("history.undo, history.redo, history.undo_change", "restore snapshots, which can bring back what a hidden operation did"),
];

/// Diagnostic code of the refusal.
pub const OPERATION_NOT_EXPOSED: &str = "OPERATION_NOT_EXPOSED";

/// Whether an MCP session may send an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// Offered in the change tool schemas and accepted.
    Exposed,
    /// GUI only: absent from the change tool schemas and refused.
    Hidden,
}

/// One line per `Operation` variant: the variant, its `op` tag, and its exposure. Generates the exhaustive [`exposure`] and
/// the tag table [`OPERATIONS`] from the same lines, so the two cannot disagree.
macro_rules! classify {
    ($($variant:ident = $tag:literal => $exposure:ident,)+) => {
        /// The exposure of `operation`. No wildcard: a new variant must be classified before the bridge compiles.
        pub fn exposure(operation: &Operation) -> Exposure {
            match operation {
                $(Operation::$variant { .. } => Exposure::$exposure,)+
            }
        }

        /// Every operation's `op` tag with its exposure, in contract order.
        pub const OPERATIONS: &[(&str, Exposure)] = &[$(($tag, Exposure::$exposure),)+];

        /// The variant name of each tag, so the tests can check each tag against serde's `snake_case` of its variant.
        #[cfg(test)]
        const VARIANTS: &[(&str, &str)] = &[$((stringify!($variant), $tag),)+];
    };
}

classify! {
    ReplaceText = "replace_text" => Exposed,
    SetText = "set_text" => Exposed,
    SetStyle = "set_style" => Exposed,
    SetTextStyleRange = "set_text_style_range" => Exposed,
    SetNamedStyle = "set_named_style" => Exposed,
    SetGeometry = "set_geometry" => Exposed,
    AddNode = "add_node" => Exposed,
    RemoveNode = "remove_node" => Exposed,
    SetParagraphFormat = "set_paragraph_format" => Exposed,
    SetPageSetup = "set_page_setup" => Exposed,
    SetTableDimensions = "set_table_dimensions" => Exposed,
    InsertFlowBlock = "insert_flow_block" => Exposed,
    InsertPage = "insert_page" => Exposed,
    RemovePage = "remove_page" => Exposed,
    ReorderPages = "reorder_pages" => Exposed,
    SetPageNotes = "set_page_notes" => Exposed,
    SetCanvasBoardMemos = "set_canvas_board_memos" => Exposed,
    SetCanvasPageSections = "set_canvas_page_sections" => Exposed,
    SetCanvasPageGuides = "set_canvas_page_guides" => Exposed,
    ConnectDiagram = "connect_diagram" => Exposed,
    AddDiagramNode = "add_diagram_node" => Exposed,
    AddPanel = "add_panel" => Exposed,
    RemovePanel = "remove_panel" => Exposed,
    ReorderPanels = "reorder_panels" => Exposed,
    SetCaption = "set_caption" => Exposed,
    PutEvidence = "put_evidence" => Exposed,
    SetPlot = "set_plot" => Exposed,
    SetPlotDisplay = "set_plot_display" => Exposed,
    SetDiagramProvenance = "set_diagram_provenance" => Exposed,
    SetCommonLegend = "set_common_legend" => Exposed,
    SetFigureVisualReview = "set_figure_visual_review" => Exposed,
    SetFigureImageTransform = "set_figure_image_transform" => Exposed,
    SetFigureImageCalibration = "set_figure_image_calibration" => Exposed,
    SetAxisLabel = "set_axis_label" => Exposed,
    SetAxisTicks = "set_axis_ticks" => Exposed,
    PutDataset = "put_dataset" => Exposed,
    ArrangePanels = "arrange_panels" => Exposed,
    SetPanelLabel = "set_panel_label" => Exposed,
    UpdateFigureLink = "update_figure_link" => Exposed,
    SetLayout = "set_layout" => Exposed,
    SetInteraction = "set_interaction" => Exposed,
    SetInteractions = "set_interactions" => Exposed,
    SetVariantStyle = "set_variant_style" => Exposed,
    ReplaceWebSource = "replace_web_source" => Exposed,
    MappedWebEdit = "mapped_web_edit" => Exposed,
    SetPageSize = "set_page_size" => Exposed,
    MoveNode = "move_node" => Exposed,
    SetZOrder = "set_z_order" => Exposed,
    SetNodeAppearance = "set_node_appearance" => Exposed,
    SetCanvasImageCrop = "set_canvas_image_crop" => Exposed,
    SetNodePath = "set_node_path" => Exposed,
    SetNodeInfo = "set_node_info" => Exposed,
    ReplaceImageAsset = "replace_image_asset" => Exposed,
    SetNodeList = "set_node_list" => Exposed,
    SetCanvasTableStructure = "set_canvas_table_structure" => Exposed,
    SetCanvasTableColumnWidths = "set_canvas_table_column_widths" => Exposed,
    RenamePage = "rename_page" => Exposed,
    SetPageRole = "set_page_role" => Exposed,
    InstantiateComponent = "instantiate_component" => Exposed,
    DetachInstance = "detach_instance" => Exposed,
    PutToken = "put_token" => Exposed,
    RemoveToken = "remove_token" => Exposed,
    SetTokenMode = "set_token_mode" => Exposed,
    BindToken = "bind_token" => Exposed,
    UnbindToken = "unbind_token" => Exposed,
    SetGroupRepeat = "set_group_repeat" => Exposed,
    SetGroupCondition = "set_group_condition" => Exposed,
    SetTheme = "set_theme" => Exposed,
}

#[cfg(test)]
thread_local! {
    /// Tags the unit tests hide on their own thread, as if [`OPERATIONS`] marked them hidden.
    static HIDDEN_IN_TEST: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// How many of [`OPERATIONS`] are hidden, counted at compile time.
const HIDDEN_COUNT: usize = {
    let mut count = 0;
    let mut i = 0;
    while i < OPERATIONS.len() {
        if matches!(OPERATIONS[i].1, Exposure::Hidden) {
            count += 1;
        }
        i += 1;
    }
    count
};

/// Whether the operation with `op` tag `tag` is hidden. An unknown tag is not: the core refuses it as no operation.
pub fn is_hidden(tag: &str) -> bool {
    #[cfg(test)]
    if HIDDEN_IN_TEST.with(|hidden| hidden.borrow().contains(&tag)) {
        return true;
    }
    if HIDDEN_COUNT == 0 {
        return false;
    }
    OPERATIONS.iter().filter(|(_, exposure)| *exposure == Exposure::Hidden).any(|(name, _)| *name == tag)
}

/// UNSUPPORTED_OPERATION with one diagnostic when the change set of a `changes.validate`, `changes.apply` or
/// `proposal.create` call (its params `arguments`) holds a hidden operation. Each operation is judged by its `op` tag alone,
/// so a hidden operation with malformed fields is refused too. None for any other method.
pub fn refusal(method: &str, arguments: &Value) -> Option<DltError> {
    refusal_with(method, arguments, is_hidden)
}

/// [`refusal`] with the hidden tags told by `hidden` (the host's tests pass their own; everyone else passes [`is_hidden`]).
pub fn refusal_with(method: &str, arguments: &Value, hidden: fn(&str) -> bool) -> Option<DltError> {
    let change_set = match method {
        "changes.validate" | "changes.apply" => arguments,
        "proposal.create" => arguments.get("change_set")?,
        _ => return None,
    };
    change_set_refusal_with(change_set, hidden)
}

/// The refusal of a ChangeSet given as JSON (a request's or a stored proposal's) that holds an operation `hidden` names.
pub fn change_set_refusal_with(change_set: &Value, hidden: fn(&str) -> bool) -> Option<DltError> {
    let mut found: Vec<&str> = Vec::new();
    for tag in change_set.get("operations")?.as_array()?.iter().filter_map(|operation| operation.get("op")?.as_str()) {
        if hidden(tag) && !found.contains(&tag) {
            found.push(tag);
        }
    }
    if found.is_empty() {
        return None;
    }
    let list = found.join(", ");
    let mut error = DltError::unsupported(format!(
        "{list}: not available outside the desktop app; only the desktop app applies it. Nothing was validated or applied"
    ));
    error.diagnostics.push(Diagnostic {
        code: OPERATION_NOT_EXPOSED.to_owned(),
        message: format!("에이전트가 보낼 수 없는 연산이다: {list}"),
        node_id: None,
        blocking: true,
        params: Default::default(),
    });
    Some(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{tools_for, Scope};
    use crate::manual::OPERATION_INTENTS;
    use crate::mcp::{McpServer, Routed, ToolBackend};
    use crate::DltResult;
    use designllm_contracts::ChangeSet;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    /// Hides `tags` on this thread until the guard drops.
    struct Hide;
    impl Hide {
        fn new(tags: &[&'static str]) -> Self {
            HIDDEN_IN_TEST.with(|hidden| hidden.borrow_mut().extend_from_slice(tags));
            Hide
        }
    }
    impl Drop for Hide {
        fn drop(&mut self) {
            HIDDEN_IN_TEST.with(|hidden| hidden.borrow_mut().clear());
        }
    }

    /// The `op` tags of the `Operation` options of a change set schema (root `definitions`).
    fn schema_tags(schema: &Value) -> BTreeSet<String> {
        schema["definitions"]["Operation"]["oneOf"]
            .as_array()
            .expect("the Operation options")
            .iter()
            .flat_map(|option| option["properties"]["op"]["enum"].as_array().cloned().unwrap_or_default())
            .filter_map(|tag| tag.as_str().map(str::to_owned))
            .collect()
    }

    fn tool_schema(name: &str) -> Value {
        let tool = tools_for(Scope::Workspace).into_iter().find(|tool| tool.name == name).expect("the tool is served");
        (tool.schema)()
    }

    fn snake_case(variant: &str) -> String {
        let mut tag = String::with_capacity(variant.len() + 4);
        for (i, c) in variant.chars().enumerate() {
            if c.is_ascii_uppercase() {
                if i > 0 {
                    tag.push('_');
                }
                tag.push(c.to_ascii_lowercase());
            } else {
                tag.push(c);
            }
        }
        tag
    }

    #[test]
    fn exposure_covers_every_operation() {
        let contract = serde_json::to_value(schemars::schema_for!(ChangeSet)).expect("the ChangeSet schema");
        let listed: BTreeSet<String> = OPERATIONS.iter().map(|(tag, _)| (*tag).to_owned()).collect();
        assert_eq!(listed.len(), OPERATIONS.len(), "each tag once");
        assert_eq!(listed, schema_tags(&contract), "the list names exactly the contract's operations");
        assert_eq!(VARIANTS.len(), OPERATIONS.len());
        for (variant, tag) in VARIANTS {
            assert_eq!(snake_case(variant), *tag, "serde names {variant} {}", snake_case(variant));
        }
    }

    /// The first list keeps the behaviour of before it: every operation is offered and passes.
    #[test]
    fn this_build_hides_nothing_and_the_change_tools_offer_every_operation() {
        assert!(OPERATIONS.iter().all(|(_, exposure)| *exposure == Exposure::Exposed));
        let all: BTreeSet<String> = OPERATIONS.iter().map(|(tag, _)| (*tag).to_owned()).collect();
        for tool in ["changes_validate", "changes_apply"] {
            assert_eq!(schema_tags(&tool_schema(tool)), all, "{tool}");
        }
        assert_eq!(schema_tags(&tool_schema("proposal_create")), all, "proposal_create");
        let intents: BTreeSet<String> =
            OPERATION_INTENTS.iter().flat_map(|(_, ops)| ops.iter().map(|op| (*op).to_owned())).collect();
        let exposed: BTreeSet<String> = OPERATIONS
            .iter()
            .filter(|(_, exposure)| *exposure == Exposure::Exposed)
            .map(|(tag, _)| (*tag).to_owned())
            .collect();
        assert_eq!(intents, exposed, "the manual's intents table lists the exposed operations");
    }

    #[test]
    fn a_hidden_operation_leaves_the_change_tool_schemas() {
        let _hide = Hide::new(&["set_theme"]);
        for tool in ["changes_validate", "changes_apply", "proposal_create"] {
            let tags = schema_tags(&tool_schema(tool));
            assert!(!tags.contains("set_theme"), "{tool} still offers set_theme");
            assert_eq!(tags.len(), OPERATIONS.len() - 1, "{tool} keeps the other operations");
        }
        let intents = crate::manual::PAGES.iter().find(|page| page.slug == "intents").expect("the intents page");
        let body = crate::manual::page_body(intents);
        let line = body.lines().find(|line| line.starts_with("- design: ")).expect("the design line");
        assert!(!line.contains("set_theme") && line.contains("set_style"), "the manual leaves set_theme out: {line}");
    }

    /// Breaks as soon as an operation is marked hidden. Close every indirect path for it first, then update this test.
    #[test]
    fn hiding_an_operation_needs_the_indirect_paths_closed() {
        let hidden: Vec<&str> =
            OPERATIONS.iter().filter(|(_, exposure)| *exposure == Exposure::Hidden).map(|(tag, _)| *tag).collect();
        let paths: Vec<String> = INDIRECT_PATHS.iter().map(|(method, how)| format!("  - {method}: {how}")).collect();
        assert!(
            HIDDEN_COUNT == 0,
            "{hidden:?} is hidden, but only change sets an agent sends are judged. Make each of these refuse it too, then update \
             this test and INDIRECT_PATHS:\n{}",
            paths.join("\n")
        );
    }

    #[test]
    fn the_intents_page_lists_every_exposed_operation() {
        assert_eq!(HIDDEN_COUNT, 0);
        let intents = crate::manual::PAGES.iter().find(|page| page.slug == "intents").expect("the intents page");
        let body = crate::manual::page_body(intents);
        for (tag, _) in OPERATIONS {
            assert!(body.contains(tag), "{tag} is not on the intents page");
        }
    }

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
    }

    impl ToolBackend for Recorder {
        fn call(&self, method: &str, _params: Value) -> DltResult<Value> {
            if method != "job.set_client" {
                self.calls.lock().unwrap().push(method.to_owned());
            }
            Ok(json!({ "ok": true }))
        }
    }

    fn session(scope: Scope) -> (Arc<Recorder>, McpServer) {
        let recorder = Arc::new(Recorder::default());
        let backend: Arc<dyn ToolBackend> = recorder.clone();
        let server = McpServer::new(backend, scope);
        let initialize = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "t", "version": "0" } } });
        assert!(matches!(server.route(initialize), Routed::Reply(_)));
        assert!(matches!(server.route(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })), Routed::Nothing));
        (recorder, server)
    }

    fn call(server: &McpServer, tool: &str, arguments: &Value) -> Value {
        let request =
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": tool, "arguments": arguments } });
        let Routed::Call(pending) = server.route(request) else { panic!("{tool} is not called") };
        server.execute(&pending)
    }

    /// `changes_validate`, `changes_apply` and `proposal_create` (the change set nested in its arguments) with `operations`.
    fn change_calls(operations: Value) -> Vec<(&'static str, Value)> {
        let change_set = json!({
            "request_id": "r-1", "actor": "agent", "workspace_id": "w", "artifact_id": "deck", "base_revision": 3,
            "allowed_intents": ["content", "design"], "operations": operations,
        });
        vec![
            ("changes_validate", change_set.clone()),
            ("changes_apply", change_set.clone()),
            ("proposal_create", json!({ "change_set": change_set })),
        ]
    }

    #[test]
    fn a_hidden_operation_is_refused_by_the_change_tools_before_the_core_and_passes_once_exposed() {
        // The hidden operation is second in the batch, after an exposed one, and its fields are not even read.
        let operations = json!([
            { "op": "set_text", "node_id": "n", "text": "x" },
            { "op": "set_theme", "patch": {} },
            { "op": "set_theme" },
        ]);
        for scope in [Scope::Workspace, Scope::JobBound] {
            for (tool, arguments) in change_calls(operations.clone()) {
                let (recorder, server) = session(scope);
                {
                    let _hide = Hide::new(&["set_theme"]);
                    let result = call(&server, tool, &arguments);
                    assert_eq!(result["isError"], true, "{scope:?} {tool}: {result}");
                    let error = &result["structuredContent"];
                    assert_eq!(error["code"], "UNSUPPORTED_OPERATION", "{error}");
                    assert_eq!(error["diagnostics"].as_array().map(Vec::len), Some(1), "{error}");
                    assert_eq!(error["diagnostics"][0]["code"], OPERATION_NOT_EXPOSED);
                    assert!(error["message"].as_str().unwrap_or("").starts_with("set_theme:"), "named once: {error}");
                    assert!(recorder.calls.lock().unwrap().is_empty(), "{scope:?} {tool}: the core is not asked");
                }
                let result = call(&server, tool, &arguments);
                assert_eq!(result["isError"], false, "{scope:?} {tool} exposed: {result}");
                let method = tools_for(scope).into_iter().find(|t| t.name == tool).map(|t| t.method).expect("served");
                assert_eq!(*recorder.calls.lock().unwrap(), [method], "{scope:?} {tool}: exposed, the core is asked");
            }
        }
    }

    #[test]
    fn only_the_change_tools_are_judged() {
        let _hide = Hide::new(&["set_theme"]);
        let arguments = json!({ "operations": [{ "op": "set_theme" }], "change_set": { "operations": [{ "op": "set_theme" }] } });
        for method in ["changes.validate", "changes.apply", "proposal.create"] {
            assert!(refusal(method, &arguments).is_some(), "{method}");
        }
        for method in ["proposal.apply", "figure.dataset.edit", "history.undo", "ui.command.run"] {
            assert!(refusal(method, &arguments).is_none(), "{method}");
        }
        assert!(refusal("changes.apply", &json!({ "operations": [{ "op": "no_such_operation" }] })).is_none());
    }
}
