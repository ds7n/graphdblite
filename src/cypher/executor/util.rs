//! Shared helpers — value type ranking, sort comparison, agg name/column mapping, public correlated wrappers, node→record helpers.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::{eval_predicate, expr_to_column_name};
use crate::cypher::record::NamedRecord;
use crate::types::*;
use crate::{edge, index, node};

use super::*;

pub(crate) fn literal_to_value(lit: &LiteralValue) -> Value {
    match lit {
        LiteralValue::Null => Value::Null,
        LiteralValue::Bool(b) => Value::Bool(*b),
        LiteralValue::I64(n) => Value::I64(*n),
        LiteralValue::F64(n) => Value::F64(*n),
        LiteralValue::String(s) => Value::String(s.clone()),
    }
}

/// Cypher type ordering rank for cross-type comparisons.
/// Order: Map < Node < Relationship < Path < List < String < Bool < Number < Null
pub(in crate::cypher::executor) fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Map(_) => 0,
        Value::Node(_) => 1,
        Value::Edge(_) => 2,
        Value::List(_) => 3,
        Value::Path(_) => 4,
        Value::String(_) => 5,
        Value::Bool(_) => 6,
        Value::I64(_) | Value::F64(_) => 7,
        Value::Null => 8,
        // Temporal types grouped after numbers.
        Value::Date(_)
        | Value::LocalTime(_)
        | Value::Time(_)
        | Value::LocalDateTime(_)
        | Value::DateTime(_)
        | Value::Duration(_) => 7,
    }
}

