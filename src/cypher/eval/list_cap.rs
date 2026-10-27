//! The aggregate list-size cap: one limit on how many values a list built
//! during a query may hold, counting the contents of nested lists and maps.
//!
//! `range()` alone was capped, but composing legal pieces was not:
//! `range(1, 10000000) + range(1, 10000000) + ...` or a comprehension that
//! maps every element to another list could allocate without bound and
//! abort the process. Every place that builds a list from other values
//! (list literals, `+`, list and pattern comprehensions, `collect()`,
//! `range()`) checks this cap before or while allocating.

use crate::types::{GraphError, Result, Value};

/// Maximum number of values one list may hold, counting the elements of
/// nested lists and the values of nested maps: `[range(1, n), range(1, n)]`
/// holds `2 + 2n`.
pub(crate) const MAX_LIST_LEN: usize = 10_000_000;

/// Number of values held beneath `v`: list elements and map values,
/// recursively (a scalar holds 0). Iterative, so arbitrarily nested values
/// cannot overflow the stack, and it stops counting once past
/// [`MAX_LIST_LEN`], so the cost is bounded by the cap.
pub(crate) fn nested_value_count(v: &Value) -> usize {
    let mut count = 0usize;
    let mut pending: Vec<&Value> = vec![v];
    while let Some(v) = pending.pop() {
        match v {
            Value::List(items) => {
                count += items.len();
                pending.extend(items.iter().filter(|i| is_container(i)));
            }
            Value::Map(map) => {
                count += map.len();
                pending.extend(map.values().filter(|i| is_container(i)));
            }
            _ => {}
        }
        if count > MAX_LIST_LEN {
            break;
        }
    }
    count
}

fn is_container(v: &Value) -> bool {
    matches!(v, Value::List(_) | Value::Map(_))
}

/// Error unless a list holding `total` values fits under [`MAX_LIST_LEN`].
pub(crate) fn check_list_len(total: usize) -> Result<()> {
    if total > MAX_LIST_LEN {
        return Err(GraphError::SizeLimit {
            what: "list size".to_string(),
            limit: MAX_LIST_LEN,
            actual: total,
            hint: Some("build smaller lists, or aggregate with UNWIND / LIMIT".to_string()),
        });
    }
    // A list of this many values must also fit what is left of the query's
    // materialization budget, so lists cannot grow without bound inside one
    // query even when each stays under the per-list cap.
    crate::cypher::executor::check_materialized_headroom(
        (total as u64).saturating_mul(std::mem::size_of::<Value>() as u64),
    )
}

/// Running size of a list being built element by element.
#[derive(Default)]
pub(crate) struct ListBudget(usize);

impl ListBudget {
    /// Account for one more element `v` (itself plus everything nested in
    /// it), failing once the list would exceed [`MAX_LIST_LEN`].
    pub(crate) fn add(&mut self, v: &Value) -> Result<()> {
        self.0 = self.0.saturating_add(1 + nested_value_count(v));
        check_list_len(self.0)
    }
}

/// Size of a list with these elements: one per element plus everything
/// nested in them. Callers check it with [`check_list_len`] before
/// allocating a combined list (e.g. for `a + b`).
pub(crate) fn elements_size(items: &[Value]) -> usize {
    items
        .iter()
        .map(|i| 1 + nested_value_count(i))
        .fold(0usize, usize::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn ints(n: i64) -> Value {
        Value::List((0..n).map(Value::I64).collect())
    }

    #[test]
    fn nested_value_count_counts_list_and_map_contents() {
        assert_eq!(nested_value_count(&Value::I64(7)), 0);
        assert_eq!(nested_value_count(&ints(3)), 3);
        // [[0, 1], [0, 1, 2], 5] = 3 elements + 2 + 3 nested.
        let nested = Value::List(vec![ints(2), ints(3), Value::I64(5)]);
        assert_eq!(nested_value_count(&nested), 8);
        // A map counts its values and what they hold.
        let map = Value::Map(BTreeMap::from([
            ("a".to_string(), ints(4)),
            ("b".to_string(), Value::Bool(true)),
        ]));
        assert_eq!(nested_value_count(&map), 6);
        assert_eq!(nested_value_count(&Value::List(vec![map])), 7);
    }

    #[test]
    fn nested_value_count_handles_deep_nesting_iteratively() {
        let mut v = Value::I64(0);
        for _ in 0..100_000 {
            v = Value::List(vec![v]);
        }
        assert_eq!(nested_value_count(&v), 100_000);
        // Drop iteratively too, so the test itself cannot overflow.
        while let Value::List(mut items) = v {
            v = items.pop().unwrap();
        }
    }

    #[test]
    fn check_list_len_boundaries() {
        assert!(check_list_len(0).is_ok());
        assert!(check_list_len(MAX_LIST_LEN).is_ok());
        match check_list_len(MAX_LIST_LEN + 1) {
            Err(GraphError::SizeLimit {
                what,
                limit,
                actual,
                ..
            }) => {
                assert_eq!(what, "list size");
                assert_eq!(limit, MAX_LIST_LEN);
                assert_eq!(actual, MAX_LIST_LEN + 1);
            }
            other => panic!("expected SizeLimit, got {other:?}"),
        }
    }

    #[test]
    fn list_budget_counts_each_element_and_its_contents() {
        let mut budget = ListBudget::default();
        budget.add(&Value::I64(1)).unwrap();
        budget.add(&ints(3)).unwrap();
        assert_eq!(budget.0, 1 + 4);
        // One element short of the cap is fine; the element that crosses
        // it fails.
        budget.0 = MAX_LIST_LEN - 1;
        budget.add(&Value::Null).unwrap();
        match budget.add(&Value::Null) {
            Err(GraphError::SizeLimit { actual, .. }) => assert_eq!(actual, MAX_LIST_LEN + 1),
            other => panic!("expected SizeLimit, got {other:?}"),
        }
    }

    #[test]
    fn elements_size_counts_elements_and_their_contents() {
        assert_eq!(elements_size(&[]), 0);
        // [[0, 1], 1] = 2 elements + 2 nested.
        assert_eq!(elements_size(&[ints(2), Value::I64(1)]), 4);
    }
}
