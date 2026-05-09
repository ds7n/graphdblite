//! Aggregation, sort, distinct, skip, limit.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::{eval_expr, expr_to_column_name};
use crate::cypher::record::NamedRecord;
use crate::types::*;

use super::util::*;
use super::*;

pub(in crate::cypher::executor) fn exec_aggregate(
    conn: &Connection,
    input: &LogicalOp,
    group_keys: &[Expr],
    aggregates: &[AggregateExpr],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
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
        let mut rec = NamedRecord::new();
        for agg in aggregates {
            let col_name = agg_col_name(agg);
            let val = compute_aggregate(agg, records, conn)?;
            rec.set(col_name, val);
        }
        return Ok(vec![rec]);
    }

    // Group by keys using a HashMap for O(1) group lookup.
    // IndexMap would preserve insertion order, but we use a separate Vec
    // to track key order so we don't need an extra dependency.
    let mut group_map: HashMap<Vec<Value>, Vec<NamedRecord>> = HashMap::new();
    let mut key_order: Vec<Vec<Value>> = Vec::new();

    for rec in records {
        let key_vals: Vec<Value> = group_keys
            .iter()
            .map(|k| eval_expr(k, rec, conn).unwrap_or(Value::Null))
            .collect();

        if let Some(group) = group_map.get_mut(&key_vals) {
            group.push(rec.clone());
        } else {
            key_order.push(key_vals.clone());
            group_map.insert(key_vals, vec![rec.clone()]);
        }
    }

    let mut results = Vec::new();
    for key_vals in &key_order {
        let group_records = &group_map[key_vals];
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
        for agg in aggregates {
            let col_name = agg_col_name(agg);
            let val = compute_aggregate(agg, group_records, conn)?;
            rec.set(col_name, val);
        }
        results.push(rec);
    }

    Ok(results)
}

