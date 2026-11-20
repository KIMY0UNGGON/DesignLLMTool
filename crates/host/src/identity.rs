//! The actor identity the host fixes for a connection (task 81, `md/03-tasks/open/80-mcp-phase.md` item 1).
//!
//! Every connection except the verified gui connection gets a prefix: `mcp:<connection id>` for an MCP bridge, `job:<job id>`
//! for a job-bound bridge and `cli` for the command line. The host rewrites the actors and the reservation owner of the
//! connection's core requests into that prefix before the core sees them: an actor that already is the prefix or starts with
//! `<prefix>:` stays, any other becomes `<prefix>:<actor>` (an empty one becomes the prefix). A client therefore keeps the name it
//! gives (`"actor":"agent"` is recorded as `mcp:<connection id>:agent`) but cannot act as `desktop`, as another connection or as a
//! job. The core lets the connection release, and see the `reservation_id` of, only reservations with its prefix
//! (`designllm_core::caller`). Receipt fingerprints use the client's process instead of the connection (`fingerprint`), so a
//! bridge that reconnects and repeats a request with the same request_id gets its receipt and nothing is applied twice.
//!
//! The gui connection is not rewritten: its own actors are `desktop` and `desktop:inline:<view>`, and the ChangeSets of a
//! `ui.command.run` carry the actor of the bridge that queued the run, which this module already fixed.

use designllm_contracts::JobRef;
use designllm_core::caller::actor_has_prefix;
use serde_json::Value;

/// The fixed identity of one connection without the gui role.
pub(crate) struct Identity {
    prefix: String,
    /// Stands for `prefix` in receipt fingerprints (`designllm_core::caller`): `mcp:pid-<client process id>-<its creation time>` for an MCP bridge,
    /// so the same bridge process repeating a request on a new connection gets its receipt; else the prefix itself.
    fingerprint: String,
    /// The prefix names the connection itself, so nothing can reconnect as it: its reservations end with the connection.
    per_connection: bool,
}

impl Identity {
    /// An MCP bridge: `job:<job id>` when job-bound, else `mcp:<connection id>`. `client_process` is the pipe client's process id
    /// and creation time (None when they could not be read: the fingerprint is then the connection's own prefix).
    pub(crate) fn bridge(connection_id: &str, binding: Option<&JobRef>, client_process: Option<(u32, u64)>) -> Identity {
        match binding {
            Some(job) => Identity::shared(format!("job:{}", job.job_id)),
            None => {
                let prefix = format!("mcp:{connection_id}");
                let fingerprint = client_process.map_or_else(|| prefix.clone(), |(pid, created)| format!("mcp:pid-{pid}-{created}"));
                Identity { prefix, fingerprint, per_connection: true }
            }
        }
    }

    /// The command line. One `--command` is one connection, so the prefix is shared by all of them: a request repeated
    /// with the same request_id from a new command keeps its receipt, and a reservation outlives the command that made it.
    pub(crate) fn cli() -> Identity {
        Identity::shared("cli".to_string())
    }

    fn shared(prefix: String) -> Identity {
        Identity { fingerprint: prefix.clone(), prefix, per_connection: false }
    }

    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Whether the connection's reservations end with it (see `per_connection`).
    pub(crate) fn per_connection(&self) -> bool {
        self.per_connection
    }

    /// Rewrites the actors of a core request into this identity: the top-level `actor` of any method, `change_set.actor` of
    /// `proposal.create`, `create.actor` of `job.start` and the `owner` of `targets.reserve`. A value that is not a string is
    /// left for the core to refuse.
    pub(crate) fn pin(&self, method: &str, params: &mut Value) {
        self.pin_field(params, "actor");
        match method {
            "proposal.create" => self.pin_nested(params, "change_set"),
            "job.start" => self.pin_nested(params, "create"),
            "targets.reserve" => self.pin_field(params, "owner"),
            _ => {}
        }
    }

    fn pin_nested(&self, params: &mut Value, key: &str) {
        if let Some(inner) = params.get_mut(key) {
            self.pin_field(inner, "actor");
        }
    }

    fn pin_field(&self, object: &mut Value, key: &str) {
        if let Some(Value::String(actor)) = object.get_mut(key) {
            if let Some(pinned) = pinned(&self.prefix, actor) {
                *actor = pinned;
            }
        }
    }
}

