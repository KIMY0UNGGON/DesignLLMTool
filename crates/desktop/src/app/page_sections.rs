//! Desktop editing for Canvas page section markers. Membership follows the existing page order.

use designllm_contracts::{ArtifactBody, ArtifactKind, CanvasPageSection, CAPABILITY_EXTENSION_PAGE_SECTIONS};
use eframe::egui::{Context, Id};
use serde_json::Value;

use super::{DesktopApp, PendingAction};
use crate::theme;

pub(super) fn section_started_at<'a>(sections: &'a [CanvasPageSection], page_id: &str) -> Option<&'a CanvasPageSection> {
    sections.iter().find(|section| section.start_page_id.as_str() == page_id)
}

#[derive(Clone)]
pub(super) struct SectionEditor {
    pub(super) artifact_id: String,
    pub(super) revision: u64,
    pub(super) expected: Vec<CanvasPageSection>,
    pub(super) section: CanvasPageSection,
    pub(super) page_index: u32,
    pub(super) is_new: bool,
    pub(super) message: Option<String>,
}

pub(super) struct SectionMutation {
    pub(super) token: String,
    artifact_id: String,
    pub(super) revision: u64,
    pub(super) expected: Vec<CanvasPageSection>,
    pub(super) desired: Vec<CanvasPageSection>,
    editor: Option<SectionEditor>,
    message: Option<String>,
    pub(super) pending: bool,
    retry_allowed: bool,
    pub(super) outcome_uncertain: bool,
    held_edit_id: Option<String>,
}

impl DesktopApp {
    pub(super) fn page_sections_write_supported(&self) -> bool {
        self.context.as_ref()
            .and_then(|context| context.capability_extensions.get(CAPABILITY_EXTENSION_PAGE_SECTIONS))
            .is_some_and(|support| support.write == designllm_contracts::Support::Supported)
    }

    pub(super) fn open_page_section_for_current_page(&mut self) {
        let page_index = self.view_ref().page_index;
        let Some((_, _, pages, sections)) = self.canvas_section_snapshot() else { return };
        let Some((page_id, ordinary_page)) = pages.get(page_index as usize) else { return };
        if !*ordinary_page {
            self.set_error("컴포넌트 원본 페이지는 섹션을 시작할 수 없습니다".into());
            return;
        }
        if let Some(existing) = sections.iter().find(|section| &section.start_page_id == page_id) {
            self.open_page_section_editor(&existing.id);
        } else {
            self.begin_page_section_editor(page_index);
        }
    }

    pub(super) fn open_page_section_editor(&mut self, section_id: &str) {
        if !self.page_sections_write_supported() {
            self.set_error("이 작업물에서는 페이지 섹션을 편집할 수 없습니다".into());
            return;
        }
        if self.board_memo_editor.is_some() || self.board_memo_mutation.is_some()
            || self.page_section_editor.is_some() || self.page_section_mutation.is_some()
        {
            return;
        }
        let Some((artifact_id, revision, pages, expected)) = self.canvas_section_snapshot() else { return };
        let Some(section) = expected.iter().find(|section| section.id == section_id).cloned() else { return };
        let Some(page_index) = pages.iter().position(|(page_id, ordinary)| page_id == &section.start_page_id && *ordinary) else {
            self.set_error("섹션의 시작 페이지가 바뀌었습니다. 최신 목록을 확인하세요".into());
            return;
        };
        if !self.canvas_section_snapshot_is_current(&artifact_id, revision, &expected) {
            self.set_error("작업물이 바뀌었습니다. 최신 상태를 불러온 뒤 섹션을 다시 열어 주세요".into());
            return;
        }
        self.page_section_editor = Some(SectionEditor {
            artifact_id,
            revision,
            expected,
            section,
            page_index: page_index as u32,
            is_new: false,
            message: None,
        });
    }

