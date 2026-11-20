//! Proposals (SPEC §5; task 17, GUI part). An AI request sent with "미리보기로 받기" is delivered as a proposal: the agent
//! stores its change set (`proposal.create`), and this window lists it with its preview render. Applying a row is one
//! `proposal.apply` (core), which commits like any change set, so one undo takes it back. Discarding a row removes it from
//! the list only, and a proposal that is never applied changes nothing. The rows are the proposals the `proposal.created`
//! events named while this window runs; a proposal made before the window started is not listed.

use designllm_contracts::{DeliverAs, DltError, Proposal};
use eframe::egui::{self, Context, CornerRadius, Sense, Ui, Vec2};
use serde_json::{json, Value};

use super::{
    messages::{change_result_unknown, user_error},
    thumbs::{self, Fit, ThumbTicket},
    DesktopApp, PendingAction,
};
use crate::{rpc::RpcPayload, theme, widgets};

/// The proposals kept for one document; when it is full, its oldest row makes room.
const KEPT: usize = 64;
/// The dpi of a row's preview, per page inch. The picture box is 48 by 34 points, and a page is several inches wide, so the
/// picture has more pixels than the box at any screen scale up to 4.
const PREVIEW_DPI: u32 = 48;
/// The height of the rows; the list scrolls past it.
const ROWS_HEIGHT: f32 = 240.0;

/// `Proposal.status` and the `proposal.created` / `proposal.applied` events name these.
const VALIDATED: &str = "validated";
const CONFLICT: &str = "conflict";
const REJECTED: &str = "rejected";
const APPLIED: &str = "applied";

/// Why a row cannot be applied when its document has moved on since the proposal was made.
pub(super) const STALE: &str = "작업물이 바뀌어 판이 달라졌습니다. 이 미리보기는 적용할 수 없습니다. 다시 요청해 주세요";
/// A row's apply whose answer was lost or unknown: the change may have landed, so the document is checked, not retried.
pub(super) const APPLY_UNKNOWN: &str = "미리보기 적용의 결과를 확인하지 못했습니다. 이미 반영됐을 수 있으니 작업물을 열어 확인하세요";

/// The proposal rows of the window and the AI request's delivery choice.
#[derive(Default)]
pub(super) struct ProposalBook {
    /// The AI request asks for its work as a proposal ("미리보기로 받기").
    deliver_as_proposal: bool,
    /// The proposals the events named, oldest first.
    entries: Vec<Entry>,
    /// The row the user picked last.
    selected: Option<String>,
}

#[derive(Clone)]
struct Entry {
    id: String,
    artifact_id: String,
    /// The revision the proposal was made on.
    base_revision: u64,
    status: String,
    /// The preview was asked for once; its picture, if it arrived, is in the thumbnail cache under `picture_key`.
    previewed: bool,
}

/// The thumbnail cache key of a proposal's preview.
pub(super) fn picture_key(proposal_id: &str) -> String {
    format!("proposal:{proposal_id}")
}

impl DesktopApp {
    /// The delivery an AI request carries: a proposal when the user chose "미리보기로 받기".
    pub(super) fn ai_deliver_as(&self) -> Option<DeliverAs> {
        self.proposals.deliver_as_proposal.then_some(DeliverAs::Proposal)
    }

    /// Keeps the rows the events name: `proposal.created` adds one, `proposal.applied` marks it applied (events.rs).
    pub(super) fn record_proposal(&mut self, event: &Value) {
        let detail = event.get("detail");
        let text = |key: &str| detail.and_then(|detail| detail.get(key)).and_then(Value::as_str).unwrap_or_default().to_owned();
        let proposal_id = text("proposal_id");
        if proposal_id.is_empty() {
            return;
        }
        match event.get("event").and_then(Value::as_str) {
            Some("proposal.created") => {
                if self.proposals.entries.iter().any(|entry| entry.id == proposal_id) {
                    return;
                }
                let artifact_id = event.get("artifact_id").and_then(Value::as_str).unwrap_or_default().to_owned();
                // A document keeps `KEPT` rows: its oldest row makes room. The rows of the other documents stay.
                let rows_of_document = self.proposals.entries.iter().filter(|entry| entry.artifact_id == artifact_id).count();
                if rows_of_document >= KEPT {
                    if let Some(oldest) = self.proposals.entries.iter().position(|entry| entry.artifact_id == artifact_id) {
                        self.proposals.entries.remove(oldest);
                    }
                }
                self.proposals.entries.push(Entry {
                    id: proposal_id,
                    artifact_id,
                    base_revision: event.get("revision").and_then(Value::as_u64).unwrap_or_default(),
                    status: text("status"),
                    previewed: false,
                });
            }
            Some("proposal.applied") => {
                if let Some(entry) = self.proposals.entries.iter_mut().find(|entry| entry.id == proposal_id) {
                    entry.status = APPLIED.to_owned();
                }
            }
            _ => {}
        }
    }

