use super::*;

fn is_follow_agent_actor(actor: &str) -> bool {
    ["mcp:", "job:"].iter().any(|&prefix| {
        actor.strip_prefix(prefix).and_then(|tail| tail.split(':').next()).is_some_and(|id| !id.is_empty())
    })
}

const MAX_WINDOW_COMMITS: usize = 4096;

#[derive(Default)]
pub(super) struct AgentFollow {
    candidates: BTreeMap<String, (u64, String, Screen)>,
    window_commits: BTreeMap<String, (u64, Option<u64>)>,
    pub(super) poll_ticket: Option<u64>,
    pub(super) home_in_poll: bool,
    pub(super) home_follow_cutoff: u64,
    pub(super) notices: VecDeque<String>,
    notice_order: BTreeMap<String, u64>,
    activity: BTreeMap<String, (u64, usize, Instant, u64)>,
    job_events: BTreeMap<String, u64>,
    revisions: BTreeMap<String, u64>,
    latest_listing: u64,
    switching: bool,
    deferred_revisions: BTreeMap<(String, String), u64>,
    last_refresh: Option<(String, String, u64)>,
    read_targets: BTreeMap<(String, String), u64>,
    baseline_done: bool,
    baseline_pending: bool,
    delivery_seq: Option<u64>,
    pub(super) proposal_delivery_seq: u64,
    deferred_edit_sync: Option<(String, String, u64, bool)>,
    deferred_text_sync: bool,
    deferred_selection_restore: bool,
}

impl DesktopApp {
    pub(super) fn agent_poll_cursor(&self) -> u64 {
        if !self.agent_follow.baseline_done && self.last_event_seq == 0 { i64::MAX as u64 }
        else { self.agent_follow.delivery_seq.unwrap_or(self.last_event_seq) }
    }

    pub(super) fn agent_delivery_cursor(&self) -> u64 {
        self.agent_follow.delivery_seq.unwrap_or(self.last_event_seq)
    }

    pub(super) fn agent_baseline_needed(&self) -> bool {
        !self.agent_follow.baseline_done && self.last_event_seq == 0
    }

    pub(super) fn begin_agent_poll(&mut self) -> u64 {
        let cursor = self.agent_poll_cursor();
        self.agent_follow.baseline_pending = self.agent_baseline_needed();
        cursor
    }

    pub(super) fn baseline_resync_changes(&self, value: &Value) -> Option<(bool, bool)> {
        if !self.agent_follow.baseline_pending || !value.get("resync_required").and_then(Value::as_bool).unwrap_or(false) {
            return None;
        }
        let returned = value.get("artifacts")?.as_array()?;
        let current: BTreeMap<_, _> = self.artifacts.iter().map(|artifact| (artifact.id.as_str(), artifact)).collect();
        let mut list_changed = !self.workspace_loaded || returned.len() != current.len();
        let mut active_found = self.active.is_none();
        let mut active_changed = false;
        for value in returned {
            let artifact = ArtifactSummary::deserialize(value).ok()?;
            if let Some(active) = self.active.as_ref().filter(|active| active.id == artifact.id) {
                active_found = true;
                active_changed = artifact.revision > active.revision;
            }
            list_changed |= current.get(artifact.id.as_str()).is_none_or(|old| {
                old.kind != artifact.kind || old.title != artifact.title || old.revision != artifact.revision
                    || old.engine_id != artifact.engine_id || old.dirty != artifact.dirty
                    || old.modified_at_ms != artifact.modified_at_ms || old.active_jobs != artifact.active_jobs
            });
        }
        Some((active_changed || !active_found, list_changed))
    }

