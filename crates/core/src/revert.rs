//! Structural undo of one committed change while later changes stay in effect
//! (`history.undo_change`): the current document with the change `before -> after` taken back.
//! A location the change touched must still hold the change's value (or already hold the value
//! from before it); otherwise a later change edited the same data and the undo is refused there.
//!
//! Arrays whose elements all carry a unique string `id` (or `value.id`, as in adjacently tagged
//! flow blocks) are matched by id, so later insertions, removals and edits of other elements are
//! kept. Every other array is one value. The engine still validates the result before it commits.
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};

/// `current` with `before -> after` taken back, or the conflict locations: JSON pointers into
/// `current`, with `[id=…]` for an element that `current` no longer has.
pub(crate) fn revert(before: &Value, after: &Value, current: &Value) -> Result<Value, Vec<String>> {
    let mut conflicts = Vec::new();
    let out = merge(Some(before), Some(after), Some(current), &mut String::new(), &mut conflicts);
    if conflicts.is_empty() { Ok(out.unwrap_or(Value::Null)) } else { Err(conflicts) }
}

/// `b` before the change, `a` after it, `c` now; `None` = absent.
fn merge(b: Option<&Value>, a: Option<&Value>, c: Option<&Value>, path: &mut String, conflicts: &mut Vec<String>) -> Option<Value> {
    if a == b { return c.cloned(); } // the change did not touch this
    if c == a { return b.cloned(); } // untouched since the change: take it back
    if c == b { return c.cloned(); } // already as before the change
    match (b, a, c) {
        (Some(Value::Object(b)), Some(Value::Object(a)), Some(Value::Object(c))) => Some(Value::Object(merge_objects(b, a, c, path, conflicts))),
        (Some(Value::Array(b)), Some(Value::Array(a)), Some(Value::Array(c))) => match (keyed(b), keyed(a), keyed(c)) {
            (Some(kb), Some(ka), Some(kc)) => Some(Value::Array(merge_keyed((b, &kb), (a, &ka), (c, &kc), path, conflicts))),
            _ => { conflicts.push(path.clone()); Some(Value::Array(c.clone())) }
        },
        _ => { conflicts.push(path.clone()); c.cloned() }
    }
}

fn push_key(path: &mut String, key: &str) {
    path.push('/');
    for ch in key.chars() {
        match ch { '~' => path.push_str("~0"), '/' => path.push_str("~1"), ch => path.push(ch) }
    }
}

fn merge_objects(b: &Map<String, Value>, a: &Map<String, Value>, c: &Map<String, Value>, path: &mut String, conflicts: &mut Vec<String>) -> Map<String, Value> {
    let keys: BTreeSet<&String> = b.keys().chain(a.keys()).chain(c.keys()).collect();
    let mut out = Map::new();
    for key in keys {
        let len = path.len();
        push_key(path, key);
        if let Some(v) = merge(b.get(key), a.get(key), c.get(key), path, conflicts) { out.insert(key.clone(), v); }
        path.truncate(len);
    }
    out
}

fn element_id(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str).or_else(|| v.get("value").and_then(|inner| inner.get("id")).and_then(Value::as_str))
}
/// Element ids when every element has one and none repeats.
fn keyed(items: &[Value]) -> Option<Vec<&str>> {
    let ids: Vec<&str> = items.iter().map(element_id).collect::<Option<_>>()?;
    let unique: BTreeSet<&str> = ids.iter().copied().collect();
    (unique.len() == ids.len()).then_some(ids)
}

type Keyed<'a> = (&'a [Value], &'a [&'a str]);

fn merge_keyed(b: Keyed<'_>, a: Keyed<'_>, c: Keyed<'_>, path: &mut String, conflicts: &mut Vec<String>) -> Vec<Value> {
    let index = |(items, ids): Keyed<'_>| -> HashMap<String, Value> { ids.iter().map(|id| id.to_string()).zip(items.iter().cloned()).collect() };
    let (mb, ma, mc) = (index(b), index(a), index(c));
    let (kb, ka, kc) = (b.1, a.1, c.1);
    let at_index = |path: &mut String, i: usize| { path.push('/'); path.push_str(&i.to_string()); };
    let at_id = |path: &mut String, id: &str| { path.push_str("[id="); path.push_str(id); path.push(']'); };
    let common = |ids: &[&str], other: &HashMap<String, Value>| -> Vec<String> { ids.iter().filter(|id| other.contains_key(**id)).map(|id| id.to_string()).collect() };
    if common(kb, &ma) != common(ka, &mb) {
        // The change reordered the list. It goes back only if nothing changed the list since.
        if kc != ka { conflicts.push(path.clone()); return c.0.to_vec(); }
        let mut out = Vec::new();
        for id in kb.iter().chain(ka.iter().filter(|id| !mb.contains_key(**id))) {
            let len = path.len();
            match kc.iter().position(|x| x == id) { Some(i) => at_index(path, i), None => at_id(path, id) }
            if let Some(v) = merge(mb.get(*id), ma.get(*id), mc.get(*id), path, conflicts) { out.push(v); }
            path.truncate(len);
        }
        return out;
    }
    // Same relative order: keep the current order, drop what the change added, put back what it removed.
    let mut out: Vec<(String, Value)> = Vec::new();
    for (i, id) in kc.iter().enumerate() {
        let len = path.len();
        at_index(path, i);
        if let Some(v) = merge(mb.get(*id), ma.get(*id), mc.get(*id), path, conflicts) { out.push((id.to_string(), v)); }
        path.truncate(len);
    }
    for (bi, id) in kb.iter().enumerate() {
        if mc.contains_key(*id) { continue; }
        let len = path.len();
        at_id(path, id);
        if let Some(v) = merge(mb.get(*id), ma.get(*id), None, path, conflicts) {
            // After its nearest earlier sibling (in the order before the change) that is in the result.
            let slot = kb[..bi].iter().rev().find_map(|prev| out.iter().position(|(oid, _)| oid == prev)).map_or(0, |p| p + 1);
            out.insert(slot, (id.to_string(), v));
        }
        path.truncate(len);
    }
    out.into_iter().map(|(_, v)| v).collect()
}

