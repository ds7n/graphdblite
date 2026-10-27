//! Aggregation, sort, distinct, skip, limit.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::{eval_expr, expr_to_column_name};
use crate::cypher::record::NamedRecord;
use crate::cypher::record_view::RecordView;
use crate::types::*;

use super::accumulate::AggAccumulator;
use super::util::*;
use super::*;

pub(in crate::cypher::executor) fn exec_aggregate(
    conn: &Connection,
    input: &LogicalOp,
    group_keys: &[Expr],
    aggregates: &[AggregateExpr],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = {
        let _uncapped = UnwindCapSuspended::new();
        exec(conn, input, ctx)?
    };
    aggregate_named_records(conn, &records, group_keys, aggregates)
}

/// Group + aggregate over a pre-materialized record set. Same column-naming
/// rules as [`exec_aggregate`] (uses [`agg_col_name`] / [`expr_to_column_name`]
/// — *not* the divergent naming in [`exec_aggregate_over_records`], which
/// only the correlated path uses).
///
/// Used by [`exec_aggregate`] and by the slot path's `AggregateSlotIter` so
/// both produce identical column headers for dual-run agreement.
pub(crate) fn aggregate_named_records(
    conn: &Connection,
    records: &[NamedRecord],
    group_keys: &[Expr],
    aggregates: &[AggregateExpr],
) -> Result<Vec<NamedRecord>> {
    if group_keys.is_empty() {
        // No grouping — aggregate over all records.
        let views: Vec<&dyn RecordView> = records.iter().map(|r| r as &dyn RecordView).collect();
        let mut rec = NamedRecord::new();
        for agg in aggregates {
            let col_name = agg_col_name(agg);
            let val = compute_aggregate(agg, &views, conn)?;
            rec.set(col_name, val);
        }
        return Ok(vec![rec]);
    }

    // Group by keys: a HashMap from the grouping form of the key values
    // (Cypher equivalence: 1 and 1.0 are one group) to the group's index in
    // `groups`, which keeps first-seen order and the first row's key values.
    let mut group_index: HashMap<Vec<Value>, usize> = HashMap::new();
    let mut groups: Vec<(Vec<Value>, Vec<&NamedRecord>)> = Vec::new();

    for rec in records {
        let key_vals: Vec<Value> = group_keys
            .iter()
            .map(|k| eval_expr(k, rec, crate::cypher::eval::EvalCx::new(conn)))
            .collect::<Result<_>>()?;

        let norm = crate::cypher::eval::grouping_key_vec(&key_vals);
        if let Some(&i) = group_index.get(norm.as_ref()) {
            groups[i].1.push(rec);
        } else {
            group_index.insert(norm.into_owned(), groups.len());
            groups.push((key_vals, vec![rec]));
        }
    }

    let mut results = Vec::new();
    for (key_vals, group_records) in &groups {
        let mut rec = NamedRecord::new();
        for (i, key_expr) in group_keys.iter().enumerate() {
            let col_name = expr_to_column_name(key_expr);
            rec.set(col_name.clone(), key_vals[i].clone());
            // If the group key is a bare variable referring to a node/relationship,
            // propagate its flattened property keys (e.g. `n.name`, `n.__id`) from
            // a representative record so that downstream clauses like
            // `RETURN n.name` continue to work after WITH/aggregation.
            if let ExprKind::Variable(var) = &key_expr.kind {
                let prefix = format!("{var}.");
                if let Some(first) = group_records.first() {
                    for (key, val) in &first.fields {
                        if key.starts_with(&prefix) {
                            rec.set(key.clone(), val.clone());
                        }
                    }
                }
            }
        }
        let group_views: Vec<&dyn RecordView> = group_records
            .iter()
            .map(|&r| r as &dyn RecordView)
            .collect();
        for agg in aggregates {
            let col_name = agg_col_name(agg);
            let val = compute_aggregate(agg, &group_views, conn)?;
            rec.set(col_name, val);
        }
        results.push(rec);
    }

    Ok(results)
}

/// One aggregate over a group's rows: feeds each row to an
/// [`AggAccumulator`], the same incremental state the streaming slot
/// aggregation uses.
pub(in crate::cypher::executor) fn compute_aggregate(
    agg: &AggregateExpr,
    records: &[&dyn RecordView],
    conn: &Connection,
) -> Result<Value> {
    let mut acc = AggAccumulator::new(agg);
    for &rec in records {
        acc.update(rec, conn)?;
    }
    acc.finish(conn)
}

pub(in crate::cypher::executor) fn exec_sort(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::SortItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    // ORDER BY holds its whole input at once, whatever LIMIT follows.
    check_row_limit(&records, ctx)?;
    sort_records(conn, &mut records, items)?;
    Ok(records)
}

