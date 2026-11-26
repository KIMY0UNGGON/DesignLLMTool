//! The window's link to its host (task 64-7). The host owns every committed edit (SPEC §6: the workspace commit is the durable
//! save), so a window whose host is gone keeps nothing of the document that the host does not have; unsent requests and editor
//! drafts are the window's own file (`drafts.rs`). When a reply says the host is not running, the window says so over the editor,
//! refuses changes, and offers to start the hidden host again (the launch path, same workspace) or to close.

use std::sync::{mpsc, Arc};

use designllm_contracts::{DltError, DltResult, ErrorCode};
use eframe::egui::{self, Context, Id};
use serde_json::json;

use designllm_contracts::{new_id, RenderProfile, SelectionRequest, ViewTransform};

use super::{DesktopApp, PendingAction};
use crate::commands::{self, CommandSpec};

/// Starts the hidden host for the window's workspace, as the launch does (`designllm_application::ensure_host`).
pub(crate) type HostStarter = Arc<dyn Fn() -> DltResult<()> + Send + Sync>;

pub(super) const HOST_LOST: &str = "호스트와 연결이 끊겼습니다";
pub(super) const HOST_LOST_REFUSAL: &str = "호스트와 연결이 끊겨 실행할 수 없습니다";
pub(super) const RECONNECT: &str = "다시 연결";
pub(super) const QUIT: &str = "종료";
pub(super) const HOST_LOST_CLOSE_NOTE: &str = "닫으면 보류 중인 변경은 버려지고, 입력 중인 글은 초안으로 남습니다";

#[derive(Default)]
pub(crate) struct HostLink {
    pub(super) lost: bool,
    pub(super) starter: Option<HostStarter>,
    /// The ticket of the `system.hello` that checks whether the host is still there.
    pub(super) probe: Option<u64>,
    pub(super) reconnecting: Option<mpsc::Receiver<DltResult<()>>>,
    /// Why the last reconnect failed, in the window's words.
    failure: Option<String>,
    /// `종료` was chosen: the close goes past the close question, which needs the host to release held work.
    pub(super) quitting: bool,
    /// The selection to give the new host once its state is read (`resume_selection_after_reconnect`).
    selection_after_reconnect: Option<designllm_contracts::Selection>,
}

impl DesktopApp {
    pub(crate) fn with_host_starter(mut self, starter: Option<HostStarter>) -> Self {
        self.host_link.starter = starter;
        self
    }

    /// A reply that says the host is not running may be one lost answer (the client connects again on the next call, and the
    /// window repeats the request with its id), so the window asks the host once more (`system.hello`). Only when that check
    /// finds no host either is the link lost.
    /// The check goes over the pipe (`host.heartbeat`, which the pipe server answers): the bridge answers `system.hello` from the
    /// handshake of a connection it still holds, so that would find a host that no longer answers. A transport error (IO: the pipe
    /// cannot be opened, or nothing comes back in time) is a host that is not there either.
    pub(super) fn note_host_reply(&mut self, ticket: u64, error: Option<&DltError>) {
        // The bridge marks its own transport failures retryable (bridge client.rs: the pipe cannot be opened, no answer in
        // time); an IO error the host reports about a file is not, and says nothing about the link.
        let unreachable = error.is_some_and(|error| error.code == ErrorCode::HostNotRunning || (error.code == ErrorCode::Io && error.retryable));
        if self.host_link.probe == Some(ticket) {
            self.host_link.probe = None;
            self.host_link.lost |= unreachable;
        } else if unreachable && !self.host_link.lost && self.host_link.probe.is_none() {
            self.host_link.probe = self.call(designllm_bridge::protocol::METHOD_HEARTBEAT, json!({}), PendingAction::HostProbe);
        }
    }

