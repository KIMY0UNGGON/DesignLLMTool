use super::{
    drafts::{ai_tab_slot, Edit, Live, Outcome},
    events::selection_target_exists,
    *,
};

/// What a request says when an item the user chose is no longer in the document (an agent's change may have removed it). The
/// request is refused with these words, and the AI tab shows them as its scope.
pub(super) const SELECTION_GONE: &str = "선택한 항목을 작업물에서 찾지 못했습니다. 다시 골라 주세요";

impl DesktopApp {
    /// The intents an AI request may change, by the toggles in the AI tab.
    pub(super) fn ai_intents(&self) -> BTreeSet<Intent> {
        let mut allowed_intents = BTreeSet::new();
        if self.allow_content {
            allowed_intents.insert(Intent::Content);
        }
        if self.allow_design {
            allowed_intents.insert(Intent::Design);
        }
        if self.allow_data {
            allowed_intents.insert(Intent::Data);
        }
        allowed_intents
    }

    /// The items the user chose on the open document (`UiChannel::user_selection`), in the order chosen. Only a choice this window made
    /// on this document counts; a reload that re-applies a selection does not change it. Empty means the whole artifact.
    fn user_chosen_ids(&self) -> &[String] {
        let Some(active) = self.active.as_ref() else {
            return &[];
        };
        match &self.ui.user_selection {
            Some((view, artifact, ids)) if *view == self.view_id && *artifact == active.id => ids.as_slice(),
            _ => &[],
        }
    }

    /// The job targets of the request (`target_ids`): the items the user chose, as they were chosen.
    pub(super) fn ai_target_ids(&self) -> Vec<String> {
        self.user_chosen_ids().to_vec()
    }