/// Stable-sort `records` by the ORDER BY `items`. Sort keys are evaluated
/// once per row up front, so an evaluation error (e.g. division by zero)
/// fails the query instead of sorting as null.
pub(in crate::cypher::executor) fn sort_records(
    conn: &Connection,
    records: &mut Vec<NamedRecord>,
    items: &[crate::cypher::ast::SortItem],
) -> Result<()> {
    let mut keyed: Vec<(Vec<Value>, NamedRecord)> = Vec::with_capacity(records.len());
    for rec in records.drain(..) {
        let keys = items
            .iter()
            .map(|item| eval_expr(&item.expr, &rec, crate::cypher::eval::EvalCx::new(conn)))
            .collect::<Result<Vec<_>>>()?;
        keyed.push((keys, rec));
    }
    keyed.sort_by(|(ka, _), (kb, _)| {
        for (item, (va, vb)) in items.iter().zip(ka.iter().zip(kb)) {
            let ord = compare_values_for_sort(va, vb);
            let ord = if item.descending { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    records.extend(keyed.into_iter().map(|(_, rec)| rec));
    Ok(())
}

pub(in crate::cypher::executor) fn exec_distinct(
    conn: &Connection,
    input: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    Ok(distinct_records(records))
}

/// Drop duplicate rows (all fields equivalent, so 1 and 1.0 match),
/// keeping first occurrences.
pub(in crate::cypher::executor) fn distinct_records(records: Vec<NamedRecord>) -> Vec<NamedRecord> {
    let mut results: Vec<NamedRecord> = Vec::new();
    for rec in records {
        if !results.iter().any(|s| rows_equivalent(s, &rec)) {
            results.push(rec);
        }
    }
    results
}

/// Whether two rows have the same columns with pairwise equivalent values
/// (Cypher grouping equivalence, see `eval::grouping_eq`).
pub(in crate::cypher::executor) fn rows_equivalent(a: &NamedRecord, b: &NamedRecord) -> bool {
    a.fields.len() == b.fields.len()
        && a.fields.iter().all(|(k, av)| {
            b.get(k)
                .is_some_and(|bv| crate::cypher::eval::grouping_eq(av, bv))
        })
}

pub(in crate::cypher::executor) fn exec_skip(
    conn: &Connection,
    input: &LogicalOp,
    count: u64,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    Ok(records.into_iter().skip(count as usize).collect())
}

pub(in crate::cypher::executor) fn exec_limit(
    conn: &Connection,
    input: &LogicalOp,
    count: u64,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    Ok(records.into_iter().take(count as usize).collect())
}

pub(in crate::cypher::executor) fn exec_aggregate_over_records(
    conn: &Connection,
    records: &[NamedRecord],
    group_keys: &[Expr],
    aggregates: &[AggregateExpr],
) -> Result<Vec<NamedRecord>> {
    if group_keys.is_empty() {
        // Global aggregation over all records.
        let views: Vec<&dyn RecordView> = records.iter().map(|r| r as &dyn RecordView).collect();
        let mut result = NamedRecord::new();
        for agg in aggregates {
            let val = compute_aggregate(agg, &views, conn)?;
            let alias = agg
                .alias
                .clone()
                .unwrap_or_else(|| format!("{:?}", agg.function));
            result.set(alias, val);
        }
        return Ok(vec![result]);
    }

    // Group by keys.
    let mut groups: Vec<(Vec<Value>, Vec<NamedRecord>)> = Vec::new();
    for rec in records {
        let key: Vec<Value> = group_keys
            .iter()
            .map(|k| eval_expr(k, rec, crate::cypher::eval::EvalCx::new(conn)))
            .collect::<Result<_>>()?;
        let same_group = |k: &Vec<Value>| {
            k.iter()
                .zip(&key)
                .all(|(a, b)| crate::cypher::eval::grouping_eq(a, b))
        };
        if let Some(group) = groups.iter_mut().find(|(k, _)| same_group(k)) {
            group.1.push(rec.clone());
        } else {
            groups.push((key, vec![rec.clone()]));
        }
    }

    let mut results = Vec::new();
    for (key_vals, group_recs) in &groups {
        let mut result = NamedRecord::new();
        // Set group key columns.
        for (i, k) in group_keys.iter().enumerate() {
            let col = match &k.kind {
                ExprKind::Variable(v) => v.clone(),
                _ => format!("{k:?}"),
            };
            result.set(col.clone(), key_vals[i].clone());
            // Carry forward internal metadata for group keys.
            if let ExprKind::Variable(v) = &k.kind {
                if let Some(first) = group_recs.first() {
                    for (fk, fv) in &first.fields {
                        if fk.starts_with(&format!("{v}.")) {
                            result.set(fk.clone(), fv.clone());
                        }
                    }
                }
            }
        }
        // Compute aggregates.
        let group_views: Vec<&dyn RecordView> =
            group_recs.iter().map(|r| r as &dyn RecordView).collect();
        for agg in aggregates {
            let val = compute_aggregate(agg, &group_views, conn)?;
            let alias = agg
                .alias
                .clone()
                .unwrap_or_else(|| format!("{:?}", agg.function));
            result.set(alias, val);
        }
        results.push(result);
    }
    Ok(results)
}
