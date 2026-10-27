//! Correlated execution: subqueries, EXISTS, multi-MATCH joins, OPTIONAL MATCH.
//!
//! Extracted from `executor::mod` to keep the dispatcher file manageable.

use rusqlite::Connection;

use crate::cypher::ast::{Expr, ExprKind};
use crate::cypher::eval::{eval_predicate, expr_to_column_name};
use crate::cypher::ir::*;
use crate::cypher::record::NamedRecord;
use crate::node;
use crate::types::{NodeId, Result, Value};

use super::read::exec_fulltext_lookup;
use super::{
    build_compound_binding, charge_named_rows, check_row_limit, compound_binding_vars,
    dedup_union_rows, distinct_records, exec, exec_aggregate_over_records, exec_id_lookup,
    exec_index_lookup, exec_scan, expand_all, extend_with_shortest_paths,
    has_duplicate_relationships, materialize_paths, materialized_mark, merge_outer, node_to_record,
    push_capped, reset_materialized, sort_records, unwind_records, AliasBinding, ExecContext,
    TransientRows, UnwindCapSuspended,
};

/// Correlated inner join: for each left record, execute the right side with
/// correlated bindings. Only emit combined rows; drop left rows with no match.
pub(super) fn exec_correlated_join(
    conn: &Connection,
    input: &LogicalOp,
    right: &LogicalOp,
    same_match: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let left_records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for l_rec in &left_records {
        let right_records = exec_correlated(conn, right, l_rec, ctx)?;
        let _transient = TransientRows::of(&right_records);
        // Inner join: only emit if right produced results.
        for r_rec in &right_records {
            let mut combined = l_rec.clone();
            for (key, val) in &r_rec.fields {
                if !combined.fields.contains_key(key) {
                    combined.set(key.clone(), val.clone());
                }
            }
            // Cross-pattern relationship uniqueness (only within the same
            // MATCH clause — separate MATCHes have independent scopes).
            if same_match && has_duplicate_relationships(&combined) {
                continue;
            }
            push_capped(&mut results, combined, ctx)?;
        }
        // If right_records is empty, left row is dropped (inner join semantics).
    }

    Ok(results)
}

pub(super) fn exec_left_outer_join(
    conn: &Connection,
    input: &LogicalOp,
    right: &LogicalOp,
    optional_aliases: &[String],
    opt_filter: Option<&Expr>,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let left_records = exec(conn, input, ctx)?;
    left_outer_join_records(
        conn,
        left_records,
        right,
        optional_aliases,
        opt_filter,
        &NamedRecord::new(),
        ctx,
    )
}

/// OPTIONAL MATCH over already executed left rows: for each left row run
/// `right` correlated with it (and with `outer`, the enclosing scope, empty
/// at top level), null-filling `optional_aliases` when nothing matches.
fn left_outer_join_records(
    conn: &Connection,
    left_records: Vec<NamedRecord>,
    right: &LogicalOp,
    optional_aliases: &[String],
    opt_filter: Option<&Expr>,
    outer: &NamedRecord,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut results = Vec::new();

    for l_rec in &left_records {
        // Execute the right side with correlated bindings from this left
        // record (over the enclosing scope, when there is one).
        let scope = if outer.is_empty() {
            l_rec.clone()
        } else {
            merge_outer(outer, l_rec)
        };
        let right_records = exec_correlated(conn, right, &scope, ctx)?;
        let _transient = TransientRows::of(&right_records);

        // Merge each right record into the left record. If there's a WHERE
        // clause on the OPTIONAL MATCH, rows that fail it are discarded.
        let mut any_passed = false;
        for r_rec in &right_records {
            let mut combined = l_rec.clone();
            for (key, val) in &r_rec.fields {
                if !combined.fields.contains_key(key) {
                    combined.set(key.clone(), val.clone());
                }
            }
            if let Some(filter) = opt_filter {
                let passes = if outer.is_empty() {
                    eval_predicate(filter, &combined, crate::cypher::eval::EvalCx::new(conn))?
                } else {
                    let merged = merge_outer(outer, &combined);
                    eval_predicate(filter, &merged, crate::cypher::eval::EvalCx::new(conn))?
                };
                if !passes {
                    continue;
                }
            }
            any_passed = true;
            push_capped(&mut results, combined, ctx)?;
        }
        // No match, or no right row passed the filter: emit the left record
        // with NULLs for optional aliases and their flattened metadata keys
        // so downstream operators (property access, labels(), type(), etc.)
        // see Null properly.
        if !any_passed {
            let mut rec = l_rec.clone();
            for alias in optional_aliases {
                rec.set(alias.clone(), Value::Null);
                for suffix in &["__id", "__labels", "__src", "__dst", "__type"] {
                    rec.set(format!("{alias}.{suffix}"), Value::Null);
                }
            }
            push_capped(&mut results, rec, ctx)?;
        }
    }

    Ok(results)
}

/// Execute a plan with correlated bindings from an outer record.
///
/// Extract a node ID from a record value — handles both `Value::I64` (flat
/// binding from MATCH) and `Value::Node` (compound binding from WITH/RETURN).
pub(super) fn value_to_node_id(val: &Value) -> Option<NodeId> {
    match val {
        Value::I64(id) => Some(NodeId(*id as u64)),
        Value::Node(n) => Some(n.id),
        _ => None,
    }
}

