//! The runner menu's model lists. Each runtime's list is asked from its CLI (`designllm_runtimes::detect::model_list`) once,
//! on a background thread, when the menu first opens, and kept for the session. Until the answer comes, and when the CLI
//! cannot be asked, the menu shows the runtime's fixed list.

use std::{sync::{Arc, Mutex, MutexGuard}, time::{Duration, Instant}};
use std::thread;

use designllm_runtimes::detect::{fallback_models, ModelList, ModelSource, RuntimeKind};
use eframe::egui;

use super::DesktopApp;

/// Asks one runtime for its list. The product asks the CLI; tests pass a fixed answer.
pub(super) type Lookup = fn(RuntimeKind) -> ModelList;

/// The effort values `home.runner` accepts (its schema in `commands.rs`), least first: the scale on which the nearest
/// accepted effort is found.
pub(super) const RUNNER_EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];
/// The efforts the menu offers for a model whose list says nothing about them.
pub(super) const DEFAULT_EFFORTS: [&str; 4] = ["low", "medium", "high", "max"];
pub(super) const RUNNERS: [(&str, &str, RuntimeKind); 3] = [
    ("claude", "Claude", RuntimeKind::Claude), ("agy", "AGY", RuntimeKind::Agy), ("codex", "Codex", RuntimeKind::Codex),
];

#[derive(Default)]
struct Authentication {
    refreshed: Option<Instant>,
    signed_in: [Option<bool>; 3],
}

#[derive(Default)]
enum Slot {
    #[default]
    Idle,
    Loading,
    /// The answer. `fresh` until the window has fitted its runner to it (`settle_runner_models`).
    Ready { list: Arc<ModelList>, fresh: bool },
}

/// The model lists of the Claude and Codex runtimes, shared with the lookup threads.
pub(super) struct RunnerModels {
    claude: Arc<Mutex<Slot>>,
    agy: Arc<Mutex<Slot>>,
    codex: Arc<Mutex<Slot>>,
    /// The fixed lists, shown until the answer is in.
    claude_fallback: Arc<ModelList>,
    agy_fallback: Arc<ModelList>,
    codex_fallback: Arc<ModelList>,
    auth: Arc<Mutex<Authentication>>,
    lookup: Lookup,
    /// A runtime change made while that runtime's list was awaited: its model is checked against the answer.
    recheck: Option<RuntimeKind>,
}

/// The runtime whose list the menu shows for an `ai_runtime` value: Codex for `codex`, else Claude (the menu marks Claude
/// for any other value, as `runner_popover` and `runner_labels` do).
pub(super) fn runtime_kind(runtime: &str) -> RuntimeKind {
    RUNNERS.iter().find(|(id, _, _)| *id == runtime).map_or(RuntimeKind::Claude, |(_, _, kind)| *kind)
}

/// The effort to change to so that a model accepting `efforts` runs: the accepted menu value nearest to `current` (the lower
/// one on a tie), or `None` when `current` is accepted or the list names no menu value.
pub(super) fn nearest_effort(current: &str, efforts: &[String]) -> Option<&'static str> {
    let mut accepted = RUNNER_EFFORTS.into_iter().filter(|effort| efforts.iter().any(|level| level == effort)).peekable();
    accepted.peek()?;
    if RUNNER_EFFORTS.contains(&current) && efforts.iter().any(|level| level == current) {
        return None;
    }
    // A value off the scale counts as `high`, the window's default.
    let rank = |effort: &str| RUNNER_EFFORTS.iter().position(|level| *level == effort).unwrap_or(3);
    let here = rank(current);
    accepted.min_by_key(|effort| rank(effort).abs_diff(here))
}

#[cfg(not(test))]
fn cli_lookup(kind: RuntimeKind) -> ModelList {
    designllm_runtimes::detect::model_list(kind, &designllm_runtimes::detect::RuntimeConfig::from_env())
}

/// A lookup that asks no CLI: the fixed list (a UI-test window, task 69, unless it is told to ask).
pub(super) fn fixed_lookup(kind: RuntimeKind) -> ModelList {
    ModelList { models: fallback_models(kind), source: ModelSource::Fallback }
}

/// Tests do not run a CLI: their lookup answers with the fixed list.
#[cfg(test)]
fn test_lookup(kind: RuntimeKind) -> ModelList {
    ModelList { models: fallback_models(kind), source: ModelSource::Fallback }
}