    pub(super) fn finish_agent_poll(&mut self, value: &Value, delivered: u64) {
        let head = value.get("last_seq").and_then(Value::as_u64).unwrap_or(delivered);
        let resync = value.get("resync_required").and_then(Value::as_bool).unwrap_or(false);
        self.last_event_seq = head;
        let has_events = value.get("events").and_then(Value::as_array).is_some_and(|events| !events.is_empty());
        self.agent_follow.delivery_seq = (!resync && has_events && delivered < head).then_some(delivered);
        self.agent_follow.baseline_done = true;
        self.agent_follow.baseline_pending = false;
        if let Some(poll_ticket) = self.agent_follow.poll_ticket.take() {
            let cursor = if resync || !has_events { head } else { delivered };
            let pending = &self.pending;
            self.agent_follow.window_commits.retain(|_, (request_ticket, fence)| {
                // Writes precede later polls in the RPC worker; keep the first head until its events are delivered.
                if fence.is_none() && poll_ticket > *request_ticket && !pending.contains_key(request_ticket) {
                    *fence = Some(head);
                }
                fence.is_none_or(|fence| cursor < fence)
            });
        }
    }

    pub(super) fn can_remember_window_commit(&self, request_id: &str) -> bool {
        self.agent_follow.window_commits.len() < MAX_WINDOW_COMMITS
            || self.agent_follow.window_commits.contains_key(&request_id.to_ascii_lowercase())
    }

    pub(super) fn require_window_commit_capacity(&mut self, request_id: &str) -> bool {
        if self.can_remember_window_commit(request_id) { return true; }
        let error = DltError::new(ErrorCode::TargetBusy, "이전 변경을 확인하는 중입니다. 잠시 후 다시 시도해 주세요");
        if let Some(run) = self.run_context.as_mut() {
            run.mutation_attempted = true;
            run.enqueue_error = Some(error.clone());
        }
        self.set_error(error.message);
        false
    }

    pub(super) fn remember_window_commit(&mut self, request_id: String, ticket: u64) {
        let request_id = request_id.to_ascii_lowercase();
        let placeholder = format!("ticket:{ticket}");
        if request_id != placeholder { self.agent_follow.window_commits.remove(&placeholder); }
        self.agent_follow.window_commits.entry(request_id).and_modify(|entry| {
            if ticket > entry.0 { *entry = (ticket, None); }
        }).or_insert((ticket, None));
    }

    #[cfg(test)]
    pub(super) fn tracked_window_commits(&self) -> usize {
        self.agent_follow.window_commits.len()
    }

    pub(super) fn cancel_pending_agent_follow(&mut self) {
        let ids: Vec<_> = self.agent_follow.candidates.keys().cloned().collect();
        self.agent_follow.candidates.clear();
        for id in ids { self.dismiss_agent_notice_for(&id); }
    }

    pub(super) fn defer_agent_edit_sync(&mut self) {
        if self.agent_follow.deferred_edit_sync.is_none() {
            let Some(active) = &self.active else { return };
            self.agent_follow.deferred_edit_sync = Some((active.id.clone(), self.view_id.clone(), self.selection_generation, false));
        }
    }

    pub(super) fn defer_agent_context(&mut self, selection_pending: bool) {
        self.defer_agent_edit_sync();
        if let Some((_, _, generation, apply_context)) = &mut self.agent_follow.deferred_edit_sync {
            *generation = self.selection_generation;
            *apply_context = !selection_pending;
        }
    }

    pub(super) fn defer_agent_text_sync(&mut self) {
        self.agent_follow.deferred_text_sync = true;
    }

    pub(super) fn restore_agent_selection(&mut self) {
        if self.protect_text_draft() && (self.reselect.is_some() || self.select_after_commit.is_some()) {
            self.agent_follow.deferred_selection_restore = true;
        } else {
            self.agent_follow.deferred_selection_restore = false;
            self.restore_selection();
        }
    }

    pub(super) fn prune_agent_follow(&mut self, now: Instant) {
        let running: BTreeSet<_> = self.job.iter().chain(&self.parked_jobs)
            .filter(|job| figure_close::job_running(job)).map(|job| job.artifact_id.as_str()).collect();
        let candidates = &self.agent_follow.candidates;
        self.agent_follow.activity.retain(|id, (_, _, at, _)| candidates.contains_key(id) || running.contains(id.as_str()) || now.saturating_duration_since(*at) < Duration::from_secs(5));
        let activity = &self.agent_follow.activity;
        let candidates = &self.agent_follow.candidates;
        self.agent_follow.revisions.retain(|id, _| activity.contains_key(id) || candidates.contains_key(id));
        self.agent_follow.job_events.retain(|id, _| self.pending.values().any(|pending| matches!(&pending.action, PendingAction::AgentJob { job_id, .. } if job_id == id)));
    }