pub(in crate::cypher::executor) fn compare_values_for_sort(
    a: &Value,
    b: &Value,
) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::I64(a), Value::I64(b)) => a.cmp(b),
        (Value::F64(a), Value::F64(b)) => match (a.is_nan(), b.is_nan()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater, // NaN sorts after numbers
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
        },
        (Value::I64(_), Value::F64(b)) if b.is_nan() => std::cmp::Ordering::Less,
        (Value::F64(a), Value::I64(_)) if a.is_nan() => std::cmp::Ordering::Greater,
        (Value::I64(a), Value::F64(b)) => (*a as f64)
            .partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::F64(a), Value::I64(b)) => a
            .partial_cmp(&(*b as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::List(a), Value::List(b)) => {
            for (x, y) in a.iter().zip(b.iter()) {
                let ord = compare_values_for_sort(x, y);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            a.len().cmp(&b.len())
        }
        // Temporal types.
        (Value::Date(a), Value::Date(b)) => a.0.cmp(&b.0),
        (Value::LocalTime(a), Value::LocalTime(b)) => a.0.cmp(&b.0),
        (Value::Time(a), Value::Time(b)) => {
            let a_utc = a.0 - a.1;
            let b_utc = b.0 - b.1;
            a_utc.cmp(&b_utc)
        }
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => a.0.cmp(&b.0),
        (Value::DateTime(a), Value::DateTime(b)) => {
            let a_utc = a.0 - a.1;
            let b_utc = b.0 - b.1;
            a_utc.cmp(&b_utc)
        }
        (Value::Duration(a), Value::Duration(b)) => {
            // Duration ordering: months first, then days, then seconds, then nanos.
            a.months
                .cmp(&b.months)
                .then(a.days.cmp(&b.days))
                .then(a.seconds.cmp(&b.seconds))
                .then(a.nanos.cmp(&b.nanos))
        }
        // Different types: compare by type rank.
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

/// Check whether any record produced by `plan` matches the given correlated
/// bindings.  Short-circuits on the first hit instead of materializing the
/// entire result set, which is the key optimisation for EXISTS subqueries.
///
/// Execute a plan as a correlated subquery, pushing the outer record's
/// bindings down so inner scans and filters can see them. Returns true
/// if at least one row is produced.
///
/// Strips the top-level projection/sort/limit layers since EXISTS only
/// cares about row existence, not projected values.
pub fn exec_correlated_exists(
    conn: &Connection,
    plan: &LogicalOp,
    outer: &NamedRecord,
) -> Result<bool> {
    let rows = exec_correlated(conn, plan, outer, &ExecContext::default())?;
    Ok(!rows.is_empty())
}

/// Execute a correlated subquery and return all matching rows.
/// Used by pattern comprehensions to collect all matches.
pub fn exec_correlated_subquery(
    conn: &Connection,
    plan: &LogicalOp,
    outer: &NamedRecord,
) -> Result<Vec<NamedRecord>> {
    exec_correlated(conn, plan, outer, &ExecContext::default())
}

/// For leaf/pipeline operators we iterate one record at a time. For operators
/// we don't special-case we fall back to `execute()` + linear scan.
pub fn execute_first_match(
    conn: &Connection,
    plan: &LogicalOp,
    correlated_bindings: &[(String, Value)],
) -> Result<bool> {
    match plan {
        LogicalOp::Scan { label, alias } => {
            let nodes = node::find_nodes_by_label(conn, label)?;
            for n in nodes {
                let rec = node_to_record(&n, alias);
                if record_matches_bindings(&rec, correlated_bindings) {
                    return Ok(true);
                }
            }
            Ok(false)
        }

        LogicalOp::IndexLookup {
            label,
            alias,
            property,
            value,
            remaining_filters,
        } => {
            let lookup_value = literal_to_value(value);
            let node_ids = index::index_lookup(conn, label, property, &lookup_value)?;
            for id in node_ids {
                let n = node::get_node(conn, id)?;
                let rec = node_to_record(&n, alias);
                if let Some(filter) = remaining_filters {
                    if !eval_predicate(filter, &rec, conn)? {
                        continue;
                    }
                }
                if record_matches_bindings(&rec, correlated_bindings) {
                    return Ok(true);
                }
            }
            Ok(false)
        }

        LogicalOp::Filter { input, predicate } => {
            // For filter over a scannable input, iterate the input one record
            // at a time, applying predicate + bindings check.
            let records = exec(conn, input, &ExecContext::default())?;
            for rec in records {
                if eval_predicate(predicate, &rec, conn)?
                    && record_matches_bindings(&rec, correlated_bindings)
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }

        LogicalOp::Expand {
            input,
            src_alias,
            dst_alias,
            edge_types,
            direction,
            min_hops,
            max_hops,
            ..
        } => {
            let input_records = exec(conn, input, &ExecContext::default())?;
            for rec in &input_records {
                let src_id = match rec.get(src_alias).and_then(value_to_node_id) {
                    Some(id) => id,
                    _ => continue,
                };
                // Discover edge labels when edge_types is empty (match any type).
                let _owned: Vec<String>;
                let labels: Vec<&str> = if edge_types.is_empty() {
                    let all = edge::get_all_edge_labels(conn, src_id, *direction)?;
                    _owned = all.into_iter().map(|(l, _)| l).collect();
                    _owned.iter().map(|s| s.as_str()).collect()
                } else {
                    edge_types.iter().map(|s| s.as_str()).collect()
                };
                let mut dst_ids = Vec::new();
                for label in &labels {
                    if *min_hops == 1 && *max_hops == 1 {
                        dst_ids.extend(edge::get_neighbors(conn, src_id, label, *direction)?);
                    } else {
                        dst_ids.extend(edge::traverse(
                            conn, src_id, label, *direction, *min_hops, *max_hops, None,
                        )?);
                    }
                }
                for dst_id in dst_ids {
                    let mut new_rec = rec.clone();
                    fetch_and_populate(conn, &mut new_rec, dst_id, dst_alias)?;
                    if record_matches_bindings(&new_rec, correlated_bindings) {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        }

        // Fallback: materialize and scan.
        _ => {
            let results = execute(conn, plan)?;
            for rec in &results {
                if record_matches_bindings(rec, correlated_bindings) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// Build a `NamedRecord` from a `Node`, keyed under the given alias.
///
/// Note: NodeId (u64) is transmitted as i64. This wraps for IDs above
/// i64::MAX (~9.2e18), which is practically unreachable — sequential IDs
/// would take thousands of years at millions of inserts per second.
/// Crate-internal alias so `iter_slot::UnwindSlotIter` can populate
/// flat node bindings the same way `exec_unwind` does, without
/// duplicating the field list.
pub(crate) fn node_to_record_pub(n: &crate::types::Node, alias: &str) -> NamedRecord {
    node_to_record(n, alias)
}

pub(crate) fn node_to_record(n: &crate::types::Node, alias: &str) -> NamedRecord {
    debug_assert!(n.id.0 <= i64::MAX as u64, "NodeId exceeds i64::MAX");
    let mut rec = NamedRecord::new();
    populate_node_bindings(&mut rec, n, alias);
    rec
}

/// Write the standard set of node bindings (`alias`, `alias.<prop>`,
/// `alias.__id`, `alias.__label`, `alias.__labels`) into `rec`. Used both by
/// `node_to_record` (fresh record) and by Expand-style operators that merge
/// new bindings into an existing record.
pub(crate) fn populate_node_bindings(rec: &mut NamedRecord, n: &crate::types::Node, alias: &str) {
    rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
    for (key, val) in &n.properties {
        rec.set(format!("{alias}.{key}"), val.clone());
    }
    // Store the colon-joined labels for label matching in filters.
    rec.set(
        format!("{alias}.__label"),
        Value::String(n.labels.join(":")),
    );
    // Store individual labels as a list for multi-label filtering.
    rec.set(
        format!("{alias}.__labels"),
        Value::List(n.labels.iter().map(|l| Value::String(l.clone())).collect()),
    );
    rec.set(format!("{alias}.__id"), Value::I64(n.id.0 as i64));
}

/// Fetch a node by ID and populate `rec` with its bindings under `alias`.
/// Returns the fetched Node so callers can inspect it (e.g. to read labels
/// or build edge bindings).
pub(crate) fn fetch_and_populate(
    conn: &Connection,
    rec: &mut NamedRecord,
    id: NodeId,
    alias: &str,
) -> Result<crate::types::Node> {
    let n = node::get_node(conn, id)?;
    populate_node_bindings(rec, &n, alias);
    Ok(n)
}

/// Check whether a record satisfies correlated bindings from an outer scope.
///
/// A binding matches if the record either doesn't contain the key (no
/// constraint) or contains it with an equal value.
pub(in crate::cypher::executor) fn record_matches_bindings(
    rec: &NamedRecord,
    bindings: &[(String, Value)],
) -> bool {
    bindings.iter().all(|(key, outer_val)| match rec.get(key) {
        Some(inner_val) => inner_val == outer_val,
        None => true,
    })
}

pub(in crate::cypher::executor) fn agg_fn_name(f: AggregateFunction) -> &'static str {
    match f {
        AggregateFunction::Count => "count",
        AggregateFunction::Sum => "sum",
        AggregateFunction::Avg => "avg",
        AggregateFunction::Min => "min",
        AggregateFunction::Max => "max",
        AggregateFunction::Collect => "collect",
        AggregateFunction::PercentileDisc => "percentileDisc",
        AggregateFunction::PercentileCont => "percentileCont",
        AggregateFunction::StDev => "stDev",
        AggregateFunction::StDevP => "stDevP",
    }
}

/// Compute the column name for an aggregate expression, matching what
/// `expr_to_column_name` produces for the original `FunctionCall` expression.
pub(in crate::cypher::executor) fn agg_col_name(agg: &AggregateExpr) -> String {
    agg.alias.clone().unwrap_or_else(|| {
        if let Some(ref text) = agg.original_call_text {
            return text.clone();
        }
        let name = if agg.original_name.is_empty() {
            agg_fn_name(agg.function).to_string()
        } else {
            agg.original_name.clone()
        };
        let expr =
            crate::cypher::ast::Expr::synthetic(crate::cypher::ast::ExprKind::FunctionCall {
                name,
                args: vec![agg.input.clone()],
                distinct: agg.distinct,
                original_text: None,
            });
        expr_to_column_name(&expr)
    })
}