impl Default for RunnerModels {
    fn default() -> Self {
        #[cfg(not(test))]
        {
            Self::with_lookup(cli_lookup)
        }
        // Tests start with both answers in, so that no lookup thread changes the menu between frames.
        #[cfg(test)]
        {
            let models = Self::with_lookup(test_lookup);
            models.answer(RuntimeKind::Claude, test_lookup(RuntimeKind::Claude));
            models.answer(RuntimeKind::Codex, test_lookup(RuntimeKind::Codex));
            models
        }
    }
}

fn locked<T>(slot: &Mutex<T>) -> MutexGuard<'_, T> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixed(kind: RuntimeKind) -> Arc<ModelList> {
    Arc::new(ModelList { models: fallback_models(kind), source: ModelSource::Fallback })
}

impl RunnerModels {
    pub(super) fn with_lookup(lookup: Lookup) -> Self {
        Self {
            claude: Arc::default(),
            agy: Arc::default(),
            codex: Arc::default(),
            claude_fallback: fixed(RuntimeKind::Claude),
            agy_fallback: fixed(RuntimeKind::Agy),
            codex_fallback: fixed(RuntimeKind::Codex),
            auth: Arc::new(Mutex::new(Authentication {
                #[cfg(test)]
                signed_in: [Some(true), Some(false), Some(true)],
                ..Default::default()
            })),
            lookup,
            recheck: None,
        }
    }

    pub(super) fn available(&self, runtime: &str) -> bool {
        RUNNERS.iter().position(|(id, _, _)| *id == runtime)
            .is_some_and(|index| locked(&self.auth).signed_in[index] == Some(true))
    }

    pub(super) fn checking(&self) -> bool {
        locked(&self.auth).signed_in.iter().any(Option::is_none)
    }

    pub(super) fn refresh_auth(&self, ctx: &egui::Context) {
        if cfg!(test) { return; }
        let mut auth = locked(&self.auth);
        if auth.refreshed.is_some_and(|at| at.elapsed() < Duration::from_secs(30)) { return; }
        auth.refreshed = Some(Instant::now());
        for (index, (_, _, kind)) in RUNNERS.into_iter().enumerate() {
            let (worker_auth, ctx) = (Arc::clone(&self.auth), ctx.clone());
            let started = thread::Builder::new().name("runner-login".into()).spawn(move || {
                let config = designllm_runtimes::detect::RuntimeConfig::from_env();
                let signed_in = designllm_runtimes::detect::logged_in(kind, &config);
                locked(&worker_auth).signed_in[index] = Some(signed_in);
                ctx.request_repaint();
            });
            if started.is_err() { auth.signed_in[index] = Some(false); }
        }
    }

    /// The slot and the fixed list of `kind`. The mock agent has no list.
    fn slot(&self, kind: RuntimeKind) -> Option<(&Arc<Mutex<Slot>>, &Arc<ModelList>)> {
        match kind {
            RuntimeKind::Claude => Some((&self.claude, &self.claude_fallback)),
            RuntimeKind::Agy => Some((&self.agy, &self.agy_fallback)),
            RuntimeKind::Codex => Some((&self.codex, &self.codex_fallback)),
            RuntimeKind::Mock => None,
        }
    }

    /// Starts the lookup of `kind` once, on a thread. The context is asked to repaint when the answer is in. A lookup that
    /// panics, or a thread that cannot start, leaves the fixed list as the answer rather than a list that never comes.
    pub(super) fn request(&self, kind: RuntimeKind, ctx: &egui::Context) {
        let Some((slot, fallback)) = self.slot(kind) else {
            return;
        };
        {
            let mut state = locked(slot);
            if !matches!(*state, Slot::Idle) {
                return;
            }
            *state = Slot::Loading;
        }
        let (worker_slot, worker_fallback, lookup, ctx) = (Arc::clone(slot), Arc::clone(fallback), self.lookup, ctx.clone());
        let spawned = thread::Builder::new().name("runner-models".into()).spawn(move || {
            let list = std::panic::catch_unwind(move || lookup(kind)).map(Arc::new).unwrap_or(worker_fallback);
            *locked(&worker_slot) = Slot::Ready { list, fresh: true };
            ctx.request_repaint();
        });
        if spawned.is_err() {
            *locked(slot) = Slot::Ready { list: Arc::clone(fallback), fresh: true };
        }
    }

    /// The list to show now, and whether the CLI's answer is still awaited. Before the answer, the fixed list.
    pub(super) fn shown(&self, kind: RuntimeKind) -> (Arc<ModelList>, bool) {
        let Some((slot, fallback)) = self.slot(kind) else {
            return (Arc::new(ModelList { models: Vec::new(), source: ModelSource::Fallback }), false);
        };
        match &*locked(slot) {
            Slot::Ready { list, .. } => (Arc::clone(list), false),
            Slot::Idle | Slot::Loading => (Arc::clone(fallback), true),
        }
    }