    pub(super) fn listed_active(&self, active: &ArtifactSummary) -> Option<ArtifactSummary> {
        let mut listed = match self.artifacts.iter().find(|item| item.id == active.id) {
            Some(listed) => listed.clone(),
            None if !self.can_follow_agent() => active.clone(),
            None => return None,
        };
        listed.revision = active.revision;
        Some(listed)
    }

    pub(super) fn advance_event_revision(&mut self, event: &Value) -> bool {
        let (Some(revision), Some(active)) = (event.get("revision").and_then(Value::as_u64), self.active.as_ref()) else {
            return false;
        };
        if revision < active.revision {
            return false;
        }
        if revision == active.revision && !matches!(event.get("event").and_then(Value::as_str),
            Some("artifact.changed" | "artifact.undone" | "artifact.redone")) {
            return false;
        }
        let artifact_id = active.id.clone();
        let key = (artifact_id, self.view_id.clone());
        self.agent_follow.read_targets.entry(key).and_modify(|target| *target = (*target).max(revision)).or_insert(revision);
        let (context_ready, artifact_ready, render_ready) = self.revision_reads_ready(revision, true);
        let already_requested = self.agent_follow.last_refresh.as_ref().is_some_and(|(id, view, seen)| {
            id == &active.id && view == &self.view_id && revision <= *seen
        });
        if (revision == active.revision && context_ready && artifact_ready && render_ready) || already_requested { return false; }
        if self.user_write_pending_for(&active.id) {
            self.agent_follow.deferred_revisions.entry((active.id.clone(), self.view_id.clone()))
                .and_modify(|target| *target = (*target).max(revision)).or_insert(revision);
            return false;
        }
        self.agent_follow.last_refresh = Some((active.id.clone(), self.view_id.clone(), revision));
        if let Some(active) = self.active.as_mut() { active.revision = revision; }
        true
    }

    fn revision_reads_ready(&self, revision: u64, include_pending: bool) -> (bool, bool, bool) {
        let Some(active) = &self.active else { return (true, true, true) };
        let pending = |matches_action: &dyn Fn(&PendingAction) -> bool| include_pending && self.pending.values().any(|request| {
            request.artifact_id.as_deref() == Some(active.id.as_str()) && matches_action(&request.action)
        });
        let context = self.context.as_ref().is_some_and(|snapshot| snapshot.artifact_id == active.id && snapshot.view_id == self.view_id && snapshot.revision >= revision)
            || pending(&|action| matches!(action, PendingAction::Context));
        let artifact = self.artifact.as_ref().is_some_and(|artifact| artifact.id == active.id && artifact.revision >= revision)
            || pending(&|action| matches!(action, PendingAction::Inspect));
        let render = active.kind == ArtifactKind::WebCode
            || self.render.as_ref().is_some_and(|render| render.artifact_id == active.id && render.revision >= revision
                && render.scene.pages.iter().any(|page| page.page_index == self.page_index))
            || self.page_textures.get(&self.page_index).is_some_and(|page| page.artifact_id == active.id && page.revision >= revision)
            || (self.requested_pages.get(&self.page_index).is_some_and(|(_, requested)| *requested >= revision)
                && pending(&|action| matches!(action, PendingAction::Render { page_index } if *page_index == self.page_index)))
            || pending(&|action| matches!(action, PendingAction::PreviewAsset { snapshot, page_index } if *page_index == self.page_index && snapshot.revision >= revision));
        (context, artifact, render)
    }

    pub(super) fn ensure_agent_revision_reads(&mut self) {
        let Some(active) = self.active.as_ref() else { return };
        let key = (active.id.clone(), self.view_id.clone());
        let Some(target) = self.agent_follow.read_targets.get(&key) else { return };
        if self.user_write_pending_for(&active.id) { return; }
        let revision = (*target).max(active.revision);
        let (context, artifact, render) = self.revision_reads_ready(revision, true);
        if !context { self.fetch_context(); }
        if !artifact { self.fetch_inspect(); }
        if !render { self.fetch_render(self.page_index); }
        if self.revision_reads_ready(revision, false) == (true, true, true) {
            self.agent_follow.read_targets.remove(&key);
        }
    }

