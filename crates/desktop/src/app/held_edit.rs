use super::*;
use super::{messages::change_result_unknown, state::{ApplyContext, HeldEditState}};

impl DesktopApp {
    pub(super) fn hold_direct_edit(&mut self, mut apply: ApplyContext, nodes: Vec<String>, error: &DltError) {
        let already_held = apply.held_edit_id.is_some();
        let id = apply.held_edit_id.clone().unwrap_or_else(new_id);
        apply.held_edit_id = Some(id.clone());
        let known_job = !already_held && self.reservations.iter().any(|reservation| {
            reservation.node_ids.iter().any(|reserved| nodes.contains(reserved))
        });
        let retry_same_request = change_result_unknown(error);
        let (state, message, can_retry) = if already_held {
            let message = if retry_same_request {
                "요청 결과를 확인하지 못했습니다. 같은 요청으로 결과를 확인하거나 변경을 취소할 수 있습니다"
            } else if error.code == ErrorCode::RevisionConflict {
                "작업물이 바뀌었습니다. 최신 상태를 확인한 뒤 다시 적용하거나 변경을 취소하세요"
            } else if error.code == ErrorCode::TargetBusy {
                "다른 편집이 진행 중입니다. 변경을 보존했습니다. 끝난 뒤 최신 상태를 확인하세요"
            } else {
                "변경을 적용하지 못했습니다. 입력을 보존했습니다"
            };
            (
                HeldEditState::NeedsReview,
                message.to_owned(),
                retry_same_request || error.code == ErrorCode::TargetBusy,
            )
        } else if known_job {
            (HeldEditState::WaitingForJobRelease, self.busy_reason(&nodes), true)
        } else {
            (
                HeldEditState::NeedsReview,
                "다른 창의 편집 때문에 변경을 보류했습니다. 최신 상태를 확인한 뒤 다시 적용하거나 취소할 수 있습니다".into(),
                true,
            )
        };
        if let Some(held) = self.held_edits.iter_mut().find(|held| held.id == id) {
            held.apply = apply;
            if !nodes.is_empty() { held.nodes = nodes; }
            held.state = state;
            held.message = message;
            held.can_retry = can_retry;
            held.retry_same_request = retry_same_request;
            held.reservations_checked = false;
            held.checked_revision = None;
        } else {
            self.held_edits.push(HeldEdit {
                id,
                apply,
                nodes,
                state,
                message,
                can_retry,
                retry_same_request,
                reservations_checked: false,
                checked_revision: None,
            });
        }
        self.refresh_held_edit_notice();
    }

    pub(super) fn rehold_edit_after_failure(&mut self, held_id: &str, apply: ApplyContext, error: &DltError) {
        let nodes = self.held_edits.iter().find(|held| held.id == held_id)
            .map(|held| held.nodes.clone()).unwrap_or_else(|| error.affected_ids.clone());
        self.hold_direct_edit(apply, nodes, error);
    }

    pub(super) fn has_reservation_for_nodes(&self, nodes: &[String]) -> bool {
        self.reservations.iter().any(|reservation| reservation.node_ids.iter().any(|id| nodes.contains(id)))
            || self.user_reservations.iter().any(|reservation| reservation.node_ids.iter().any(|id| nodes.contains(id)))
    }

    pub(super) fn refresh_held_edit_notice(&mut self) {
        let active_id = self.active.as_ref().map(|active| active.id.as_str());
        let held = active_id.and_then(|active_id| self.held_edits.iter().find(|held| held.apply.artifact_id == active_id))
            .map(|held| (held.id.clone(), held.message.clone(), held.nodes.clone()));
        if let Some((id, message, nodes)) = held {
            self.refusal = Some(message);
            self.refusal_ids = nodes;
            self.refusal_fixes.clear();
            self.held_refusal_id = Some(id);
        } else if self.held_refusal_id.is_some() {
            self.refusal = None;
            self.refusal_ids.clear();
            self.refusal_fixes.clear();
            self.held_refusal_id = None;
        }
    }

    pub(super) fn check_held_edit(&mut self, held_id: &str) {
        let Some(held) = self.held_edits.iter().find(|held| held.id == held_id) else { return };
        if !held.can_retry || !matches!(held.state, HeldEditState::NeedsReview | HeldEditState::Ready) { return; }
        let artifact_id = held.apply.artifact_id.clone();
        if self.active.as_ref().is_none_or(|active| active.id != artifact_id) {
            if let Some(held) = self.held_edits.iter_mut().find(|held| held.id == held_id) {
                held.message = "보류한 변경의 작업물을 다시 연 뒤 최신 상태를 확인하세요".into();
            }
            self.refresh_held_edit_notice();
            return;
        }
        if let Some(held) = self.held_edits.iter_mut().find(|held| held.id == held_id) {
            held.state = HeldEditState::Checking;
            held.reservations_checked = false;
            held.checked_revision = None;
            held.message = "최신 작업물과 대상 예약을 확인하고 있습니다".into();
        }
        self.refresh_held_edit_notice();
        self.fetch_context();
        self.fetch_inspect();
        if self.call("targets.list", json!({ "artifact_id": artifact_id }), PendingAction::HeldEditReservations { held_id: held_id.to_owned() }).is_none() {
            self.fail_held_edit_check(held_id, &DltError::new(ErrorCode::Unavailable, "대상 예약을 확인하지 못했습니다"));
        }
    }