    fn begin_page_section_editor(&mut self, page_index: u32) {
        if !self.page_sections_write_supported() {
            self.set_error("이 작업물에서는 페이지 섹션을 편집할 수 없습니다".into());
            return;
        }
        if self.board_memo_editor.is_some() || self.board_memo_mutation.is_some()
            || self.page_section_editor.is_some() || self.page_section_mutation.is_some()
        {
            return;
        }
        let Some((artifact_id, revision, pages, expected)) = self.canvas_section_snapshot() else { return };
        if !self.canvas_section_snapshot_is_current(&artifact_id, revision, &expected) {
            self.set_error("작업물이 바뀌었습니다. 최신 상태를 불러온 뒤 섹션을 다시 추가해 주세요".into());
            return;
        }
        let Some((page_id, ordinary_page)) = pages.get(page_index as usize) else { return };
        if !*ordinary_page {
            self.set_error("컴포넌트 원본 페이지는 섹션을 시작할 수 없습니다".into());
            return;
        }
        if expected.iter().any(|section| &section.start_page_id == page_id) {
            self.set_error("이 페이지부터 시작하는 섹션이 이미 있습니다".into());
            return;
        }
        self.page_section_editor = Some(SectionEditor {
            artifact_id,
            revision,
            expected,
            section: CanvasPageSection {
                id: designllm_contracts::new_id(),
                name: "새 섹션".into(),
                start_page_id: page_id.clone(),
            },
            page_index,
            is_new: true,
            message: None,
        });
    }

    pub(super) fn draw_page_section_editor(&mut self, ctx: &Context) {
        if self.page_section_editor.as_ref().is_some_and(|editor| editor.message.is_none() && !self.section_editor_is_current(editor)) {
            if let Some(editor) = self.page_section_editor.as_mut() {
                editor.message = Some("작업물 또는 페이지 순서가 바뀌었습니다. 섹션 초안은 최신 상태에 자동 적용하지 않습니다.".into());
            }
        }
        let Some(editor) = self.page_section_editor.as_ref() else { return };
        let section_id = editor.section.id.clone();
        let artifact_id = editor.artifact_id.clone();
        let start_page_id = editor.section.start_page_id.clone();
        let page_index = editor.page_index;
        let is_new = editor.is_new;
        let message = editor.message.clone();
        let can_edit = message.is_none() && self.run_context.is_none();
        let mut name = editor.section.name.clone();
        use crate::widgets::{DialogButton, DialogChoice};
        let palette = crate::theme::palette(ctx);
        let shown = crate::widgets::dialog_with_choices(
            ctx,
            &palette,
            Id::new(("page-section-editor", artifact_id.as_str(), section_id.as_str())),
            if is_new { "페이지 섹션 추가" } else { "페이지 섹션" },
            crate::widgets::DialogPlace::Screen,
            |ui| {
                if let Some(message) = message.as_deref() {
                    ui.label(crate::i18n::tr(&message));
                } else if self.run_context.is_some() {
                    ui.label(crate::i18n::tr("앱 요청이 끝난 뒤 편집할 수 있습니다. 초안은 보존됩니다."));
                } else {
                    ui.label(crate::i18n::tr(&format!("시작 페이지 {} · {}", page_index + 1, start_page_id)));
                }
                ui.add_space(4.0);
                ui.add_enabled_ui(can_edit, |ui| {
                    crate::widgets::dialog_row(ui, &palette, "섹션 이름", |ui, width| crate::widgets::text_field(ui, &palette, &mut name, "섹션 이름", width));
                });
                let mut choices = Vec::new();
                if can_edit {
                    choices.push(DialogChoice::new("저장", DialogButton::Primary));
                    if !is_new {
                        choices.push(DialogChoice::new("섹션 삭제", DialogButton::Danger));
                    }
                    choices.push(DialogChoice::new("취소", DialogButton::Plain));
                } else {
                    choices.push(DialogChoice::new("초안 버리기", DialogButton::Danger));
                    choices.push(DialogChoice::new("초안 복사", DialogButton::Plain));
                }
                (choices.iter().map(|choice| choice.label).collect::<Vec<_>>(), choices)
            },
        );
        // While the section can be edited, Enter saves (the name field ends with Enter, then the dialog takes it) and Esc is
        // `취소`. A draft that can no longer be applied takes no keys: its `초안 버리기` throws the draft away.
        let (labels, pressed) = shown.inner;
        let clicked = |label: &str| labels.iter().zip(&pressed).any(|(shown, pressed)| *shown == label && *pressed);
        let (save, delete, cancel, copy) = (clicked("저장"), clicked("섹션 삭제"), clicked("취소") || clicked("초안 버리기"), clicked("초안 복사"));
        let save = save || (can_edit && shown.enter);
        let cancel = cancel || (can_edit && shown.dismissed);
        if let Some(active_editor) = self.page_section_editor.as_mut() {
            if active_editor.section.id == section_id && active_editor.artifact_id == artifact_id {
                active_editor.section.name = name;
            }
        }
        if copy {
            if let Some(editor) = self.page_section_editor.as_ref() {
                ctx.copy_text(section_copy_payload(&editor.section, editor.page_index));
            }
        }
        if save { self.save_page_section_editor(); }
        if delete { self.delete_page_section_editor(); }
        if cancel { self.page_section_editor = None; }
    }