    fn user_write_pending_for(&self, artifact_id: &str) -> bool {
        self.pending.values().any(|pending| (pending.artifact_id.as_deref() == Some(artifact_id)
            || pending.apply.as_ref().is_some_and(|apply| apply.artifact_id == artifact_id))
            && (pending.apply.is_some() || matches!(pending.method.as_str(),
            "changes.apply" | "history.undo" | "history.redo" | "history.undo_change" | "figure.dataset.edit"
                | "proposal.apply" | "design_system.publish" | "design_system.set")))
    }

    pub(super) fn resume_deferred_agent_revision(&mut self) {
        if self.agent_follow.deferred_edit_sync.is_some() && !self.protect_document_sync()
            && !self.pending.values().any(|pending| pending.method == "selection.set") {
            if let Some((artifact, view, generation, apply_context)) = self.agent_follow.deferred_edit_sync.take() {
                if self.active.as_ref().is_some_and(|active| active.id == artifact) && self.view_id == view {
                    if apply_context && self.selection_generation == generation {
                        if let Some(snapshot) = self.context.as_ref().cloned() {
                            self.set_selected_ids(snapshot.selection.node_ids.clone());
                            self.local_selection = Some(snapshot.selection.clone());
                            self.reselect = super::events::stale_selection(&snapshot);
                        }
                    }
                    self.restore_agent_selection();
                    self.resync_edit_state();
                }
            }
        }
        if self.agent_follow.deferred_selection_restore && !self.protect_document_sync() && !self.protect_text_draft() {
            self.restore_agent_selection();
        }
        if self.agent_follow.deferred_text_sync && !self.protect_document_sync() && !self.protect_text_draft() {
            self.agent_follow.deferred_text_sync = false;
            self.sync_text_target();
        }
        let Some(artifact) = self.active.as_ref().map(|active| active.id.clone()) else { return };
        if self.user_write_pending_for(&artifact) { return; }
        let key = (artifact.clone(), self.view_id.clone());
        let Some(revision) = self.agent_follow.deferred_revisions.remove(&key) else { return };
        let Some(active) = self.active.as_mut().filter(|active| active.id == artifact) else { return };
        if revision > active.revision {
            active.revision = revision;
            self.refresh_active();
        }
    }

    pub(super) fn agent_opened_view(&mut self) {
        let Some(active) = &self.active else { return };
        self.agent_follow.read_targets.retain(|(id, view), _| id == &active.id && view == &self.view_id);
        self.agent_follow.deferred_revisions.retain(|(id, view), _| id == &active.id && view == &self.view_id);
        self.agent_follow.deferred_edit_sync = None;
        self.agent_follow.deferred_text_sync = false;
        self.agent_follow.deferred_selection_restore = false;
    }

    pub(super) fn context_reply_is_older(&self, snapshot: &ContextSnapshot) -> bool {
        self.active.as_ref().is_some_and(|active| active.revision > snapshot.revision)
    }

    pub(super) fn inspect_reply_is_older(&self, artifact: &Artifact) -> bool {
        self.active.as_ref().is_some_and(|active| active.revision > artifact.revision)
    }

