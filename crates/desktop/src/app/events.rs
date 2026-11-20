use super::*;
use super::{doc_tree::{find_node, find_paragraph, paragraph_text}, state::{is_inline_text_actor, AGENT_BATCH_LIMIT}};

use std::ops::{Deref, DerefMut};

/// Marks the window's selection changes as not the user's choice while the guard lives (`UiChannel::restoring_selection`). It goes back
/// to its earlier state when dropped, so a panic inside the change does not leave the mark set.
pub(super) struct NotUserChoice<'a> {
    app: &'a mut DesktopApp,
    previous: bool,
}

impl<'a> NotUserChoice<'a> {
    /// The changes made through the guard are not the user's choice when `not_user` is set; otherwise the guard changes nothing.
    pub(super) fn enter(app: &'a mut DesktopApp, not_user: bool) -> Self {
        let previous = app.ui.restoring_selection;
        app.ui.restoring_selection = previous || not_user;
        Self { app, previous }
    }
}

impl Deref for NotUserChoice<'_> {
    type Target = DesktopApp;

    fn deref(&self) -> &DesktopApp {
        self.app
    }
}

impl DerefMut for NotUserChoice<'_> {
    fn deref_mut(&mut self) -> &mut DesktopApp {
        self.app
    }
}

impl Drop for NotUserChoice<'_> {
    fn drop(&mut self) {
        self.app.ui.restoring_selection = self.previous;
    }
}

/// Whether a change came from an agent: neither the window's own edits (`desktop`) nor the text typed into the window
/// (`desktop:inline:<view>`).
fn is_agent_actor(actor: &str) -> bool {
    actor != "desktop" && !is_inline_text_actor(actor)
}

/// The ids of a selection, each once, in the order they were chosen.
fn distinct_ids(ids: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    ids.iter().filter(|id| seen.insert(id.as_str())).cloned().collect()
}

impl DesktopApp {
    pub(super) fn poll_events(&mut self) {
        // A UI-test window polls more often, so that an agent's input runs reach it soon (task 69).
        let interval = if self.ui_test.is_some() { super::ui_test_input::TEST_EVENT_POLL } else { Duration::from_secs(2) };
        if self.host_lost() || Instant::now().duration_since(self.last_event_poll) < interval {
            return;
        }
        self.last_event_poll = Instant::now();
        let mut params = json!({ "after_seq": self.last_event_seq });
        if let Some(token) = self.ui.token.clone() {
            params["view_id"] = json!(self.ui.view_id);
            params["view_token"] = json!(token);
        }
        self.call("events.poll", params, PendingAction::PollEvents);
    }