    /// Why the next request cannot name the items the user chose, `None` when it can: one of them is no longer in the document this
    /// window shows (an agent's change may have removed it). The request (`ai_request_block`) and the AI tab's scope ask this. A
    /// document this window cannot check against refuses too; the host checks the targets again when the job starts.
    pub(super) fn ai_selection_refusal(&self) -> Option<&'static str> {
        let chosen = self.user_chosen_ids();
        if chosen.is_empty() {
            return None;
        }
        let gone = match (self.artifact.as_ref(), self.active.as_ref()) {
            (Some(artifact), Some(active)) if artifact.id == active.id => chosen.iter().any(|id| !selection_target_exists(artifact, id)),
            _ => true,
        };
        gone.then_some(SELECTION_GONE)
    }

    /// The scope of the next request as the AI tab words it: the words of its refusal while it has one, else the whole artifact or
    /// the number of selected targets.
    pub(super) fn ai_scope_label(&self) -> String {
        if let Some(reason) = self.ai_selection_refusal() {
            return reason.to_owned();
        }
        match self.ai_target_ids().len() {
            0 => "작업물 전체".to_owned(),
            count => format!("선택한 항목 {count}개"),
        }
    }

    pub(crate) fn change_actor_intents(&self, fallback: BTreeSet<Intent>) -> (String, BTreeSet<Intent>) {
        self.run_context.as_ref().map_or_else(
            || ("desktop".into(), fallback),
            |run| (run.actor.clone(), run.allowed_intents.clone()),
        )
    }

    /// Whether a request can be sent now; the request button is enabled by exactly this.
    pub(super) fn can_start_ai_job(&self) -> bool {
        self.ai_request_block(&self.ai_prompt).is_none()
    }

    pub(super) fn start_ai_job(&mut self) {
        if !self.can_start_ai_job() {
            return;
        }
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let allowed_intents = self.ai_intents();
        // The AI tab holds its text as a draft (drafts.rs). The request is marked in that draft before it goes out, as a request
        // with evidence files is (figure_ai_attach.rs), so that its text and its id come back after a crash.
        let Edit::Has(live) = self.ai_tab_edit() else { return };
        let slot = ai_tab_slot(active.kind, &active.id);
        let mut request = JobStartRequest {
            figure_refs: self.ai_figure_refs.clone(),
            target_ids: self.ai_target_ids(),
            deliver_as: self.ai_deliver_as(),
            ..JobStartRequest::new(
                new_id(),
                active.id.clone(),
                active.revision,
                allowed_intents,
                self.ai_runtime.clone(),
                self.ai_model.clone(),
                self.ai_effort.clone(),
                self.ai_prompt.trim(),
            )
        };
        if self.figure_ai_sources_apply() {
            let sources = self.drafts.restored_mark(&slot).and_then(|id| self.unknown_starts.sources_for_repeat(id, &request, &live.body));
            let Some(sources) = sources else {
                self.start_figure_ai_request();
                return;
            };
            request.source_refs = sources;
        }
        self.send_job_start(request, &slot, live, PendingAction::StartJob, true);
    }

    /// Sends `job.start` for a request whose content is `live`, held as the draft `slot` (drafts.rs). The editor's draft is brought
    /// up to the editor first. The request id is written before sending, and a start the window cannot send restores the previous
    /// mark. `keep_owner` keeps the editor holding the draft (the figure's AI tab keeps its text). Returns false when the start
    /// was not sent; the words for it are set here, and the caller ends its own state.
    pub(super) fn send_job_start(&mut self, request: JobStartRequest, slot: &str, live: Live, action: PendingAction, keep_owner: bool) -> bool {
        // Every AI request carries the window's delivery choice, the figure's request included (proposals.rs).
        let request = JobStartRequest { deliver_as: self.ai_deliver_as(), ..request };
        let Ok(value) = serde_json::to_value(request) else {
            self.set_error(String::from("AI 작업을 시작하지 못했습니다. 요청은 남겨 두었습니다"));
            return false;
        };
        // The draft is brought up to the editor first, so the mark lands on the draft that holds the content.
        self.sync_drafts();
        // The start's record keeps the draft it is sent from, so that asking for the same draft again finds it (`asked_again_by`).
        let draft = live.body.clone();
        let (value, note) = self.with_job_request_id(value, Some(&draft));
        // `with_job_request_id` may send an unanswered start again, so the mark takes the id the start is sent with.
        let request_id = value.get("request_id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let artifact_id = value.get("artifact_id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let ticket = self.next_ticket;
        // The draft keeps the params with its mark: after a restart they are sent again as they were (`Draft::start`). The params
        // go to the worker too, so the draft holds a copy.
        let start = Some(value.clone());
        if let Err(message) = self.drafts.begin_send_start(ticket, slot, live, &request_id, start, keep_owner, Instant::now()) {
            self.set_error(format!("AI 요청을 보내지 못했습니다. 요청은 남겨 두었습니다: {message}"));
            return false;
        }
        match self.call("job.start", value, action) {
            Some(sent) => {
                self.drafts.rebind(ticket, sent);
                if let Some(start) = self.job_starts.get_mut(&sent) {
                    start.draft = Some(draft);
                }
                // The start went out, so the unanswered start it repeated is no longer kept. A start that did not go out, or one with
                // another id, leaves it.
                self.unknown_start_sent(&artifact_id, &request_id, note);
                true
            }
            None => {
                self.answer_sent_outcome(ticket, Outcome::NotSent);
                false
            }
        }
    }

    pub(super) fn request_job_status(&mut self) {
        let Some(job) = self.job.as_ref() else {
            return;
        };
        self.last_job_poll = Instant::now();
        self.call(
            "job.get",
            json!({ "job_id": job.id, "attempt_id": job.attempt_id }),
            PendingAction::GetJob { job_id: job.id.clone() },
        );
    }

    pub(super) fn cancel_job(&mut self) {
        let Some(job) = self.job.as_ref() else {
            return;
        };
        // Only the job of the open document is cancelled from here (a job kept aside belongs to another document).
        if !self.active.as_ref().is_some_and(|active| active.id == job.artifact_id) {
            return;
        }
        self.call(
            "job.cancel",
            json!({ "job_id": job.id, "attempt_id": job.attempt_id }),
            PendingAction::CancelJob,
        );
    }

    pub(super) fn cancel_job_for_ui(&mut self, args: &Value) -> Result<(), String> {
        let job_id = args.get("job_id").and_then(Value::as_str).ok_or("취소할 job_id를 지정해 주세요")?;
        let attempt_id = args.get("attempt_id").and_then(Value::as_str).ok_or("취소할 attempt_id를 지정해 주세요")?;
        let (Some(active), Some(job)) = (&self.active, &self.job) else {
            return Err("현재 화면에 취소할 작업이 없습니다".into());
        };
        if active.id != job.artifact_id || job.id != job_id || job.attempt_id != attempt_id {
            return Err("취소할 작업이 현재 화면의 작업과 달라 실행하지 않았습니다. 최신 상태를 다시 읽어 주세요".into());
        }
        if !matches!(job.status, JobStatus::Queued | JobStatus::Running | JobStatus::Validating | JobStatus::Failed | JobStatus::Conflict) {
            return Err("현재 작업은 취소할 수 있는 상태가 아닙니다".into());
        }
        self.call(
            "job.cancel",
            json!({ "job_id": job_id, "attempt_id": attempt_id }),
            PendingAction::CancelJob,
        );
        Ok(())
    }

    /// The refusal's action: cancels the job that holds the refused nodes by its job id (`job.cancel` needs no attempt
    /// id from this connection). The held edit is re-sent once the job releases them (`targets.released`).
    pub(super) fn stop_job_for_edit(&mut self, job_id: &str) {
        self.call("job.cancel", json!({ "job_id": job_id }), PendingAction::StopForEdit);
    }
}