/// The actor `actor` becomes under `prefix`, or None when it already has the prefix.
fn pinned(prefix: &str, actor: &str) -> Option<String> {
    if actor_has_prefix(actor, prefix) {
        None
    } else if actor.trim().is_empty() {
        Some(prefix.to_string())
    } else {
        Some(format!("{prefix}:{actor}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mcp() -> Identity {
        Identity::bridge("c1", None, Some((7, 99)))
    }

    fn pinned_actor(identity: &Identity, actor: &str) -> Value {
        let mut params = json!({ "actor": actor });
        identity.pin("changes.apply", &mut params);
        params["actor"].clone()
    }

    #[test]
    fn identities_name_their_connection_or_job() {
        let job = JobRef { job_id: "j1".into(), attempt_id: "a1".into() };
        assert_eq!((mcp().prefix(), mcp().per_connection()), ("mcp:c1", true));
        let bound = Identity::bridge("c1", Some(&job), Some((7, 99)));
        assert_eq!((bound.prefix(), bound.fingerprint(), bound.per_connection()), ("job:j1", "job:j1", false));
        assert_eq!((Identity::cli().prefix(), Identity::cli().fingerprint(), Identity::cli().per_connection()), ("cli", "cli", false));
        assert_eq!(mcp().fingerprint(), "mcp:pid-7-99", "an MCP bridge's receipts follow its process");
        assert_eq!(Identity::bridge("c1", None, None).fingerprint(), "mcp:c1", "without a process id they follow the connection");
    }

    #[test]
    fn a_free_actor_keeps_its_name_under_the_prefix_and_cannot_claim_another_identity() {
        let identity = mcp();
        assert_eq!(pinned_actor(&identity, "agent"), "mcp:c1:agent");
        assert_eq!(pinned_actor(&identity, "desktop"), "mcp:c1:desktop");
        assert_eq!(pinned_actor(&identity, "desktop:inline:00000000-0000-0000-0000-000000000000"), "mcp:c1:desktop:inline:00000000-0000-0000-0000-000000000000");
        assert_eq!(pinned_actor(&identity, "job:j1"), "mcp:c1:job:j1");
        assert_eq!(pinned_actor(&identity, "mcp:c2:agent"), "mcp:c1:mcp:c2:agent");
        assert_eq!(pinned_actor(&identity, "mcp:c12"), "mcp:c1:mcp:c12");
        assert_eq!(pinned_actor(&identity, "cli"), "mcp:c1:cli");
        assert_eq!(pinned_actor(&Identity::cli(), "desktop"), "cli:desktop");
    }

    #[test]
    fn an_actor_with_the_prefix_stays_and_an_empty_one_becomes_the_prefix() {
        let identity = mcp();
        assert_eq!(pinned_actor(&identity, "mcp:c1"), "mcp:c1");
        assert_eq!(pinned_actor(&identity, "mcp:c1:agent"), "mcp:c1:agent");
        assert_eq!(pinned_actor(&identity, ""), "mcp:c1");
        assert_eq!(pinned_actor(&identity, "  "), "mcp:c1");
    }

    #[test]
    fn nested_actors_and_the_reservation_owner_are_pinned_only_where_the_method_names_them() {
        let identity = mcp();
        let mut proposal = json!({ "change_set": { "actor": "desktop", "operations": [] } });
        identity.pin("proposal.create", &mut proposal);
        assert_eq!(proposal["change_set"]["actor"], "mcp:c1:desktop");
        let mut start = json!({ "create": { "actor": "desktop", "kind": "canvas" } });
        identity.pin("job.start", &mut start);
        assert_eq!(start["create"]["actor"], "mcp:c1:desktop");
        let mut reserve = json!({ "owner": "desktop:inline:v", "view_id": "v" });
        identity.pin("targets.reserve", &mut reserve);
        assert_eq!(reserve["owner"], "mcp:c1:desktop:inline:v");
        // Elsewhere the same keys are data, not actors.
        let mut other = json!({ "owner": "x", "change_set": { "actor": "y" }, "create": { "actor": "z" } });
        identity.pin("changes.apply", &mut other);
        assert_eq!(other, json!({ "owner": "x", "change_set": { "actor": "y" }, "create": { "actor": "z" } }));
    }

    #[test]
    fn a_non_string_actor_is_left_for_the_core_to_refuse() {
        let mut params = json!({ "actor": 7 });
        mcp().pin("changes.apply", &mut params);
        assert_eq!(params["actor"], 7);
    }
}