    pub(super) fn observe_agent_event(&mut self, event: &Value, sequence: u64, can_follow: bool) {
        let Some(artifact_id) = event.get("artifact_id").and_then(Value::as_str) else { return };
        let kind = event.get("event").and_then(Value::as_str).unwrap_or_default();
        let open = self.screen == Screen::Editor && self.active.as_ref().is_some_and(|active| active.id == artifact_id);
        if kind == "job.status" && open {
            if let Some(job_id) = event.pointer("/detail/job_id").and_then(Value::as_str) {
                self.request_agent_job(artifact_id, job_id, sequence);
            }
        }
        let actor = event.pointer("/detail/actor").and_then(Value::as_str).unwrap_or_default();
        if !matches!(kind, "artifact.created" | "artifact.changed") || !is_follow_agent_actor(actor) {
            return;
        }
        if let Some(id) = event.pointer("/detail/request_id").and_then(Value::as_str) {
            if self.agent_follow.window_commits.contains_key(id) {
                return;
            }
        }
        if kind == "artifact.created" {
            if let Some(job_id) = event.pointer("/detail/job_id").and_then(Value::as_str) {
                let key = format!("job:{job_id}");
                if self.agent_follow.window_commits.contains_key(&key) {
                    return;
                }
            }
        }
        if sequence <= self.agent_follow.home_follow_cutoff { return; }
        let revision = event.get("revision").and_then(Value::as_u64).unwrap_or(0);
        if open && self.active.as_ref().is_some_and(|active| revision < active.revision) {
            return;
        }
        if self.agent_follow.revisions.get(artifact_id).is_some_and(|seen| revision <= *seen) {
            return;
        }
        self.agent_follow.revisions.insert(artifact_id.to_owned(), revision);
        let changes = event.pointer("/detail/changed_ids").and_then(Value::as_array).map_or(0, Vec::len);
        self.agent_follow.activity.insert(artifact_id.to_owned(), (revision, changes, Instant::now(), sequence));
        if !open {
            if !can_follow { self.queue_agent_notice(artifact_id, sequence); }
            self.agent_follow.candidates.entry(artifact_id.to_owned()).or_insert((sequence, self.view_id.clone(), self.screen));
        }
    }

    pub(super) fn request_agent_follow_listing(&mut self) -> bool {
        let waiting: Vec<_> = self.agent_follow.candidates.iter()
            .map(|(id, (sequence, view, screen))| (*sequence, id.clone(), view.clone(), *screen))
            .filter(|(sequence, id, _, _)| !self.pending.values().any(|pending| {
                matches!(&pending.action, PendingAction::AgentWorkspaceList { candidates } if candidates.iter().any(|(sent, artifact_id, _, _)| sent == sequence && artifact_id == id))
            })).collect();
        !waiting.is_empty() && self.call("workspace.list", json!({}), PendingAction::AgentWorkspaceList { candidates: waiting }).is_some()
    }

    pub(super) fn accept_workspace_listing(&mut self, ticket: u64) -> bool {
        if ticket < self.agent_follow.latest_listing { return false; }
        self.agent_follow.latest_listing = ticket;
        true
    }

    pub(super) fn agent_workspace_listed(&mut self, value: Value, ticket: u64, mut candidates: Vec<(u64, String, String, Screen)>) {
        if !self.accept_workspace_listing(ticket) {
            self.request_agent_follow_listing();
            return;
        }
        let Ok(listing) = super::state::WorkspaceListing::deserialize(&value) else {
            self.workspace_listed(value);
            return;
        };
        let listed: BTreeSet<_> = listing.artifacts.iter().map(|item| item.id.clone()).collect();
        let view_changed = candidates.iter().any(|(_, _, view, screen)| self.view_id != *view || self.screen != *screen);
        let contains_active = self.active.as_ref().is_none_or(|active| listing.artifacts.iter().any(|item| item.id == active.id));
        if !view_changed || contains_active {
            self.workspace_listed(value);
        } else {
            for item in listing.artifacts {
                if !self.artifacts.iter().any(|existing| existing.id == item.id) { self.artifacts.push(item); }
            }
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(
            self.agent_follow.activity.get(&candidate.1).map_or(candidate.0, |(_, _, _, sequence)| *sequence)
        ));
        let can_follow = self.can_follow_agent();
        for (sequence, artifact_id, view_id, screen) in candidates {
            if !listed.contains(&artifact_id) {
                if self.agent_follow.candidates.get(&artifact_id).is_some_and(|candidate| candidate.0 == sequence) {
                    self.agent_follow.candidates.remove(&artifact_id);
                    self.dismiss_agent_notice_for(&artifact_id);
                }
                continue;
            }
            self.finish_follow_candidate(sequence, &artifact_id, &view_id, screen, can_follow);
        }
    }

