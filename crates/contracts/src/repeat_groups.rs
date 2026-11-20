//! Canvas repeat and conditional groups (D21): the limits and the diagnostic codes the engine, core and desktop share.
//! The types are `RepeatSpec`, `RepeatBinding`, `ShowCondition` and `ConditionOp` in the crate root, next to `NodeExt`.

/// Data rows a repeat group's table may have (the header row is not counted).
pub const REPEAT_MAX_ROWS: usize = 1_000;
/// Columns a repeat group's table may have.
pub const REPEAT_MAX_COLUMNS: usize = 64;
/// UTF-8 bytes of one cell value a repeat group reads.
pub const REPEAT_MAX_CELL_BYTES: usize = 4 * 1024;
/// Nodes one repeat group may expand to, over all its instances.
pub const REPEAT_MAX_EXPANDED_NODES: usize = 10_000;

/// `SetGroupRepeat` on a Group that already has children.
pub const CANVAS_REPEAT_GROUP_NOT_EMPTY: &str = "CANVAS_REPEAT_GROUP_NOT_EMPTY";
/// A condition outside a repeat component, or a component page used both by a repeat and as an ordinary instance.
pub const CANVAS_REPEAT_CONDITION_SCOPE: &str = "CANVAS_REPEAT_CONDITION_SCOPE";
/// A table or an expansion over the limits above.
pub const CANVAS_REPEAT_LIMIT: &str = "CANVAS_REPEAT_LIMIT";
/// A stored expansion that differs from expanding again (blocking, when a document is opened).
pub const CANVAS_REPEAT_STALE: &str = "CANVAS_REPEAT_STALE";
/// An export wrote the expanded nodes only; the data binding is not kept in the output (not blocking).
pub const CANVAS_REPEAT_FLATTENED: &str = "CANVAS_REPEAT_FLATTENED";
/// The target of SetGroupRepeat is not a Group with a row, column or grid layout, or is itself an instance or a table.
pub const CANVAS_REPEAT_TARGET: &str = "CANVAS_REPEAT_TARGET";
/// The data table is missing, is not a Canvas table, has merged cells, lies in the repeated component page or in an instance.
pub const CANVAS_REPEAT_TABLE: &str = "CANVAS_REPEAT_TABLE";
/// A repeat inside a repeat: a repeat group in a component page or an instance, or a component that holds a repeat group.
pub const CANVAS_REPEAT_NESTED: &str = "CANVAS_REPEAT_NESTED";
/// A binding or a condition names a column the table does not have.
pub const CANVAS_REPEAT_COLUMN: &str = "CANVAS_REPEAT_COLUMN";
/// A binding names a missing or non-text component node, the same node twice, or a cell a row does not have.
pub const CANVAS_REPEAT_BINDING: &str = "CANVAS_REPEAT_BINDING";
/// A condition's value does not fit its comparison (equals and not_equals need one, the empty checks take none).
pub const CANVAS_REPEAT_CONDITION: &str = "CANVAS_REPEAT_CONDITION";
/// `expected` is not the stored repeat spec or condition (RevisionConflict).
pub const CANVAS_REPEAT_EXPECTED: &str = "CANVAS_REPEAT_EXPECTED";

#[cfg(test)]
mod tests {
    use crate::{ConditionOp, Intent, NodeExt, NodeOverride, Operation, RepeatBinding, RepeatSpec, ShowCondition};
    use serde_json::json;

    fn spec() -> RepeatSpec {
        RepeatSpec {
            table_id: "table".into(),
            component_page_id: "card".into(),
            bindings: vec![RepeatBinding { source_node_id: "title".into(), column: 1 }],
        }
    }

    #[test]
    fn unset_fields_keep_the_previous_json() {
        assert_eq!(serde_json::to_value(NodeExt::default()).unwrap(), json!({}));
        assert_eq!(serde_json::to_value(NodeOverride::default()).unwrap(), json!({}));
        let text_only = NodeOverride { text: Some("x".into()), ..NodeOverride::default() };
        assert_eq!(serde_json::to_value(&text_only).unwrap(), json!({ "text": "x" }));
        let empty_bindings = RepeatSpec { bindings: Vec::new(), ..spec() };
        assert_eq!(serde_json::to_value(&empty_bindings).unwrap(), json!({ "table_id": "table", "component_page_id": "card" }));
        let no_value = ShowCondition { column: 0, op: ConditionOp::IsEmpty, value: None };
        assert_eq!(serde_json::to_value(&no_value).unwrap(), json!({ "column": 0, "op": "is_empty" }));
    }

    #[test]
    fn the_fields_round_trip_under_their_names() {
        let condition = ShowCondition { column: 2, op: ConditionOp::NotEquals, value: Some("sold out".into()) };
        let ext = NodeExt { repeat: Some(spec()), condition: Some(condition.clone()), ..NodeExt::default() };
        let value = serde_json::to_value(&ext).unwrap();
        assert_eq!(value, json!({
            "repeat": { "table_id": "table", "component_page_id": "card", "bindings": [{ "source_node_id": "title", "column": 1 }] },
            "condition": { "column": 2, "op": "not_equals", "value": "sold out" },
        }));
        assert_eq!(serde_json::from_value::<NodeExt>(value).unwrap(), ext);
        assert_eq!(ext.set_fields(), ["repeat", "condition"]);
        let hidden = NodeOverride { hidden: Some(true), ..NodeOverride::default() };
        assert_eq!(serde_json::to_value(&hidden).unwrap(), json!({ "hidden": true }));
        let ops = [ConditionOp::Equals, ConditionOp::NotEquals, ConditionOp::IsEmpty, ConditionOp::IsNotEmpty];
        assert_eq!(serde_json::to_value(ops).unwrap(), json!(["equals", "not_equals", "is_empty", "is_not_empty"]));
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(serde_json::from_value::<RepeatSpec>(json!({ "table_id": "t", "component_page_id": "c", "rows": 3 })).is_err());
        assert!(serde_json::from_value::<RepeatBinding>(json!({ "source_node_id": "n", "column": 0, "format": "x" })).is_err());
        assert!(serde_json::from_value::<ShowCondition>(json!({ "column": 0, "op": "equals", "case": "fold" })).is_err());
        assert!(serde_json::from_value::<ShowCondition>(json!({ "column": 0, "op": "greater_than" })).is_err());
        assert!(serde_json::from_value::<NodeOverride>(json!({ "hidden": true, "locked": true })).is_err());
    }

    #[test]
    fn both_operations_are_content_changes() {
        let repeat = Operation::SetGroupRepeat { node_id: "group".into(), expected: None, repeat: Some(spec()) };
        let condition = Operation::SetGroupCondition {
            node_id: "badge".into(),
            expected: None,
            condition: Some(ShowCondition { column: 0, op: ConditionOp::IsNotEmpty, value: None }),
        };
        assert_eq!((repeat.intent(), condition.intent()), (Intent::Content, Intent::Content));
        assert_eq!(serde_json::to_value(&repeat).unwrap()["op"], "set_group_repeat");
        assert_eq!(serde_json::to_value(&condition).unwrap()["op"], "set_group_condition");
        let cleared: Operation = serde_json::from_value(json!({ "op": "set_group_repeat", "node_id": "group" })).unwrap();
        assert!(matches!(cleared, Operation::SetGroupRepeat { expected: None, repeat: None, .. }));
    }
}