    /// The rows of the open document's list, newest first. Every row is listed; a row asks for its preview once it is scrolled
    /// into view.
    fn listed_entries(&self) -> Vec<Entry> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        self.proposals.entries.iter().rev().filter(|entry| entry.artifact_id == active.id).cloned().collect()
    }

    /// The ids of the rows the open document's list draws, newest first (the tests read the list this way).
    #[cfg(test)]
    pub(super) fn listed_proposal_ids(&self) -> Vec<String> {
        self.listed_entries().into_iter().map(|entry| entry.id).collect()
    }

    /// The number of rows the book holds, of every document (the tests read it this way).
    #[cfg(test)]
    pub(super) fn proposal_rows(&self) -> usize {
        self.proposals.entries.len()
    }

    /// The row the user picked last (the tests read it this way).
    #[cfg(test)]
    pub(super) fn selected_proposal(&self) -> Option<&str> {
        self.proposals.selected.as_deref()
    }

    /// The row of a proposal of the open document, from the whole book (a row past the newest ones is still a row).
    fn proposal_in_document(&self, proposal_id: &str) -> Result<Entry, String> {
        let active = self.active.as_ref().map(|active| active.id.as_str());
        self.proposals
            .entries
            .iter()
            .find(|entry| entry.id == proposal_id && Some(entry.artifact_id.as_str()) == active)
            .cloned()
            .ok_or_else(|| "이 작업물의 목록에 없는 미리보기입니다".to_owned())
    }

    /// Why the row cannot be applied now, `None` when it can. The apply button and `proposal.apply` both ask this.
    fn proposal_block_reason(&self, entry: &Entry) -> Option<&'static str> {
        let current = self.active.as_ref().map(|active| active.revision);
        match entry.status.as_str() {
            APPLIED => Some("이미 적용했습니다"),
            VALIDATED if current == Some(entry.base_revision) => None,
            VALIDATED | CONFLICT => Some(STALE),
            REJECTED => Some("검증에서 거부된 미리보기입니다"),
            _ => Some("아직 검증되지 않은 미리보기입니다"),
        }
    }

    /// The commands of the AI request's delivery and of the rows (commands.rs; dispatch.rs).
    pub(super) fn proposal_command(&mut self, id: &str, args: &Value) -> Result<(), String> {
        if id == "ai.deliver_as" {
            self.proposals.deliver_as_proposal = args.get("deliver_as").and_then(Value::as_str) == Some("proposal");
            return Ok(());
        }
        let proposal_id = args.get("proposal_id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let entry = self.proposal_in_document(&proposal_id)?;
        match id {
            "proposal.select" => self.proposals.selected = Some(entry.id),
            "proposal.discard" => {
                // The row leaves the book; its picture leaves the cache at the next draw (`draw_proposals`).
                self.proposals.entries.retain(|kept| kept.id != entry.id);
                if self.proposals.selected.as_deref() == Some(entry.id.as_str()) {
                    self.proposals.selected = None;
                }
            }
            "proposal.apply" => {
                if let Some(reason) = self.proposal_block_reason(&entry) {
                    return Err(reason.to_owned());
                }
                self.call("proposal.apply", json!({ "proposal_id": entry.id }), PendingAction::Apply);
                // The host answers the preview after the apply (requests are answered in order), so the row takes the applied
                // state, or the refusal, from that answer rather than from the next event poll.
                self.request_preview(&entry.id);
            }
            _ => return Err(format!("알 수 없는 명령: {id}")),
        }
        Ok(())
    }

    /// The AI tab's delivery choice, then the rows of the open document's proposals (sidebar.rs draws it under the intent switch).
    pub(super) fn draw_proposals(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        let choice = usize::from(self.proposals.deliver_as_proposal);
        if let Some(index) = widgets::segmented(ui, palette, &["바로 적용", "미리보기로 받기"], choice) {
            let deliver_as = if index == 1 { "proposal" } else { "apply" };
            self.invoke_ui("ai.deliver_as", json!({ "deliver_as": deliver_as }));
        }
        // The pictures of rows the book no longer holds (discarded, or past `KEPT`) leave the cache here.
        let ctx = ui.ctx().clone();
        let held: Vec<String> = self.proposals.entries.iter().map(|entry| picture_key(&entry.id)).collect();
        thumbs::drop_pictures(&ctx, |key| key.starts_with("proposal:") && !held.iter().any(|kept| kept.as_str() == key));
        let rows = self.listed_entries();
        egui::ScrollArea::vertical().max_height(ROWS_HEIGHT).auto_shrink([false, true]).show(ui, |ui| {
            for entry in &rows {
                self.draw_proposal_row(ui, palette, entry);
            }
        });
    }

    /// One row: its picture, its reason when it cannot be applied, and its buttons. A row in view asks for its preview.
    fn draw_proposal_row(&mut self, ui: &mut Ui, palette: &theme::Palette, entry: &Entry) {
        let block = self.proposal_block_reason(entry);
        let selected = self.proposals.selected.as_deref() == Some(entry.id.as_str());
        let ctx = ui.ctx().clone();
        let picture = thumbs::picture_of(&ctx, &picture_key(&entry.id));
        let (mut select, mut apply, mut discard) = (false, false, false);
        let fill = if selected { palette.accent_soft } else { palette.surface_2 };
        let row = egui::Frame::new()
            .fill(fill)
            .corner_radius(CornerRadius::same(theme::RADIUS_CONTROL))
            .inner_margin(egui::Margin::same(6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, response) = ui.allocate_exact_size(Vec2::new(48.0, 34.0), Sense::click());
                    ui.painter().rect_filled(rect, CornerRadius::ZERO, palette.artboard);
                    if let Some((texture, page)) = &picture {
                        thumbs::paint_picture(ui, rect, texture, *page, Fit::Contain { margin: 2.0 }, CornerRadius::ZERO);
                    }
                    select |= response.clicked();
                    ui.vertical(|ui| {
                        if let Some(reason) = block {
                            let words = egui::RichText::new(reason).font(theme::display_font(false, 12.0)).color(palette.ink_3);
                            ui.add(egui::Label::new(words).wrap());
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let discard_button = widgets::icon_button(ui, palette, "close", "버리기", false);
                        crate::ui_record::note_human_only(ui, "버리기", &discard_button);
                        discard = discard_button.clicked();
                        if block.is_none() {
                            let apply_button = widgets::labeled_button(ui, palette, Some("check"), "적용", true);
                            crate::ui_record::note_human_only(ui, "적용", &apply_button);
                            apply = apply_button.clicked();
                        }
                    });
                });
            });
        if ui.is_rect_visible(row.response.rect) && !entry.previewed && entry.status != APPLIED {
            self.request_preview(&entry.id);
        }
        if select {
            self.invoke_ui("proposal.select", json!({ "proposal_id": entry.id }));
        }
        if apply {
            self.invoke_ui("proposal.apply", json!({ "proposal_id": entry.id }));
        }
        if discard {
            self.invoke_ui("proposal.discard", json!({ "proposal_id": entry.id }));
        }
    }

    /// Asks the host for a row's preview: its status, its base revision and its picture (`proposal.preview`). The row is marked
    /// as asked once the request is sent.
    fn request_preview(&mut self, proposal_id: &str) {
        let params = json!({ "proposal_id": proposal_id, "page_index": 0, "profile": { "dpi": PREVIEW_DPI, "transparent": false } });
        let ticket = ThumbTicket::Proposal { proposal_id: proposal_id.to_owned(), dpi: PREVIEW_DPI };
        if self.send_home_read("proposal.preview", params, ticket) {
            if let Some(kept) = self.proposals.entries.iter_mut().find(|kept| kept.id == proposal_id) {
                kept.previewed = true;
            }
        }
    }

    /// The `proposal.preview` answer of a row (thumbs.rs). The row takes the proposal's status and base revision from it, and its
    /// picture is read the way a thumbnail is. A failed answer leaves the row without a picture.
    pub(super) fn proposal_previewed(&mut self, ctx: &Context, proposal_id: &str, dpi: u32, result: Result<RpcPayload, DltError>) {
        let key = picture_key(proposal_id);
        let proposal = match result {
            Ok(RpcPayload::Json(value)) => serde_json::from_value::<Proposal>(value).ok(),
            _ => None,
        };
        let Some(proposal) = proposal else {
            thumbs::thumb_failed(ctx, &key, 0, dpi);
            return;
        };
        if let Some(entry) = self.proposals.entries.iter_mut().find(|entry| entry.id == proposal_id) {
            entry.status = proposal.status;
            entry.base_revision = proposal.change_set.base_revision;
        }
        match proposal.preview.and_then(thumbs::snapshot_picture) {
            Some((asset, page)) => self.read_home_asset(ctx, asset, key, 0, dpi, page),
            None => thumbs::thumb_failed(ctx, &key, 0, dpi),
        }
    }
}

/// The words for a `proposal.apply` that failed (replies.rs): an answer that was lost may still have landed, a refusal says why.
pub(super) fn apply_failure_message(error: &DltError) -> String {
    if change_result_unknown(error) {
        APPLY_UNKNOWN.to_owned()
    } else {
        user_error(error)
    }
}
