//! The registered commands of the five operations of task 39 (GUI <-> MCP parity): the error-key switch of a plot, the
//! canvas image replacement and crop, and the board memos and page sections of a canvas. Each command sends the change set
//! that an MCP `changes_apply` call sends for the same arguments, so the command and the tool give the same artifact
//! (`parity_tests.rs`). The windows' own editors keep their flows, and their buttons invoke these commands. A change that a
//! button started completes as the button's own change (its `PendingAction` with the editor's token); any other call completes
//! as `Apply`, so the guard of `apply_changes` refuses it while the button's change is applied.

use std::collections::BTreeSet;

use designllm_contracts::{CanvasBoardMemo, CanvasPageSection, Intent, Operation, PanelBody, PixelRect, PlotDisplayMode};
use serde::Deserialize;
use serde_json::Value;

use super::{DesktopApp, ImageCropStage, ImageReplacementStage, PendingAction};

/// Why `figure.plot_display` does not turn the error key on for a plot without error bars (the switch is not offered there).
pub(super) const PLOT_DISPLAY_NO_ERROR: &str = "오차 막대가 없는 그래프에는 오차 기준을 표시할 수 없습니다";
use crate::commands::CommandSpec;

impl DesktopApp {
    /// Runs one of the five commands. `None` for any other command, so `dispatch` asks the next module.
    pub(super) fn dispatch_operation_commands(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        let result = match spec.id {
            "figure.plot_display" => self.plot_display_command(args),
            "canvas.image.replace" => self.image_replace_command(args),
            "canvas.image.crop" => self.image_crop_command(args),
            "canvas.board_memos.set" => self.board_memos_command(args),
            "canvas.page_sections.set" => self.page_sections_command(args),
            _ => return None,
        };
        Some(result)
    }

    /// Switches the error key of one plot panel. A plot that already shows the wanted mode is left as it is.
    fn plot_display_command(&mut self, args: &Value) -> Result<(), String> {
        let panel_id = required_text(args, "panel_id")?;
        let on = args.get("on").and_then(Value::as_bool).ok_or("`on` 값(true 또는 false)이 필요합니다")?;
        let figure = self.figure_document().ok_or("그림을 열어 주세요")?;
        let panel = figure.panels.iter().find(|panel| panel.id == panel_id).ok_or("패널을 찾지 못했습니다")?;
        let PanelBody::Plot(plot) = &panel.body else {
            return Err("이 패널은 그래프가 아닙니다".into());
        };
        if (plot.display == Some(PlotDisplayMode::ErrorKey)) == on {
            return Ok(());
        }
        // The rule of the switch (`plot_display_ready`): a plot without error bars takes no error key.
        if on && plot.error.is_none() {
            return Err(PLOT_DISPLAY_NO_ERROR.into());
        }
        if self.set_plot_display(&figure, panel_id, on) {
            Ok(())
        } else {
            Err("오차 기준 표시를 바꾸지 못했습니다. 그림이 최신 상태이고 다른 변경이 끝났는지 확인하세요".into())
        }
    }