    pub(super) fn draw_page_section_mutation(&mut self, ctx: &Context) {
        let Some(mutation) = self.page_section_mutation.as_ref() else { return };
        let Some(board_rect) = self.board_rect else { return };
        let token = mutation.token.clone();
        let message = mutation.message.clone();
        let pending = mutation.pending || self.page_section_request_in_flight();
        let retry_allowed = self.section_mutation_retry_allowed(mutation);
        let draft = mutation.editor.as_ref().map(|editor| editor.section.clone());
        let page_index = mutation.editor.as_ref().map(|editor| editor.page_index).unwrap_or(0);
        let held_edit_id = mutation.held_edit_id.clone();
        use crate::widgets::{dialog_buttons, DialogButton, DialogChoice};
        let (mut copy, mut retry, mut discard) = (false, false, false);
        let palette = theme::palette(ctx);
        let title = if pending { "섹션 저장 중" } else if mutation.outcome_uncertain { "섹션 결과 확인 필요" } else { "섹션 변경 보존됨" };
        crate::widgets::dialog(ctx, &palette, Id::new("page-section-conflict"), title, crate::widgets::DialogPlace::BoardTop(board_rect), |ui| {
                ui.label(crate::i18n::tr(&message.as_deref().unwrap_or("최신 섹션 상태를 확인할 수 없습니다. 초안을 보존했습니다.")));
                if let Some(section) = draft.as_ref() {
                    ui.label(crate::i18n::tr(&format!("섹션: {name} · 시작 페이지 {page}", name = section.name, page = page_index + 1)));
                    let pressed = dialog_buttons(ui, &palette, &[
                        DialogChoice::new("초안 복사", DialogButton::Plain).enabled(!pending),
                        DialogChoice::new("같은 기준으로 다시 시도", DialogButton::Primary).enabled(retry_allowed),
                        DialogChoice::new("초안 버리기", DialogButton::Danger).enabled(!pending),
                    ]);
                    (copy, retry, discard) = (pressed[0].clicked(), pressed[1].clicked(), pressed[2].clicked());
                } else {
                    discard = dialog_buttons(ui, &palette, &[DialogChoice::new("변경 버리기", DialogButton::Danger).enabled(!pending)])[0].clicked();
                }
                // A held section change never takes the held-edit check: it is held only by the apply preflight (apply.rs: a video,
                // image replacement or crop being applied), which sets `can_retry` false, and the replies leave its failures out of the
                // re-hold (replies.rs), so `check_held_edit` would do nothing and `Ready` is never reached. The draft's own copy and
                // discard buttons above are its way out; only the automatic retry after a job's release is reported.
                let held = held_edit_id.as_deref().and_then(|held_id| self.held_edits.iter().find(|held| held.id == held_id));
                if held.is_some_and(|held| matches!(held.state, super::state::HeldEditState::Sending | super::state::HeldEditState::WaitingForJobRelease)) {
                    ui.label(crate::i18n::tr("보류한 섹션 변경을 처리 중입니다."));
                }
            });
        if copy {
            if let Some(editor) = self.page_section_mutation.as_ref().and_then(|mutation| mutation.editor.as_ref()) {
                ctx.copy_text(section_copy_payload(&editor.section, editor.page_index));
            }
        }
        if retry { self.retry_page_section_mutation(&token); }
        if discard { self.discard_page_section_mutation(&token); }
    }