pub(in crate::cypher::executor) fn compute_aggregate(
    agg: &AggregateExpr,
    records: &[NamedRecord],
    conn: &Connection,
) -> Result<Value> {
    // When DISTINCT is set, deduplicate input values (skip nulls).
    let deduped_records: Vec<NamedRecord>;
    let effective_records = if agg.distinct && !matches!(agg.input.kind, ExprKind::Star) {
        let mut seen: Vec<Value> = Vec::new();
        let mut kept = Vec::new();
        for rec in records {
            let val = eval_expr(&agg.input, rec, conn)?;
            if matches!(val, Value::Null) {
                continue;
            }
            if !seen.contains(&val) {
                seen.push(val);
                kept.push(rec.clone());
            }
        }
        deduped_records = kept;
        &deduped_records
    } else {
        records
    };

    match agg.function {
        AggregateFunction::Count => {
            if matches!(agg.input.kind, ExprKind::Star) {
                Ok(Value::I64(effective_records.len() as i64))
            } else {
                let count = effective_records
                    .iter()
                    .filter(|r| !matches!(eval_expr(&agg.input, r, conn), Ok(Value::Null)))
                    .count();
                Ok(Value::I64(count as i64))
            }
        }
        AggregateFunction::Sum => {
            let mut i64_sum: i64 = 0;
            let mut f64_sum: f64 = 0.0;
            let mut all_integer = true;
            for rec in effective_records {
                match eval_expr(&agg.input, rec, conn)? {
                    Value::I64(n) => {
                        i64_sum = i64_sum.wrapping_add(n);
                        f64_sum += n as f64;
                    }
                    Value::F64(n) => {
                        all_integer = false;
                        f64_sum += n;
                    }
                    _ => {}
                }
            }
            if all_integer {
                Ok(Value::I64(i64_sum))
            } else {
                Ok(Value::F64(f64_sum))
            }
        }
        AggregateFunction::Avg => {
            let mut sum = 0.0f64;
            let mut count = 0;
            for rec in effective_records {
                match eval_expr(&agg.input, rec, conn)? {
                    Value::I64(n) => {
                        sum += n as f64;
                        count += 1;
                    }
                    Value::F64(n) => {
                        sum += n;
                        count += 1;
                    }
                    _ => {}
                }
            }
            if count > 0 {
                Ok(Value::F64(sum / count as f64))
            } else {
                Ok(Value::Null)
            }
        }
        AggregateFunction::Min => {
            let mut min: Option<Value> = None;
            for rec in effective_records {
                let val = eval_expr(&agg.input, rec, conn)?;
                if !matches!(val, Value::Null) {
                    min = Some(match min {
                        None => val,
                        Some(ref current) => {
                            if compare_values_for_sort(&val, current) == std::cmp::Ordering::Less {
                                val
                            } else {
                                current.clone()
                            }
                        }
                    });
                }
            }
            Ok(min.unwrap_or(Value::Null))
        }
        AggregateFunction::Max => {
            let mut max: Option<Value> = None;
            for rec in effective_records {
                let val = eval_expr(&agg.input, rec, conn)?;
                if !matches!(val, Value::Null) {
                    max = Some(match max {
                        None => val,
                        Some(ref current) => {
                            if compare_values_for_sort(current, &val) == std::cmp::Ordering::Less {
                                val
                            } else {
                                current.clone()
                            }
                        }
                    });
                }
            }
            Ok(max.unwrap_or(Value::Null))
        }
        AggregateFunction::Collect => {
            let mut items = Vec::new();
            for rec in effective_records {
                let val = eval_expr(&agg.input, rec, conn)?;
                if !matches!(val, Value::Null) {
                    items.push(val);
                }
            }
            Ok(Value::List(items))
        }
        AggregateFunction::PercentileDisc | AggregateFunction::PercentileCont => {
            // Evaluate the percentile parameter from extra_arg.
            let pct = match &agg.extra_arg {
                Some(pct_expr) => {
                    let empty_rec = NamedRecord::new();
                    let first_rec = effective_records.first().unwrap_or(&empty_rec);
                    match eval_expr(pct_expr, first_rec, conn)? {
                        Value::F64(v) => v,
                        Value::I64(v) => v as f64,
                        other => {
                            return Err(GraphError::argument(
                                crate::types::QueryPhase::Runtime,
                                format!("expected number but got {other:?}"),
                            )
                            .with_code(ErrorCode::NumberOutOfRange));
                        }
                    }
                }
                None => {
                    return Err(GraphError::argument(
                        crate::types::QueryPhase::Runtime,
                        "percentile function requires a second argument".to_string(),
                    )
                    .with_code(ErrorCode::NumberOutOfRange));
                }
            };
            if !(0.0..=1.0).contains(&pct) {
                return Err(GraphError::argument(
                    crate::types::QueryPhase::Runtime,
                    format!("percentile must be between 0.0 and 1.0, got {pct}"),
                )
                .with_code(ErrorCode::NumberOutOfRange));
            }
            // Collect numeric values.
            let mut values: Vec<f64> = Vec::new();
            for rec in effective_records {
                match eval_expr(&agg.input, rec, conn)? {
                    Value::I64(n) => values.push(n as f64),
                    Value::F64(n) => values.push(n),
                    _ => {}
                }
            }
            if values.is_empty() {
                return Ok(Value::Null);
            }
            values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            if matches!(agg.function, AggregateFunction::PercentileDisc) {
                let idx = (pct * (values.len() - 1) as f64).round() as usize;
                Ok(Value::F64(values[idx]))
            } else {
                // PercentileCont: linear interpolation.
                let pos = pct * (values.len() - 1) as f64;
                let lower = pos.floor() as usize;
                let upper = pos.ceil() as usize;
                if lower == upper {
                    Ok(Value::F64(values[lower]))
                } else {
                    let frac = pos - lower as f64;
                    Ok(Value::F64(
                        values[lower] * (1.0 - frac) + values[upper] * frac,
                    ))
                }
            }
        }
        AggregateFunction::StDev | AggregateFunction::StDevP => {
            let mut values: Vec<f64> = Vec::new();
            for rec in effective_records {
                match eval_expr(&agg.input, rec, conn)? {
                    Value::I64(n) => values.push(n as f64),
                    Value::F64(n) => values.push(n),
                    _ => {}
                }
            }
            let n = values.len();
            let is_sample = matches!(agg.function, AggregateFunction::StDev);
            if n == 0 || (is_sample && n < 2) {
                return Ok(Value::F64(0.0));
            }
            let mean = values.iter().sum::<f64>() / n as f64;
            let variance: f64 = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>()
                / if is_sample { (n - 1) as f64 } else { n as f64 };
            Ok(Value::F64(variance.sqrt()))
        }
    }
}

pub(in crate::cypher::executor) fn exec_sort(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::SortItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    records.sort_by(|a, b| {
        for item in items {
            let va = eval_expr(&item.expr, a, conn).unwrap_or(Value::Null);
            let vb = eval_expr(&item.expr, b, conn).unwrap_or(Value::Null);
            let ord = compare_values_for_sort(&va, &vb);
            let ord = if item.descending { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(records)
}

pub(in crate::cypher::executor) fn exec_distinct(
    conn: &Connection,
    input: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut seen = Vec::new();
    let mut results = Vec::new();
    for rec in records {
        if !seen.iter().any(|s: &NamedRecord| s.fields == rec.fields) {
            seen.push(rec.clone());
            results.push(rec);
        }
    }
    Ok(results)
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
        let mut result = NamedRecord::new();
        for agg in aggregates {
            let val = compute_aggregate(agg, records, conn)?;
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
            .map(|k| eval_expr(k, rec, conn).unwrap_or(Value::Null))
            .collect();
        if let Some(group) = groups.iter_mut().find(|(k, _)| k == &key) {
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
        for agg in aggregates {
            let val = compute_aggregate(agg, group_recs, conn)?;
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
