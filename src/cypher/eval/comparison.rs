//! Value comparison and equality operations for Cypher eval.
//!
//! Three-valued equality (with null propagation), ordering, and type utilities.

use std::borrow::Cow;
use std::cmp::Ordering::{self, Equal};

use crate::types::{ErrorCode, Value};

/// Exact ordering of an Integer against a Float. Converting the integer to
/// f64 would round above 2^53 (making 2^53 + 1 equal to the float 2^53), so
/// compare the float's integral part as an integer instead. `None` for NaN.
pub(super) fn cmp_int_float(i: i64, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    // 2^63 is exactly representable; every i64 is below it.
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if f >= TWO_POW_63 {
        return Some(Ordering::Less);
    }
    if f < -TWO_POW_63 {
        return Some(Ordering::Greater);
    }
    let whole = f.trunc();
    // In range, so the cast is exact.
    match i.cmp(&(whole as i64)) {
        Equal if f > whole => Some(Ordering::Less),
        Equal if f < whole => Some(Ordering::Greater),
        other => Some(other),
    }
}

/// The integer equal to `f`, if `f` is integral and fits an i64.
fn integral_float(f: f64) -> Option<i64> {
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    (f.trunc() == f && (-TWO_POW_63..TWO_POW_63).contains(&f)).then_some(f as i64)
}

/// Whether [`grouping_key`] would change `v`.
fn needs_grouping_norm(v: &Value) -> bool {
    match v {
        Value::F64(f) => integral_float(*f).is_some(),
        Value::List(items) => items.iter().any(needs_grouping_norm),
        Value::Map(m) => m.values().any(needs_grouping_norm),
        _ => false,
    }
}

/// The form of `v` used to group and deduplicate (DISTINCT, grouping keys,
/// `count(DISTINCT ...)`, UNION). Cypher treats `1` and `1.0` as the same
/// value there, so integral floats that fit an i64 become Integers
/// (including `-0.0`), recursively inside lists and maps. Comparing two keys
/// with `Value`'s `Eq`/`Hash` then matches Cypher equivalence; NaN stays a
/// Float and groups with itself.
pub(crate) fn grouping_key(v: &Value) -> Cow<'_, Value> {
    if !needs_grouping_norm(v) {
        return Cow::Borrowed(v);
    }
    Cow::Owned(into_grouping_key(v.clone()))
}

/// Owned form of [`grouping_key`].
pub(crate) fn into_grouping_key(v: Value) -> Value {
    match v {
        Value::F64(f) => integral_float(f).map_or(Value::F64(f), Value::I64),
        Value::List(items) if items.iter().any(needs_grouping_norm) => {
            Value::List(items.into_iter().map(into_grouping_key).collect())
        }
        Value::Map(m) if m.values().any(needs_grouping_norm) => Value::Map(
            m.into_iter()
                .map(|(k, v)| (k, into_grouping_key(v)))
                .collect(),
        ),
        other => other,
    }
}

/// [`grouping_key`] for a row of key values.
pub(crate) fn grouping_key_vec(vals: &[Value]) -> Cow<'_, [Value]> {
    if !vals.iter().any(needs_grouping_norm) {
        return Cow::Borrowed(vals);
    }
    Cow::Owned(vals.iter().cloned().map(into_grouping_key).collect())
}

/// Whether two values are the same for grouping / DISTINCT purposes.
pub(crate) fn grouping_eq(a: &Value, b: &Value) -> bool {
    a == b || *grouping_key(a) == *grouping_key(b)
}

/// Three-valued equality: returns Null if either operand is null.
pub(super) fn values_equal(a: &Value, b: &Value) -> Value {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => Value::Bool(a == b),
        (Value::I64(a), Value::I64(b)) => Value::Bool(a == b),
        (Value::F64(a), Value::F64(b)) => Value::Bool(a == b),
        (Value::I64(a), Value::F64(b)) => Value::Bool(cmp_int_float(*a, *b) == Some(Equal)),
        (Value::F64(a), Value::I64(b)) => Value::Bool(cmp_int_float(*b, *a) == Some(Equal)),
        (Value::String(a), Value::String(b)) => Value::Bool(a == b),
        (Value::List(a), Value::List(b)) => lists_equal(a, b),
        (Value::Map(a), Value::Map(b)) => maps_equal(a, b),
        (Value::Node(a), Value::Node(b)) => Value::Bool(a.id == b.id),
        // Relationships compare by identity, not by (possibly stale) properties.
        (Value::Edge(a), Value::Edge(b)) => Value::Bool(same_edge(a, b)),
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

/// Whether two relationship values denote the same stored relationship:
/// equal `(src, dst, type, seq)` identity. Parallel edges differ in `seq`.
fn same_edge(a: &crate::types::Edge, b: &crate::types::Edge) -> bool {
    a.src == b.src && a.dst == b.dst && a.label == b.label && a.seq == b.seq
}

/// Cypher `=` collapsed to two values: true only when the comparison is
/// true (null or false otherwise). Used where a stored value is matched
/// against a pattern constraint, e.g. var-length relationship property maps.
pub(crate) fn cypher_equals(a: &Value, b: &Value) -> bool {
    values_equal(a, b) == Value::Bool(true)
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
        .all(|(e1, e2)| same_edge(e1, e2));
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
        (Value::I64(a), Value::F64(b)) => cmp_int_float(*a, *b),
        (Value::F64(a), Value::I64(b)) => cmp_int_float(*b, *a).map(Ordering::reverse),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
        (Value::LocalTime(a), Value::LocalTime(b)) => Some(a.cmp(b)),
        (Value::Time(a), Value::Time(b)) => {
            // Compare by converting to UTC.
            Some(a.utc_sort_key().cmp(&b.utc_sort_key()))
        }
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => Some(a.cmp(b)),
        (Value::DateTime(a), Value::DateTime(b)) => Some(a.utc_sort_key().cmp(&b.utc_sort_key())),
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
    crate::types::value_type_name(v)
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
            code: ErrorCode::Other,
            hint: None,
            span: None,
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