    fn canvas_section_snapshot(&self) -> Option<(String, u64, Vec<(String, bool)>, Vec<CanvasPageSection>)> {
        let active = self.active.as_ref().filter(|active| active.kind == ArtifactKind::Canvas)?;
        let artifact = self.artifact.as_ref().filter(|artifact| artifact.id == active.id && artifact.kind == ArtifactKind::Canvas)?;
        let designllm_contracts::ArtifactBody::Canvas(document) = &artifact.body else { return None };
        let pages = document.pages.iter().map(|page| (page.id.clone(), page.role.is_none())).collect();
        Some((artifact.id.clone(), artifact.revision, pages, document.page_sections.clone()))
    }

    fn canvas_section_snapshot_is_current(&self, artifact_id: &str, revision: u64, expected: &[CanvasPageSection]) -> bool {
        self.active.as_ref().zip(self.artifact.as_ref()).zip(self.context.as_ref()).is_some_and(|((active, artifact), context)| {
            if active.id != artifact_id || artifact.id != artifact_id || context.artifact_id != artifact_id
                || active.revision != revision || artifact.revision != revision || context.revision != revision
            {
                return false;
            }
            matches!(&artifact.body, ArtifactBody::Canvas(document) if document.page_sections == expected)
        })
    }

    fn section_editor_is_current(&self, editor: &SectionEditor) -> bool {
        if !self.canvas_section_snapshot_is_current(&editor.artifact_id, editor.revision, &editor.expected) { return false; }
        let Some((_, _, pages, _)) = self.canvas_section_snapshot() else { return false };
        pages.get(editor.page_index as usize).is_some_and(|(page_id, ordinary)| {
            page_id == &editor.section.start_page_id && *ordinary
        })
    }

    pub(super) fn save_page_section_editor(&mut self) {
        let Some(mut editor) = self.page_section_editor.clone() else { return };
        if self.run_context.is_some() {
            self.set_error("앱 요청이 끝난 뒤 섹션을 저장하세요. 초안은 보존했습니다".into());
            return;
        }
        if editor.section.name.trim().is_empty() {
            self.set_error("섹션 이름을 입력하세요".into());
            return;
        }
        editor.section.name = editor.section.name.trim().to_owned();
        if !self.section_editor_is_current(&editor) {
            if let Some(current) = self.page_section_editor.as_mut() {
                current.message = Some("작업물 또는 시작 페이지가 바뀌었습니다. 섹션 초안은 최신 상태에 자동 적용하지 않습니다.".into());
            }
            return;
        }
        let Some(desired) = replace_page_section(&editor.expected, &editor.section, editor.is_new) else {
            if let Some(current) = self.page_section_editor.as_mut() { current.message = Some("해당 페이지에서 시작하는 섹션이 이미 있습니다. 초안은 보존했습니다.".into()); }
            return;
        };
        self.submit_page_section_change(editor, desired);
    }

    fn delete_page_section_editor(&mut self) {
        let Some(editor) = self.page_section_editor.clone().filter(|editor| !editor.is_new) else { return };
        if self.run_context.is_some() {
            self.set_error("앱 요청이 끝난 뒤 섹션을 삭제하세요. 초안은 보존했습니다".into());
            return;
        }
        if !self.section_editor_is_current(&editor) {
            if let Some(current) = self.page_section_editor.as_mut() { current.message = Some("작업물 또는 시작 페이지가 바뀌었습니다. 섹션 삭제를 보내지 않았고 초안은 보존했습니다.".into()); }
            return;
        }
        let Some(desired) = remove_page_section(&editor.expected, &editor.section.id) else {
            if let Some(current) = self.page_section_editor.as_mut() { current.message = Some("삭제할 섹션이 없습니다. 초안을 보존했습니다".into()); }
            return;
        };
        self.submit_page_section_change(editor, desired);
    }

