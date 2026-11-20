//! The formula commands of a figure diagram (`figure_math.rs`). A formula added or edited by the command gives the same figure as the
//! same change sent through the MCP `changes_apply` tool, a source the grammar refuses sends nothing and is shown inline, and the
//! formula is kept when the figure is opened again. The host is the real one (`test_support.rs`); no window opens.

use designllm_contracts::{
    figure_math::{MATH_MAX_TOKENS, MATH_MAX_BYTES},
    figure_rules::USER_AUTHORED_ORIGIN, new_id, Artifact, ArtifactBody, FigureDocument, Intent, MathSyntax, Node, NodeExt, NodeKind,
    Operation, PanelBody, Style,
};
use eframe::egui;
use serde_json::json;

use super::{
    figure_math::{source_refusal, EquationDraft},
    figure_r07::draft_id,
    figure_tools::{chart_change, new_diagram_panel, node_addition, panel_additions},
    test_support::{change_set_arguments, content_of, drawn_text, screen, Host, HostBackend, Mcp, Window},
};

/// The diagram panel of the figure, and its nodes.
fn diagram_panel(figure: &FigureDocument) -> (String, Vec<Node>) {
    figure
        .panels
        .iter()
        .find_map(|panel| match &panel.body {
            PanelBody::Diagram(diagram) => Some((panel.id.clone(), diagram.nodes.clone())),
            _ => None,
        })
        .expect("a diagram panel")
}

/// The formula nodes of a figure.
fn formula_nodes(host: &mut Host, figure_id: &str) -> Vec<Node> {
    let (figure, _) = host.figure(figure_id);
    diagram_panel(&figure).1.into_iter().filter(|node| node.ext.math.is_some()).collect()
}

/// Two empty figures with the same diagram panel (the same ids), built by the same change, so one operation applies to both.
fn twin_figures(host: &mut Host) -> (String, String) {
    let gui = host.create("figure", "그림");
    let mcp = host.create("figure", "그림");
    let (figure, _) = host.figure(&gui);
    let panel = new_diagram_panel(&figure);
    for id in [&gui, &mcp] {
        let (figure, _) = host.figure(id);
        host.apply(id, panel_additions(&figure, panel.clone()), [Intent::Content, Intent::Design].into_iter().collect())
            .expect("the diagram panel");
    }
    (gui, mcp)
}

/// One change sent through MCP `changes_apply` at the artifact's current revision.
fn mcp_change(backend: &HostBackend, mcp: &Mcp, artifact: &str, operations: Vec<Operation>) {
    let (workspace, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(artifact).revision));
    mcp.call("changes_apply", change_set_arguments(&workspace, artifact, revision, &[Intent::Content], operations));
}

/// The body of an artifact as JSON, with each id in `ids` replaced by a fixed word: two figures built apart carry different node ids.
fn body_without_ids(artifact: &Artifact, ids: &[&str]) -> String {
    let mut body = serde_json::to_string(&artifact.body).expect("the body serialises");
    for id in ids {
        body = body.replace(id, "NODE");
    }
    body
}

#[test]
fn the_refusal_names_the_limit_or_the_unsupported_syntax_and_passes_the_grammar() {
    let tokens_over = "x".repeat(MATH_MAX_TOKENS + 1);
    let bytes_over = format!("{} x", " ".repeat(MATH_MAX_BYTES + 1));
    for source in ["\\mp", "x^", "{x", "\\frac12{1}", "$x$", "x\\equiv y", "", "   ", tokens_over.as_str(), bytes_over.as_str()] {
        assert!(source_refusal(source).is_some(), "{source:?} is refused");
    }
    let bytes_at_limit = format!("x{}", " ".repeat(MATH_MAX_BYTES - 1));
    for source in ["x^2", "x_{i+1}", "\\frac{a}{b}", "\\sqrt{x}", "\\alpha\\cdot\\beta", "{}^{14}C", bytes_at_limit.as_str()] {
        assert_eq!(source_refusal(source), None, "{source:?} is read");
    }
    assert!(source_refusal(&"x".repeat(MATH_MAX_TOKENS)).is_none(), "the most tokens allowed are read");
    assert!(source_refusal(&tokens_over).is_some_and(|message| message.contains("너무 깁니다")), "over the token limit");
    let limit = source_refusal(&bytes_over).expect("over the byte limit");
    assert!(limit.contains("1,024바이트") && limit.contains("토큰 512개"), "the limits are written with thousands separators: {limit}");
}