    fn finish_follow_candidate(&mut self, sequence: u64, artifact_id: &str, view_id: &str, screen: Screen, can_follow: bool) {
        if self.agent_follow.candidates.get(artifact_id).is_none_or(|candidate| candidate.0 != sequence) {
            return;
        }
        self.agent_follow.candidates.remove(artifact_id);
        if self.view_id != view_id || self.screen != screen {
            if self.screen == Screen::Home {
                self.dismiss_agent_notice_for(artifact_id);
                return;
            }
            if self.screen != Screen::Editor || self.active.as_ref().is_none_or(|active| active.id != artifact_id) {
                self.queue_agent_notice(artifact_id, sequence);
            }
            return;
        }
        let Some(summary) = self.artifacts.iter().find(|item| item.id == artifact_id).cloned() else { return };
        let input_arrived = self.exit_ctx.as_ref().is_some_and(|ctx| ctx.input(|input| !input.events.is_empty()));
        if !input_arrived && can_follow {
            if let Some(ctx) = self.exit_ctx.clone() { self.handle_nudge_keys(&ctx); }
        }
        if !input_arrived && can_follow && self.can_follow_agent() {
            self.dismiss_agent_notice_for(artifact_id);
            self.agent_follow.switching = true;
            self.activate(summary);
            self.agent_follow.switching = false;
        } else {
            self.queue_agent_notice(artifact_id, sequence);
        }
    }

    pub(super) fn can_follow_agent(&self) -> bool {
        if self.protect_editor_input() { return false; }
        let selection_settled = self.selected_nodes.is_empty() || self.active.as_ref().is_some_and(|active| {
            self.context.as_ref().is_some_and(|context| context.revision == active.revision)
                && self.artifact.as_ref().is_some_and(|artifact| artifact.revision == active.revision)
                && !self.pending.values().any(|pending| pending.apply.is_some())
        });
        selection_settled && (self.figure_switch_warning().is_none() || self.figure_only_job_remains())
    }

    pub(super) fn protect_editor_input(&self) -> bool {
        let input_busy = self.exit_ctx.as_ref().is_some_and(|ctx| {
            ctx.wants_keyboard_input() || ctx.dragged_id().is_some() || egui::Popup::is_any_open(ctx)
                || super::toolbox::icon_picker_open(ctx)
                || ctx.memory(|memory| memory.top_modal_layer()).is_some()
                || ctx.input(|input| input.pointer.any_down())
        });
        input_busy || self.protect_text_draft() || self.inline_text_edit.is_some() || self.text_adapter.is_ime_active()
            || self.drag.is_some() || self.marquee.is_some() || self.layer_rename.is_some()
            || self.image_crop_editor.is_some() || self.video_insert.is_some() || self.pending_image_replacement.is_some()
            || self.board_memo_editor.is_some() || self.board_memo_gesture.is_some() || self.page_section_editor.is_some()
            || self.dataset_edit.is_some() || self.csv_draft.is_some() || self.figure_form.is_some()
            || !self.ai_prompt.trim().is_empty() || !self.ai_figure_refs.is_empty()
            || (self.screen == Screen::Home && (!self.home.prompt.trim().is_empty() || !self.home.attachments.is_empty()))
            || self.key_dialog_open() || self.ui_test_approval_open() || self.has_close_work_besides_figure_jobs()
            || self.pending_switch.is_some() || self.export_confirmation.is_some() || self.close_confirmation_open
            || !self.drafts.recovered().is_empty() || self.run_context.is_some()
            || self.pending.values().any(|pending| pending.action.user_initiated())
    }

    pub(super) fn protect_document_sync(&self) -> bool {
        self.text_editor_focused || self.inline_text_edit.is_some() || self.text_adapter.is_ime_active()
            || self.drag.is_some() || self.marquee.is_some() || self.board_memo_gesture.is_some()
            || self.exit_ctx.as_ref().is_some_and(|ctx| ctx.wants_keyboard_input() || ctx.dragged_id().is_some()
                || egui::Popup::is_any_open(ctx) || super::toolbox::icon_picker_open(ctx)
                || ctx.memory(|memory| memory.top_modal_layer()).is_some()
                || ctx.input(|input| input.pointer.any_down()))
    }

    pub(super) fn protect_text_draft(&self) -> bool {
        self.text_editor_focused || self.text_target.as_ref().is_some_and(|target| {
            let original = match target {
                TextTarget::Node { original, .. } | TextTarget::Paragraph { original, .. }
                | TextTarget::WebFile { original, .. } | TextTarget::PanelLabel { original, .. } => original,
            };
            self.text_draft != *original
        })
    }