    pub(super) fn confirm_held_edit_retry(&mut self, held_id: &str) {
        let Some(index) = self.held_edits.iter().position(|held| held.id == held_id && held.state == HeldEditState::Ready) else { return };
        let Some((active, artifact, context)) = self.active.as_ref().zip(self.artifact.as_ref()).zip(self.context.as_ref())
            .map(|((active, artifact), context)| (active, artifact, context)) else { return };
        let held = self.held_edits[index].clone();
        let synced = active.id == held.apply.artifact_id && artifact.id == active.id && context.artifact_id == active.id
            && active.revision == artifact.revision && artifact.revision == context.revision
            && held.checked_revision == Some(context.revision);
        let base_still_current = held.retry_same_request || held.apply.base_revision == context.revision;
        if !synced || !base_still_current || self.has_reservation_for_nodes(&held.nodes) {
            if let Some(held) = self.held_edits.get_mut(index) {
                held.state = HeldEditState::NeedsReview;
                held.reservations_checked = false;
                held.checked_revision = None;
                if !base_still_current {
                    held.can_retry = false;
                    held.message = "문서가 바뀌어 보류한 변경을 다시 적용하지 않습니다. 변경 내용을 복사하거나 최신 화면에서 새로 실행하세요".into();
                } else {
                    held.message = "작업물 또는 대상 예약이 다시 바뀌었습니다. 최신 상태를 다시 확인하세요".into();
                }
            }
            self.refresh_held_edit_notice();
            return;
        }

        let mut apply = held.apply.clone();
        if !held.retry_same_request {
            if apply.base_revision != context.revision {
                apply.request_id = new_id();
            }
            apply.base_revision = context.revision;
        }
        apply.retryable = false;
        apply.held_edit_id = Some(held_id.to_owned());
        let action = apply.after_action.clone();
        if let Some(held) = self.held_edits.get_mut(index) {
            held.apply = apply.clone();
            held.state = HeldEditState::Sending;
            held.message = if held.retry_same_request {
                "같은 요청으로 결과를 확인하고 있습니다".into()
            } else {
                "확인한 최신 상태에 변경을 적용하고 있습니다".into()
            };
        }
        self.refresh_held_edit_notice();
        if self.send_apply_context(apply, action).is_none() {
            if let Some(held) = self.held_edits.iter_mut().find(|held| held.id == held_id) {
                held.state = HeldEditState::NeedsReview;
                held.message = "변경을 전송하지 못했습니다. 입력을 보존했습니다".into();
            }
            self.refresh_held_edit_notice();
        }
    }

    pub(super) fn discard_held_edit(&mut self, held_id: &str) {
        if self.held_edits.iter().any(|held| held.id == held_id && held.state == HeldEditState::Sending) { return; }
        self.held_edits.retain(|held| held.id != held_id);
        self.refresh_held_edit_notice();
    }

    pub(super) fn copy_held_edit(&self, ctx: &Context, held_id: &str) {
        let Some(held) = self.held_edits.iter().find(|held| held.id == held_id) else { return };
        let operation = json!({
            "artifact_id": held.apply.artifact_id,
            "base_revision": held.apply.base_revision,
            "operations": &held.apply.operations,
        });
        if let Ok(copy) = serde_json::to_string_pretty(&operation) {
            ctx.copy_text(copy);
        }
    }

    pub(super) fn fail_held_edit_check(&mut self, held_id: &str, error: &DltError) {
        let Some(held) = self.held_edits.iter_mut().find(|held| held.id == held_id) else { return };
        held.state = HeldEditState::NeedsReview;
        held.reservations_checked = false;
        held.checked_revision = None;
        held.message = format!("최신 상태를 확인하지 못했습니다. 변경은 보존했습니다: {}", user_error(error));
        self.refresh_held_edit_notice();
        self.set_error(user_error(error));
    }

    pub(super) fn reset_held_edit_checks(&mut self, message: &str) {
        for held in &mut self.held_edits {
            if held.state == HeldEditState::Checking {
                held.state = HeldEditState::NeedsReview;
                held.reservations_checked = false;
                held.checked_revision = None;
                held.message = message.to_owned();
            }
        }
        self.refresh_held_edit_notice();
    }

    pub(super) fn finish_held_edit_retry(&mut self, held_id: &str) {
        self.held_edits.retain(|held| held.id != held_id);
        self.refresh_held_edit_notice();
    }