    /// True once for an answer that has come in since the last call.
    fn take_fresh(&self, kind: RuntimeKind) -> bool {
        let Some((slot, _)) = self.slot(kind) else {
            return false;
        };
        match &mut *locked(slot) {
            Slot::Ready { fresh, .. } => std::mem::take(fresh),
            Slot::Idle | Slot::Loading => false,
        }
    }

    /// Test support: the answer for `kind` is already in.
    #[cfg(test)]
    pub(super) fn answer(&self, kind: RuntimeKind, list: ModelList) {
        if let Some((slot, _)) = self.slot(kind) {
            *locked(slot) = Slot::Ready { list: Arc::new(list), fresh: false };
        }
    }
}

impl DesktopApp {
    pub(super) fn runner_can_start(&self, runtime: &str) -> bool {
        self.runner_models.available(runtime) || (runtime == "mock" && (cfg!(test) || self.ui_test.is_some()))
    }

    /// Fits the runner to the current runtime's list (`home.runner`, and an answer coming in). After a runtime change
    /// without a model given, a model the list does not name is cleared; while the list is still awaited the check waits
    /// for the answer (`settle_runner_models`), so that the fixed list does not clear a model only the CLI names. Unless
    /// `effort_given`, an effort the listed model does not accept becomes the nearest one it does. True when the model or
    /// effort changed.
    pub(super) fn fit_runner(&mut self, runtime_changed: bool, model_given: bool, effort_given: bool) -> bool {
        let kind = runtime_kind(&self.ai_runtime);
        let (list, awaiting) = self.runner_models.shown(kind);
        if runtime_changed || model_given {
            self.runner_models.recheck = None;
        }
        let check_model = runtime_changed && !model_given;
        match list.models.iter().find(|model| model.id == self.ai_model) {
            Some(model) if !effort_given => match nearest_effort(&self.ai_effort, &model.efforts) {
                Some(effort) => {
                    self.ai_effort = effort.to_owned();
                    true
                }
                None => false,
            },
            None if check_model && !self.ai_model.is_empty() => {
                if awaiting {
                    self.runner_models.recheck = Some(kind);
                    false
                } else {
                    self.ai_model.clear();
                    true
                }
            }
            _ => false,
        }
    }

    /// At each frame's start: asks for the list a waiting model check needs, and when the current runtime's answer has just
    /// come in, fits the runner to it and saves a change.
    pub(super) fn settle_runner_models(&mut self, ctx: &egui::Context) {
        self.runner_models.refresh_auth(ctx);
        if let Some(kind) = self.runner_models.recheck {
            self.runner_models.request(kind, ctx);
        }
        let kind = runtime_kind(&self.ai_runtime);
        if !self.runner_models.take_fresh(kind) {
            return;
        }
        let waiting_check = self.runner_models.recheck == Some(kind);
        if self.fit_runner(waiting_check, false, false) {
            self.save_settings();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn only_confirmed_sign_ins_are_available() {
        let runners = RunnerModels::default();
        locked(&runners.auth).signed_in = [None, Some(true), Some(false)];
        assert!(runners.checking());
        assert!(runners.available("agy"));
        assert!(!runners.available("claude"));
        assert!(!runners.available("codex"));
        assert!(!runners.available("mock"));

        locked(&runners.auth).signed_in = [Some(true), Some(false), Some(false)];
        assert!(!runners.checking());
        assert!(runners.available("claude"));
        assert!(!runners.available("agy"));
    }

    #[test]
    fn an_effort_the_model_does_not_accept_becomes_the_nearest_accepted_one() {
        assert_eq!(nearest_effort("max", &levels(&["low", "medium", "high"])), Some("high"));
        assert_eq!(nearest_effort("low", &levels(&["high", "max"])), Some("high"));
        // A tie goes to the lower value.
        assert_eq!(nearest_effort("medium", &levels(&["low", "high"])), Some("low"));
        // Codex lists `minimal` and `xhigh` but no `max`.
        assert_eq!(nearest_effort("max", &levels(&["minimal", "low", "medium", "high", "xhigh"])), Some("xhigh"));
        assert_eq!(nearest_effort("xhigh", &levels(&["low", "medium", "high", "xhigh", "max"])), None, "an accepted effort stays");
        assert_eq!(nearest_effort("minimal", &levels(&["low", "medium", "high", "xhigh", "max"])), Some("low"), "Claude has no minimal");
        assert_eq!(nearest_effort("max", &[]), None, "a list without efforts says nothing");
        assert_eq!(nearest_effort("max", &levels(&["ultra"])), None, "no value the command can send");
    }
}
