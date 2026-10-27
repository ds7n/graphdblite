//! Approximate resident size of query values and rows, for the per-query
//! materialization budget (`Config::max_materialized_bytes`).
//!
//! The estimate is deliberately simple: every `Value` costs its inline size
//! (96 bytes) plus the heap it owns (string capacity, list and map
//! contents), and every record field adds its key and a fixed bookkeeping
//! allowance. It tracks real memory within a small factor, which is all a
//! guard rail needs, and it never undercounts a row by orders of magnitude
//! the way a row count does.

use crate::cypher::record::NamedRecord;
use crate::cypher::record_v2::Record as SlotRecord;
use crate::types::{Edge, Node, Properties, Value};

/// Inline size of one `Value`.
const VALUE_BYTES: u64 = std::mem::size_of::<Value>() as u64;
/// Allowance for a map entry or record field beyond its key bytes and value:
/// the key `String`'s header and heap block, the hash and the bucket.
const ENTRY_OVERHEAD: u64 = 96;

/// Bytes of a value that owns no nested values, `None` for a container.
fn leaf_bytes(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => Some(VALUE_BYTES + s.capacity() as u64),
        Value::List(_) | Value::Map(_) | Value::Node(_) | Value::Edge(_) | Value::Path(_) => None,
        _ => Some(VALUE_BYTES),
    }
}

/// Account for a child value: leaves are added at once, containers queued.
fn add_child<'a>(total: &mut u64, pending: &mut Vec<&'a Value>, child: &'a Value) {
    match leaf_bytes(child) {
        Some(b) => *total = total.saturating_add(b),
        None => pending.push(child),
    }
}

/// Approximate bytes held by `v`, including its inline size and everything
/// it owns. Iterative, so deeply nested values cannot overflow the stack,
/// and allocation-free unless containers are nested inside containers (this
/// runs for every buffered row).
pub(crate) fn approx_value_bytes(v: &Value) -> u64 {
    if let Some(b) = leaf_bytes(v) {
        return b;
    }
    let mut total = 0u64;
    let mut pending: Vec<&Value> = Vec::new();
    let mut next = Some(v);
    while let Some(v) = next.take().or_else(|| pending.pop()) {
        total = total.saturating_add(VALUE_BYTES);
        match v {
            Value::String(s) => total = total.saturating_add(s.capacity() as u64),
            Value::List(items) => {
                for item in items {
                    add_child(&mut total, &mut pending, item);
                }
            }
            Value::Map(map) => {
                for (k, val) in map {
                    total = total.saturating_add(k.len() as u64 + ENTRY_OVERHEAD);
                    add_child(&mut total, &mut pending, val);
                }
            }
            Value::Node(n) => add_node(&mut total, &mut pending, n),
            Value::Edge(e) => add_edge(&mut total, &mut pending, e),
            Value::Path(p) => {
                for n in &p.nodes {
                    add_node(&mut total, &mut pending, n);
                }
                for e in &p.edges {
                    add_edge(&mut total, &mut pending, e);
                }
            }
            _ => {}
        }
    }
    total
}

fn add_properties<'a>(total: &mut u64, pending: &mut Vec<&'a Value>, props: &'a Properties) {
    for (k, val) in props {
        *total = total.saturating_add(k.len() as u64 + ENTRY_OVERHEAD);
        add_child(total, pending, val);
    }
}

/// Add a node's heap bytes beyond the inline `Value` (labels and
/// properties); property values that are containers are queued.
fn add_node<'a>(total: &mut u64, pending: &mut Vec<&'a Value>, n: &'a Node) {
    for l in &n.labels {
        *total = total.saturating_add(l.capacity() as u64 + 24);
    }
    add_properties(total, pending, &n.properties);
}

fn add_edge<'a>(total: &mut u64, pending: &mut Vec<&'a Value>, e: &'a Edge) {
    *total = total.saturating_add(e.label.capacity() as u64);
    add_properties(total, pending, &e.properties);
}

/// Approximate bytes of a named (flat-key) record.
pub(crate) fn approx_named_bytes(rec: &NamedRecord) -> u64 {
    rec.iter().fold(0u64, |acc, (k, v)| {
        acc.saturating_add(k.len() as u64 + ENTRY_OVERHEAD)
            .saturating_add(approx_value_bytes(v))
    })
}

/// Approximate bytes of a slot record.
pub(crate) fn approx_slot_bytes(rec: &SlotRecord) -> u64 {
    rec.iter()
        .fold(0u64, |acc, v| acc.saturating_add(approx_value_bytes(v)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn scalar_costs_inline_size_only() {
        assert_eq!(approx_value_bytes(&Value::I64(1)), VALUE_BYTES);
        assert_eq!(approx_value_bytes(&Value::Null), VALUE_BYTES);
    }

    #[test]
    fn string_adds_its_capacity() {
        let s = String::with_capacity(100);
        assert_eq!(approx_value_bytes(&Value::String(s)), VALUE_BYTES + 100);
    }

    #[test]
    fn list_and_map_count_their_contents() {
        let list = Value::List(vec![Value::I64(1), Value::I64(2), Value::I64(3)]);
        assert_eq!(approx_value_bytes(&list), 4 * VALUE_BYTES);
        let map = Value::Map(BTreeMap::from([("ab".to_string(), Value::I64(1))]));
        assert_eq!(
            approx_value_bytes(&map),
            2 * VALUE_BYTES + 2 + ENTRY_OVERHEAD
        );
    }

    #[test]
    fn record_adds_key_and_overhead_per_field() {
        let mut rec = NamedRecord::new();
        rec.set("abc".to_string(), Value::I64(1));
        assert_eq!(approx_named_bytes(&rec), 3 + ENTRY_OVERHEAD + VALUE_BYTES);
    }

    #[test]
    fn node_counts_labels_and_property_values() {
        let node = Node {
            id: crate::types::NodeId(1),
            labels: vec![String::with_capacity(5)],
            properties: Properties::from([("ab".to_string(), Value::List(vec![Value::I64(1)]))]),
        };
        // Node value + label (capacity + 24) + property key and overhead
        // + the list value and its one element.
        assert_eq!(
            approx_value_bytes(&Value::Node(node)),
            VALUE_BYTES + 5 + 24 + 2 + ENTRY_OVERHEAD + 2 * VALUE_BYTES
        );
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        let mut v = Value::I64(0);
        for _ in 0..100_000 {
            v = Value::List(vec![v]);
        }
        assert_eq!(approx_value_bytes(&v), 100_001 * VALUE_BYTES);
        while let Value::List(mut items) = v {
            v = items.pop().unwrap();
        }
    }
}