/// Execute `plan` with correlated bindings from `outer`, the row of the
/// enclosing scope (left side of a join, the row an EXISTS / pattern
/// predicate / pattern comprehension is evaluated for).
///
/// Leaf operators whose alias is already bound in `outer` return just that
/// node (if it still satisfies the operator), turning O(N*M) joins into
/// O(N) lookups; expressions see `outer` merged with the current row.
/// Every operator reuses the materialized executor's implementation
/// (`expand_all`, `exec_index_lookup`, `materialize_paths`, ...), so the two
/// engines cannot drift apart.
pub(super) fn exec_correlated(
    conn: &Connection,
    plan: &LogicalOp,
    outer: &NamedRecord,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    // Same accounting as `exec`: only the output stays charged once the
    // operator (and its transient inputs) is done.
    let mark = materialized_mark();
    let rows = exec_correlated_op(conn, plan, outer, ctx)?;
    reset_materialized(mark);
    charge_named_rows(&rows)?;
    Ok(rows)
}

fn exec_correlated_op(
    conn: &Connection,
    plan: &LogicalOp,
    outer: &NamedRecord,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    match plan {
        LogicalOp::Scan { label, alias } => match AliasBinding::of(outer, alias) {
            AliasBinding::Null => Ok(vec![]),
            AliasBinding::Free => exec_scan(conn, label, alias, ctx),
            AliasBinding::Node(node_id) => {
                let node = node::get_node(conn, node_id)?;
                // Verify label matches if the scan has a label filter.
                if !label.is_empty() && !node.labels.contains(label) {
                    return Ok(vec![]);
                }
                Ok(vec![node_to_record(&node, alias)])
            }
        },

        LogicalOp::IdLookup { alias, value_expr } => match AliasBinding::of(outer, alias) {
            AliasBinding::Null => Ok(vec![]),
            bound => exec_id_lookup(conn, alias, value_expr, outer, bound.node()),
        },

        LogicalOp::FullTextLookup {
            label,
            alias,
            property,
            op,
            term,
            case_insensitive,
            remaining_filters,
        } => match AliasBinding::of(outer, alias) {
            AliasBinding::Null => Ok(vec![]),
            // `outer` lets the term and residual filter resolve variables
            // from the enclosing scope (e.g. an UNWIND'd value).
            bound => exec_fulltext_lookup(
                conn,
                label,
                alias,
                property,
                *op,
                *case_insensitive,
                term,
                remaining_filters.as_ref(),
                outer,
                bound.node(),
            ),
        },

        LogicalOp::IndexLookup {
            label,
            alias,
            index_properties,
            lookups,
            remaining_filters,
        } => match AliasBinding::of(outer, alias) {
            AliasBinding::Null => Ok(vec![]),
            bound => exec_index_lookup(
                conn,
                label,
                alias,
                index_properties,
                lookups,
                remaining_filters.as_ref(),
                outer,
                bound.node(),
            ),
        },

        LogicalOp::Expand { input, .. } => {
            let input_records = exec_correlated(conn, input, outer, ctx)?;
            expand_all(conn, plan, &input_records, outer, ctx)
        }

        LogicalOp::Filter { input, predicate } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();
            for rec in records {
                // Merge outer bindings into the record so nested EXISTS
                // predicates can see variables from enclosing scopes.
                let merged = merge_outer(outer, &rec);
                if eval_predicate(predicate, &merged, crate::cypher::eval::EvalCx::new(conn))? {
                    results.push(rec);
                }
            }
            Ok(results)
        }

        LogicalOp::CrossProduct {
            left,
            right,
            same_match,
        } => {
            let left_records = exec_correlated(conn, left, outer, ctx)?;
            let mut results = Vec::new();
            for l in &left_records {
                // Merge left record with outer so the right side sees both.
                let merged = merge_outer(outer, l);
                let right_records = exec_correlated(conn, right, &merged, ctx)?;
                let _transient = TransientRows::of(&right_records);
                for r in &right_records {
                    let mut combined = l.clone();
                    for (key, val) in &r.fields {
                        combined.set(key.clone(), val.clone());
                    }
                    if *same_match && has_duplicate_relationships(&combined) {
                        continue;
                    }
                    push_capped(&mut results, combined, ctx)?;
                }
            }
            Ok(results)
        }

        LogicalOp::CorrelatedJoin {
            input,
            right,
            same_match,
        } => {
            let left_records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();
            for l in &left_records {
                // Merge outer into left so the right side sees both scopes.
                let merged = merge_outer(outer, l);
                let right_records = exec_correlated(conn, right, &merged, ctx)?;
                let _transient = TransientRows::of(&right_records);
                for r in &right_records {
                    let mut combined = l.clone();
                    for (key, val) in &r.fields {
                        if !combined.fields.contains_key(key) {
                            combined.set(key.clone(), val.clone());
                        }
                    }
                    if *same_match && has_duplicate_relationships(&combined) {
                        continue;
                    }
                    push_capped(&mut results, combined, ctx)?;
                }
            }
            Ok(results)
        }

        LogicalOp::LeftOuterJoin {
            input,
            right,
            optional_aliases,
            opt_filter,
        } => {
            let left_records = exec_correlated(conn, input, outer, ctx)?;
            left_outer_join_records(
                conn,
                left_records,
                right,
                optional_aliases,
                opt_filter.as_ref(),
                outer,
                ctx,
            )
        }

        // Project: push correlation into the input, then apply projection
        // normally via the non-correlated exec path for the Project node alone.
        LogicalOp::Project {
            input,
            items,
            emit_compound,
        } => {
            let input_records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();
            for rec in &input_records {
                let mut projected = NamedRecord::new();
                for item in items {
                    match &item.expr.kind {
                        ExprKind::Star => {
                            for (key, val) in &rec.fields {
                                projected.set(key.clone(), val.clone());
                            }
                        }
                        _ => {
                            let col = item
                                .alias
                                .clone()
                                .or_else(|| {
                                    if let ExprKind::Variable(v) = &item.expr.kind {
                                        Some(v.clone())
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_else(|| format!("{:?}", item.expr));
                            // Try alias-based lookup first (aggregate results
                            // are stored under alias by exec_aggregate).
                            let expr_col = expr_to_column_name(&item.expr);
                            let val = if let Some(existing) = rec.get(&expr_col) {
                                existing.clone()
                            } else if let Some(existing) = rec.get(&col) {
                                existing.clone()
                            } else {
                                crate::cypher::eval::eval_expr(
                                    &item.expr,
                                    rec,
                                    crate::cypher::eval::EvalCx::new(conn),
                                )?
                            };
                            projected.set(col, val);
                            // Carry forward internal metadata for bound variables.
                            if let ExprKind::Variable(v) = &item.expr.kind {
                                let alias = item.alias.as_deref().unwrap_or(v.as_str());
                                for (key, val) in &rec.fields {
                                    if key.starts_with(&format!("{v}.")) {
                                        let suffix = &key[v.len()..];
                                        projected.set(format!("{alias}{suffix}"), val.clone());
                                    }
                                }
                            }
                        }
                    }
                }
                if *emit_compound {
                    // For final RETURN, also build compound node/relationship values.
                    let mut compound = NamedRecord::new();
                    let bound_vars = compound_binding_vars(&projected);
                    for var in &bound_vars {
                        if let Some(cv) = build_compound_binding(&projected, var) {
                            compound.set(var.clone(), cv);
                        }
                    }
                    // Also carry non-compound columns.
                    for (key, val) in &projected.fields {
                        if !key.contains('.') && !bound_vars.contains(key) {
                            compound.set(key.clone(), val.clone());
                        }
                    }
                    results.push(compound);
                } else {
                    results.push(projected);
                }
            }
            Ok(results)
        }

        // Aggregate: push correlation into the input, then aggregate.
        LogicalOp::Aggregate {
            input,
            group_keys,
            aggregates,
        } => {
            // Execute the input with correlation, then aggregate the results.
            let records = {
                let _uncapped = UnwindCapSuspended::new();
                exec_correlated(conn, input, outer, ctx)?
            };
            exec_aggregate_over_records(conn, &records, group_keys, aggregates)
        }

        // MaterializePath: push correlation into the input, then materialize paths.
        LogicalOp::MaterializePath {
            input,
            path_alias,
            node_aliases,
            rel_aliases,
        } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            materialize_paths(conn, records, path_alias, node_aliases, rel_aliases)
        }

        // ShortestPath: push correlation into the endpoint scans so endpoints
        // bound by an earlier clause constrain the search.
        LogicalOp::ShortestPath { input, .. } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            extend_with_shortest_paths(conn, plan, records, ctx)
        }

        // Row-shaping operators of a subquery body (EXISTS { ... RETURN x
        // LIMIT 1 }, ...): keep the correlation flowing into their input.
        LogicalOp::Sort { input, items } => {
            let mut records = exec_correlated(conn, input, outer, ctx)?;
            // ORDER BY holds its whole input at once, whatever LIMIT follows.
            check_row_limit(&records, ctx)?;
            sort_records(conn, &mut records, items)?;
            Ok(records)
        }

        LogicalOp::Distinct { input } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            Ok(distinct_records(records))
        }

        LogicalOp::Skip { input, count } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            Ok(records.into_iter().skip(*count as usize).collect())
        }

        LogicalOp::Limit { input, count } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            Ok(records.into_iter().take(*count as usize).collect())
        }

        LogicalOp::Unwind { input, expr, alias } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            unwind_records(conn, records, expr, alias, outer, ctx)
        }

        LogicalOp::Union { inputs, all } => {
            let mut results = Vec::new();
            for input in inputs {
                results.extend(exec_correlated(conn, input, outer, ctx)?);
            }
            if !all {
                dedup_union_rows(&mut results);
            }
            Ok(results)
        }

        // Everything else (constant rows, writes, CALL, DDL) does not read
        // variables of an enclosing scope: execute normally.
        _ => exec(conn, plan, ctx),
    }
}