#[test]
fn the_equation_command_gives_the_same_figure_by_command_and_by_tool() {
    let mut host = Host::start();
    let (gui, mcp_figure) = twin_figures(&mut host);
    let before = host.artifact(&gui);
    let source = "x_i^2 + \\frac{a}{b}";

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "figure.add_equation", json!({ "source": source })).expect("the formula is sent");
    drop(window);
    let gui_after = host.artifact(&gui);
    let added = formula_nodes(&mut host, &gui);
    assert_eq!(added.len(), 1, "the command adds one formula node");
    assert_eq!(added[0].text, source);
    assert_eq!(added[0].ext.math, Some(MathSyntax::TexSubset));
    let gui_node = added[0].id.clone();

    // The tool builds its own node from the same arguments: the source, the formula syntax, and the placement of a new box. Its id
    // is its own; the comparison replaces the node id on both sides.
    let (panel_id, _) = diagram_panel(&host.figure(&mcp_figure).0);
    let (figure, _) = host.figure(&mcp_figure);
    let placed = match node_addition(&figure, &panel_id, NodeKind::Text) {
        Some(Operation::AddDiagramNode { node, .. }) => node.bounds,
        _ => panic!("room for a node"),
    };
    let tool_node = Node {
        id: new_id(),
        kind: NodeKind::Text,
        bounds: placed,
        text: source.into(),
        style: Style::default(),
        asset: None,
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: Default::default(),
        ext: NodeExt { math: Some(MathSyntax::TexSubset), ..Default::default() },
    };
    let tool_id = tool_node.id.clone();
    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let operation = Operation::AddDiagramNode { panel_id, node: tool_node, evidence_ids: Vec::new(), origin: USER_AUTHORED_ORIGIN.into() };
    mcp_change(&backend, &mcp, &mcp_figure, vec![operation]);
    let mcp_after = backend.with(|host| host.artifact(&mcp_figure));
    assert_ne!(content_of(&before).1, content_of(&gui_after).1, "the command changed the figure");
    assert_eq!(
        body_without_ids(&gui_after, &[&gui_node]),
        body_without_ids(&mcp_after, &[&tool_id]),
        "the command and the MCP tool left different figures"
    );
}

#[test]
fn a_source_the_grammar_refuses_sends_nothing_by_command() {
    let mut host = Host::start();
    let gui = host.create("figure", "그림");
    let (figure, _) = host.figure(&gui);
    host.apply(&gui, panel_additions(&figure, new_diagram_panel(&figure)), [Intent::Content, Intent::Design].into_iter().collect())
        .expect("the diagram panel");
    let before = host.artifact(&gui);
    let tokens_over = "x".repeat(MATH_MAX_TOKENS + 1);
    let mut window = Window::open(&mut host, &gui);
    for source in ["\\mp", "x^", "{x", "$x$", tokens_over.as_str()] {
        let result = window.step(&mut host, "figure.add_equation", json!({ "source": source }));
        assert!(result.is_err(), "{source:?} is refused");
    }
    drop(window);
    assert_eq!(host.artifact(&gui).revision, before.revision, "nothing was sent");
    assert!(formula_nodes(&mut host, &gui).is_empty());
}

#[test]
fn the_command_names_a_missing_panel_and_a_panel_that_is_not_a_diagram() {
    let mut host = Host::start();
    let figure = host.create("figure", "그림");
    let mut draft = host.declared_draft("id,value\nA,12\nB,18\n", |_| {});
    draft.y_unit = "%".into();
    let (document, _) = host.figure(&figure);
    let (operations, intents) = chart_change(&document, &draft, true).expect("a chart");
    host.apply(&figure, operations, intents).expect("the chart");
    let (document, _) = host.figure(&figure);
    let plot = document.panels.iter().find(|panel| matches!(panel.body, PanelBody::Plot(_))).expect("the chart panel").id.clone();

    let mut window = Window::open(&mut host, &figure);
    let not_diagram = window.step(&mut host, "figure.add_equation", json!({ "source": "x", "panel_id": plot }));
    assert_eq!(not_diagram, Err("구조도 패널이 아닙니다".to_owned()));
    let missing = window.step(&mut host, "figure.add_equation", json!({ "source": "x", "panel_id": "no-such-panel" }));
    assert_eq!(missing, Err("패널을 찾지 못했습니다".to_owned()));
}