#[cfg(test)]
mod tests {
    use designllm_contracts::{new_id, ArtifactBody, ErrorCode, FlowBlock, Intent, Job, Node, Operation, PanelBody};
    use serde_json::{json, Value};

    use crate::app::{
        events::NotUserChoice,
        figure_tools::connection,
        link_flow_tests::{canvas_with_three_boxes, drawn_figure, paragraph},
        test_support::{canvas_with_page, change_set_arguments, content_of, Host, HostBackend, Mcp, Window},
    };

    /// A canvas with the text boxes `a`, `b` and `c` on the page `page-1`. Two such canvases hold the same content, so their bodies
    /// can be compared (a page that `artifact.create` makes has a new id).
    fn canvas_with_boxes(host: &mut Host, title: &str) -> String {
        let id = canvas_with_page(host, title);
        let operations: Vec<Operation> = ["a", "b", "c"]
            .into_iter()
            .zip([0, 1_000_000, 3_000_000])
            .map(|(name, x)| {
                let node: Node = serde_json::from_value(json!({
                    "id": name, "kind": "text", "text": "가",
                    "bounds": { "x": x, "y": 1_000_000, "width": 1_000_000, "height": 400_000 },
                }))
                .expect("a text node");
                Operation::AddNode { page_id: "page-1".into(), parent_id: None, node }
            })
            .collect();
        host.apply(&id, operations, [Intent::Content].into_iter().collect()).expect("the boxes");
        id
    }