    pub(super) fn auto_following_figure_job_only(&self) -> bool {
        self.agent_follow.switching && self.figure_only_job_remains()
    }

    pub(super) fn open_agent_notice(&mut self) {
        while let Some(artifact_id) = self.agent_follow.notices.front().cloned() {
            if self.artifacts.iter().any(|artifact| artifact.id == artifact_id) { break; }
            if self.agent_follow.candidates.contains_key(&artifact_id) { return; }
            self.dismiss_agent_notice_for(&artifact_id);
        }
        let Some(artifact_id) = self.agent_follow.notices.front().cloned() else { return };
        if self.can_follow_agent() {
            if let Some(ctx) = self.exit_ctx.clone() { self.handle_nudge_keys(&ctx); }
        }
        if !self.can_follow_agent() {
            self.notices.info(messages::AGENT_FOLLOW_BUSY, Instant::now());
            return;
        }
        self.invoke_ui("nav.open_project", json!({ "artifact_id": artifact_id }));
    }

    pub(super) fn dismiss_agent_notice_for(&mut self, artifact_id: &str) {
        self.agent_follow.notices.retain(|id| id != artifact_id);
        self.agent_follow.notice_order.remove(artifact_id);
    }

    pub(super) fn prune_unlisted_agent_notices(&mut self) {
        let candidates = &self.agent_follow.candidates;
        self.agent_follow.notices.retain(|id| candidates.contains_key(id) || self.artifacts.iter().any(|artifact| &artifact.id == id));
        self.agent_follow.notice_order.retain(|id, _| self.agent_follow.notices.contains(id));
    }

    fn queue_agent_notice(&mut self, artifact_id: &str, sequence: u64) {
        if !self.agent_follow.notices.iter().any(|id| id == artifact_id) {
            let position = self.agent_follow.notices.iter().position(|id| {
                self.agent_follow.notice_order.get(id).is_some_and(|order| *order > sequence)
            }).unwrap_or(self.agent_follow.notices.len());
            self.agent_follow.notices.insert(position, artifact_id.to_owned());
            self.agent_follow.notice_order.insert(artifact_id.to_owned(), sequence);
        }
        if self.notices.current(Instant::now()).is_none() {
            self.notices.info(messages::AGENT_FOLLOW_NOTICE, Instant::now());
        }
    }

    pub(super) fn agent_poll_interval(&self) -> Duration {
        // Half-second polls keep active edits visible; idle workspaces keep the existing two-second interval.
        if self.agent_is_working(Instant::now()) { Duration::from_millis(500) } else { Duration::from_secs(2) }
    }

    pub(super) fn agent_repaint_interval(&self) -> Duration {
        self.agent_poll_interval().min(Duration::from_secs(1))
    }

    pub(super) fn agent_is_working(&self, now: Instant) -> bool {
        self.agent_follow.activity.values().any(|(_, _, at, _)| now.saturating_duration_since(*at) < Duration::from_secs(5))
            || self.job.iter().chain(&self.parked_jobs).any(figure_close::job_running)
    }

    pub(super) fn agent_status(&self, now: Instant) -> Option<String> {
        let active = self.active.as_ref()?;
        let running = self.job.iter().chain(&self.parked_jobs).any(|job| job.artifact_id == active.id && figure_close::job_running(job));
        let recent = self.agent_follow.activity.get(&active.id).filter(|(_, _, at, _)| {
            running || now.saturating_duration_since(*at) < Duration::from_secs(5)
        });
        if !running && recent.is_none() { return None; }
        let (revision, changes) = recent.map_or((active.revision, 0), |(revision, changes, _, _)| (*revision, *changes));
        Some(messages::agent_editing(revision, changes))
    }