    /// Replaces the canvas image `node_id` with an asset already in the asset store (`asset_sha256`, as `asset.import` or
    /// `asset_import` returns it). `node_id` must be the selected image: the same checks as the image button apply, and the
    /// asset the image shows now is the expected value. Choosing a file stays with the image button's dialog.
    fn image_replace_command(&mut self, args: &Value) -> Result<(), String> {
        let node_id = required_text(args, "node_id")?.to_owned();
        let asset_sha256 = required_text(args, "asset_sha256")?.to_owned();
        if asset_sha256.len() != 64 || !asset_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("자산 해시(asset_sha256)는 64자리 16진수여야 합니다".into());
        }
        // The image button's import, applying on this node under its token (image_edit.rs), is the change the button started.
        let started = self.pending_image_replacement.as_ref()
            .filter(|pending| pending.stage == ImageReplacementStage::Applying && pending.intent.node_id == node_id)
            .map(|pending| {
                let token = pending.intent.token.clone();
                let sent = self.change_in_flight(|action| {
                    matches!(action, PendingAction::ImageReplacementApply { token: changing } if changing == &token)
                });
                ((PendingAction::ImageReplacementApply { token }, pending.intent.expected_revision, pending.intent.expected_asset_sha256.clone()), sent)
            });
        // Any other call starts a replacement of its own, which `image_replacement_start` refuses while the button's one is under way.
        let (action, expected_revision, expected_asset_sha256) = match self.own_started(started) {
            Some(own) => own,
            None => {
                let intent = self.image_replacement_start()?;
                if intent.node_id != node_id {
                    return Err("지정한 이미지가 선택한 이미지와 다릅니다. 그 이미지를 먼저 고르세요".into());
                }
                (PendingAction::Apply, intent.expected_revision, intent.expected_asset_sha256)
            }
        };
        // The button does nothing for the asset the image already shows (image_edit.rs), so the command sends nothing either.
        if expected_asset_sha256 == asset_sha256 {
            return Ok(());
        }
        let operation = Operation::ReplaceImageAsset { node_id, expected_asset_sha256, asset_sha256 };
        self.send_command_change(vec![operation], &[Intent::Content], action, expected_revision)
    }

    /// Crops the canvas image `node_id` to the pixel rectangle given, or clears its crop when no rectangle is given.
    fn image_crop_command(&mut self, args: &Value) -> Result<(), String> {
        let node_id = required_text(args, "node_id")?.to_owned();
        let expected_asset_sha256 = required_text(args, "expected_asset_sha256")?.to_owned();
        let crop_pixel_rect = crop_argument(args)?;
        // The crop button refuses an image that a job holds too (image_edit.rs), so the command does the same.
        if self.has_reservation_for_nodes(std::slice::from_ref(&node_id)) {
            return Err("다른 작업이 이 이미지를 사용하는 중입니다. 끝난 뒤 다시 시도하세요".into());
        }
        // The crop editor applying the crop on the snapshot the call names (the asset it was made on) is the button's change.
        let started = self.image_crop_editor.as_ref()
            .filter(|editor| editor.stage == ImageCropStage::Applying && editor.intent.node_id == node_id
                && editor.intent.expected_asset_sha256 == expected_asset_sha256)
            .map(|editor| {
                let token = editor.intent.token.clone();
                let sent = self.change_in_flight(|action| {
                    matches!(action, PendingAction::ImageCropApply { token: changing } if changing == &token)
                });
                ((PendingAction::ImageCropApply { token }, editor.intent.expected_revision), sent)
            });
        let (action, revision) = self.completion(started)?;
        let operation = Operation::SetCanvasImageCrop { node_id, expected_asset_sha256, crop_pixel_rect };
        self.send_command_change(vec![operation], &[Intent::Design], action, revision)
    }

    /// Sets the memos of one page: `expected` is the list the change is made against, `memos` the list it leaves.
    fn board_memos_command(&mut self, args: &Value) -> Result<(), String> {
        let page_id = required_text(args, "page_id")?.to_owned();
        let expected = list_argument::<CanvasBoardMemo>(args, "expected")?;
        let memos = list_argument::<CanvasBoardMemo>(args, "memos")?;
        // The memo editor's mutation of these lists is the button's change, until it is sent.
        let started = self.board_memo_mutation.as_ref()
            .filter(|mutation| mutation.page_id == page_id && mutation.expected == expected && mutation.desired == memos)
            .map(|mutation| ((PendingAction::BoardMemosChange { token: mutation.token.clone() }, mutation.revision), mutation.pending));
        let (action, revision) = self.completion(started)?;
        let operation = Operation::SetCanvasBoardMemos { page_id, expected, memos };
        self.send_command_change(vec![operation], &[Intent::Content], action, revision)
    }

    /// Sets the page sections of the canvas, against the list `expected`.
    fn page_sections_command(&mut self, args: &Value) -> Result<(), String> {
        let expected = list_argument::<CanvasPageSection>(args, "expected")?;
        let sections = list_argument::<CanvasPageSection>(args, "sections")?;
        // The section editor's mutation of these lists is the button's change, until it is sent.
        let started = self.page_section_mutation.as_ref()
            .filter(|mutation| mutation.expected == expected && mutation.desired == sections)
            .map(|mutation| ((PendingAction::PageSectionsChange { token: mutation.token.clone() }, mutation.revision), mutation.pending));
        let (action, revision) = self.completion(started)?;
        let operation = Operation::SetCanvasPageSections { expected, sections };
        self.send_command_change(vec![operation], &[Intent::Content], action, revision)
    }

    /// The revision the window shows: a change that no button started is made against it.
    fn command_revision(&self) -> Result<u64, String> {
        self.context.as_ref().map(|context| context.revision).ok_or_else(|| "작업물을 아직 읽는 중입니다".into())
    }

    /// Whether a change of the window whose action `same` accepts is in flight: sent, and not answered yet.
    fn change_in_flight(&self, same: impl Fn(&PendingAction) -> bool) -> bool {
        self.pending.values().any(|pending| same(&pending.action))
    }

    /// The change a button started, when this call is the button's own: a call from the window (not an agent run) while that change
    /// is not sent yet. `started` is the change, with whether it is sent.
    fn own_started<T>(&self, started: Option<(T, bool)>) -> Option<T> {
        started.filter(|(_, sent)| !*sent && self.run_context.is_none()).map(|(change, _)| change)
    }

    /// The action and the revision that a command's change completes with: the button's own change when `own_started` takes it, else
    /// `Apply` against the revision the window shows.
    fn completion(&self, started: Option<((PendingAction, u64), bool)>) -> Result<(PendingAction, u64), String> {
        match self.own_started(started) {
            Some(own) => Ok(own),
            None => Ok((PendingAction::Apply, self.command_revision()?)),
        }
    }

    /// Sends a command's change under `action` against `revision`. A refused send is an error for the window's click, which shows it.
    /// For an agent run the refusal is the run's enqueue error (`apply_changes`), which `execute_ui_run` reports, so it is no error here.
    fn send_command_change(&mut self, operations: Vec<Operation>, allowed: &[Intent], action: PendingAction, revision: u64) -> Result<(), String> {
        let allowed: BTreeSet<Intent> = allowed.iter().cloned().collect();
        let sent = self.apply_changes(operations, allowed, revision, action).is_some();
        if sent || self.run_context.is_some() {
            return Ok(());
        }
        Err("변경을 보내지 못했습니다. 최신 상태를 확인한 뒤 다시 시도하세요".into())
    }
}

