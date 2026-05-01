//! Value comparison and equality operations for Cypher eval.
//!
//! Three-valued equality (with null propagation), ordering, and type utilities.

use crate::types::Value;

/// Three-valued equality: returns Null if either operand is null.
pub(super) fn values_equal(a: &Value, b: &Value) -> Value {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => Value::Bool(a == b),
        (Value::I64(a), Value::I64(b)) => Value::Bool(a == b),
        (Value::F64(a), Value::F64(b)) => Value::Bool(a == b),
        (Value::I64(a), Value::F64(b)) => Value::Bool((*a as f64) == *b),
        (Value::F64(a), Value::I64(b)) => Value::Bool(*a == (*b as f64)),
        (Value::String(a), Value::String(b)) => Value::Bool(a == b),
        (Value::List(a), Value::List(b)) => lists_equal(a, b),
        (Value::Map(a), Value::Map(b)) => maps_equal(a, b),
        (Value::Node(a), Value::Node(b)) => Value::Bool(a.id == b.id),
        (Value::Edge(a), Value::Edge(b)) => Value::Bool(a == b),
        (Value::Date(a), Value::Date(b)) => Value::Bool(a == b),
        (Value::LocalTime(a), Value::LocalTime(b)) => Value::Bool(a == b),
        (Value::Time(a), Value::Time(b)) => Value::Bool(a == b),
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => Value::Bool(a == b),
        (Value::DateTime(a), Value::DateTime(b)) => Value::Bool(a == b),
        (Value::Duration(a), Value::Duration(b)) => Value::Bool(a == b),
        (Value::Path(a), Value::Path(b)) => paths_equal(a, b),
        _ => Value::Bool(false),
    }
}

/// Path equality: same sequence of node IDs and edge identities (src, dst, type, seq).
fn paths_equal(a: &crate::types::PathValue, b: &crate::types::PathValue) -> Value {
    if a.nodes.len() != b.nodes.len() || a.edges.len() != b.edges.len() {
        return Value::Bool(false);
    }
    let nodes_eq = a.nodes.iter().zip(&b.nodes).all(|(n1, n2)| n1.id == n2.id);
    let edges_eq = a
        .edges
        .iter()
        .zip(&b.edges)
        .all(|(e1, e2)| e1.src == e2.src && e1.dst == e2.dst && e1.label == e2.label);
    Value::Bool(nodes_eq && edges_eq)
}

/// Three-valued list equality: propagates null if any element comparison yields null.
fn lists_equal(a: &[Value], b: &[Value]) -> Value {
    if a.len() != b.len() {
        return Value::Bool(false);
    }
    let mut has_null = false;
    for (ae, be) in a.iter().zip(b.iter()) {
        match values_equal(ae, be) {
            Value::Bool(false) => return Value::Bool(false),
            Value::Null => has_null = true,
            _ => {} // true, continue
        }
    }
    if has_null {
        Value::Null
    } else {
        Value::Bool(true)
    }
}

/// Three-valued map equality: propagates null if values with matching keys compare as null.
pub(super) fn maps_equal(
    a: &std::collections::BTreeMap<String, Value>,
    b: &std::collections::BTreeMap<String, Value>,
) -> Value {
    // Maps with different key sets are not equal (keys with null values still count).
    let a_keys: std::collections::BTreeSet<&String> = a.keys().collect();
    let b_keys: std::collections::BTreeSet<&String> = b.keys().collect();
    if a_keys != b_keys {
        return Value::Bool(false);
    }
    let mut has_null = false;
    for key in &a_keys {
        let av = a.get(*key).unwrap();
        let bv = b.get(*key).unwrap();
        match values_equal(av, bv) {
            Value::Bool(false) => return Value::Bool(false),
            Value::Null => has_null = true,
            _ => {}
        }
    }
    if has_null {
        Value::Null
    } else {
        Value::Bool(true)
    }
}