    pub(super) fn draw_other_running_jobs(&self, ui: &mut Ui, palette: &crate::theme::Palette) {
        let Some(active) = &self.active else { return };
        for job in self.parked_jobs.iter().filter(|job| job.artifact_id == active.id && figure_close::job_running(job)) {
            let name = job.client_name.as_deref().filter(|name| !name.is_empty()).unwrap_or(&job.runtime);
            ui.label(format!("{} · {}", if name.is_empty() { "AI 작업" } else { name }, job.id));
            let stages = match job.status {
                JobStatus::Queued => [crate::widgets::Stage::Current(0.0), crate::widgets::Stage::Pending, crate::widgets::Stage::Pending],
                JobStatus::Running => [crate::widgets::Stage::Done, crate::widgets::Stage::Current(0.5), crate::widgets::Stage::Pending],
                _ => [crate::widgets::Stage::Done, crate::widgets::Stage::Done, crate::widgets::Stage::Current(0.5)],
            };
            crate::widgets::progress_rail(ui, palette, stages, palette.agents[0]);
            crate::widgets::divider(ui, palette);
        }
    }

    pub(super) fn discover_listed_jobs(&mut self) {
        let Some(active) = self.active.as_ref() else { return };
        let artifact_id = active.id.clone();
        let jobs: Vec<String> = active.active_jobs.iter().map(|job| job.job_id.clone()).collect();
        for job_id in jobs {
            let known = self.job.iter().chain(&self.parked_jobs).any(|job| job.id == job_id);
            let pending = self.pending.values().any(|pending| matches!(&pending.action, PendingAction::AgentJob { job_id: id, view_id, .. } if id == &job_id && view_id == &self.view_id));
            if !known && !pending {
                self.request_agent_job(&artifact_id, &job_id, self.agent_delivery_cursor());
            }
        }
    }

    fn request_agent_job(&mut self, artifact_id: &str, job_id: &str, sequence: u64) {
        self.agent_follow.job_events.insert(job_id.to_owned(), sequence);
        self.call("job.get", json!({ "job_id": job_id }), PendingAction::AgentJob {
            sequence, artifact_id: artifact_id.to_owned(), job_id: job_id.to_owned(), view_id: self.view_id.clone(),
        });
    }

    pub(super) fn agent_job_read(&mut self, value: Value, sequence: u64, artifact_id: &str, job_id: &str, view_id: &str) {
        if self.view_id != view_id || self.active.as_ref().is_none_or(|active| active.id != artifact_id)
            || self.agent_follow.job_events.get(job_id) != Some(&sequence) {
            return;
        }
        let Ok(job) = serde_json::from_value::<Job>(value) else { return };
        if job.id != job_id || job.artifact_id != artifact_id { return; }
        if self.job.iter().chain(&self.parked_jobs).any(|known| known.id == job_id) {
            self.update_job(job);
        } else {
            self.record_started_job(job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::*;

    #[test]
    fn expired_activity_and_finished_job_tracking_are_pruned_without_reviving_old_replies() {
        let mut h = Harness::new();
        h.open_document("canvas", 1);
        h.app.handle_events(json!({ "events": [{ "event_seq": 1, "artifact_id": "a1", "revision": 2,
            "event": "artifact.changed", "detail": { "actor": "mcp:connection" } }] }));
        h.sent();
        let job_event = |seq| json!({ "events": [{ "event_seq": seq, "artifact_id": "a1", "revision": 2,
            "event": "job.status", "detail": { "job_id": "job-1" } }] });
        h.app.handle_events(job_event(2));
        let old = h.sent();
        h.app.handle_events(job_event(3));
        let new = h.sent();
        let job = |status| json!({ "id": "job-1", "attempt_id": "attempt-1", "artifact_id": "a1", "base_revision": 1,
            "status": status, "allowed_intents": ["content"], "model": "mock", "effort": "high", "error": null });
        h.answer(&new, "job.get", Ok(job("completed")));
        h.app.prune_agent_follow(Instant::now() + Duration::from_secs(10));
        assert!(h.app.agent_follow.activity.is_empty());
        assert!(h.app.agent_follow.revisions.is_empty());
        assert_eq!(h.app.agent_follow.job_events.get("job-1"), Some(&3));
        h.answer(&old, "job.get", Ok(job("running")));
        assert_eq!(h.app.job.as_ref().unwrap().status, JobStatus::Completed);
        h.app.prune_agent_follow(Instant::now() + Duration::from_secs(10));
        assert!(h.app.agent_follow.job_events.is_empty());
    }
}