#[cfg(test)]
mod tests {
    use super::revert;
    use serde_json::json;

    fn node(id: &str, text: &str) -> serde_json::Value { json!({ "id": id, "text": text, "style": { "fill": "#fff" } }) }

    #[test]
    fn an_edit_is_taken_back_while_later_edits_of_other_data_stay() {
        let before = json!({ "nodes": [node("a", "1"), node("b", "1")], "title": "t" });
        let after = json!({ "nodes": [node("a", "2"), node("b", "1")], "title": "t" });
        let current = json!({ "nodes": [node("a", "2"), node("b", "9")], "title": "u" });
        assert_eq!(revert(&before, &after, &current).unwrap(), json!({ "nodes": [node("a", "1"), node("b", "9")], "title": "u" }));
    }

    #[test]
    fn the_same_data_edited_later_is_a_conflict_named_by_pointer() {
        let before = json!({ "nodes": [node("a", "1"), node("b", "1")] });
        let after = json!({ "nodes": [node("a", "2"), node("b", "1")] });
        let current = json!({ "nodes": [node("b", "1"), node("a", "3")] });
        assert_eq!(revert(&before, &after, &current).unwrap_err(), ["/nodes/1/text"]);
        // Already back to the value before the change: nothing to do there.
        let current = json!({ "nodes": [node("a", "1"), node("b", "5")] });
        assert_eq!(revert(&before, &after, &current).unwrap(), current);
    }

    #[test]
    fn an_added_element_is_removed_and_a_removed_one_returns_to_its_place() {
        let before = json!([node("a", ""), node("b", ""), node("c", "")]);
        // The change removed b and added d at the end.
        let after = json!([node("a", ""), node("c", ""), node("d", "")]);
        // Later: e inserted first, c edited.
        let current = json!([node("e", ""), node("a", ""), node("c", "x"), node("d", "")]);
        assert_eq!(revert(&before, &after, &current).unwrap(), json!([node("e", ""), node("a", ""), node("b", ""), node("c", "x")]));
        // An added element edited later cannot be removed silently; a removed element that came
        // back with other content conflicts too.
        let edited = json!([node("a", ""), node("c", ""), node("d", "edited")]);
        assert_eq!(revert(&before, &after, &edited).unwrap_err(), ["/2"]);
        let back = json!([node("a", ""), node("b", "other"), node("c", ""), node("d", "")]);
        assert_eq!(revert(&before, &after, &back).unwrap_err(), ["/1"]);
    }

    #[test]
    fn a_reorder_goes_back_only_when_the_list_is_unchanged_since() {
        let before = json!([node("a", ""), node("b", "")]);
        let after = json!([node("b", ""), node("a", "")]);
        let edited_inside = json!([node("b", "x"), node("a", "")]);
        assert_eq!(revert(&before, &after, &edited_inside).unwrap(), json!([node("a", ""), node("b", "x")]));
        let grown = json!([node("b", ""), node("a", ""), node("c", "")]);
        assert_eq!(revert(&before, &after, &grown).unwrap_err(), [""]);
    }

    #[test]
    fn flow_blocks_match_by_value_id_and_other_arrays_are_one_value() {
        let block = |id: &str, text: &str| json!({ "type": "paragraph", "value": { "id": id, "runs": [{ "text": text }] } });
        let before = json!({ "blocks": [block("p1", "a"), block("p2", "a")], "tags": ["x"] });
        let after = json!({ "blocks": [block("p1", "b"), block("p2", "a")], "tags": ["x"] });
        let current = json!({ "blocks": [block("p1", "b"), block("p2", "c")], "tags": ["x", "y"] });
        assert_eq!(revert(&before, &after, &current).unwrap(), json!({ "blocks": [block("p1", "a"), block("p2", "c")], "tags": ["x", "y"] }));
        let tags_after = json!({ "blocks": [block("p1", "a"), block("p2", "a")], "tags": ["z"] });
        assert_eq!(revert(&before, &tags_after, &current).unwrap_err(), ["/tags"]);
    }
}