    /// Keeps the job reservations of the open document: `targets.reserved` adds one, `targets.released` ends it.
    pub(super) fn record_reservation(&mut self, event: &Value, about_active: bool) {
        if !about_active {
            return;
        }
        let detail = event.get("detail");
        let text = |key: &str| -> String { detail.and_then(|detail| detail.get(key)).and_then(Value::as_str).unwrap_or_default().to_owned() };
        match event.get("event").and_then(Value::as_str) {
            Some("targets.reserved") => {
                let node_ids: Vec<String> = detail
                    .and_then(|detail| detail.get("node_ids"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let reservation = JobReservation {
                    reservation_id: text("reservation_id"),
                    job_id: text("job_id"),
                    owner: text("owner"),
                    node_ids,
                };
                self.reservations.retain(|kept| kept.reservation_id != reservation.reservation_id);
                self.reservations.push(reservation);
            }
            Some("targets.released") => {
                let released = text("reservation_id");
                self.reservations.retain(|kept| kept.reservation_id != released);
                self.resume_held_edits();
            }
            _ => {}
        }
    }

    /// Keeps the agent's change batches of the open document for taking one back. The window's own edits
    /// are not listed; a batch leaves the list when it is undone.
    pub(super) fn record_batch(&mut self, event: &Value, about_active: bool) {
        let detail = event.get("detail");
        let text = |key: &str| detail.and_then(|detail| detail.get(key)).and_then(Value::as_str).unwrap_or_default().to_owned();
        match event.get("event").and_then(Value::as_str) {
            Some("artifact.changed") if about_active => {
                let (request_id, actor) = (text("request_id"), text("actor"));
                if is_agent_actor(&actor) && !request_id.is_empty() {
                    self.agent_batches.push(AgentBatch { request_id, actor });
                    if self.agent_batches.len() > AGENT_BATCH_LIMIT {
                        self.agent_batches.remove(0);
                    }
                }
            }
            Some("artifact.undone") => {
                let undone = text("undone_change");
                self.agent_batches.retain(|batch| batch.request_id != undone);
            }
            _ => {}
        }
    }

    pub(super) fn handle_events(&mut self, value: Value) {
        self.prune_pending_save_notices(Instant::now());
        self.take_ui_runs(&value);
        let events = value
            .get("events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // Someone else's commit (an agent, another window) moves the active document past the
        // revision this window shows; anything else only needs the work list re-read.
        let mut document_moved = value
            .get("resync_required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut list_changed = false;
        for event in events {
            let Some(sequence) = event.get("event_seq").and_then(Value::as_u64) else {
                continue;
            };
            if sequence <= self.last_event_seq {
                continue;
            }
            self.last_event_seq = sequence;
            list_changed = true;
            self.record_save_notice(&event);
            let about_active = self.active.as_ref().is_some_and(|active| {
                event.get("artifact_id").and_then(Value::as_str) == Some(active.id.as_str())
            });
            let newer = match (event.get("revision").and_then(Value::as_u64), self.active.as_ref()) {
                (Some(revision), Some(active)) => revision > active.revision,
                _ => false,
            };
            if about_active && newer {
                document_moved = true;
            }
            self.record_batch(&event, about_active);
            self.record_reservation(&event, about_active);
            self.record_proposal(&event);
        }
        if document_moved {
            self.refresh_active();
        } else if list_changed {
            self.call("workspace.list", json!({}), PendingAction::WorkspaceList);
        }
    }

    /// Selections last for one revision at the host: after every commit the host reports the
    /// previous selection as stale and an empty current one. The window asks for the same
    /// element again, so the inspector does not empty itself after each edit. An element that
    /// no longer exists (an addition that was undone, or one an agent removed) is dropped from the window's selection. The items
    /// the user chose stay in `user_selection`: an AI request names those and refuses while one of them is gone.
    pub(super) fn restore_selection(&mut self) {
        // A picture the window just put into the document is selected once the document that
        // holds it has been loaded; it takes the place of whatever was selected before.
        if let Some(ids) = self.select_after_commit.take() {
            let ticket = self.select_after_commit_ticket.take();
            // The selection a commit an agent started brings is the agent's work: it moves the window's selection, not `user_selection`.
            let agent_commit = ticket.is_some_and(|ticket| self.ui.take_agent_commit(ticket));
            if let Some(guard) = self.select_after_commit_guard.take() {
                if Some(guard.ticket) == ticket {
                    let core_cleared_selection = |selection: &Selection| selection.node_ids.is_empty() && selection.text_range.is_none();
                    let context_still_matches = self.context.as_ref().is_some_and(|context| {
                        context.revision == guard.committed_revision
                            && context.visible_page == guard.expected_page
                            && (selection_matches(&context.selection, &guard.expected_selection) || core_cleared_selection(&context.selection))
                    });
                    let local_still_matches = self.local_selection.as_ref().is_none_or(|selection| {
                        selection_matches(selection, &guard.expected_selection) || core_cleared_selection(selection)
                    });
                    let view_still_matches = self.view_ref().page_index == guard.expected_page
                        && self.selection_generation == guard.expected_generation;
                    if !context_still_matches || !local_still_matches || !view_still_matches {
                        self.reselect = None;
                        return;
                    }
                }
            }
            let ids: Vec<String> = ids.into_iter().filter(|id| self.artifact.as_ref().is_some_and(|artifact| find_node(artifact, id).is_some())).collect();
            if !ids.is_empty() {
                NotUserChoice::enter(self, agent_commit).select_ids(ids);
                return;
            }
        } else {
            self.select_after_commit_ticket = None;
            self.select_after_commit_guard = None;
        }
        let Some(previous) = self.reselect.take() else {
            return;
        };
        let Some(artifact) = self.artifact.as_ref() else {
            return;
        };
        // Every selected element the document still holds stays selected, in its order (task 63 item 5: a multi-selection
        // used to shrink to its first element after each commit).
        let mut nodes: Vec<String> = previous.node_ids.iter().filter(|id| selection_target_exists(artifact, id)).cloned().collect();
        let paragraph = previous
            .text_range
            .as_ref()
            .and_then(|range| find_paragraph(artifact, &range.paragraph_id))
            .map(|paragraph| (paragraph.id.clone(), paragraph_text(paragraph)));
        // Re-applying the window's own selection is not a choice of the user: `user_selection` keeps what the user chose.
        let mut restored = NotUserChoice::enter(self, true);
        if nodes.len() > 1 {
            restored.select_ids(nodes);
        } else if let Some(id) = nodes.pop() {
            restored.select_node(id);
        } else if let Some((id, text)) = paragraph {
            restored.select_paragraph(id, &text);
        }
    }

    pub(super) fn set_selection(&mut self, selection: Selection) {
        if self.inline_text_edit.as_ref().is_some_and(|edit| {
            selection.node_ids.len() != 1 || selection.node_ids.first() != Some(&edit.node_id)
        }) {
            return;
        }
        // The items the user chose are what an AI request names. A selection a reload re-applies, or one an agent's run makes, is not a choice.
        if self.user_chooses_selection() {
            let view_id = self.view_id.clone();
            let artifact = self.active.as_ref().map(|active| active.id.clone());
            let mut node_ids = distinct_ids(&selection.node_ids);
            // The paragraph a text range of a text document names is chosen too (the outline, a find result, the caret). A paragraph the
            // document does not hold stays in the choice: the request is then refused, not sent for the whole document.
            if let Some(range) = &selection.text_range {
                let text_document = self.artifact.as_ref().is_some_and(|document| matches!(&document.body, ArtifactBody::Flow(_)));
                if text_document && !node_ids.contains(&range.paragraph_id) {
                    node_ids.push(range.paragraph_id.clone());
                }
            }
            self.ui.user_selection = artifact.map(|artifact| (view_id, artifact, node_ids));
        }
        self.covered_table_text_selection = None;
        self.selection_generation = self.selection_generation.saturating_add(1);
        if self.pending_image_replacement.as_ref().is_some_and(|pending| pending.stage == ImageReplacementStage::Importing) {
            self.pending_image_replacement = None;
        }
        // Asking for a selection settles the matter; an older one is not restored over it.
        self.reselect = None;
        self.local_selection = Some(selection.clone());
        let (Some(active), Some(context)) = (&self.active, &self.context) else {
            return;
        };
        let request = SelectionRequest {
            artifact_id: active.id.clone(),
            expected_revision: context.revision,
            view_id: self.view_id.clone(),
            selection,
            visible_page: self.page_index,
        };
        match serde_json::to_value(request) {
            Ok(params) => {
                self.call("selection.set", params, PendingAction::Selection);
            }
            Err(_) => self.set_error(String::from("선택을 저장하지 못했습니다")),
        }
    }

    /// Whether the selection change under way is the user's own choice, the only kind `user_selection` keeps. An agent's run
    /// (`run_context`) and a reload that re-applies a selection (`restoring_selection`) are not.
    pub(super) fn user_chooses_selection(&self) -> bool {
        self.run_context.is_none() && !self.ui.restoring_selection
    }

    /// The selection, page and selection generation that a commit moving the selection must still find when its answer comes
    /// (`PendingAction::ApplyWithSelectionIfUnchanged`).
    pub(super) fn selection_commit_guard(&self) -> Result<(Selection, u32, u64), String> {
        let context = self.context.as_ref().ok_or("작업물 상태를 읽는 중입니다")?;
        Ok((context.selection.clone(), context.visible_page, self.selection_generation))
    }

    /// Enqueues a commit whose answer brings a selection. A commit an agent's run started is remembered, so that the selection it brings
    /// does not become the user's choice when it is applied (`restore_selection`).
    pub(super) fn enqueue_selecting_commit(&mut self, operations: Vec<Operation>, allowed_intents: BTreeSet<Intent>, action: PendingAction) {
        let by_agent = !self.user_chooses_selection();
        if let Some(ticket) = self.enqueue_operations_for(operations, allowed_intents, action) {
            if by_agent {
                self.ui.remember_agent_commit(ticket);
            }
        }
    }

    pub(super) fn select_node(&mut self, node_id: String) {
        if self.inline_text_edit.as_ref().is_some_and(|edit| edit.node_id != node_id) {
            return;
        }
        self.set_selected_ids(vec![node_id.clone()]);
        self.set_selection(Selection {
            node_ids: vec![node_id],
            text_range: None,
        });
        self.sync_style_fields();
        self.sync_text_target();
    }

    pub(super) fn select_paragraph(&mut self, paragraph_id: String, text: &str) {
        let end = text.graphemes(true).count().min(u32::MAX as usize) as u32;
        self.set_selection(Selection {
            node_ids: Vec::new(),
            text_range: Some(TextRange {
                paragraph_id,
                start: 0,
                end,
            }),
        });
    }
}

/// The artifact in an `artifact.inspect` reply: the artifact itself, or wrapped in `artifact`.
pub(super) fn parse_artifact(value: &Value) -> Option<Artifact> {
    serde_json::from_value::<Artifact>(value.clone())
        .or_else(|_| {
            serde_json::from_value::<Artifact>(value.get("artifact").cloned().unwrap_or(Value::Null))
        })
        .ok()
}

/// The selection the host dropped when the document moved to a new revision, when it names one.
pub(super) fn selection_matches(left: &Selection, right: &Selection) -> bool {
    left.node_ids == right.node_ids
        && match (&left.text_range, &right.text_range) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                left.paragraph_id == right.paragraph_id && left.start == right.start && left.end == right.end
            }
            _ => false,
        }
}

pub(super) fn stale_selection(snapshot: &ContextSnapshot) -> Option<Selection> {
    if !snapshot.selection.node_ids.is_empty() || snapshot.selection.text_range.is_some() {
        return None;
    }
    let stale = snapshot
        .selected_properties
        .get("stale_selection")?
        .get("selection")?;
    let selection: Selection = serde_json::from_value(stale.clone()).ok()?;
    (!selection.node_ids.is_empty() || selection.text_range.is_some()).then_some(selection)
}

/// Whether `id` still names something that can be selected: a canvas or slide node, a diagram
/// node, a figure panel or connector, a paragraph or a file of a web project.
pub(super) fn selection_target_exists(artifact: &Artifact, id: &str) -> bool {
    find_node(artifact, id).is_some()
        || find_paragraph(artifact, id).is_some()
        || matches!(&artifact.body, ArtifactBody::Figure(figure) if figure.panels.iter().any(|panel| panel.id == id))
        || matches!(&artifact.body, ArtifactBody::Figure(figure) if figure.panels.iter().any(|panel| matches!(&panel.body, designllm_contracts::PanelBody::Diagram(diagram) if diagram.edges.iter().any(|edge| edge.id == id))))
        || matches!(&artifact.body, ArtifactBody::WebCode(document) if document.files.contains_key(id))
}