/// Compare two values, returning Null if either is null or types are incomparable.
pub(super) fn compare_to_value(
    a: &Value,
    b: &Value,
    pred: impl Fn(std::cmp::Ordering) -> bool,
) -> Value {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    match compare_values(a, b) {
        Some(ord) => Value::Bool(pred(ord)),
        None => {
            // NaN comparisons with numeric types yield false (not null).
            let both_numeric = is_numeric(a) && is_numeric(b);
            if both_numeric {
                Value::Bool(false) // NaN involved
            } else {
                Value::Null // cross-type → null
            }
        }
    }
}

/// Check if a value is a numeric type (integer or float).
pub(super) fn is_numeric(v: &Value) -> bool {
    matches!(v, Value::I64(_) | Value::F64(_))
}

/// Compare two values for ordering. Returns None for incomparable types.
pub(super) fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::I64(a), Value::I64(b)) => Some(a.cmp(b)),
        (Value::F64(a), Value::F64(b)) => a.partial_cmp(b),
        (Value::I64(a), Value::F64(b)) => (*a as f64).partial_cmp(b),
        (Value::F64(a), Value::I64(b)) => a.partial_cmp(&(*b as f64)),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Some(a.0.cmp(&b.0)),
        (Value::LocalTime(a), Value::LocalTime(b)) => Some(a.0.cmp(&b.0)),
        (Value::Time(a), Value::Time(b)) => {
            // Compare by converting to UTC.
            let a_utc = a.0 - a.1;
            let b_utc = b.0 - b.1;
            Some(a_utc.cmp(&b_utc))
        }
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => Some(a.0.cmp(&b.0)),
        (Value::DateTime(a), Value::DateTime(b)) => {
            let a_utc = a.0 - a.1;
            let b_utc = b.0 - b.1;
            Some(a_utc.cmp(&b_utc))
        }
        // List comparison: lexicographic ordering.
        (Value::List(a), Value::List(b)) => {
            for (ae, be) in a.iter().zip(b.iter()) {
                match compare_values(ae, be) {
                    Some(std::cmp::Ordering::Equal) => continue,
                    other => return other,
                }
            }
            Some(a.len().cmp(&b.len()))
        }
        // Duration is NOT orderable per Cypher spec.
        _ => None,
    }
}

/// Get a human-readable type name for a value.
pub(super) fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "Boolean",
        Value::I64(_) => "Integer",
        Value::F64(_) => "Float",
        Value::String(_) => "String",
        Value::List(_) => "List",
        Value::Map(_) => "Map",
        Value::Null => "Null",
        Value::Node(_) => "Node",
        Value::Edge(_) => "Relationship",
        Value::Path(_) => "Path",
        Value::Date(_) => "Date",
        Value::LocalTime(_) => "LocalTime",
        Value::Time(_) => "Time",
        Value::LocalDateTime(_) => "LocalDateTime",
        Value::DateTime(_) => "DateTime",
        Value::Duration(_) => "Duration",
    }
}

/// Convert a tribool Value to Option<bool>.
pub(super) fn to_tribool(v: &Value) -> crate::types::Result<Option<bool>> {
    use crate::types::{GraphError, QueryError, QueryPhase};
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        _ => Err(GraphError::Query(QueryError::SyntaxError {
            phase: QueryPhase::Runtime,
            message: format!(
                "Type mismatch: expected Boolean but was {}",
                value_type_name(v)
            ),
        })),
    }
}

/// Convert a LiteralValue AST node to a runtime Value.
pub(super) fn literal_to_value(lit: &crate::cypher::ast::LiteralValue) -> Value {
    use crate::cypher::ast::LiteralValue;
    match lit {
        LiteralValue::Null => Value::Null,
        LiteralValue::Bool(b) => Value::Bool(*b),
        LiteralValue::I64(n) => Value::I64(*n),
        LiteralValue::F64(n) => Value::F64(*n),
        LiteralValue::String(s) => Value::String(s.clone()),
    }
}