/// A text argument that must be present.
fn required_text<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("`{key}` 값이 필요합니다"))
}

/// A list argument, read from the JSON straight into the model type.
fn list_argument<T: for<'de> Deserialize<'de>>(args: &Value, key: &str) -> Result<Vec<T>, String> {
    let value = args.get(key).ok_or_else(|| format!("`{key}` 값이 필요합니다"))?;
    Vec::<T>::deserialize(value).map_err(|error| format!("`{key}`를 읽지 못했습니다: {error}"))
}

/// The crop rectangle from `x`, `y`, `width` and `height`: all four, or none (no crop).
fn crop_argument(args: &Value) -> Result<Option<PixelRect>, String> {
    let values = ["x", "y", "width", "height"].map(|key| args.get(key).and_then(Value::as_u64));
    let pixel = |value: u64| u32::try_from(value).map_err(|_| "자르기 값이 너무 큽니다".to_owned());
    match values {
        [None, None, None, None] => Ok(None),
        [Some(x), Some(y), Some(width), Some(height)] => {
            Ok(Some(PixelRect { x: pixel(x)?, y: pixel(y)?, width: pixel(width)?, height: pixel(height)? }))
        }
        _ => Err("자르기 사각형은 x, y, width, height를 모두 주거나 모두 빼세요".into()),
    }
}