    fn submit_page_section_change(&mut self, editor: SectionEditor, desired: Vec<CanvasPageSection>) {
        let token = designllm_contracts::new_id();
        let args = serde_json::json!({ "expected": editor.expected, "sections": desired });
        self.page_section_editor = None;
        self.page_section_mutation = Some(SectionMutation {
            token: token.clone(),
            artifact_id: editor.artifact_id.clone(),
            revision: editor.revision,
            expected: editor.expected.clone(),
            desired,
            editor: Some(editor),
            message: None,
            pending: false,
            retry_allowed: true,
            outcome_uncertain: false,
            held_edit_id: None,
        });
        if self.run_context.is_some() {
            if let Some(mutation) = self.page_section_mutation.as_mut().filter(|mutation| mutation.token == token) {
                mutation.message = Some("앱 요청 중에는 섹션을 변경하지 않았습니다. 초안은 보존했습니다.".into());
                mutation.retry_allowed = false;
            }
            return;
        }
        // The change goes through the section command (operation_commands.rs), which completes it as this mutation's own change.
        if self.invoke("canvas.page_sections.set", args).is_ok() {
            if let Some(mutation) = self.page_section_mutation.as_mut().filter(|mutation| mutation.token == token) { mutation.pending = true; }
        } else {
            let held_id = self.held_edits.iter().find(|held| {
                matches!(&held.apply.after_action, PendingAction::PageSectionsChange { token: held_token } if held_token == &token)
            }).map(|held| held.id.clone());
            if let Some(mutation) = self.page_section_mutation.as_mut().filter(|mutation| mutation.token == token) {
                mutation.held_edit_id = held_id;
                mutation.retry_allowed = mutation.held_edit_id.is_none();
                mutation.message = Some("섹션 변경 요청을 보내지 못했습니다. 초안을 보존했습니다.".into());
            }
        }
    }

    pub(super) fn fail_page_sections_change(&mut self, token: &str, error: &designllm_contracts::DltError) {
        let uncertain = matches!(&error.code,
            &designllm_contracts::ErrorCode::Io | &designllm_contracts::ErrorCode::Internal
                | &designllm_contracts::ErrorCode::HostNotRunning | &designllm_contracts::ErrorCode::Unavailable
                | &designllm_contracts::ErrorCode::StorageFailure
        );
        let mut refresh = false;
        if let Some(mutation) = self.page_section_mutation.as_mut().filter(|mutation| mutation.token == token) {
            mutation.pending = false;
            mutation.outcome_uncertain = uncertain;
            mutation.retry_allowed = !uncertain && (error.retryable || error.code == designllm_contracts::ErrorCode::TargetBusy);
            mutation.message = Some(if uncertain {
                format!("{} 섹션 저장 결과가 불명확하며 이미 반영됐을 수 있습니다. 초안을 복사하고 최신 상태를 확인하세요.", super::user_error(error))
            } else {
                format!("{} 섹션 초안은 보존했고 최신 상태에 자동 적용하지 않습니다.", super::user_error(error))
            });
            refresh = self.active.as_ref().is_some_and(|active| active.id == mutation.artifact_id);
        }
        if uncertain && refresh { self.refresh_active(); }
    }

    pub(super) fn finish_page_sections_change(&mut self, token: &str, value: &Value) {
        if self.page_section_mutation.as_ref().is_some_and(|mutation| mutation.token == token) {
            self.page_section_mutation = None;
        }
        self.committed(value);
    }

    pub(super) fn finish_page_sections_change_offscreen(&mut self, token: &str) {
        if self.page_section_mutation.as_ref().is_some_and(|mutation| mutation.token == token) {
            self.page_section_mutation = None;
        }
    }