    /// A change of `job` to the text box `node` of `artifact`, with the job's reference. MCP `changes_apply` takes the same arguments
    /// as `changes.apply`.
    fn job_change(backend: &HostBackend, artifact: &str, job: &Job, node: &str) -> Value {
        let (workspace, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(artifact).revision));
        let operations = vec![Operation::SetText { node_id: node.into(), text: format!("{node} 고침") }];
        let mut change = change_set_arguments(&workspace, artifact, revision, &[Intent::Content], operations);
        change["job_ref"] = json!({ "job_id": job.id, "attempt_id": job.attempt_id });
        change
    }

    /// The targets are the items the user chose, in the order chosen. A chosen item the document no longer holds refuses the request.
    #[test]
    fn the_targets_are_the_items_the_user_chose_in_choice_order() {
        let mut host = Host::start();
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let mut window = Window::open(&mut host, &canvas);
        assert!(window.app.ai_target_ids().is_empty(), "nothing chosen: the request covers the whole artifact");

        window.step(&mut host, "select.many", json!({ "node_ids": ["c", "a"] })).expect("two boxes are chosen");
        assert_eq!(window.app.ai_target_ids(), ["c", "a"]);
        assert_eq!(window.app.ai_selection_refusal(), None);

        // A chosen item the document no longer holds refuses the request, with the words the AI tab shows.
        let (view, artifact) = (window.app.view_id.clone(), canvas.clone());
        window.app.ui.user_selection = Some((view, artifact, vec!["b".into(), "gone".into()]));
        assert_eq!(window.app.ai_selection_refusal(), Some(super::SELECTION_GONE));
        assert_eq!(window.app.ai_request_block("첫 상자"), Some(super::SELECTION_GONE));
    }

    /// A node of a Slides document is a target as a Canvas node is.
    #[test]
    fn a_slide_node_is_a_target() {
        let mut host = Host::start();
        let slides = host.create("slides", "발표");
        let page = match &host.artifact(&slides).body {
            ArtifactBody::Slides(document) => document.pages[0].id.clone(),
            _ => panic!("slides"),
        };
        let node: Node = serde_json::from_value(json!({
            "id": "title", "kind": "text", "text": "제목", "bounds": { "x": 0, "y": 0, "width": 1_000_000, "height": 400_000 }
        }))
        .expect("a text node");
        host.apply(&slides, vec![Operation::AddNode { page_id: page, parent_id: None, node }], [Intent::Content].into_iter().collect())
            .expect("the slide text");
        let mut window = Window::open(&mut host, &slides);
        window.step(&mut host, "select.node", json!({ "node_id": "title" })).expect("the text is selected");
        assert_eq!(window.app.ai_target_ids(), ["title"]);
    }

    /// A Figure's panel and its diagram node are targets.
    #[test]
    fn a_figure_panel_or_diagram_node_is_a_target() {
        let mut host = Host::start();
        let figure = drawn_figure(&mut host, "그림", 1);
        let (document, _) = host.figure(&figure);
        let panel = document.panels[0].id.clone();
        let node = match &document.panels[0].body {
            PanelBody::Diagram(diagram) => diagram.nodes[0].id.clone(),
            _ => panic!("a diagram panel"),
        };
        let mut window = Window::open(&mut host, &figure);
        window.step(&mut host, "select.node", json!({ "node_id": panel })).expect("the panel is selected");
        assert_eq!(window.app.ai_target_ids(), [panel.as_str()]);
        window.step(&mut host, "select.node", json!({ "node_id": node })).expect("the diagram node is selected");
        assert_eq!(window.app.ai_target_ids(), [node.as_str()]);
    }

    /// A text document of `kind` with the paragraphs 첫 문단, 둘째 문단 and 셋째 문단 at the end of its first section. Returns the
    /// document and the ids of those three paragraphs, in that order.
    fn text_document_with_paragraphs(host: &mut Host, kind: &str) -> (String, Vec<String>) {
        let document = host.create(kind, "보고서");
        let (section, end) = match &host.artifact(&document).body {
            ArtifactBody::Flow(flow) => (flow.sections[0].id.clone(), flow.sections[0].blocks.len() as u32),
            _ => panic!("{kind} is a text document"),
        };
        let operations = ["첫 문단", "둘째 문단", "셋째 문단"]
            .into_iter()
            .enumerate()
            .map(|(offset, text)| Operation::InsertFlowBlock { section_id: section.clone(), index: end + offset as u32, block: paragraph(text) })
            .collect();
        host.apply(&document, operations, [Intent::Content].into_iter().collect()).expect("the paragraphs are inserted");
        let ArtifactBody::Flow(flow) = host.artifact(&document).body else { panic!("{kind} is a text document") };
        let ids = flow.sections[0].blocks[end as usize..]
            .iter()
            .filter_map(|block| match block {
                FlowBlock::Paragraph(found) => Some(found.id.clone()),
                _ => None,
            })
            .collect();
        (document, ids)
    }

    /// In a text document (DOCX, HWPX, HWP) the paragraphs the user chose are the request's targets, in the order chosen. A paragraph
    /// chosen by its text range counts too. A selection the window re-applies on a reload is not the user's choice.
    #[test]
    fn a_text_document_sends_the_paragraphs_the_user_chose() {
        for kind in ["docx", "hwpx", "hwp"] {
            let mut host = Host::start();
            let (document, ids) = text_document_with_paragraphs(&mut host, kind);
            let mut window = Window::open(&mut host, &document);
            window.app.ai_model = "mock-model".into();
            assert!(window.app.ai_target_ids().is_empty(), "{kind}: nothing chosen covers the whole document");
            assert_eq!(window.app.ai_scope_label(), "작업물 전체", "{kind}");

            window.step(&mut host, "select.many", json!({ "node_ids": [ids[2], ids[0]] })).expect("two paragraphs are chosen");
            assert_eq!(window.app.ai_target_ids(), [ids[2].as_str(), ids[0].as_str()], "{kind}: the order chosen");
            assert_eq!(window.app.ai_scope_label(), "선택한 항목 2개", "{kind}");

            window.app.select_paragraph(ids[1].clone(), "둘째 문단");
            window.settle(&mut host);
            assert_eq!(window.app.ai_target_ids(), [ids[1].as_str()], "{kind}: the paragraph chosen by its text range");
            NotUserChoice::enter(&mut window.app, true).select_paragraph(ids[2].clone(), "셋째 문단");
            window.settle(&mut host);
            assert_eq!(window.app.ai_target_ids(), [ids[1].as_str()], "{kind}: a re-applied selection is not the choice");

            window.step(&mut host, "ai.request", json!({ "prompt": "정리" })).expect("the request is sent");
            let job = window.app.job.clone().expect("the job is shown");
            assert_eq!(job.target_ids, [ids[1].as_str()], "{kind}: the job names the paragraph the user chose");
        }
    }

    /// A file of a web project the user chose is the request's target. A file the project does not hold refuses the request.
    #[test]
    fn a_web_file_the_user_chose_is_the_request_target() {
        let mut host = Host::start();
        let project = host.create("web_code", "웹 화면");
        let mut window = Window::open(&mut host, &project);
        window.app.ai_model = "mock-model".into();
        assert!(window.app.ai_target_ids().is_empty(), "nothing chosen covers the whole project");
        assert_eq!(window.app.ai_scope_label(), "작업물 전체");

        window.step(&mut host, "select.node", json!({ "node_id": "missing.html" })).expect("the choice is made");
        assert_eq!(window.app.ai_selection_refusal(), Some(super::SELECTION_GONE));
        assert_eq!(window.app.invoke("ai.request", json!({ "prompt": "제목을 고쳐 주세요" })), Err(super::SELECTION_GONE.into()));

        window.step(&mut host, "select.node", json!({ "node_id": "index.html" })).expect("the file is chosen");
        assert_eq!(window.app.ai_target_ids(), ["index.html"]);
        assert_eq!(window.app.ai_scope_label(), "선택한 항목 1개");
        window.step(&mut host, "ai.request", json!({ "prompt": "제목을 고쳐 주세요" })).expect("the request is sent");
        let job = window.app.job.clone().expect("the job is shown");
        assert_eq!(job.target_ids, ["index.html"]);
    }

    /// A connector of a figure's diagram the user chose is the request's target, as a diagram node is.
    #[test]
    fn a_figure_connector_the_user_chose_is_the_request_target() {
        let mut host = Host::start();
        let figure = drawn_figure(&mut host, "그림", 2);
        let (document, _) = host.figure(&figure);
        let panel = document.panels[0].id.clone();
        let PanelBody::Diagram(diagram) = &document.panels[0].body else { panic!("a diagram panel") };
        let (from, to) = (diagram.nodes[0].id.clone(), diagram.nodes[1].id.clone());
        host.apply(&figure, vec![connection(&panel, &from, &to)], [Intent::Content, Intent::Design].into_iter().collect())
            .expect("the connector is drawn");
        let (document, _) = host.figure(&figure);
        let PanelBody::Diagram(diagram) = &document.panels[0].body else { panic!("a diagram panel") };
        let edge = diagram.edges[0].id.clone();

        let mut window = Window::open(&mut host, &figure);
        window.app.ai_model = "mock-model".into();
        window.step(&mut host, "select.node", json!({ "node_id": edge })).expect("the connector is chosen");
        assert_eq!(window.app.ai_selection_refusal(), None, "the connector is in the figure");
        assert_eq!(window.app.ai_target_ids(), [edge.as_str()]);
        window.step(&mut host, "ai.request", json!({ "prompt": "연결선을 고쳐 주세요" })).expect("the request is sent");
        let job = window.app.job.clone().expect("the job is shown");
        assert_eq!(job.target_ids, [edge.as_str()]);
    }

    /// A paragraph the text document does not hold is still the user's choice: the request is refused, not sent for the whole document.
    #[test]
    fn a_text_range_on_a_paragraph_the_document_lacks_refuses_the_request() {
        let mut host = Host::start();
        let (document, _) = text_document_with_paragraphs(&mut host, "docx");
        let mut window = Window::open(&mut host, &document);
        window.app.ai_model = "mock-model".into();

        window.app.select_paragraph("missing".into(), "");
        window.settle(&mut host);
        assert_eq!(window.app.ai_target_ids(), ["missing"]);
        assert_eq!(window.app.ai_selection_refusal(), Some(super::SELECTION_GONE));
        assert_eq!(window.app.invoke("ai.request", json!({ "prompt": "정리" })), Err(super::SELECTION_GONE.into()));
        window.settle(&mut host);
        assert!(window.app.job.is_none(), "a refused request starts no job");
    }

    /// The same scoped request, sent by the window's command and by the MCP tool, starts the same job, and the job's writes leave the
    /// same canvas: its change to the target goes through, and a change outside the target is refused with the canvas unchanged.
    #[test]
    fn a_scoped_request_gives_the_same_job_and_the_same_canvas_by_command_and_by_tool() {
        let mut host = Host::start();
        let by_command_canvas = canvas_with_boxes(&mut host, "그림판");
        let by_tool_canvas = canvas_with_boxes(&mut host, "그림판");
        let before = content_of(&host.artifact(&by_command_canvas));

        let mut window = Window::open(&mut host, &by_command_canvas);
        window.app.ai_model = "mock-model".into();
        window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");
        window.step(&mut host, "ai.request", json!({ "prompt": "첫 상자를 고쳐 주세요" })).expect("the request is sent");
        let by_command: Job = window.app.job.clone().expect("the job the request started is shown");
        drop(window);

        let backend = HostBackend::new(host);
        let mcp = Mcp::connect(backend.clone());
        let revision = backend.with(|host| host.artifact(&by_tool_canvas).revision);
        let started = mcp.call(
            "job_start",
            json!({
                "request_id": new_id(),
                "artifact_id": by_tool_canvas,
                "base_revision": revision,
                "allowed_intents": ["content", "design"],
                "runtime": "claude",
                "model": "mock-model",
                "effort": "high",
                "prompt": "첫 상자를 고쳐 주세요",
                "target_ids": ["a"],
            }),
        );
        let by_tool: Job = serde_json::from_value(started).expect("the job the tool started");
        assert_eq!(by_command.target_ids, ["a"], "the window's request names the selected box");
        assert_eq!(by_tool.target_ids, by_command.target_ids, "the tool's request names the same box");
        assert_eq!(by_tool.allowed_intents, by_command.allowed_intents);

        // The job's change to its target goes through in both canvases, and the two end up with the same content.
        mcp.call("changes_apply", job_change(&backend, &by_command_canvas, &by_command, "a"));
        mcp.call("changes_apply", job_change(&backend, &by_tool_canvas, &by_tool, "a"));
        let after_command = content_of(&backend.with(|host| host.artifact(&by_command_canvas)));
        let after_tool = content_of(&backend.with(|host| host.artifact(&by_tool_canvas)));
        assert_ne!(before, after_command, "the job changed its target");
        assert_eq!(after_command, after_tool, "the command and the tool left different content");

        // A change outside the target is refused, and the canvas keeps its content.
        for (artifact, job) in [(&by_command_canvas, &by_command), (&by_tool_canvas, &by_tool)] {
            let change = job_change(&backend, artifact, job, "c");
            let refusal = backend
                .with(|host| host.call("changes.apply", change))
                .expect_err("a change outside the target is refused");
            assert_eq!(refusal.code, ErrorCode::ContentLocked);
        }
        assert_eq!(content_of(&backend.with(|host| host.artifact(&by_command_canvas))), after_command, "the refusal left the canvas as it was");
        assert_eq!(content_of(&backend.with(|host| host.artifact(&by_tool_canvas))), after_tool, "the refusal left the canvas as it was");
    }
}