#[test]
fn the_formula_edit_gives_the_same_figure_by_command_and_by_tool_and_refuses_a_bad_source() {
    let mut host = Host::start();
    let (gui, mcp_figure) = twin_figures(&mut host);
    let (figure, _) = host.figure(&gui);
    let (panel_id, _) = diagram_panel(&figure);
    let mut operation = node_addition(&figure, &panel_id, NodeKind::Text).expect("room");
    if let Operation::AddDiagramNode { node, .. } = &mut operation {
        node.text = "x^2".into();
        node.ext.math = Some(MathSyntax::TexSubset);
    }
    host.apply(&gui, vec![operation.clone()], [Intent::Content].into_iter().collect()).expect("the formula");
    host.apply(&mcp_figure, vec![operation], [Intent::Content].into_iter().collect()).expect("the same formula");
    let node_id = formula_nodes(&mut host, &gui)[0].id.clone();
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    let refused = window.step(&mut host, "text.set", json!({ "node_id": node_id, "text": "x^" }));
    assert!(refused.is_err(), "a bad source is refused by the command");
    assert_eq!(host.artifact(&gui).revision, before.revision, "the refused edit changed nothing");
    window.step(&mut host, "text.set", json!({ "node_id": node_id, "text": "y_{j}^{2}" })).expect("the edit is sent");
    drop(window);
    let gui_after = host.artifact(&gui);
    assert_ne!(gui_after.revision, before.revision, "the edit changed the figure");
    assert_eq!(formula_nodes(&mut host, &gui)[0].text, "y_{j}^{2}");

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    mcp_change(&backend, &mcp, &mcp_figure, vec![Operation::SetText { node_id, text: "y_{j}^{2}".into() }]);
    let mcp_after = backend.with(|host| host.artifact(&mcp_figure));
    assert_eq!(content_of(&gui_after), content_of(&mcp_after), "the command and the MCP tool left different content");
}

#[test]
fn the_formula_is_kept_when_the_figure_is_stored_and_opened_again() {
    let mut host = Host::start();
    let figure = host.create("figure", "그림");
    let (document, _) = host.figure(&figure);
    host.apply(&figure, panel_additions(&document, new_diagram_panel(&document)), [Intent::Content, Intent::Design].into_iter().collect())
        .expect("the diagram panel");
    let mut window = Window::open(&mut host, &figure);
    window.step(&mut host, "figure.add_equation", json!({ "source": "\\sqrt{x}+\\alpha" })).expect("the formula is sent");
    drop(window);

    let window = Window::open(&mut host, &figure);
    drop(window);
    // The stored body is written as JSON and read back the way a reopened file is.
    let stored = serde_json::to_string(&host.artifact(&figure).body).expect("the body serialises");
    let reopened: ArtifactBody = serde_json::from_str(&stored).expect("the body reads back");
    let ArtifactBody::Figure(reopened) = reopened else { panic!("a figure") };
    let nodes = diagram_panel(&reopened).1;
    let formula: Vec<&Node> = nodes.iter().filter(|node| node.ext.math.is_some()).collect();
    assert_eq!(formula.len(), 1);
    assert_eq!(formula[0].text, "\\sqrt{x}+\\alpha");
    assert_eq!(formula[0].ext.math, Some(MathSyntax::TexSubset));
}

#[test]
fn a_refused_source_is_shown_inline_in_the_formula_section() {
    let mut host = Host::start();
    let figure = host.create("figure", "그림");
    let (document, _) = host.figure(&figure);
    host.apply(&figure, panel_additions(&document, new_diagram_panel(&document)), [Intent::Content, Intent::Design].into_iter().collect())
        .expect("the diagram panel");
    let (document, _) = host.figure(&figure);
    let revision = host.artifact(&figure).revision;
    let mut window = Window::open(&mut host, &figure);
    let ctx = window.ctx.clone();
    // The draft the section keeps for the add field: the user typed a source the grammar refuses.
    ctx.data_mut(|data| data.insert_temp(draft_id("figure-math-add", &figure, ()), EquationDraft { base: String::new(), text: "x^".into() }));
    let output = ctx.run(screen(1.0, 640.0, 480.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| window.app.draw_equation(ui, &document));
    });
    let message = source_refusal("x^").expect("refused");
    assert!(drawn_text(&output).iter().any(|text| *text == message), "the refusal is drawn: {:?}", drawn_text(&output));
    drop(window);
    assert_eq!(host.artifact(&figure).revision, revision, "nothing was sent");
}