    /// Re-sends held job edits only if the original base revision is still current. Other reservations
    /// require a visible refresh and an explicit retry or discard by the user.
    pub(super) fn resume_held_edits(&mut self) {
        let context_synced = self.active.as_ref().zip(self.context.as_ref()).is_some_and(|(active, context)| {
            active.id == context.artifact_id && active.revision == context.revision
                && self.reservations_for.as_deref() == Some(active.id.as_str())
        });
        let document_synced = self.active.as_ref().zip(self.artifact.as_ref()).zip(self.context.as_ref()).is_some_and(|((active, artifact), context)| {
            active.id == artifact.id && artifact.id == context.artifact_id
                && active.revision == artifact.revision && artifact.revision == context.revision
                && self.reservations_for.as_deref() == Some(active.id.as_str())
        });
        let revision = self.context.as_ref().map(|context| context.revision);
        let reserved_nodes: BTreeSet<String> = self.reservations.iter().flat_map(|reservation| reservation.node_ids.iter().cloned())
            .chain(self.user_reservations.iter().flat_map(|reservation| reservation.node_ids.iter().cloned()))
            .collect();
        let mut retry = Vec::new();
        for held in &mut self.held_edits {
            if held.state == HeldEditState::WaitingForJobRelease {
                if self.reservations.iter().any(|reservation| reservation.node_ids.iter().any(|id| held.nodes.contains(id))) {
                    continue;
                }
                if !context_synced || self.active.as_ref().is_none_or(|active| active.id != held.apply.artifact_id) {
                    continue;
                }
                if held.nodes.iter().any(|id| reserved_nodes.contains(id)) {
                    held.state = HeldEditState::NeedsReview;
                    held.message = "예약이 끝나지 않았거나 문서가 바뀌었습니다. 최신 상태를 확인한 뒤 다시 적용하세요".into();
                } else if held.apply.retryable && revision == Some(held.apply.base_revision) {
                    held.apply.retryable = false;
                    held.state = HeldEditState::Sending;
                    held.message = "작업 예약이 끝나 변경을 다시 적용하고 있습니다".into();
                    retry.push((held.id.clone(), held.apply.clone(), held.apply.after_action.clone()));
                } else if revision == Some(held.apply.base_revision) {
                    held.state = HeldEditState::NeedsReview;
                    held.message = "자동 재시도 횟수를 넘겼습니다. 대상이 해제됐는지 확인한 뒤 명시적으로 다시 적용하세요".into();
                } else {
                    held.state = HeldEditState::NeedsReview;
                    held.can_retry = false;
                    held.message = "작업물이 바뀌어 보류한 변경을 다시 적용하지 않습니다. 변경 내용을 복사하거나 최신 화면에서 새로 실행하세요".into();
                }
            } else if held.state == HeldEditState::Checking {
                if !held.reservations_checked { continue; }
                if !document_synced || self.active.as_ref().is_none_or(|active| active.id != held.apply.artifact_id) {
                    let refresh_pending = self.pending.values().any(|pending| matches!(pending.method.as_str(), "context.read" | "artifact.inspect"));
                    if refresh_pending { continue; }
                    held.state = HeldEditState::NeedsReview;
                    held.message = "최신 문서 상태를 맞추지 못했습니다. 다시 확인하거나 변경을 취소하세요".into();
                } else if held.nodes.iter().any(|id| reserved_nodes.contains(id)) {
                    held.state = HeldEditState::NeedsReview;
                    held.message = "다른 편집이 계속 진행 중입니다. 입력은 보존했습니다".into();
                } else if !held.retry_same_request && revision != Some(held.apply.base_revision) {
                    held.state = HeldEditState::NeedsReview;
                    held.can_retry = false;
                    held.checked_revision = None;
                    held.message = "문서가 바뀌어 보류한 변경을 다시 적용하지 않습니다. 변경 내용을 복사하거나 최신 화면에서 새로 실행하세요".into();
                } else {
                    held.state = HeldEditState::Ready;
                    held.checked_revision = revision;
                    held.message = "최신 상태를 확인했습니다. 보류한 변경을 다시 적용하거나 취소하세요".into();
                }
            } else if held.state == HeldEditState::Ready {
                if !document_synced || held.checked_revision != revision || held.nodes.iter().any(|id| reserved_nodes.contains(id)) {
                    held.state = HeldEditState::NeedsReview;
                    held.reservations_checked = false;
                    held.checked_revision = None;
                    held.message = "확인 뒤 작업물 또는 예약이 바뀌었습니다. 다시 확인하세요".into();
                } else if !held.retry_same_request && revision != Some(held.apply.base_revision) {
                    held.state = HeldEditState::NeedsReview;
                    held.can_retry = false;
                    held.checked_revision = None;
                    held.message = "문서가 바뀌어 보류한 변경을 다시 적용하지 않습니다. 변경 내용을 복사하거나 최신 화면에서 새로 실행하세요".into();
                }
            }
        }
        for (held_id, apply, action) in retry {
            if self.send_apply_context(apply, action).is_none() {
                if let Some(held) = self.held_edits.iter_mut().find(|held| held.id == held_id) {
                    held.state = HeldEditState::NeedsReview;
                    held.message = "보류한 변경을 다시 전송하지 못했습니다".into();
                }
            }
        }
        self.refresh_held_edit_notice();
    }
}
