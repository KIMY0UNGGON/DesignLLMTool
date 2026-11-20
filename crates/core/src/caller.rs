//! The identity of the connection whose request is being dispatched (task 81).
//!
//! The host fixes an actor prefix for every connection except the verified gui connection: it rewrites the actors and the
//! reservation owner of the connection's requests into that prefix ([`actor_has_prefix`]) and runs their core calls inside
//! [`Workspace::with_caller`]. The core then lets that connection release only its own reservations and shows it the
//! `reservation_id` of its own reservations only. In-process calls and the gui connection have no caller and keep the
//! earlier behavior.
//!
//! A request's receipt fingerprint (`receipts::key_for`) does not use the caller prefix itself but `fingerprint`, which the host
//! gives per client: for an MCP bridge it names the client process, not the connection. Inside the scope an actor of any
//! connection of the caller's family (`mcp:<connection>`) counts as that fingerprint, which also covers the change set a
//! proposal stored under the connection that made it. A bridge that lost its connection and repeats a request with the same
//! request_id on a new one therefore gets the stored receipt, while the recorded actor keeps the connection's prefix.

use std::cell::RefCell;

use designllm_contracts::{JobRef, RpcRequest};
use serde_json::Value;

use crate::{Dispatch, Workspace};

/// The scope of one caller's core calls on this thread.
#[derive(Clone)]
struct Fingerprint {
    caller: String,
    fingerprint: String,
    /// `mcp:` when the fingerprint stands for every connection of the caller's family; None when it is the caller itself.
    family: Option<String>,
}

thread_local! {
    static FINGERPRINT: RefCell<Option<Fingerprint>> = const { RefCell::new(None) };
}

/// Puts the caller in place for a scope and restores what was there before when dropped, also by a panic or an early return:
/// neither the workspace's caller nor the thread's fingerprint reaches the next request.
struct CallerScope<'a> {
    workspace: &'a mut Workspace,
    previous_caller: Option<String>,
    previous_fingerprint: Option<Fingerprint>,
}

impl<'a> CallerScope<'a> {
    fn enter(workspace: &'a mut Workspace, caller: &str, fingerprint: &str) -> Self {
        let family = (fingerprint != caller).then(|| caller.split_once(':').map(|(kind, _)| format!("{kind}:"))).flatten();
        let scope = Fingerprint { caller: caller.to_string(), fingerprint: fingerprint.to_string(), family };
        let previous_fingerprint = FINGERPRINT.with(|current| current.replace(Some(scope)));
        let previous_caller = workspace.caller.replace(caller.to_string());
        CallerScope { workspace, previous_caller, previous_fingerprint }
    }
}