#[test]
fn the_tool_path_refuses_a_formula_source_the_grammar_refuses() {
    let mut host = Host::start();
    let figure = twin_figures(&mut host).0;
    let (document, _) = host.figure(&figure);
    let (panel_id, _) = diagram_panel(&document);
    let mut operation = node_addition(&document, &panel_id, NodeKind::Text).expect("room");
    if let Operation::AddDiagramNode { node, .. } = &mut operation {
        node.text = "x^".into();
        node.ext.math = Some(MathSyntax::TexSubset);
    }
    let workspace = host.workspace_id.clone();
    let revision = host.artifact(&figure).revision;
    let add = change_set_arguments(&workspace, &figure, revision, &[Intent::Content], vec![operation]);
    let refused = host.call("changes.apply", add).expect_err("an added formula node with a refused source is refused");
    assert_eq!(refused.code, designllm_contracts::ErrorCode::UnsupportedOperation, "the refusal of the add");
    assert!(refused.diagnostics.iter().any(|d| d.code == "FIGURE_MATH_UNSUPPORTED"), "the grammar's code: {:?}", refused.diagnostics);
    assert_eq!(host.artifact(&figure).revision, revision, "nothing was applied");

    // A valid formula is stored; an edit of its source to the refused `x^` is refused the same way and changes nothing.
    let (document, _) = host.figure(&figure);
    let mut valid = node_addition(&document, &panel_id, NodeKind::Text).expect("room");
    if let Operation::AddDiagramNode { node, .. } = &mut valid {
        node.text = "x^2".into();
        node.ext.math = Some(MathSyntax::TexSubset);
    }
    let valid_id = match &valid {
        Operation::AddDiagramNode { node, .. } => node.id.clone(),
        _ => unreachable!(),
    };
    let revision = host.artifact(&figure).revision;
    host.call("changes.apply", change_set_arguments(&workspace, &figure, revision, &[Intent::Content], vec![valid]))
        .expect("the valid formula is stored");
    let revision = host.artifact(&figure).revision;
    let edit = change_set_arguments(
        &workspace,
        &figure,
        revision,
        &[Intent::Content],
        vec![Operation::SetText { node_id: valid_id, text: "x^".into() }],
    );
    let refused = host.call("changes.apply", edit).expect_err("an edit to a refused source is refused");
    assert!(refused.diagnostics.iter().any(|d| d.code == "FIGURE_MATH_UNSUPPORTED"), "the grammar's code: {:?}", refused.diagnostics);
    assert_eq!(host.artifact(&figure).revision, revision, "the refused edit changed nothing");
}

/// A figure with one diagram panel, and the panel's id.
fn figure_with_diagram(host: &mut Host) -> (String, String) {
    let figure = host.create("figure", "그림");
    let (document, _) = host.figure(&figure);
    let panel = new_diagram_panel(&document);
    host.apply(&figure, panel_additions(&document, panel), [Intent::Content, Intent::Design].into_iter().collect()).expect("the diagram panel");
    let (document, _) = host.figure(&figure);
    let (panel_id, _) = diagram_panel(&document);
    (figure, panel_id)
}

/// The formula section drawn once for `document` (with `events`), as the panel draws it.
fn draw_section(window: &mut Window, document: &FigureDocument, events: Vec<egui::Event>) -> egui::FullOutput {
    let ctx = window.ctx.clone();
    let mut input = screen(1.0, 640.0, 480.0);
    input.events = events;
    ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| window.app.draw_equation(ui, document));
    })
}