    fn section_mutation_retry_allowed(&self, mutation: &SectionMutation) -> bool {
        if mutation.pending || !mutation.retry_allowed || mutation.held_edit_id.is_some() || self.run_context.is_some() { return false; }
        let Some((artifact_id, revision, pages, sections)) = self.canvas_section_snapshot() else { return false };
        artifact_id == mutation.artifact_id && revision == mutation.revision && sections == mutation.expected
            && self.canvas_section_snapshot_is_current(&mutation.artifact_id, mutation.revision, &mutation.expected)
            && mutation.desired.iter().all(|section| pages.iter().any(|(page_id, ordinary)| page_id == &section.start_page_id && *ordinary))
            && self.page_sections_write_supported()
    }

    fn retry_page_section_mutation(&mut self, token: &str) {
        let Some(mutation) = self.page_section_mutation.as_ref().filter(|mutation| mutation.token == token) else { return };
        if !self.section_mutation_retry_allowed(mutation) { return; }
        let args = serde_json::json!({ "expected": mutation.expected, "sections": mutation.desired });
        if let Some(current) = self.page_section_mutation.as_mut().filter(|current| current.token == token) {
            current.message = None;
            current.retry_allowed = false;
        }
        // The retry goes through the section command, which completes it as this mutation's own change. `pending` is set when it is sent.
        if self.invoke("canvas.page_sections.set", args).is_ok() {
            if let Some(current) = self.page_section_mutation.as_mut().filter(|current| current.token == token) {
                current.pending = true;
            }
        } else if let Some(current) = self.page_section_mutation.as_mut().filter(|current| current.token == token) {
            current.message = Some("섹션 재시도를 보내지 못했습니다. 초안을 보존했습니다.".into());
            current.retry_allowed = false;
        }
    }

    fn discard_page_section_mutation(&mut self, token: &str) {
        if self.page_section_mutation.as_ref().is_none_or(|mutation| mutation.token != token || mutation.pending) { return; }
        let held_id = self.page_section_mutation.as_ref().and_then(|mutation| mutation.held_edit_id.clone());
        if let Some(held_id) = held_id {
            if self.held_edits.iter().any(|held| held.id == held_id && held.state == super::state::HeldEditState::Sending) { return; }
            self.held_edits.retain(|held| held.id != held_id);
            if self.held_refusal_id.as_deref() == Some(held_id.as_str()) {
                self.held_refusal_id = None;
                self.refusal = None;
                self.refusal_ids.clear();
                self.refusal_fixes.clear();
            }
            self.refresh_held_edit_notice();
        }
        self.page_section_mutation = None;
    }
}

fn replace_page_section(expected: &[CanvasPageSection], section: &CanvasPageSection, is_new: bool) -> Option<Vec<CanvasPageSection>> {
    if expected.iter().any(|existing| existing.start_page_id == section.start_page_id && existing.id != section.id) { return None; }
    let mut desired = expected.to_vec();
    if is_new {
        if desired.iter().any(|existing| existing.id == section.id) { return None; }
        desired.push(section.clone());
    } else {
        let target = desired.iter_mut().find(|existing| existing.id == section.id)?;
        *target = section.clone();
    }
    desired.sort_by(|left, right| left.id.cmp(&right.id));
    Some(desired)
}

fn remove_page_section(expected: &[CanvasPageSection], section_id: &str) -> Option<Vec<CanvasPageSection>> {
    expected.iter().any(|section| section.id == section_id).then(|| {
        expected.iter().filter(|section| section.id != section_id).cloned().collect()
    })
}

fn section_copy_payload(section: &CanvasPageSection, page_index: u32) -> String {
    format!("섹션 이름: {}\n시작 페이지: {}\n시작 페이지 ID: {}\n섹션 ID: {}", section.name, page_index + 1, section.start_page_id, section.id)
}

#[cfg(test)]
#[path = "page_section_tests.rs"]
mod tests;