    /// The refusal of a command while the host is gone. Only the commands that call no host run (`commands::core_calls` states
    /// none for them: the window's panels, tabs and zoom); a read through the host or a document switch would empty the window.
    pub(super) fn host_lost_refusal(&self, spec: &CommandSpec) -> Option<&'static str> {
        let local = commands::core_calls(spec.id).is_some_and(<[&str]>::is_empty);
        (self.host_link.lost && !local).then_some(HOST_LOST_REFUSAL)
    }

    /// Whether the window is to send nothing to its host now (its polls and shortcuts wait).
    pub(super) fn host_lost(&self) -> bool {
        self.host_link.lost
    }

    /// Starts the hidden host again on a worker thread; `poll_reconnect` takes its answer.
    pub(super) fn reconnect_host(&mut self) {
        if self.host_link.reconnecting.is_some() {
            return;
        }
        let Some(starter) = self.host_link.starter.clone() else {
            return;
        };
        let (send, receive) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("designllm-host-reconnect".into()).spawn(move || {
            let _ = send.send(starter());
        });
        match spawned {
            Ok(_) => {
                self.host_link.failure = None;
                self.host_link.reconnecting = Some(receive);
            }
            Err(_) => self.host_link.failure = Some("호스트를 시작하지 못했습니다".into()),
        }
    }

    /// The answer of a reconnect: the window reads its work list and its open document again from the new host, keeping what it
    /// shows (the screen, the page, the selection and the AI target), or shows why the start failed.
    pub(super) fn poll_reconnect(&mut self) {
        let Some(receive) = self.host_link.reconnecting.as_ref() else {
            return;
        };
        let answer = match receive.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(DltError::new(ErrorCode::Internal, "the reconnect thread stopped")),
        };
        self.host_link.reconnecting = None;
        match answer {
            Ok(()) => {
                self.host_link.lost = false;
                self.host_link.failure = None;
                // A preview that failed with the old host is asked of the new one.
                self.export_preview_failed = None;
                self.host_link.probe = None;
                // The new host keeps no view of the old one (UI channel and views are in memory only, docs/UI_CHANNEL.md): the
                // registry, the token and the page transforms are sent to it again.
                self.ui.token = None;
                self.ui.state_seq = None;
                self.ui.run_guard_version = None;
                self.ui.commands_sent = false;
                self.ui.last_publish = None;
                self.page_transforms.clear();
                self.requested_pages.clear();
                self.requested_regions.clear();
                self.call("system.hello", json!({}), PendingAction::Hello);
                self.call("workspace.list", json!({}), PendingAction::WorkspaceList);
                self.restore_view_after_reconnect();
            }
            Err(error) => self.host_link.failure = Some(super::user_error(&error)),
        }
    }

    /// The open document's view on the new host, under the same view id: its transform (the core knows a view only after one),
    /// the window's selection, then the state, the structure and the page shown.
    fn restore_view_after_reconnect(&mut self) {
        let Some(artifact_id) = self.active.as_ref().map(|active| active.id.clone()) else {
            return;
        };
        let page_index = self.page_index;
        let transform = ViewTransform { id: new_id(), dpi: RenderProfile::default().dpi, zoom_milli: 1000, offset_x_micropx: 0, offset_y_micropx: 0, crop: None };
        self.call(
            "view.transform.set",
            json!({ "artifact_id": artifact_id, "view_id": self.view_id, "page_index": page_index, "transform": transform }),
            PendingAction::Transform,
        );
        // The selection goes once the window has read the new host's state: its revision may be newer than the one last read.
        self.host_link.selection_after_reconnect =
            self.local_selection.clone().filter(|selection| !selection.node_ids.is_empty() || selection.text_range.is_some());
        // The old host's reservations went with it.
        self.reservations.clear();
        self.user_reservations.clear();
        self.reservations_for = None;
        self.call("targets.list", json!({ "artifact_id": artifact_id }), PendingAction::Reservations);
        self.fetch_context();
        self.fetch_inspect();
        self.fetch_render(page_index);
    }

    /// After a reconnect, once the new host's state is read: the window's selection, at the revision that state names.
    pub(super) fn resume_selection_after_reconnect(&mut self) {
        if self.host_link.selection_after_reconnect.is_none() || self.pending.values().any(|pending| pending.method == "context.read") {
            return;
        }
        let (Some(active), Some(context)) = (self.active.as_ref(), self.context.as_ref()) else {
            return;
        };
        let Some(selection) = self.host_link.selection_after_reconnect.take() else {
            return;
        };
        let request = SelectionRequest {
            artifact_id: active.id.clone(),
            expected_revision: context.revision,
            view_id: self.view_id.clone(),
            selection,
            visible_page: self.page_index,
        };
        if let Ok(params) = serde_json::to_value(request) {
            self.call("selection.set", params, PendingAction::Selection);
        }
    }

    /// The lost link over the whole window: what happened, why a restart failed, what closing keeps, and the two choices.
    pub(super) fn draw_host_link(&mut self, ctx: &Context) {
        self.poll_reconnect();
        if !self.host_link.lost {
            return;
        }
        let waiting = self.host_link.reconnecting.is_some();
        let can_restart = self.host_link.starter.is_some() && !waiting;
        let close_work = self.has_close_work();
        use crate::widgets::{DialogButton, DialogChoice};
        let palette = crate::theme::palette(ctx);
        let shown = crate::widgets::dialog_with_choices(ctx, &palette, Id::new("host-link-lost"), HOST_LOST, crate::widgets::DialogPlace::Modal, |ui| {
            if let Some(failure) = &self.host_link.failure {
                ui.label(crate::i18n::tr(&failure));
            }
            if close_work {
                ui.label(crate::i18n::tr(&HOST_LOST_CLOSE_NOTE));
            }
            ((), vec![
                DialogChoice::new(RECONNECT, DialogButton::Primary).enabled(can_restart).human_only(),
                DialogChoice::new(QUIT, DialogButton::Plain).human_only(),
            ])
        });
        // Enter is `다시 연결` while it can run. Without the host there is no safe side to go back to: Esc and a press on the
        // backdrop do nothing, and closing stays the `종료` button's.
        let (reconnect, quit) = (shown.inner.1[0], shown.inner.1[1]);
        let reconnect = reconnect || (shown.enter && can_restart);
        if waiting {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if reconnect {
            self.reconnect_host();
        }
        if quit {
            self.quit_after_host_loss(ctx);
        }
    }

    /// `종료`: closes the window past the close question. Releasing held work needs the host, so held edits go; the text being
    /// typed stays in the drafts file (`drafts.rs`), written again before the window closes.
    pub(super) fn quit_after_host_loss(&mut self, ctx: &Context) {
        self.close_after_host_loss();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// What a close after the host was lost does before the window goes, whether by `종료` or by the window's own close.
    pub(super) fn close_after_host_loss(&mut self) {
        self.host_link.quitting = true;
        self.close_confirmation_open = false;
        let _ = self.drafts.flush();
    }
}