/// Backlog desktop 60: while a formula is selected, the section edits it and offers no add field.
#[test]
fn a_selected_formula_shows_its_edit_and_no_add_field() {
    let mut host = Host::start();
    let (figure, _) = figure_with_diagram(&mut host);
    let mut window = Window::open(&mut host, &figure);
    window.step(&mut host, "figure.add_equation", json!({ "source": "x^2" })).expect("the formula is added");
    let node_id = formula_nodes(&mut host, &figure)[0].id.clone();
    let (document, _) = host.figure(&figure);

    let shown = drawn_text(&draw_section(&mut window, &document, Vec::new()));
    assert!(shown.iter().any(|text| *text == "수식 추가") && !shown.iter().any(|text| *text == "수식 적용"), "nothing selected: {shown:?}");
    window.app.selected_node = Some(node_id.clone());
    window.app.selected_nodes = vec![node_id];
    let shown = drawn_text(&draw_section(&mut window, &document, Vec::new()));
    assert!(shown.iter().any(|text| *text == "수식 적용"), "the selected formula is edited: {shown:?}");
    assert!(!shown.iter().any(|text| *text == "수식 추가" || *text == "수식 넣기"), "no add field beside it: {shown:?}");
}

/// Backlog desktop 60: a click on the add button while it is off (the source is refused) sends nothing.
#[test]
fn a_click_on_the_add_button_while_it_is_off_sends_nothing() {
    use eframe::egui::epaint::Shape;
    let mut host = Host::start();
    let (figure, _) = figure_with_diagram(&mut host);
    let (document, _) = host.figure(&figure);
    let revision = host.artifact(&figure).revision;
    let mut window = Window::open(&mut host, &figure);
    window.ctx.data_mut(|data| data.insert_temp(draft_id("figure-math-add", &figure, ()), EquationDraft { base: String::new(), text: "x^".into() }));
    let output = draw_section(&mut window, &document, Vec::new());
    fn find(shape: &Shape, found: &mut Option<egui::Pos2>) {
        match shape {
            Shape::Text(text) if text.galley.text() == "수식 넣기" => *found = Some(text.pos + text.galley.size() / 2.0),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| find(shape, found)),
            _ => {}
        }
    }
    let mut button = None;
    output.shapes.iter().for_each(|clipped| find(&clipped.shape, &mut button));
    let button = button.expect("the add button is drawn");
    let press = |pressed| egui::Event::PointerButton { pos: button, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    let _ = draw_section(&mut window, &document, vec![egui::Event::PointerMoved(button), press(true), press(false)]);
    window.settle(&mut host);
    assert!(!window.calls.iter().any(|(method, _, _)| method == "changes.apply"), "no change is sent");
    drop(window);
    assert_eq!(host.artifact(&figure).revision, revision, "the figure did not change");
}

/// Backlog desktop 60: with two diagrams, a formula added without a panel goes to the diagram that owns the selection.
#[test]
fn a_formula_goes_to_the_diagram_of_the_selection_among_several() {
    let mut host = Host::start();
    let (figure, first) = figure_with_diagram(&mut host);
    let (document, _) = host.figure(&figure);
    host.apply(&figure, panel_additions(&document, new_diagram_panel(&document)), [Intent::Content, Intent::Design].into_iter().collect())
        .expect("a second diagram panel");
    let (document, _) = host.figure(&figure);
    let second = document.panels.iter().filter(|panel| matches!(panel.body, PanelBody::Diagram(_))).map(|panel| panel.id.clone()).find(|id| *id != first).expect("the second diagram");
    host.apply(&figure, vec![node_addition(&document, &second, NodeKind::Rectangle).expect("room")], [Intent::Content].into_iter().collect())
        .expect("a box in the second diagram");
    let (document, _) = host.figure(&figure);
    let boxed = document.panels.iter().find(|panel| panel.id == second).and_then(|panel| match &panel.body {
        PanelBody::Diagram(diagram) => diagram.nodes.first().map(|node| node.id.clone()),
        _ => None,
    }).expect("the box");

    let mut window = Window::open(&mut host, &figure);
    window.step(&mut host, "select.node", json!({ "node_id": boxed })).expect("the box is selected");
    window.step(&mut host, "figure.add_equation", json!({ "source": "y" })).expect("the formula is added");
    let (document, _) = host.figure(&figure);
    let formulas_of = |panel_id: &str| document.panels.iter().find(|panel| panel.id == panel_id).map_or(0, |panel| match &panel.body {
        PanelBody::Diagram(diagram) => diagram.nodes.iter().filter(|node| node.ext.math.is_some()).count(),
        _ => 0,
    });
    assert_eq!((formulas_of(&first), formulas_of(&second)), (0, 1), "the formula is in the selected box's diagram");
}