impl Drop for CallerScope<'_> {
    fn drop(&mut self) {
        self.workspace.caller = self.previous_caller.take();
        let previous = self.previous_fingerprint.take();
        FINGERPRINT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Whether `actor` is `prefix` itself or `prefix:` followed by anything.
pub fn actor_has_prefix(actor: &str, prefix: &str) -> bool {
    actor.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
}

impl Fingerprint {
    /// `actor` as fingerprints see it: its caller prefix, or the `<family><connection>` prefix of another connection of the
    /// family, replaced by the fingerprint.
    fn swap(&self, actor: &str) -> Option<String> {
        if actor_has_prefix(actor, &self.caller) {
            return Some(format!("{}{}", self.fingerprint, &actor[self.caller.len()..]));
        }
        let rest = actor.strip_prefix(self.family.as_deref()?)?;
        let tail = rest.find(':').map_or("", |at| &rest[at..]);
        (rest.len() > tail.len()).then(|| format!("{}{tail}", self.fingerprint))
    }
}

/// The request as its receipt fingerprint sees it: the actors at the top level, in `change_set` and in `create` are swapped
/// (`Fingerprint::swap`). Unchanged outside [`Workspace::with_caller`].
pub(crate) fn fingerprint_actors(request: &mut Value) {
    FINGERPRINT.with(|current| {
        let current = current.borrow();
        let Some(scope) = current.as_ref() else { return };
        let swap = |object: &mut Value| {
            if let Some(Value::String(actor)) = object.get_mut("actor") {
                if let Some(swapped) = scope.swap(actor) {
                    *actor = swapped;
                }
            }
        };
        swap(request);
        for key in ["change_set", "create"] {
            if let Some(inner) = request.get_mut(key) {
                swap(inner);
            }
        }
    });
}

impl Workspace {
    /// Runs `call` for a connection whose actors the host fixed to `caller` (`mcp:<connection>`, `job:<job>` or `cli`).
    /// `fingerprint` stands for `caller` in receipt fingerprints (equal to it when the client has no identity beyond the
    /// connection's). The caller is gone again when `call` returns or panics.
    pub fn with_caller<R>(&mut self, caller: &str, fingerprint: &str, call: impl FnOnce(&mut Workspace) -> R) -> R {
        let scope = CallerScope::enter(self, caller, fingerprint);
        call(&mut *scope.workspace)
    }

    /// [`Workspace::dispatch_start`] inside [`Workspace::with_caller`].
    pub fn dispatch_start_as(&mut self, request: RpcRequest, bound_job: Option<JobRef>, caller: &str, fingerprint: &str) -> Dispatch {
        self.with_caller(caller, fingerprint, |ws| ws.dispatch_start(request, bound_job))
    }

    /// Whether the request being dispatched may use a reservation of `owner`: always without a caller.
    pub(crate) fn caller_owns(&self, owner: &str) -> bool {
        self.caller.as_deref().is_none_or(|caller| actor_has_prefix(owner, caller))
    }

    /// Ends the user reservations whose owner has the prefix `caller`, for a connection whose identity ends with it. Job
    /// reservations stay until their job ends. Returns how many ended.
    pub fn release_caller_reservations(&mut self, caller: &str) -> usize {
        let before = self.views.reservations.len();
        self.views.reservations.retain(|_, r| r.job_id.is_some() || !actor_has_prefix(&r.owner, caller));
        before - self.views.reservations.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{actor_has_prefix, fingerprint_actors, Fingerprint, FINGERPRINT};
    use serde_json::json;

    fn fingerprinted(scope: Option<Fingerprint>, request: &serde_json::Value) -> serde_json::Value {
        let previous = FINGERPRINT.with(|current| current.replace(scope));
        let mut value = request.clone();
        fingerprint_actors(&mut value);
        FINGERPRINT.with(|current| *current.borrow_mut() = previous);
        value
    }

    fn mcp_scope() -> Option<Fingerprint> {
        Some(Fingerprint { caller: "mcp:c1".into(), fingerprint: "mcp:pid-7-99".into(), family: Some("mcp:".into()) })
    }

    #[test]
    fn a_fingerprint_stands_for_every_connection_of_the_family_and_only_inside_a_scope() {
        let request = json!({ "actor": "mcp:c1:agent", "change_set": { "actor": "mcp:c2" }, "create": { "actor": "cli:agent" }, "operations": [{ "actor": "mcp:c1:x" }] });
        assert_eq!(fingerprinted(None, &request), request, "in-process requests keep their fingerprint");
        assert_eq!(
            fingerprinted(mcp_scope(), &request),
            json!({ "actor": "mcp:pid-7-99:agent", "change_set": { "actor": "mcp:pid-7-99" }, "create": { "actor": "cli:agent" }, "operations": [{ "actor": "mcp:c1:x" }] })
        );
        let empty_connection = json!({ "actor": "mcp:" });
        assert_eq!(fingerprinted(mcp_scope(), &empty_connection), empty_connection, "no connection id, no swap");
        // A fingerprint equal to its caller (cli, a job) swaps nothing else.
        let cli = Some(Fingerprint { caller: "cli".into(), fingerprint: "cli".into(), family: None });
        let foreign = json!({ "actor": "cli:agent", "change_set": { "actor": "mcp:c2:agent" } });
        assert_eq!(fingerprinted(cli, &foreign), foreign);
    }

    #[test]
    fn a_prefix_matches_itself_and_colon_continuations_only() {
        assert!(actor_has_prefix("mcp:c1", "mcp:c1"));
        assert!(actor_has_prefix("mcp:c1:agent", "mcp:c1"));
        assert!(actor_has_prefix("mcp:c1:", "mcp:c1"));
        assert!(!actor_has_prefix("mcp:c12", "mcp:c1"));
        assert!(!actor_has_prefix("mcp:c12:agent", "mcp:c1"));
        assert!(!actor_has_prefix("agent", "mcp:c1"));
        assert!(!actor_has_prefix("desktop", "cli"));
        assert!(actor_has_prefix("cli:desktop", "cli"));
    }
}
