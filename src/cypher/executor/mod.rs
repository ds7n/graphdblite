use std::collections::HashMap;

use rusqlite::Connection;
use tracing::{debug, instrument};

use crate::cypher::ast::{Expr, ExprKind, LiteralValue, PatternElement, SetItem};
use crate::cypher::eval::{eval_expr, eval_predicate, expr_to_column_name};
use crate::cypher::ir::*;
use crate::cypher::record::NamedRecord;
use crate::edge;
use crate::index;
use crate::node;
use crate::types::{
    Direction, ErrorCode, GraphError, NodeId, PathValue, Properties, Result, Value,
};

use crate::cypher::procedure::ProcedureRegistry;

mod correlated;
use correlated::{exec_correlated, exec_correlated_join, exec_left_outer_join, value_to_node_id};

/// Execution context carrying runtime limits.
pub struct ExecContext {
    /// Maximum rows any operator may produce. 0 = unlimited.
    pub max_result_rows: usize,
    /// Maximum hop count for any pattern traversal (`Expand` / `ShortestPath`).
    /// Enforced before execution. 0 = unlimited.
    pub max_traversal_depth: u32,
    /// Maximum total edge-visit budget for a single var-length traversal.
    /// Enforced inside `traverse_paths`. 0 = unlimited.
    pub max_traversal_work: u64,
    /// Test procedure registry for CALL statements.
    pub procedures: ProcedureRegistry,
    /// When true, reject any plan that contains write operators. Set by
    /// `Database::execute_with_params` and the typed `ReadTransaction::query`
    /// so write Cypher inside a read-only transaction fails fast instead of
    /// silently upgrading the SQLite lock.
    pub require_read_only: bool,
}

impl Default for ExecContext {
    fn default() -> Self {
        Self {
            max_result_rows: 0,
            max_traversal_depth: 0,
            max_traversal_work: 10_000_000,
            procedures: ProcedureRegistry::default(),
            require_read_only: false,
        }
    }
}

/// Walk the plan tree and reject any traversal whose hop count exceeds the
/// configured `max_traversal_depth`. Returns early when the cap is 0
/// (unlimited).
pub(super) fn validate_traversal_depth(plan: &LogicalOp, ctx: &ExecContext) -> Result<()> {
    if ctx.max_traversal_depth == 0 {
        return Ok(());
    }
    check_depth_recursive(plan, ctx.max_traversal_depth)
}

fn check_depth_recursive(op: &LogicalOp, cap: u32) -> Result<()> {
    match op {
        LogicalOp::Expand {
            input, max_hops, ..
        } => {
            if *max_hops > cap {
                return Err(GraphError::query(
                    crate::types::QueryPhase::SemanticAnalysis,
                    crate::types::ErrorCode::NumberOutOfRange,
                    format!(
                        "var-length hop count `{max_hops}` exceeds the configured max traversal depth of `{cap}`"
                    ),
                ));
            }
            check_depth_recursive(input, cap)
        }
        LogicalOp::ShortestPath {
            input, max_hops, ..
        } => {
            if *max_hops > cap {
                return Err(GraphError::query(
                    crate::types::QueryPhase::SemanticAnalysis,
                    crate::types::ErrorCode::NumberOutOfRange,
                    format!(
                        "shortestPath hop count `{max_hops}` exceeds the configured max traversal depth of `{cap}`"
                    ),
                ));
            }
            check_depth_recursive(input, cap)
        }
        LogicalOp::Filter { input, .. }
        | LogicalOp::Project { input, .. }
        | LogicalOp::Aggregate { input, .. }
        | LogicalOp::Sort { input, .. }
        | LogicalOp::Distinct { input }
        | LogicalOp::Skip { input, .. }
        | LogicalOp::Limit { input, .. }
        | LogicalOp::MatchCreate { input, .. }
        | LogicalOp::Delete { input, .. }
        | LogicalOp::SetProperty { input, .. }
        | LogicalOp::SetLabel { input, .. }
        | LogicalOp::SetProperties { input, .. }
        | LogicalOp::Remove { input, .. }
        | LogicalOp::MatchMerge { input, .. }
        | LogicalOp::MaterializePath { input, .. }
        | LogicalOp::Unwind { input, .. }
        | LogicalOp::Call { input, .. } => check_depth_recursive(input, cap),
        LogicalOp::CrossProduct { left, right, .. } => {
            check_depth_recursive(left, cap)?;
            check_depth_recursive(right, cap)
        }
        LogicalOp::CorrelatedJoin { input, right, .. }
        | LogicalOp::LeftOuterJoin { input, right, .. } => {
            check_depth_recursive(input, cap)?;
            check_depth_recursive(right, cap)
        }
        LogicalOp::Union { inputs, .. } => {
            for inp in inputs {
                check_depth_recursive(inp, cap)?;
            }
            Ok(())
        }
        LogicalOp::CreateSequence { ops } => {
            for inner in ops {
                check_depth_recursive(inner, cap)?;
            }
            Ok(())
        }
        LogicalOp::SingleRow
        | LogicalOp::Scan { .. }
        | LogicalOp::IndexLookup { .. }
        | LogicalOp::CreateNode { .. }
        | LogicalOp::CreateEdge { .. }
        | LogicalOp::Merge { .. }
        | LogicalOp::EmptyRow => Ok(()),
    }
}

/// Check that a result set hasn't exceeded the row cap.
pub(super) fn check_row_limit(results: &[NamedRecord], ctx: &ExecContext) -> Result<()> {
    if ctx.max_result_rows > 0 && results.len() > ctx.max_result_rows {
        return Err(GraphError::constraint(format!(
            "result set exceeded maximum of {} rows",
            ctx.max_result_rows
        )));
    }
    Ok(())
}

/// Execute a logical plan against the database, producing result records.
///
/// For read-only plans, uses the pull-based iterator model so that pipeline
/// operators (Filter, Limit, Project) stream without full materialization.
#[instrument(skip_all, level = "debug")]
pub fn execute(conn: &Connection, plan: &LogicalOp) -> Result<Vec<NamedRecord>> {
    let ctx = ExecContext::default();
    validate_traversal_depth(plan, &ctx)?;
    if is_read_only(plan) {
        let mut iter = crate::cypher::iter::build_iter(conn, plan, ctx.max_traversal_work)?;
        crate::cypher::iter::collect_all(&mut *iter)
    } else {
        let result = exec(conn, plan, &ctx)?;
        // Write-only queries (no RETURN clause) should return empty results.
        // When a RETURN is present, the planner wraps the write op in a Project,
        // so the top-level op will be Project/Sort/Skip/Limit/etc., not a bare write.
        if is_bare_write(plan) {
            Ok(vec![])
        } else {
            Ok(result)
        }
    }
}

/// Check if the top-level plan is a bare write op (no RETURN projection).
fn is_bare_write(plan: &LogicalOp) -> bool {
    matches!(
        plan,
        LogicalOp::CreateNode { .. }
            | LogicalOp::CreateEdge { .. }
            | LogicalOp::CreateSequence { .. }
            | LogicalOp::MatchCreate { .. }
            | LogicalOp::Delete { .. }
            | LogicalOp::SetProperty { .. }
            | LogicalOp::SetLabel { .. }
            | LogicalOp::SetProperties { .. }
            | LogicalOp::Remove { .. }
            | LogicalOp::Merge { .. }
            | LogicalOp::MatchMerge { .. }
    )
}

/// Execute with an explicit context carrying runtime limits.
///
/// Dispatches to the legacy `IndexMap`-backed `execute_with_ctx_named` by
/// default, or the slot-indexed `execute_with_ctx_slot` when the
/// `record-v2` feature is on. Both functions live side-by-side during the
/// migration so `dual_run` can call them in-process for the same plan and
/// compare results — see `plans/record-v2.md` Phase 3a.
pub fn execute_with_ctx(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    #[cfg(feature = "record-v2")]
    {
        execute_with_ctx_slot(conn, plan, ctx)
    }
    #[cfg(not(feature = "record-v2"))]
    {
        execute_with_ctx_named(conn, plan, ctx)
    }
}

/// Legacy path: produces `NamedRecord`s via the existing `IndexMap`-backed
/// executor. Definitive truth during the record-v2 migration; every dual-run
/// comparison treats this output as the reference.
pub fn execute_with_ctx_named(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    validate_traversal_depth(plan, ctx)?;
    let result = exec(conn, plan, ctx)?;
    if is_bare_write(plan) {
        Ok(vec![])
    } else {
        Ok(result)
    }
}

/// Slot-indexed path. Phase 3b: handles `Scan` / `IndexLookup` / `Project`
/// (plus `EmptyRow` / `SingleRow` leaves) end-to-end via the slot iterator
/// stack in `cypher::iter_slot`; falls back to `execute_with_ctx_named`
/// for any plan containing operators not yet migrated. The fallback is
/// transparent — same `Vec<NamedRecord>` return shape — so callers don't
/// need to know which path ran.
///
/// Phase 3c–3g progressively widen [`is_slot_supported`] until every
/// read-side operator runs natively on slots; Phase 4 does the same for
/// writes. Once that's done, the boundary conversion in `collect_to_named`
/// becomes the only place strings are reified per row.
#[allow(dead_code)] // exercised by dual_run tests + dispatcher when feature is on
pub fn execute_with_ctx_slot(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    use crate::cypher::iter_slot;

    validate_traversal_depth(plan, ctx)?;
    if !iter_slot::is_slot_supported(plan) {
        return execute_with_ctx_named(conn, plan, ctx);
    }
    let mut iter = iter_slot::build_slot_iter(conn, plan, ctx)?;
    let result = iter_slot::collect_to_named(&mut *iter)?;
    if is_bare_write(plan) {
        Ok(vec![])
    } else {
        Ok(result)
    }
}

/// Check whether a plan tree contains only read-only operators.
pub(crate) fn is_read_only(plan: &LogicalOp) -> bool {
    match plan {
        LogicalOp::Scan { .. }
        | LogicalOp::IndexLookup { .. }
        | LogicalOp::EmptyRow
        | LogicalOp::SingleRow => true,

        LogicalOp::Filter { input, .. }
        | LogicalOp::Project { input, .. }
        | LogicalOp::Distinct { input }
        | LogicalOp::Sort { input, .. }
        | LogicalOp::Skip { input, .. }
        | LogicalOp::Limit { input, .. }
        | LogicalOp::Unwind { input, .. }
        | LogicalOp::Aggregate { input, .. }
        | LogicalOp::ShortestPath { input, .. }
        | LogicalOp::MaterializePath { input, .. }
        | LogicalOp::Call { input, .. } => is_read_only(input),

        LogicalOp::Expand { input, .. } => is_read_only(input),

        LogicalOp::CrossProduct { left, right, .. }
        | LogicalOp::CorrelatedJoin {
            input: left, right, ..
        }
        | LogicalOp::LeftOuterJoin {
            input: left, right, ..
        } => is_read_only(left) && is_read_only(right),

        LogicalOp::Union { inputs, .. } => inputs.iter().all(is_read_only),

        // Write operations.
        LogicalOp::CreateNode { .. }
        | LogicalOp::CreateEdge { .. }
        | LogicalOp::CreateSequence { .. }
        | LogicalOp::MatchCreate { .. }
        | LogicalOp::Delete { .. }
        | LogicalOp::SetProperty { .. }
        | LogicalOp::SetLabel { .. }
        | LogicalOp::SetProperties { .. }
        | LogicalOp::Remove { .. }
        | LogicalOp::Merge { .. }
        | LogicalOp::MatchMerge { .. } => false,
    }
}

/// Crate-internal alias so `iter_slot` can run subtrees through the named
/// materialized executor when no slot impl exists yet (e.g. correlated
/// joins in 3g.1 — we present the named output as a slot iter via
/// `NamedToSlotAdapter` so upstream operators stay on the slot path).
pub(crate) fn exec_pub(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    exec(conn, plan, ctx)
}

pub(super) fn exec(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    debug!(op = %plan.op_name(), "executing operator");
    match plan {
        LogicalOp::SingleRow => Ok(vec![NamedRecord::new()]),

        LogicalOp::EmptyRow => Ok(vec![NamedRecord::new()]),

        LogicalOp::Scan { label, alias } => exec_scan(conn, label, alias, ctx),

        LogicalOp::IndexLookup {
            label,
            alias,
            property,
            value,
            remaining_filters,
        } => exec_index_lookup(
            conn,
            label,
            alias,
            property,
            value,
            remaining_filters.as_ref(),
        ),

        LogicalOp::Expand {
            input,
            src_alias,
            dst_alias,
            rel_alias,
            edge_types,
            direction,
            min_hops,
            max_hops,
            var_length,
            var_length_prop_filters,
            result_cap,
        } => exec_expand(
            conn,
            input,
            src_alias,
            dst_alias,
            rel_alias.as_deref(),
            edge_types,
            *direction,
            *min_hops,
            *max_hops,
            *var_length,
            var_length_prop_filters,
            result_cap.map(|c| c as usize),
            ctx,
        ),

        LogicalOp::CrossProduct {
            left,
            right,
            same_match,
        } => exec_cross_product(conn, left, right, *same_match, ctx),

        LogicalOp::Filter { input, predicate } => exec_filter(conn, input, predicate, ctx),

        LogicalOp::Project {
            input,
            items,
            emit_compound,
        } => exec_project(conn, input, items, *emit_compound, ctx),

        LogicalOp::Aggregate {
            input,
            group_keys,
            aggregates,
        } => exec_aggregate(conn, input, group_keys, aggregates, ctx),

        LogicalOp::Distinct { input } => exec_distinct(conn, input, ctx),

        LogicalOp::Sort { input, items } => exec_sort(conn, input, items, ctx),

        LogicalOp::Skip { input, count } => exec_skip(conn, input, *count, ctx),

        LogicalOp::Limit { input, count } => exec_limit(conn, input, *count, ctx),

        LogicalOp::CreateNode {
            labels,
            alias,
            properties,
        } => exec_create_node(conn, labels, alias.as_deref(), properties),

        LogicalOp::CreateEdge {
            src_alias,
            dst_alias,
            edge_type,
            properties,
            ..
        } => exec_create_edge(conn, src_alias, dst_alias, edge_type, properties),

        LogicalOp::CreateSequence { ops } => exec_create_sequence(conn, ops),

        LogicalOp::MatchCreate { input, create_ops } => {
            exec_match_create(conn, input, create_ops, ctx)
        }

        LogicalOp::Delete {
            input,
            exprs,
            detach,
        } => exec_delete(conn, input, exprs, *detach, ctx),

        LogicalOp::SetProperty { input, assignments } => {
            exec_set_property(conn, input, assignments, ctx)
        }

        LogicalOp::SetLabel {
            input,
            variable,
            labels,
        } => exec_set_label(conn, input, variable, labels, ctx),

        LogicalOp::SetProperties {
            input,
            variable,
            value,
            merge,
        } => exec_set_properties(conn, input, variable, value, *merge, ctx),

        LogicalOp::Remove { input, items } => exec_remove(conn, input, items, ctx),

        LogicalOp::Merge {
            pattern,
            on_create,
            on_match,
        } => exec_merge(conn, pattern, on_create, on_match),

        LogicalOp::MatchMerge {
            input,
            merge_pattern,
            on_create,
            on_match,
        } => exec_match_merge(conn, input, merge_pattern, on_create, on_match, ctx),

        LogicalOp::Unwind { input, expr, alias } => exec_unwind(conn, input, expr, alias, ctx),

        LogicalOp::MaterializePath {
            input,
            path_alias,
            node_aliases,
            rel_aliases,
        } => exec_materialize_path(conn, input, path_alias, node_aliases, rel_aliases, ctx),

        LogicalOp::CorrelatedJoin {
            input,
            right,
            same_match,
        } => exec_correlated_join(conn, input, right, *same_match, ctx),

        LogicalOp::LeftOuterJoin {
            input,
            right,
            optional_aliases,
            opt_filter,
        } => exec_left_outer_join(
            conn,
            input,
            right,
            optional_aliases,
            opt_filter.as_ref(),
            ctx,
        ),

        LogicalOp::ShortestPath {
            input,
            src_alias,
            dst_alias,
            path_alias,
            edge_type,
            direction,
            max_hops,
            all_paths,
        } => exec_shortest_path(
            conn,
            input,
            src_alias,
            dst_alias,
            path_alias,
            edge_type.as_deref(),
            *direction,
            *max_hops,
            *all_paths,
            ctx,
        ),

        LogicalOp::Call {
            input,
            procedure_name,
            args,
            yield_items,
            yield_star,
        } => exec_call(
            conn,
            input,
            procedure_name,
            args,
            yield_items,
            *yield_star,
            ctx,
        ),

        LogicalOp::Union { inputs, all } => {
            let mut results = Vec::new();
            for input in inputs {
                results.extend(exec(conn, input, ctx)?);
            }
            if !all {
                // Deduplicate for plain UNION.
                let mut seen = Vec::new();
                results.retain(|rec| {
                    if seen.iter().any(|s: &NamedRecord| s.fields == rec.fields) {
                        false
                    } else {
                        seen.push(rec.clone());
                        true
                    }
                });
            }
            Ok(results)
        }
    }
}

pub(super) fn exec_scan(
    conn: &Connection,
    label: &str,
    alias: &str,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let nodes = node::find_nodes_by_label(conn, label)?;
    let mut records = Vec::with_capacity(nodes.len());
    for n in nodes {
        records.push(node_to_record(&n, alias));
    }
    check_row_limit(&records, ctx)?;
    Ok(records)
}

pub(super) fn exec_index_lookup(
    conn: &Connection,
    label: &str,
    alias: &str,
    property: &str,
    value: &LiteralValue,
    remaining_filters: Option<&Expr>,
) -> Result<Vec<NamedRecord>> {
    let lookup_value = literal_to_value(value);
    let node_ids = index::index_lookup(conn, label, property, &lookup_value)?;
    let mut records = Vec::new();

    for id in node_ids {
        let n = node::get_node(conn, id)?;
        let rec = node_to_record(&n, alias);

        if let Some(filter) = remaining_filters {
            if !eval_predicate(filter, &rec, conn)? {
                continue;
            }
        }

        records.push(rec);
    }

    Ok(records)
}

#[allow(clippy::too_many_arguments)]
fn exec_expand(
    conn: &Connection,
    input: &LogicalOp,
    src_alias: &str,
    dst_alias: &str,
    rel_alias: Option<&str>,
    edge_types: &[String],
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    var_length: bool,
    var_length_prop_filters: &HashMap<String, Expr>,
    result_cap: Option<usize>,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let input_records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &input_records {
        if let Some(cap) = result_cap {
            if results.len() >= cap {
                break;
            }
        }
        let src_id = match rec.get(src_alias).and_then(value_to_node_id) {
            Some(id) => id,
            _ => continue,
        };

        // If no types specified, discover all edge types for this node.
        // For var-length, pass empty labels so traverse_paths discovers
        // types at each hop (different nodes may have different edge types).
        let _owned_labels: Vec<String>;
        let labels: Vec<&str> = if edge_types.is_empty() {
            let all = edge::get_all_edge_labels(conn, src_id, direction)?;
            _owned_labels = all.into_iter().map(|(l, _)| l).collect();
            _owned_labels.iter().map(|s| s.as_str()).collect()
        } else {
            edge_types.iter().map(|s| s.as_str()).collect()
        };
        let var_length_labels: Vec<&str> = if edge_types.is_empty() {
            vec![] // signal traverse_paths to discover per-hop
        } else {
            labels.clone()
        };

        // If the destination alias is already bound in the record (cyclic
        // pattern like `(a)-[:R]->(b)-[:S]->(a)`), we must only keep
        // expansions where the destination equals the bound node.
        let bound_dst_id = rec.get(dst_alias).and_then(value_to_node_id);

        if var_length {
            // Variable-length traversal — pass ALL labels at once for mixed-type support.
            let prop_filter_values: HashMap<String, Value> = var_length_prop_filters
                .iter()
                .filter_map(|(k, expr)| match &expr.kind {
                    ExprKind::Literal(lit) => Some((k.clone(), literal_to_value(lit))),
                    _ => None,
                })
                .collect();
            // Pass remaining budget so traverse_paths can stop early.
            let per_call_cap = result_cap.map(|c| c.saturating_sub(results.len()));
            let paths = edge::traverse_paths(
                conn,
                src_id,
                &var_length_labels,
                direction,
                min_hops,
                max_hops,
                &prop_filter_values,
                per_call_cap,
                ctx.max_traversal_work,
            )?;
            for (dst_id, steps) in paths {
                if let Some(required) = bound_dst_id {
                    if dst_id != required {
                        continue;
                    }
                }
                let mut new_rec = rec.clone();
                let _dst_node = fetch_and_populate(conn, &mut new_rec, dst_id, dst_alias)?;
                // Bind relationship variable as a list of edges.
                if let Some(r_alias) = rel_alias {
                    let edge_list: Vec<Value> = steps
                        .iter()
                        .map(|step| {
                            let props = edge::get_edge_properties_at(
                                conn,
                                step.edge_src,
                                step.edge_dst,
                                &step.edge_label,
                                step.edge_seq,
                            )
                            .unwrap_or_default();
                            Value::Edge(crate::types::Edge {
                                src: step.edge_src,
                                dst: step.edge_dst,
                                label: step.edge_label.clone(),
                                properties: props,
                            })
                        })
                        .collect();
                    new_rec.set(r_alias.to_string(), Value::List(edge_list));
                }
                results.push(new_rec);
            }
        } else {
            for &label in &labels {
                // Single hop — direct neighbor lookup.
                let neighbors = edge::get_neighbors(conn, src_id, label, direction)?;
                for dst_id in neighbors {
                    // Skip if destination doesn't match the already-bound node.
                    if let Some(required) = bound_dst_id {
                        if dst_id != required {
                            continue;
                        }
                    }

                    let (edge_src, edge_dst) = match direction {
                        Direction::Incoming => (dst_id, src_id),
                        Direction::Outgoing => (src_id, dst_id),
                        Direction::Both => {
                            if edge::edge_exists(conn, src_id, dst_id, label).unwrap_or(false) {
                                (src_id, dst_id)
                            } else {
                                (dst_id, src_id)
                            }
                        }
                    };

                    // Get all parallel edges for this (src, dst, label) pair.
                    let all_edges = edge::get_all_edge_props(conn, edge_src, edge_dst, label)?;
                    // If no edges found (shouldn't happen since neighbor exists),
                    // fall back to a single empty-props edge.
                    let edge_list: Vec<(u64, Properties)> = if all_edges.is_empty() {
                        vec![(0, Properties::new())]
                    } else {
                        all_edges
                    };

                    let dst_node = node::get_node(conn, dst_id)?;

                    for (edge_seq, edge_props) in &edge_list {
                        let mut new_rec = rec.clone();
                        populate_node_bindings(&mut new_rec, &dst_node, dst_alias);

                        if let Some(r_alias) = rel_alias {
                            // Relationship uniqueness: within a MATCH pattern,
                            // different named relationship variables must refer to
                            // different edges. Include edge_seq in the uniqueness key
                            // so parallel edges are distinguished.
                            let (es, ed) = (edge_src.0 as i64, edge_dst.0 as i64);
                            let edge_key = (es.min(ed), es.max(ed), label, *edge_seq);
                            let mut duplicate = false;
                            for (key, _val) in &new_rec.fields {
                                if key.ends_with(".__src") && key != &format!("{r_alias}.__src") {
                                    let other_alias = &key[..key.len() - 6];
                                    let other_seq =
                                        match new_rec.get(&format!("{other_alias}.__edge_seq")) {
                                            Some(Value::I64(s)) => *s as u64,
                                            _ => 0,
                                        };
                                    if let (
                                        Some(Value::I64(os)),
                                        Some(Value::I64(od)),
                                        Some(Value::String(ot)),
                                    ) = (
                                        new_rec.get(key),
                                        new_rec.get(&format!("{other_alias}.__dst")),
                                        new_rec.get(&format!("{other_alias}.__type")),
                                    ) {
                                        let other_key = (
                                            (*os).min(*od),
                                            (*os).max(*od),
                                            ot.as_str(),
                                            other_seq,
                                        );
                                        if other_key == edge_key {
                                            duplicate = true;
                                            break;
                                        }
                                    }
                                }
                            }
                            if duplicate {
                                continue;
                            }

                            new_rec.set(r_alias.to_string(), Value::String(label.to_string()));
                            new_rec.set(format!("{r_alias}.__src"), Value::I64(edge_src.0 as i64));
                            new_rec.set(format!("{r_alias}.__dst"), Value::I64(edge_dst.0 as i64));
                            new_rec.set(
                                format!("{r_alias}.__type"),
                                Value::String(label.to_string()),
                            );
                            new_rec.set(
                                format!("{r_alias}.__edge_seq"),
                                Value::I64(*edge_seq as i64),
                            );
                            for (key, val) in edge_props {
                                new_rec.set(format!("{r_alias}.{key}"), val.clone());
                            }
                        }
                        results.push(new_rec);
                    }
                }
            }
        }
    }

    check_row_limit(&results, ctx)?;
    Ok(results)
}

fn exec_cross_product(
    conn: &Connection,
    left: &LogicalOp,
    right: &LogicalOp,
    same_match: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    // Materialize only the left side. Re-execute the right side per left
    // record so peak memory is O(left + right + output) instead of
    // O(left * right).
    let left_records = exec(conn, left, ctx)?;
    let mut results = Vec::new();
    for l in &left_records {
        let right_records = exec(conn, right, ctx)?;
        for r in &right_records {
            let mut combined = l.clone();
            for (key, val) in &r.fields {
                combined.set(key.clone(), val.clone());
            }
            // Cross-pattern relationship uniqueness (only within the same
            // MATCH clause — separate MATCHes have independent scopes).
            if same_match && has_duplicate_relationships(&combined) {
                continue;
            }
            results.push(combined);
            if ctx.max_result_rows > 0 && results.len() > ctx.max_result_rows {
                return Err(GraphError::constraint(format!(
                    "cross product exceeded maximum of {} rows",
                    ctx.max_result_rows
                )));
            }
        }
    }
    Ok(results)
}

/// Collect edge identities from flat relationship bindings in a record
/// (`alias.__src`, `alias.__dst`, `alias.__type`). Returns a vec of
/// `(min(src,dst), max(src,dst), type, seq)` tuples.
pub(super) fn collect_flat_edge_ids(rec: &NamedRecord) -> Vec<(i64, i64, String, u64)> {
    let mut edges = Vec::new();
    for (key, _) in &rec.fields {
        if key.ends_with(".__src") {
            let alias = &key[..key.len() - 6];
            let seq = match rec.get(&format!("{alias}.__edge_seq")) {
                Some(Value::I64(s)) => *s as u64,
                _ => 0,
            };
            if let (Some(Value::I64(s)), Some(Value::I64(d)), Some(Value::String(t))) = (
                rec.get(key),
                rec.get(&format!("{alias}.__dst")),
                rec.get(&format!("{alias}.__type")),
            ) {
                edges.push(((*s).min(*d), (*s).max(*d), t.clone(), seq));
            }
        }
    }
    edges
}

/// Check if a record contains two relationship bindings that refer to the
/// same underlying edge (same normalized src/dst/type/seq).
pub(super) fn has_duplicate_relationships(rec: &NamedRecord) -> bool {
    let mut seen: Vec<(i64, i64, String, u64)> = Vec::new();
    for (key, _) in &rec.fields {
        if key.ends_with(".__src") {
            let alias = &key[..key.len() - 6];
            let seq = match rec.get(&format!("{alias}.__edge_seq")) {
                Some(Value::I64(s)) => *s as u64,
                _ => 0,
            };
            if let (Some(Value::I64(s)), Some(Value::I64(d)), Some(Value::String(t))) = (
                rec.get(key),
                rec.get(&format!("{alias}.__dst")),
                rec.get(&format!("{alias}.__type")),
            ) {
                let edge_key = ((*s).min(*d), (*s).max(*d), t.clone(), seq);
                if seen.contains(&edge_key) {
                    return true;
                }
                seen.push(edge_key);
            }
        }
    }
    false
}

fn exec_filter(
    conn: &Connection,
    input: &LogicalOp,
    predicate: &Expr,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();
    for rec in records {
        if eval_predicate(predicate, &rec, conn)? {
            results.push(rec);
        }
    }
    Ok(results)
}

/// Reconstruct a compound `Value::Node` or `Value::Edge` from a variable's
/// flat bindings in a record. Returns `None` if the variable has no node/edge
/// metadata (i.e. it's an expression/aggregate result, not a pattern binding).
///
/// Records flow through the pipeline with a flattened shape (`n.name`,
/// `n.__id`, etc.); this helper materializes the compound at projection time
/// so query results look the way openCypher specifies.
pub(crate) fn build_compound_binding(rec: &NamedRecord, var: &str) -> Option<Value> {
    use crate::types::{Edge, Node, Properties};

    // Var-length relationship variables stored directly as Value::List.
    if let Some(val @ Value::List(_)) = rec.get(var) {
        return Some(val.clone());
    }
    // Path values stored directly.
    if let Some(val @ Value::Path(_)) = rec.get(var) {
        return Some(val.clone());
    }

    let prefix = format!("{var}.");

    // Edge binding: has __src / __dst / __type metadata.
    let src_key = format!("{var}.__src");
    let dst_key = format!("{var}.__dst");
    let type_key = format!("{var}.__type");
    if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) =
        (rec.get(&src_key), rec.get(&dst_key), rec.get(&type_key))
    {
        let mut properties = Properties::new();
        for (key, val) in &rec.fields {
            if let Some(prop) = key.strip_prefix(&prefix) {
                if !prop.starts_with("__") {
                    properties.insert(prop.to_string(), val.clone());
                }
            }
        }
        return Some(Value::Edge(Edge {
            src: NodeId(*src as u64),
            dst: NodeId(*dst as u64),
            label: label.clone(),
            properties,
        }));
    }

    // Node binding: has __id / __label metadata.
    let id_key = format!("{var}.__id");
    let label_key = format!("{var}.__label");
    if let (Some(Value::I64(id)), Some(Value::String(label_str))) =
        (rec.get(&id_key), rec.get(&label_key))
    {
        let mut properties = Properties::new();
        for (key, val) in &rec.fields {
            if let Some(prop) = key.strip_prefix(&prefix) {
                if !prop.starts_with("__") {
                    properties.insert(prop.to_string(), val.clone());
                }
            }
        }
        // Reconstruct labels from the colon-joined __label string.
        let labels: Vec<String> = if label_str.is_empty() {
            Vec::new()
        } else {
            label_str.split(':').map(|s| s.to_string()).collect()
        };
        return Some(Value::Node(Node {
            id: NodeId(*id as u64),
            labels,
            properties,
        }));
    }

    None
}

/// Collect the set of variable names in a record that are bound as compound
/// entities (nodes or edges). Used by `RETURN *` to know which prefixes to
/// fold into compound columns rather than emitting as flat properties.
pub(crate) fn compound_binding_vars(rec: &NamedRecord) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut vars: BTreeSet<String> = BTreeSet::new();
    for key in rec.fields.keys() {
        if let Some((var, prop)) = key.split_once('.') {
            if prop == "__id" || prop == "__src" {
                vars.insert(var.to_string());
            }
        }
    }
    vars.into_iter().collect()
}

fn exec_project(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::ReturnItem],
    emit_compound: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &records {
        let mut projected = NamedRecord::new();
        for item in items {
            match &item.expr.kind {
                ExprKind::Star => {
                    if emit_compound {
                        // Final RETURN * — emit one compound column per bound
                        // variable (plus any non-binding user-visible scalars).
                        let bound_vars = compound_binding_vars(rec);
                        for var in &bound_vars {
                            if let Some(compound) = build_compound_binding(rec, var) {
                                projected.set(var.clone(), compound);
                            }
                        }
                        for (key, val) in &rec.fields {
                            if let Some((owner, prop)) = key.split_once('.') {
                                // Dotted key: skip internal fields and fields
                                // belonging to compound-bound variables.
                                if prop.starts_with("__") {
                                    continue;
                                }
                                if bound_vars.iter().any(|v| v == owner) {
                                    continue;
                                }
                                projected.set(key.clone(), val.clone());
                            } else {
                                // Bare key: emit if it's a scalar value from
                                // WITH/UNWIND (no accompanying __id metadata)
                                // and not already emitted as a compound binding.
                                if !bound_vars.iter().any(|v| v == key) {
                                    let has_id = rec.get(&format!("{key}.__id")).is_some();
                                    if !has_id {
                                        projected.set(key.clone(), val.clone());
                                    }
                                }
                            }
                        }
                    } else {
                        // Intermediate WITH * — preserve flat shape so
                        // downstream pattern-matching / joins / ORDER BY keep
                        // working against `var.__id` / `var.prop` fields.
                        for (key, val) in &rec.fields {
                            projected.set(key.clone(), val.clone());
                        }
                    }
                }
                ExprKind::Variable(var) => {
                    let col_name = item.alias.clone().unwrap_or_else(|| var.clone());
                    if emit_compound {
                        if let Some(compound) = build_compound_binding(rec, var) {
                            projected.set(col_name, compound);
                        } else if let Some(existing) = rec.get(&col_name) {
                            projected.set(col_name, existing.clone());
                        } else {
                            let val = eval_expr(&item.expr, rec, conn)?;
                            projected.set(col_name, val);
                        }
                    } else {
                        // Intermediate: carry forward the flat binding shape so
                        // downstream operators can still access `var.prop` and
                        // `var.__id`. Rename prefixes when an alias was given.
                        let src_prefix = format!("{var}.");
                        let dst_prefix = format!("{col_name}.");
                        let mut propagated_any = false;
                        for (key, val) in &rec.fields {
                            if let Some(rest) = key.strip_prefix(&src_prefix) {
                                projected.set(format!("{dst_prefix}{rest}"), val.clone());
                                propagated_any = true;
                            }
                        }
                        // Also propagate the bare variable column if present
                        // (used by some operators as a compact id reference).
                        if let Some(existing) = rec.get(var) {
                            projected.set(col_name.clone(), existing.clone());
                            propagated_any = true;
                        }
                        if !propagated_any {
                            let val = eval_expr(&item.expr, rec, conn)?;
                            projected.set(col_name, val);
                        }
                    }
                }
                _ => {
                    let col_name = item
                        .alias
                        .clone()
                        .unwrap_or_else(|| expr_to_column_name(&item.expr));
                    let expr_col = expr_to_column_name(&item.expr);
                    // Check for deleted entity access before any cache lookup.
                    // Property access on a deleted entity must raise an error
                    // even if the value is still in the record.
                    if let ExprKind::Property(var, prop) = &item.expr.kind {
                        if rec.get(&format!("{var}.__deleted")) == Some(&Value::Bool(true)) {
                            return Err(GraphError::Query(
                                crate::types::QueryError::EntityNotFound {
                                    phase: crate::types::QueryPhase::Runtime,
                                    message: format!(
                                        "DeletedEntityAccess: cannot access property `{prop}` on deleted entity `{var}`"
                                    ),
                                    code: ErrorCode::Other,
                                    hint: None,
                                    span: None,
                                },
                            ));
                        }
                    }
                    // Check if the col_name collides with a MATCH variable binding
                    // (which stores raw node IDs). MATCH variables always have
                    // accompanying `var.__id` metadata; aggregate results don't.
                    let is_match_binding = rec.get(&format!("{col_name}.__id")).is_some();
                    let val = if !is_match_binding {
                        // First try the expression's natural column name — this
                        // is the key used by the Aggregate operator for group
                        // keys and the only safe name to match (it can never
                        // collide with an upstream variable).
                        if let Some(existing) = rec.get(&expr_col) {
                            existing.clone()
                        } else if col_name == expr_col {
                            // No alias rename — safe to check col_name too
                            // (already checked above, so this is a miss → eval).
                            eval_expr(&item.expr, rec, conn)?
                        } else if let Some(existing) = rec.get(&col_name) {
                            // Alias differs from expression name. The col_name
                            // value in the record may be a pre-computed aggregate
                            // result (stored under alias by agg_col_name) or a
                            // stale upstream variable. Aggregate results are
                            // stored under the alias, so check for that.
                            // Heuristic: if the expression is an aggregate
                            // function, trust the cached value; otherwise
                            // evaluate to avoid shadowing bugs.
                            let is_agg = matches!(
                                &item.expr.kind,
                                ExprKind::FunctionCall { name, .. }
                                    if matches!(name.to_ascii_lowercase().as_str(),
                                        "count" | "sum" | "avg" | "min" | "max" | "collect"
                                        | "percentiledisc" | "percentilecont" | "stdev" | "stdevp")
                            );
                            if is_agg {
                                existing.clone()
                            } else {
                                eval_expr(&item.expr, rec, conn)?
                            }
                        } else {
                            eval_expr(&item.expr, rec, conn)?
                        }
                    } else {
                        eval_expr(&item.expr, rec, conn)?
                    };
                    projected.set(col_name, val);
                }
            }
        }
        results.push(projected);
    }

    Ok(results)
}

/// Returns true if a record field should be visible to the user.
/// Filters out bare alias keys (no dot — raw node IDs) and internal `__` properties.
pub(crate) fn is_user_visible_field(key: &str) -> bool {
    match key.split_once('.') {
        Some((_, prop)) => !prop.starts_with("__"),
        None => false, // bare alias like "n" is the raw node ID — hide it
    }
}

fn exec_aggregate(
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

fn compute_aggregate(
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

fn exec_sort(
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

fn exec_distinct(
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

fn exec_skip(
    conn: &Connection,
    input: &LogicalOp,
    count: u64,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    Ok(records.into_iter().skip(count as usize).collect())
}

fn exec_limit(
    conn: &Connection,
    input: &LogicalOp,
    count: u64,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    Ok(records.into_iter().take(count as usize).collect())
}

fn exec_create_node(
    conn: &Connection,
    labels: &[String],
    alias: Option<&str>,
    properties: &HashMap<String, Expr>,
) -> Result<Vec<NamedRecord>> {
    let mut props = Properties::new();
    let dummy_rec = NamedRecord::new();
    for (key, expr) in properties {
        let val = eval_expr(expr, &dummy_rec, conn)?;
        if val == Value::Null {
            continue;
        }
        props.insert(key.clone(), val);
    }

    let id = node::create_node(conn, labels, props.clone())?;
    let primary_label = labels.first().map(|s| s.as_str()).unwrap_or("");
    index::update_indexes_for_node(conn, id, primary_label, None, &props)?;

    let mut rec = NamedRecord::new();
    if let Some(alias) = alias {
        rec.set(alias.to_string(), Value::I64(id.0 as i64));
        rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
        // Populate property bindings so the slot bridge can fill prop slots
        // declared in the inferred schema. Deliberately *not* setting
        // `__label`/`__labels` — that pair triggers `build_compound_binding`,
        // and a Variable-resolution shift here would change downstream
        // semantics (e.g. `WITH a, ...` where `a` was previously `Value::I64`
        // but would become `Value::Node`).
        for (k, v) in &props {
            rec.set(format!("{alias}.{k}"), v.clone());
        }
    }
    Ok(vec![rec])
}

fn exec_create_edge(
    _conn: &Connection,
    _src_alias: &str,
    _dst_alias: &str,
    _edge_type: &str,
    _properties: &HashMap<String, Expr>,
) -> Result<Vec<NamedRecord>> {
    // Standalone edge creation is handled by exec_create_sequence.
    // This path is only reached for isolated CreateEdge ops (shouldn't happen in practice).
    Ok(vec![])
}

fn exec_create_sequence(conn: &Connection, ops: &[LogicalOp]) -> Result<Vec<NamedRecord>> {
    // Track variable → NodeId bindings for edge creation.
    let mut bindings: HashMap<String, NodeId> = HashMap::new();
    let mut last_record = NamedRecord::new();

    for op in ops {
        match op {
            LogicalOp::CreateNode {
                labels,
                alias,
                properties,
            } => {
                let mut props = Properties::new();
                for (key, expr) in properties {
                    let val = eval_expr(expr, &last_record, conn)?;
                    if val == Value::Null {
                        continue;
                    }
                    props.insert(key.clone(), val);
                }
                let id = node::create_node(conn, labels, props.clone())?;
                let primary_label = labels.first().map(|s| s.as_str()).unwrap_or("");
                index::update_indexes_for_node(conn, id, primary_label, None, &props)?;
                if let Some(alias) = alias {
                    bindings.insert(alias.clone(), id);
                    last_record.set(alias.clone(), Value::I64(id.0 as i64));
                    last_record.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
                    for (k, v) in &props {
                        last_record.set(format!("{alias}.{k}"), v.clone());
                    }
                }
            }
            LogicalOp::CreateEdge {
                src_alias,
                dst_alias,
                edge_type,
                rel_alias,
                properties,
            } => {
                let src = bindings.get(src_alias).ok_or_else(|| {
                    GraphError::semantic(format!("unbound variable: {src_alias}"))
                })?;
                let dst = bindings.get(dst_alias).ok_or_else(|| {
                    GraphError::semantic(format!("unbound variable: {dst_alias}"))
                })?;
                let mut props = Properties::new();
                for (key, expr) in properties {
                    let val = eval_expr(expr, &last_record, conn)?;
                    if val == Value::Null {
                        continue;
                    }
                    props.insert(key.clone(), val);
                }
                edge::create_edge(conn, *src, *dst, edge_type, props.clone())?;
                // Bind edge metadata for RETURN access.
                if let Some(r_alias) = rel_alias {
                    last_record.set(r_alias.clone(), Value::String(edge_type.clone()));
                    last_record.set(format!("{r_alias}.__src"), Value::I64(src.0 as i64));
                    last_record.set(format!("{r_alias}.__dst"), Value::I64(dst.0 as i64));
                    last_record.set(
                        format!("{r_alias}.__type"),
                        Value::String(edge_type.clone()),
                    );
                    for (key, val) in &props {
                        last_record.set(format!("{r_alias}.{key}"), val.clone());
                    }
                }
            }
            _ => {
                // Shouldn't happen in a CreateSequence.
            }
        }
    }

    Ok(vec![last_record])
}

fn exec_match_create(
    conn: &Connection,
    input: &LogicalOp,
    create_ops: &[LogicalOp],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut result = Vec::with_capacity(records.len());

    for rec in &records {
        // Seed bindings from MATCH-bound variables (var → NodeId).
        let mut bindings: HashMap<String, NodeId> = HashMap::new();
        for (key, val) in &rec.fields {
            if !key.contains('.') {
                if let Value::I64(id) = val {
                    bindings.insert(key.clone(), NodeId(*id as u64));
                }
            }
        }

        // Start with a copy of the input record so MATCH-bound vars are available.
        let mut out_rec = rec.clone();

        // Execute each CREATE op using the bindings.
        for op in create_ops {
            match op {
                LogicalOp::CreateNode {
                    labels,
                    alias,
                    properties,
                } => {
                    // Skip if this alias is already bound from the MATCH pipeline.
                    if let Some(a) = alias {
                        if bindings.contains_key(a) {
                            continue;
                        }
                    }
                    let mut props = Properties::new();
                    for (key, expr) in properties {
                        let val = eval_expr(expr, rec, conn)?;
                        if val == Value::Null {
                            continue;
                        }
                        props.insert(key.clone(), val);
                    }
                    let id = node::create_node(conn, labels, props.clone())?;
                    let primary_label = labels.first().map(|s| s.as_str()).unwrap_or("");
                    index::update_indexes_for_node(conn, id, primary_label, None, &props)?;
                    if let Some(alias) = alias {
                        bindings.insert(alias.clone(), id);
                        out_rec.set(alias.clone(), Value::I64(id.0 as i64));
                        out_rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
                        // Property keys only — see exec_create_node note re:
                        // compound-binding heuristic and __label.
                        for (k, v) in &props {
                            out_rec.set(format!("{alias}.{k}"), v.clone());
                        }
                    }
                }
                LogicalOp::CreateEdge {
                    src_alias,
                    dst_alias,
                    edge_type,
                    rel_alias,
                    properties,
                } => {
                    let src = bindings.get(src_alias).ok_or_else(|| {
                        GraphError::semantic(format!("unbound variable: {src_alias}"))
                    })?;
                    let dst = bindings.get(dst_alias).ok_or_else(|| {
                        GraphError::semantic(format!("unbound variable: {dst_alias}"))
                    })?;
                    let mut props = Properties::new();
                    for (key, expr) in properties {
                        let val = eval_expr(expr, rec, conn)?;
                        if val == Value::Null {
                            continue;
                        }
                        props.insert(key.clone(), val);
                    }
                    edge::create_edge(conn, *src, *dst, edge_type, props.clone())?;
                    // Bind edge metadata for RETURN access.
                    if let Some(r_alias) = rel_alias {
                        out_rec.set(r_alias.clone(), Value::String(edge_type.clone()));
                        out_rec.set(format!("{r_alias}.__src"), Value::I64(src.0 as i64));
                        out_rec.set(format!("{r_alias}.__dst"), Value::I64(dst.0 as i64));
                        out_rec.set(
                            format!("{r_alias}.__type"),
                            Value::String(edge_type.clone()),
                        );
                        for (key, val) in &props {
                            out_rec.set(format!("{r_alias}.{key}"), val.clone());
                        }
                    }
                }
                _ => {}
            }
        }

        result.push(out_rec);
    }

    Ok(result)
}

fn exec_delete(
    conn: &Connection,
    input: &LogicalOp,
    exprs: &[Expr],
    detach: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;

    // Two-phase delete: collect all entities first, then delete edges, then nodes.
    // This prevents DeleteConnectedNode errors when multiple paths share nodes.
    let mut edges_to_delete: Vec<(NodeId, NodeId, String, Option<u64>)> = Vec::new();
    let mut nodes_to_delete: Vec<NodeId> = Vec::new();

    for rec in &mut records {
        for expr in exprs {
            if let ExprKind::Variable(var) = &expr.kind {
                collect_var_entities(rec, var, &mut edges_to_delete, &mut nodes_to_delete);
                rec.set(format!("{var}.__deleted"), Value::Bool(true));
            } else {
                let val = eval_expr(expr, rec, conn)?;
                collect_value_entities(&val, &mut edges_to_delete, &mut nodes_to_delete);
            }
        }
    }

    // Phase 1: delete all edges.
    for (src, dst, label, seq) in &edges_to_delete {
        if let Some(s) = seq {
            let _ = edge::delete_single_edge(conn, *src, *dst, label, *s);
        } else {
            let _ = edge::delete_edge(conn, *src, *dst, label);
        }
    }

    // Phase 2: delete all nodes (edges already removed).
    for node_id in &nodes_to_delete {
        if !detach && node::node_has_edges(conn, *node_id)? {
            return Err(GraphError::constraint(format!(
                "cannot delete node {} because it still has relationships. Use DETACH DELETE.",
                node_id
            ))
            .with_code(ErrorCode::DeleteConnectedNode));
        }
        let _ = node::delete_node(conn, *node_id);
    }

    Ok(records)
}

/// Collect entities from a variable binding for two-phase delete.
fn collect_var_entities(
    rec: &NamedRecord,
    var: &str,
    edges: &mut Vec<(NodeId, NodeId, String, Option<u64>)>,
    nodes: &mut Vec<NodeId>,
) {
    let edge_src_key = format!("{var}.__src");
    let edge_dst_key = format!("{var}.__dst");
    let edge_type_key = format!("{var}.__type");
    if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
        rec.get(&edge_src_key),
        rec.get(&edge_dst_key),
        rec.get(&edge_type_key),
    ) {
        let edge_seq_key = format!("{var}.__edge_seq");
        let seq = if let Some(Value::I64(s)) = rec.get(&edge_seq_key) {
            Some(*s as u64)
        } else {
            None
        };
        edges.push((NodeId(*src as u64), NodeId(*dst as u64), label.clone(), seq));
    } else if let Some(Value::I64(id)) = rec.get(var) {
        nodes.push(NodeId(*id as u64));
    } else if let Some(val) = rec.get(var).cloned() {
        collect_value_entities(&val, edges, nodes);
    }
}

/// Collect entities from a Value for two-phase delete.
fn collect_value_entities(
    val: &Value,
    edges: &mut Vec<(NodeId, NodeId, String, Option<u64>)>,
    nodes: &mut Vec<NodeId>,
) {
    match val {
        Value::Node(n) => {
            nodes.push(n.id);
        }
        Value::Edge(e) => {
            edges.push((e.src, e.dst, e.label.clone(), None));
        }
        Value::Path(p) => {
            for e in &p.edges {
                edges.push((e.src, e.dst, e.label.clone(), None));
            }
            for n in &p.nodes {
                nodes.push(n.id);
            }
        }
        Value::I64(id) => {
            nodes.push(NodeId(*id as u64));
        }
        Value::List(items) => {
            for item in items {
                collect_value_entities(item, edges, nodes);
            }
        }
        _ => {}
    }
}

fn exec_set_property(
    conn: &Connection,
    input: &LogicalOp,
    assignments: &[crate::cypher::ast::Assignment],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        for assignment in assignments {
            let var = &assignment.variable;
            // Check if this is a relationship variable (has edge identity metadata).
            let edge_src_key = format!("{var}.__src");
            let edge_dst_key = format!("{var}.__dst");
            let edge_type_key = format!("{var}.__type");
            if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
                rec.get(&edge_src_key),
                rec.get(&edge_dst_key),
                rec.get(&edge_type_key),
            ) {
                let val = eval_expr(&assignment.value, rec, conn)?;
                validate_property_value(&val)?;
                let edge_seq_key = format!("{var}.__edge_seq");
                if let Some(Value::I64(seq)) = rec.get(&edge_seq_key) {
                    edge::set_edge_property_at(
                        conn,
                        NodeId(*src as u64),
                        NodeId(*dst as u64),
                        label,
                        *seq as u64,
                        &assignment.property,
                        val.clone(),
                    )?;
                } else {
                    edge::set_edge_property(
                        conn,
                        NodeId(*src as u64),
                        NodeId(*dst as u64),
                        label,
                        &assignment.property,
                        val.clone(),
                    )?;
                }
                // Update record so downstream RETURN sees the new value.
                let prop_key = format!("{var}.{}", assignment.property);
                if val == Value::Null {
                    rec.fields.swap_remove(&prop_key);
                } else {
                    rec.set(prop_key, val);
                }
            } else if let Some(Value::I64(id)) = rec.get(var) {
                let node_id = NodeId(*id as u64);
                let old = node::get_node(conn, node_id)?;
                let val = eval_expr(&assignment.value, rec, conn)?;
                validate_property_value(&val)?;
                node::set_node_property(conn, node_id, &assignment.property, val.clone())?;
                let mut new_props = old.properties.clone();
                if val == Value::Null {
                    new_props.remove(&assignment.property);
                } else {
                    new_props.insert(assignment.property.clone(), val.clone());
                }
                index::update_indexes_for_node(
                    conn,
                    node_id,
                    old.labels.first().map(|s| s.as_str()).unwrap_or(""),
                    Some(&old.properties),
                    &new_props,
                )?;
                // Update record so downstream RETURN sees the new value.
                let prop_key = format!("{var}.{}", assignment.property);
                if val == Value::Null {
                    rec.fields.swap_remove(&prop_key);
                } else {
                    rec.set(prop_key, val);
                }
            }
        }
    }
    Ok(records)
}

/// Validate that a value is storable as a property (no nested maps/nodes/edges).
fn validate_property_value(val: &Value) -> Result<()> {
    match val {
        Value::Map(_) | Value::Node(_) | Value::Edge(_) | Value::Path(_) => {
            return Err(GraphError::type_error(
                crate::types::QueryPhase::Runtime,
                "maps, nodes, relationships, and paths cannot be stored as properties".to_string(),
            )
            .with_code(ErrorCode::InvalidPropertyType));
        }
        Value::List(items) => {
            for item in items {
                validate_property_value(item)?;
            }
        }
        _ => {} // Scalars and Null are fine.
    }
    Ok(())
}

fn exec_set_label(
    conn: &Connection,
    input: &LogicalOp,
    variable: &str,
    labels: &[String],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        // Skip null variables (from OPTIONAL MATCH).
        if let Some(Value::I64(id)) = rec.get(variable) {
            let node_id = NodeId(*id as u64);
            for label in labels {
                node::add_node_label(conn, node_id, label)?;
            }
            // Update the labels in the record (__labels list and __label colon-joined string).
            let labels_key = format!("{variable}.__labels");
            if let Some(Value::List(current_labels)) = rec.get(&labels_key) {
                let mut updated = current_labels.clone();
                for label in labels {
                    let val = Value::String(label.clone());
                    if !updated.contains(&val) {
                        updated.push(val);
                    }
                }
                // Sort for consistency.
                updated.sort_by(|a, b| {
                    let sa = if let Value::String(s) = a {
                        s.as_str()
                    } else {
                        ""
                    };
                    let sb = if let Value::String(s) = b {
                        s.as_str()
                    } else {
                        ""
                    };
                    sa.cmp(sb)
                });
                // Update the colon-joined __label string.
                let label_strs: Vec<&str> = updated
                    .iter()
                    .filter_map(|v| {
                        if let Value::String(s) = v {
                            Some(s.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                let label_key = format!("{variable}.__label");
                rec.set(label_key, Value::String(label_strs.join(":")));
                rec.set(labels_key, Value::List(updated));
            }
        }
    }
    Ok(records)
}

fn exec_set_properties(
    conn: &Connection,
    input: &LogicalOp,
    variable: &str,
    value_expr: &crate::cypher::ast::Expr,
    merge: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        // Edge variant: the variable carries edge identity metadata
        // (`__src`/`__dst`/`__type`, plus `__edge_seq` for parallel edges).
        let edge_src_key = format!("{variable}.__src");
        let edge_dst_key = format!("{variable}.__dst");
        let edge_type_key = format!("{variable}.__type");
        if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
            rec.get(&edge_src_key),
            rec.get(&edge_dst_key),
            rec.get(&edge_type_key),
        ) {
            let src = NodeId(*src as u64);
            let dst = NodeId(*dst as u64);
            let label = label.clone();
            let edge_seq_key = format!("{variable}.__edge_seq");
            let seq = match rec.get(&edge_seq_key) {
                Some(Value::I64(s)) => *s as u64,
                _ => {
                    let prefix = crate::edge::edge_props_prefix(src, dst, &label);
                    let entries = crate::storage::kv::scan_prefix(
                        conn,
                        crate::storage::kv::TABLE_EDGE_PROPS,
                        &prefix,
                    )?;
                    entries
                        .first()
                        .map(|(k, _)| crate::edge::edge_seq_from_key(k, prefix.len()))
                        .unwrap_or(0)
                }
            };

            let map_val = eval_expr(value_expr, rec, conn)?;
            let map = match &map_val {
                Value::Map(m) => m,
                Value::Null => continue,
                _ => {
                    return Err(GraphError::semantic("SET properties requires a map value"));
                }
            };

            let old_props = edge::get_edge_properties_at(conn, src, dst, &label, seq)?;
            let new_props: Properties = if merge {
                let mut props = old_props.clone();
                for (k, v) in map {
                    if *v == Value::Null {
                        props.remove(k);
                    } else {
                        validate_property_value(v)?;
                        props.insert(k.clone(), v.clone());
                    }
                }
                props
            } else {
                let mut props = Properties::new();
                for (k, v) in map {
                    if *v != Value::Null {
                        validate_property_value(v)?;
                        props.insert(k.clone(), v.clone());
                    }
                }
                props
            };

            edge::set_all_edge_properties_at(conn, src, dst, &label, seq, new_props.clone())?;

            // Update the record so downstream RETURN sees the new values.
            for key in old_props.keys() {
                let prop_key = format!("{variable}.{key}");
                rec.remove(&prop_key);
            }
            for (key, val) in &new_props {
                let prop_key = format!("{variable}.{key}");
                rec.set(prop_key, val.clone());
            }
            continue;
        }

        // Skip null variables (from OPTIONAL MATCH).
        if let Some(Value::I64(id)) = rec.get(variable) {
            let node_id = NodeId(*id as u64);
            let map_val = eval_expr(value_expr, rec, conn)?;

            // The map value must be a Map (or Null to skip).
            let map = match &map_val {
                Value::Map(m) => m,
                Value::Null => continue,
                _ => {
                    return Err(GraphError::semantic("SET properties requires a map value"));
                }
            };

            let old = node::get_node(conn, node_id)?;
            let old_props = old.properties.clone();

            let new_props: Properties = if merge {
                // Merge: start from existing, overlay map, remove nulls.
                let mut props = old.properties.clone();
                for (k, v) in map {
                    if *v == Value::Null {
                        props.remove(k);
                    } else {
                        props.insert(k.clone(), v.clone());
                    }
                }
                props
            } else {
                // Overwrite: start from empty, add non-null entries from map.
                let mut props = Properties::new();
                for (k, v) in map {
                    if *v != Value::Null {
                        props.insert(k.clone(), v.clone());
                    }
                }
                props
            };

            node::set_all_node_properties(conn, node_id, new_props.clone())?;
            index::update_indexes_for_node(
                conn,
                node_id,
                old.labels.first().map(|s| s.as_str()).unwrap_or(""),
                Some(&old_props),
                &new_props,
            )?;

            // Update the record: remove old property keys, add new ones.
            // First, remove all old flattened property keys.
            for key in old_props.keys() {
                let prop_key = format!("{variable}.{key}");
                rec.remove(&prop_key);
            }
            // Add new property keys.
            for (key, val) in &new_props {
                let prop_key = format!("{variable}.{key}");
                rec.set(prop_key, val.clone());
            }
        }
    }
    Ok(records)
}

fn exec_remove(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::RemoveItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        for item in items {
            match item {
                crate::cypher::ast::RemoveItem::Property { variable, property } => {
                    // Check if this is an edge variable.
                    let edge_src_key = format!("{variable}.__src");
                    let edge_dst_key = format!("{variable}.__dst");
                    let edge_type_key = format!("{variable}.__type");
                    if let (
                        Some(Value::I64(src)),
                        Some(Value::I64(dst)),
                        Some(Value::String(label)),
                    ) = (
                        rec.get(&edge_src_key),
                        rec.get(&edge_dst_key),
                        rec.get(&edge_type_key),
                    ) {
                        let src = *src;
                        let dst = *dst;
                        let label = label.clone();
                        // Remove edge property by setting to Null.
                        let edge_seq_key = format!("{variable}.__edge_seq");
                        if let Some(Value::I64(seq)) = rec.get(&edge_seq_key) {
                            let seq = *seq;
                            edge::set_edge_property_at(
                                conn,
                                NodeId(src as u64),
                                NodeId(dst as u64),
                                &label,
                                seq as u64,
                                property,
                                Value::Null,
                            )?;
                        } else {
                            edge::set_edge_property(
                                conn,
                                NodeId(src as u64),
                                NodeId(dst as u64),
                                &label,
                                property,
                                Value::Null,
                            )?;
                        }
                        // Update record to reflect removal.
                        let prop_key = format!("{variable}.{property}");
                        rec.fields.swap_remove(&prop_key);
                    } else if let Some(Value::I64(id)) = rec.get(variable) {
                        let id = *id;
                        let node_id = NodeId(id as u64);
                        let old = node::get_node(conn, node_id)?;
                        node::remove_node_property(conn, node_id, property)?;
                        let mut new_props = old.properties.clone();
                        new_props.remove(property);
                        index::update_indexes_for_node(
                            conn,
                            node_id,
                            old.labels.first().map(|s| s.as_str()).unwrap_or(""),
                            Some(&old.properties),
                            &new_props,
                        )?;
                        // Update record to reflect removal.
                        let prop_key = format!("{variable}.{property}");
                        rec.fields.swap_remove(&prop_key);
                    }
                }
                crate::cypher::ast::RemoveItem::Label { variable, labels } => {
                    if let Some(Value::I64(id)) = rec.get(variable) {
                        let id = *id;
                        let node_id = NodeId(id as u64);
                        for label in labels {
                            node::remove_node_label(conn, node_id, label)?;
                        }
                        // Update the labels in the record.
                        let label_key = format!("{variable}.__labels");
                        if let Some(Value::List(current_labels)) = rec.get(&label_key) {
                            let updated: Vec<Value> = current_labels
                                .iter()
                                .filter(|l| {
                                    if let Value::String(s) = l {
                                        !labels.contains(s)
                                    } else {
                                        true
                                    }
                                })
                                .cloned()
                                .collect();
                            rec.set(label_key, Value::List(updated));
                        }
                    }
                }
            }
        }
    }
    Ok(records)
}

/// Apply a single SetItem to a node in a MERGE ON CREATE/ON MATCH context.
fn apply_merge_set_item_node(
    conn: &Connection,
    item: &SetItem,
    node_id: NodeId,
    rec: &NamedRecord,
) -> Result<()> {
    match item {
        SetItem::Property(assignment) => {
            let old = node::get_node(conn, node_id)?;
            let mut a_rec = rec.clone();
            a_rec.set(assignment.variable.clone(), Value::I64(node_id.0 as i64));
            let val = eval_expr(&assignment.value, &a_rec, conn)?;
            node::set_node_property(conn, node_id, &assignment.property, val.clone())?;
            let mut new_props = old.properties.clone();
            new_props.insert(assignment.property.clone(), val);
            index::update_indexes_for_node(
                conn,
                node_id,
                old.labels.first().map(|s| s.as_str()).unwrap_or(""),
                Some(&old.properties),
                &new_props,
            )?;
        }
        SetItem::Label {
            variable: _,
            labels,
        } => {
            for label in labels {
                node::add_node_label(conn, node_id, label)?;
            }
        }
        SetItem::MapMerge { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            for (k, v) in &map {
                node::set_node_property(conn, node_id, k, v.clone())?;
            }
        }
        SetItem::MapOverwrite { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            // Clear existing properties and set new ones.
            let old = node::get_node(conn, node_id)?;
            for key in old.properties.keys() {
                node::set_node_property(conn, node_id, key, Value::Null)?;
            }
            for (k, v) in &map {
                node::set_node_property(conn, node_id, k, v.clone())?;
            }
        }
    }
    Ok(())
}

/// Apply a single SetItem to an edge in a MERGE ON CREATE/ON MATCH context.
fn apply_merge_set_item_edge(
    conn: &Connection,
    item: &SetItem,
    src_id: NodeId,
    dst_id: NodeId,
    edge_type: &str,
    rec: &NamedRecord,
) -> Result<()> {
    match item {
        SetItem::Property(assignment) => {
            let val = eval_expr(&assignment.value, rec, conn)?;
            edge::set_edge_property(conn, src_id, dst_id, edge_type, &assignment.property, val)?;
        }
        SetItem::Label { .. } => {
            // Labels on edges are not standard Cypher; ignore.
        }
        SetItem::MapMerge { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            for (k, v) in &map {
                edge::set_edge_property(conn, src_id, dst_id, edge_type, k, v.clone())?;
            }
        }
        SetItem::MapOverwrite { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            // For overwrite on edges, clear existing props then set new ones.
            let old_props = edge::get_edge_properties(conn, src_id, dst_id, edge_type)?;
            for key in old_props.keys() {
                edge::set_edge_property(conn, src_id, dst_id, edge_type, key, Value::Null)?;
            }
            for (k, v) in &map {
                edge::set_edge_property(conn, src_id, dst_id, edge_type, k, v.clone())?;
            }
        }
    }
    Ok(())
}

/// Apply a single SetItem to a specific parallel edge by sequence number.
fn apply_merge_set_item_edge_at(
    conn: &Connection,
    item: &SetItem,
    src_id: NodeId,
    dst_id: NodeId,
    edge_type: &str,
    seq: u64,
    rec: &NamedRecord,
) -> Result<()> {
    match item {
        SetItem::Property(assignment) => {
            let val = eval_expr(&assignment.value, rec, conn)?;
            edge::set_edge_property_at(
                conn,
                src_id,
                dst_id,
                edge_type,
                seq,
                &assignment.property,
                val,
            )?;
        }
        SetItem::Label { .. } => {}
        SetItem::MapMerge { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            for (k, v) in &map {
                edge::set_edge_property_at(conn, src_id, dst_id, edge_type, seq, k, v.clone())?;
            }
        }
        SetItem::MapOverwrite { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            let old_props = edge::get_edge_properties_at(conn, src_id, dst_id, edge_type, seq)?;
            for key in old_props.keys() {
                edge::set_edge_property_at(conn, src_id, dst_id, edge_type, seq, key, Value::Null)?;
            }
            for (k, v) in &map {
                edge::set_edge_property_at(conn, src_id, dst_id, edge_type, seq, k, v.clone())?;
            }
        }
    }
    Ok(())
}

/// Resolve an expression to a property map. If the expression evaluates to a
/// node ID, load that node's properties. If it evaluates to an edge, use the
/// edge's properties. If it's already a map, use it directly.
fn resolve_to_map(expr: &Expr, rec: &NamedRecord, conn: &Connection) -> Result<Properties> {
    let val = eval_expr(expr, rec, conn)?;
    match val {
        Value::Map(map) => Ok(map.into_iter().collect()),
        Value::Node(n) => Ok(n.properties),
        Value::Edge(e) => Ok(e.properties),
        Value::I64(id) => {
            // Could be a node ID — try to load its properties.
            match node::get_node(conn, NodeId(id as u64)) {
                Ok(n) => Ok(n.properties),
                Err(_) => Ok(Properties::new()),
            }
        }
        Value::Null => Ok(Properties::new()),
        _ => Ok(Properties::new()),
    }
}

fn exec_merge(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
    if pattern.elements.len() == 1 {
        return exec_merge_node(conn, pattern, on_create, on_match);
    }
    // 3-element relationship MERGE: (a:L {p})-[:TYPE]->(b:L {p})
    exec_merge_relationship(conn, pattern, on_create, on_match)
}

/// Single-node MERGE: find-or-create a node matching the pattern.
fn exec_merge_node(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
    let node_pat = match pattern.elements.first() {
        Some(PatternElement::Node(n)) => n,
        _ => unreachable!("MERGE pattern validated at plan time"),
    };

    let all_labels: Vec<&str> = node_pat.labels.iter().map(|s| s.as_str()).collect();
    let alias = node_pat.variable.as_deref().unwrap_or("_merge");

    // Pre-evaluate properties.
    let dummy_rec = NamedRecord::new();
    let mut props = Properties::new();
    for (key, expr) in &node_pat.properties {
        let val = eval_expr(expr, &dummy_rec, conn)?;
        // Null property in MERGE is MergeReadOwnWrites.
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        props.insert(key.clone(), val);
    }

    // Find a node that has ALL required labels and matching properties.
    let matched = find_merge_match_multi_label(conn, &all_labels, &props)?;

    let node_id = match matched {
        Some(n) => {
            let rec = NamedRecord::new();
            for item in on_match {
                apply_merge_set_item_node(conn, item, n.id, &rec)?;
            }
            n.id
        }
        None => {
            let labels: Vec<String> = all_labels.iter().map(|s| s.to_string()).collect();
            let id = node::create_node(conn, &labels, props.clone())?;
            // Update indexes for every label.
            for lbl in &all_labels {
                index::update_indexes_for_node(conn, id, lbl, None, &props)?;
            }

            let rec = NamedRecord::new();
            for item in on_create {
                apply_merge_set_item_node(conn, item, id, &rec)?;
            }
            id
        }
    };

    let mut rec = NamedRecord::new();
    rec.set(alias.to_string(), Value::I64(node_id.0 as i64));
    rec.set(format!("{alias}.__id"), Value::I64(node_id.0 as i64));
    // Populate property bindings from the resolved node so slot-path Property
    // reads land on populated slots. Skip `__label`/`__labels`: that pair
    // triggers `build_compound_binding`, shifting Variable resolution from
    // `Value::I64` to `Value::Node` and breaking downstream code paths that
    // expect the flat shape (see exec_create_node note).
    let resolved = node::get_node(conn, node_id)?;
    for (k, v) in &resolved.properties {
        rec.set(format!("{alias}.{k}"), v.clone());
    }

    // Bind path variable if present: MERGE p = (a {props})
    if let Some(ref path_var) = pattern.path_variable {
        rec.set(path_var.clone(), Value::Path(PathValue::single(resolved)));
    }

    Ok(vec![rec])
}

/// Relationship MERGE: find-or-create nodes and the edge between them.
fn exec_merge_relationship(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
    let src_pat = match &pattern.elements[0] {
        PatternElement::Node(n) => n,
        _ => unreachable!(),
    };
    let rel = match &pattern.elements[1] {
        PatternElement::Relationship(r) => r,
        _ => unreachable!(),
    };
    let dst_pat = match &pattern.elements[2] {
        PatternElement::Node(n) => n,
        _ => unreachable!(),
    };

    let src_label = src_pat.labels.first().map(|s| s.as_str()).unwrap_or("");
    let dst_label = dst_pat.labels.first().map(|s| s.as_str()).unwrap_or("");
    let edge_type = rel.rel_types.first().cloned().unwrap_or_default();

    // Find or create source and destination nodes.
    let src_id = find_or_create_merge_node(conn, src_label, &src_pat.properties)?;
    let dst_id = find_or_create_merge_node(conn, dst_label, &dst_pat.properties)?;

    // Pre-evaluate edge properties and check for null.
    let mut edge_props = Properties::new();
    let dummy_rec = NamedRecord::new();
    for (key, expr) in &rel.properties {
        let val = eval_expr(expr, &dummy_rec, conn)?;
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        edge_props.insert(key.clone(), val);
    }

    // Find or create the edge (checking properties too).
    let edge_match = if edge::edge_exists(conn, src_id, dst_id, &edge_type)? {
        if edge_props.is_empty() {
            true
        } else {
            let existing_props = edge::get_edge_properties(conn, src_id, dst_id, &edge_type)?;
            edge_props
                .iter()
                .all(|(k, v)| existing_props.get(k) == Some(v))
        }
    } else {
        false
    };
    if !edge_match {
        edge::create_edge(conn, src_id, dst_id, &edge_type, edge_props)?;
        let mut rec = NamedRecord::new();
        if let Some(ref v) = src_pat.variable {
            rec.set(v.clone(), Value::I64(src_id.0 as i64));
        }
        if let Some(ref v) = dst_pat.variable {
            rec.set(v.clone(), Value::I64(dst_id.0 as i64));
        }
        for item in on_create {
            apply_merge_set_item_edge(conn, item, src_id, dst_id, &edge_type, &rec)?;
        }
    } else {
        let mut rec = NamedRecord::new();
        if let Some(ref v) = src_pat.variable {
            rec.set(v.clone(), Value::I64(src_id.0 as i64));
        }
        if let Some(ref v) = dst_pat.variable {
            rec.set(v.clone(), Value::I64(dst_id.0 as i64));
        }
        for item in on_match {
            apply_merge_set_item_edge(conn, item, src_id, dst_id, &edge_type, &rec)?;
        }
    }

    let mut rec = NamedRecord::new();
    if let Some(ref v) = src_pat.variable {
        rec.set(v.clone(), Value::I64(src_id.0 as i64));
    }
    if let Some(ref v) = dst_pat.variable {
        rec.set(v.clone(), Value::I64(dst_id.0 as i64));
    }

    // Bind path variable if present: MERGE p = (a)-[:R]->(b)
    if let Some(ref path_var) = pattern.path_variable {
        let src_node = node::get_node(conn, src_id)?;
        let dst_node = node::get_node(conn, dst_id)?;
        let edge_props = edge::get_edge_properties(conn, src_id, dst_id, &edge_type)?;
        let path = PathValue {
            nodes: vec![src_node, dst_node],
            edges: vec![crate::types::Edge {
                src: src_id,
                dst: dst_id,
                label: edge_type.clone(),
                properties: edge_props,
            }],
        };
        rec.set(path_var.clone(), Value::Path(path));
    }

    Ok(vec![rec])
}

/// Find a node matching the MERGE pattern properties, or create it if not found.
fn find_or_create_merge_node(
    conn: &Connection,
    label: &str,
    properties: &HashMap<String, Expr>,
) -> Result<NodeId> {
    let dummy_rec = NamedRecord::new();
    let mut props = Properties::new();
    for (key, expr) in properties {
        let val = eval_expr(expr, &dummy_rec, conn)?;
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        props.insert(key.clone(), val);
    }
    let matched = find_merge_match_evaluated(conn, label, &props)?;
    match matched {
        Some(n) => Ok(n.id),
        None => {
            let labels: Vec<String> = if label.is_empty() {
                vec![]
            } else {
                vec![label.to_string()]
            };
            let id = node::create_node(conn, &labels, props.clone())?;
            index::update_indexes_for_node(conn, id, label, None, &props)?;
            Ok(id)
        }
    }
}

fn exec_match_merge(
    conn: &Connection,
    input: &LogicalOp,
    merge_pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut result = Vec::with_capacity(records.len());

    // Extract the merge pattern structure: (src_node)-[:TYPE]->(dst_node) or single node.
    let elements = &merge_pattern.elements;

    if elements.len() == 1 {
        // Single node MERGE — match-or-create per input record.
        // MERGE produces one output row per matching node (all matches, not just first).
        for rec in &records {
            let node_pat = match &elements[0] {
                PatternElement::Node(n) => n,
                _ => unreachable!(),
            };
            let label = node_pat.labels.first().map(|s| s.as_str()).unwrap_or("");
            let mut props = Properties::new();
            for (key, expr) in &node_pat.properties {
                let val = eval_expr(expr, rec, conn)?;
                // Null property in MERGE is MergeReadOwnWrites.
                if val == Value::Null {
                    return Err(GraphError::semantic("MERGE with null property value")
                        .with_code(ErrorCode::MergeReadOwnWrites));
                }
                props.insert(key.clone(), val);
            }
            let matches = find_merge_matches_evaluated(conn, label, &props)?;
            let alias = node_pat.variable.as_deref().unwrap_or("_merge");

            if matches.is_empty() {
                // No match — create a new node.
                let labels: Vec<String> = if label.is_empty() {
                    vec![]
                } else {
                    vec![label.to_string()]
                };
                let id = node::create_node(conn, &labels, props.clone())?;
                index::update_indexes_for_node(conn, id, label, None, &props)?;
                for item in on_create {
                    apply_merge_set_item_node(conn, item, id, rec)?;
                }
                let mut out_rec = rec.clone();
                out_rec.set(alias.to_string(), Value::I64(id.0 as i64));
                out_rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
                // Populate prop keys (post-ON CREATE) so slot-path Property
                // reads land on populated slots. Skip __label/__labels —
                // see exec_create_node note re: compound-binding heuristic.
                let resolved = node::get_node(conn, id)?;
                for (k, v) in &resolved.properties {
                    out_rec.set(format!("{alias}.{k}"), v.clone());
                }
                // Bind path variable if present.
                if let Some(ref path_var) = merge_pattern.path_variable {
                    out_rec.set(path_var.clone(), Value::Path(PathValue::single(resolved)));
                }
                result.push(out_rec);
            } else {
                // One or more matches — produce one row per matching node.
                for n in &matches {
                    for item in on_match {
                        apply_merge_set_item_node(conn, item, n.id, rec)?;
                    }
                    let mut out_rec = rec.clone();
                    out_rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
                    out_rec.set(format!("{alias}.__id"), Value::I64(n.id.0 as i64));
                    // Re-fetch (ON MATCH may have written) and populate prop keys.
                    let resolved = node::get_node(conn, n.id)?;
                    for (k, v) in &resolved.properties {
                        out_rec.set(format!("{alias}.{k}"), v.clone());
                    }
                    // Bind path variable if present.
                    if let Some(ref path_var) = merge_pattern.path_variable {
                        out_rec.set(path_var.clone(), Value::Path(PathValue::single(resolved)));
                    }
                    result.push(out_rec);
                }
            }
        }
        return Ok(result);
    }

    // Relationship merge: (src)-[:TYPE {props}]->(dst)
    if elements.len() != 3 {
        return Err(GraphError::semantic(
            "MERGE pattern must be a single node or (node)-[rel]->(node)",
        ));
    }

    let src_node = match &elements[0] {
        PatternElement::Node(n) => n,
        _ => return Err(GraphError::semantic("expected node pattern")),
    };
    let rel = match &elements[1] {
        PatternElement::Relationship(r) => r,
        _ => return Err(GraphError::semantic("expected relationship pattern")),
    };
    let dst_node = match &elements[2] {
        PatternElement::Node(n) => n,
        _ => return Err(GraphError::semantic("expected node pattern")),
    };

    let src_var = src_node
        .variable
        .as_deref()
        .ok_or_else(|| GraphError::semantic("MERGE relationship source must have a variable"))?;
    let dst_var = dst_node
        .variable
        .as_deref()
        .ok_or_else(|| GraphError::semantic("MERGE relationship target must have a variable"))?;
    let edge_type = rel.rel_types.first().cloned().unwrap_or_default();
    let undirected = matches!(rel.direction, crate::cypher::ast::RelDirection::Undirected);

    for rec in &records {
        let src_id = match rec.get(src_var) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };
        let dst_id = match rec.get(dst_var) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };

        let mut out_rec = rec.clone();

        // Pre-evaluate merge pattern edge properties.
        let mut merge_edge_props = Properties::new();
        for (key, expr) in &rel.properties {
            let val = eval_expr(expr, rec, conn)?;
            if val == Value::Null {
                return Err(GraphError::semantic("MERGE with null property value")
                    .with_code(ErrorCode::MergeReadOwnWrites));
            }
            merge_edge_props.insert(key.clone(), val);
        }

        // Find all matching edges (possibly multiple parallel edges).
        // Each matching edge produces one output row.
        let find_matching_edges = |s: NodeId, d: NodeId| -> Result<Vec<(u64, Properties)>> {
            let all = edge::get_all_edge_props(conn, s, d, &edge_type)?;
            if merge_edge_props.is_empty() {
                return Ok(all);
            }
            Ok(all
                .into_iter()
                .filter(|(_, props)| {
                    merge_edge_props
                        .iter()
                        .all(|(k, v)| props.get(k) == Some(v))
                })
                .collect())
        };

        // Collect matches from forward direction, and reverse for undirected.
        let mut matched_edges: Vec<(NodeId, NodeId, u64, Properties)> = Vec::new();
        for (seq, props) in find_matching_edges(src_id, dst_id)? {
            matched_edges.push((src_id, dst_id, seq, props));
        }
        if undirected {
            for (seq, props) in find_matching_edges(dst_id, src_id)? {
                matched_edges.push((dst_id, src_id, seq, props));
            }
        }

        if matched_edges.is_empty() {
            // No match — create new edge.
            edge::create_edge(conn, src_id, dst_id, &edge_type, merge_edge_props)?;
            for item in on_create {
                apply_merge_set_item_edge(conn, item, src_id, dst_id, &edge_type, rec)?;
            }
            // Bind the relationship variable for the newly created edge.
            if let Some(ref r_alias) = rel.variable {
                out_rec.set(r_alias.clone(), Value::String(edge_type.clone()));
                out_rec.set(format!("{r_alias}.__src"), Value::I64(src_id.0 as i64));
                out_rec.set(format!("{r_alias}.__dst"), Value::I64(dst_id.0 as i64));
                out_rec.set(
                    format!("{r_alias}.__type"),
                    Value::String(edge_type.clone()),
                );
                let edge_props = edge::get_edge_properties(conn, src_id, dst_id, &edge_type)?;
                for (key, val) in &edge_props {
                    out_rec.set(format!("{r_alias}.{key}"), val.clone());
                }
            }
            // Bind path variable if present: MERGE p = (a)-[:R]->(b)
            if let Some(ref path_var) = merge_pattern.path_variable {
                let src_node = node::get_node(conn, src_id)?;
                let dst_node = node::get_node(conn, dst_id)?;
                let ep = edge::get_edge_properties(conn, src_id, dst_id, &edge_type)?;
                out_rec.set(
                    path_var.clone(),
                    Value::Path(PathValue {
                        nodes: vec![src_node, dst_node],
                        edges: vec![crate::types::Edge {
                            src: src_id,
                            dst: dst_id,
                            label: edge_type.clone(),
                            properties: ep,
                        }],
                    }),
                );
            }
            result.push(out_rec);
        } else {
            // One or more matches — produce one row per matching edge.
            for (m_src, m_dst, m_seq, _) in &matched_edges {
                for item in on_match {
                    apply_merge_set_item_edge_at(
                        conn, item, *m_src, *m_dst, &edge_type, *m_seq, rec,
                    )?;
                }
            }
            for (m_src, m_dst, m_seq, _) in &matched_edges {
                let mut match_rec = rec.clone();
                if let Some(ref r_alias) = rel.variable {
                    match_rec.set(r_alias.clone(), Value::String(edge_type.clone()));
                    match_rec.set(format!("{r_alias}.__src"), Value::I64(m_src.0 as i64));
                    match_rec.set(format!("{r_alias}.__dst"), Value::I64(m_dst.0 as i64));
                    match_rec.set(
                        format!("{r_alias}.__type"),
                        Value::String(edge_type.clone()),
                    );
                    match_rec.set(format!("{r_alias}.__edge_seq"), Value::I64(*m_seq as i64));
                    let edge_props =
                        edge::get_edge_properties_at(conn, *m_src, *m_dst, &edge_type, *m_seq)?;
                    for (key, val) in &edge_props {
                        match_rec.set(format!("{r_alias}.{key}"), val.clone());
                    }
                }
                // Bind path variable if present.
                if let Some(ref path_var) = merge_pattern.path_variable {
                    let src_node = node::get_node(conn, *m_src)?;
                    let dst_node = node::get_node(conn, *m_dst)?;
                    let ep =
                        edge::get_edge_properties_at(conn, *m_src, *m_dst, &edge_type, *m_seq)?;
                    match_rec.set(
                        path_var.clone(),
                        Value::Path(PathValue {
                            nodes: vec![src_node, dst_node],
                            edges: vec![crate::types::Edge {
                                src: *m_src,
                                dst: *m_dst,
                                label: edge_type.clone(),
                                properties: ep,
                            }],
                        }),
                    );
                }
                result.push(match_rec);
            }
        }
    }

    Ok(result)
}

/// Execute a CALL procedure: evaluate arguments, filter matching rows, yield columns.
fn exec_call(
    conn: &Connection,
    input: &LogicalOp,
    procedure_name: &str,
    args: &[Expr],
    yield_items: &[(String, Option<String>)],
    _yield_star: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;

    let proc_def = ctx.procedures.get(procedure_name).ok_or_else(|| {
        GraphError::Query(crate::types::QueryError::ProcedureError {
            phase: crate::types::QueryPhase::Runtime,
            message: format!("ProcedureNotFound: unknown procedure `{procedure_name}`"),
            code: ErrorCode::Other,
            hint: None,
            span: None,
        })
    })?;

    let mut results = Vec::new();

    for rec in &records {
        // Evaluate argument expressions.
        let mut eval_args = Vec::new();
        for arg in args {
            eval_args.push(eval_expr(arg, rec, conn)?);
        }

        // Filter procedure data rows by matching input values.
        let matching_rows: Vec<_> = if proc_def.inputs.is_empty() || eval_args.is_empty() {
            proc_def.rows.iter().collect()
        } else {
            proc_def
                .rows
                .iter()
                .filter(|row| {
                    proc_def
                        .inputs
                        .iter()
                        .zip(&eval_args)
                        .all(|(param, arg_val)| match row.get(&param.name) {
                            Some(row_val) => values_match(row_val, arg_val),
                            None => true,
                        })
                })
                .collect()
        };

        if yield_items.is_empty() {
            // No columns to yield. For standalone CALL, this produces an empty result.
            // For in-query CALL (multi-clause), the rows pass through unchanged.
            // In multi-clause context, the input records carry bindings from prior clauses.
            // We check if the input record has any bindings: if yes, it's in-query context.
            if !rec.fields.is_empty() {
                results.push(rec.clone());
            }
            // Otherwise, standalone CALL with no outputs → produce no rows.
        } else if matching_rows.is_empty() {
            // Procedure has outputs but no matching rows — produce no rows.
        } else {
            for data_row in &matching_rows {
                let mut new_rec = rec.clone();
                for (col, alias) in yield_items {
                    let bind_name = alias.as_ref().unwrap_or(col);
                    if let Some(val) = data_row.get(col) {
                        new_rec.set(bind_name.clone(), val.clone());
                    } else {
                        new_rec.set(bind_name.clone(), Value::Null);
                    }
                }
                results.push(new_rec);
            }
        }
    }

    check_row_limit(&results, ctx)?;
    Ok(results)
}

/// Compare two values for procedure row filtering, with numeric coercion.
fn values_match(row_val: &Value, arg_val: &Value) -> bool {
    match (row_val, arg_val) {
        (Value::I64(a), Value::I64(b)) => a == b,
        (Value::F64(a), Value::F64(b)) => a == b,
        (Value::I64(a), Value::F64(b)) => (*a as f64) == *b,
        (Value::F64(a), Value::I64(b)) => *a == (*b as f64),
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Null, Value::Null) => true,
        _ => row_val == arg_val,
    }
}

fn exec_unwind(
    conn: &Connection,
    input: &LogicalOp,
    expr: &Expr,
    alias: &str,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &records {
        let val = eval_expr(expr, rec, conn)?;
        match val {
            Value::List(items) => {
                for item in items {
                    let mut new_rec = rec.clone();
                    // Expand nodes/edges into flat bindings for property access.
                    match &item {
                        Value::Node(n) => {
                            let node_rec = node_to_record(n, alias);
                            for (k, v) in &node_rec.fields {
                                new_rec.set(k.clone(), v.clone());
                            }
                        }
                        Value::Edge(e) => {
                            new_rec.set(alias.to_string(), Value::Edge(e.clone()));
                            new_rec.set(format!("{alias}.__src"), Value::I64(e.src.0 as i64));
                            new_rec.set(format!("{alias}.__dst"), Value::I64(e.dst.0 as i64));
                            new_rec.set(format!("{alias}.__type"), Value::String(e.label.clone()));
                            for (k, v) in &e.properties {
                                new_rec.set(format!("{alias}.{k}"), v.clone());
                            }
                        }
                        _ => {
                            new_rec.set(alias.to_string(), item);
                        }
                    }
                    results.push(new_rec);
                }
            }
            Value::Null => {
                // UNWIND null produces no rows (like UNWIND []).
            }
            _ => {
                return Err(GraphError::semantic(format!(
                    "UNWIND requires a list, got: {val}"
                )));
            }
        }
    }

    Ok(results)
}

/// Build a Path value from node and edge bindings in each record.
fn exec_materialize_path(
    conn: &Connection,
    input: &LogicalOp,
    path_alias: &str,
    node_aliases: &[String],
    rel_aliases: &[String],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in records {
        // If any node alias is Null (from unmatched OPTIONAL MATCH),
        // the entire path is Null.
        let mut has_null = false;
        let mut nodes = Vec::new();
        for alias in node_aliases {
            if rec.get(alias) == Some(&Value::Null) {
                has_null = true;
                break;
            }
            if let Some(Value::I64(id)) = rec.get(&format!("{alias}.__id")) {
                match node::get_node(conn, NodeId(*id as u64)) {
                    Ok(n) => nodes.push(n),
                    Err(_) => break,
                }
            }
        }
        let mut edges = Vec::new();
        for alias in rel_aliases {
            if rec.get(alias) == Some(&Value::Null) {
                has_null = true;
                break;
            }
            if let Some(Value::List(edge_list)) = rec.get(alias) {
                for item in edge_list {
                    if let Value::Edge(e) = item {
                        edges.push(e.clone());
                    }
                }
            } else if let Some(Value::Edge(e)) = build_compound_binding(&rec, alias) {
                edges.push(e);
            }
        }

        // Var-length path node reconstruction.
        let has_var_length_rel = rel_aliases
            .iter()
            .any(|a| matches!(rec.get(a), Some(Value::List(_))));
        if !has_null && has_var_length_rel {
            if edges.is_empty() {
                // Zero-length var-length path: collapse to single start node.
                nodes.truncate(1);
            } else {
                // Rebuild full node list from edge endpoints. This is necessary
                // because with multiple var-length segments and zero-length matches,
                // the static node_aliases may contain duplicate intermediate nodes.
                let mut full_nodes = vec![nodes[0].clone()];
                for edge in &edges {
                    // Determine next node: for undirected traversals, the
                    // edge may be stored in either direction. Pick the
                    // endpoint that differs from the previous node.
                    let prev_id = full_nodes.last().unwrap().id;
                    let next_id = if prev_id == edge.src {
                        edge.dst
                    } else {
                        edge.src
                    };
                    let n = node::get_node(conn, next_id)?;
                    full_nodes.push(n);
                }
                // Use the last node from node_aliases if available (has correct label filter).
                if nodes.len() >= 2 {
                    *full_nodes.last_mut().unwrap() = nodes.last().unwrap().clone();
                }
                nodes = full_nodes;
            }
        }

        let mut new_rec = rec;
        if has_null {
            new_rec.set(path_alias.to_string(), Value::Null);
        } else if !nodes.is_empty() {
            new_rec.set(
                path_alias.to_string(),
                Value::Path(PathValue { nodes, edges }),
            );
        }
        results.push(new_rec);
    }

    Ok(results)
}

/// Aggregate pre-computed records (used by correlated execution).
/// Reuses the existing `compute_aggregate` function.
pub(super) fn exec_aggregate_over_records(
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

/// Materialize a `PathValue` by resolving each node and each hop's edge.
///
/// `label` is the edge type the path was traversed on. Edge direction for each
/// hop is resolved by checking both orientations — required for
/// `Direction::Both` traversals where any given hop may run forward or backward.
fn build_path_value(
    conn: &Connection,
    node_ids: Vec<NodeId>,
    label: &str,
) -> Result<crate::types::PathValue> {
    use crate::types::{Edge, PathValue};

    if node_ids.is_empty() {
        return Ok(PathValue {
            nodes: Vec::new(),
            edges: Vec::new(),
        });
    }

    let mut nodes = Vec::with_capacity(node_ids.len());
    for id in &node_ids {
        nodes.push(crate::node::get_node(conn, *id)?);
    }

    let mut edges = Vec::with_capacity(node_ids.len().saturating_sub(1));
    for pair in node_ids.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        // Try forward first; fall back to reverse for Direction::Both traversals.
        let (src, dst) = if edge::edge_exists(conn, a, b, label)? {
            (a, b)
        } else {
            (b, a)
        };
        let properties = edge::get_edge_properties(conn, src, dst, label)?;
        edges.push(Edge {
            src,
            dst,
            label: label.to_string(),
            properties,
        });
    }

    Ok(PathValue { nodes, edges })
}

#[allow(clippy::too_many_arguments)]
fn exec_shortest_path(
    conn: &Connection,
    input: &LogicalOp,
    src_alias: &str,
    dst_alias: &str,
    path_alias: &str,
    edge_type: Option<&str>,
    direction: Direction,
    max_hops: u32,
    all_paths: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let label = edge_type.unwrap_or("");
    let mut results = Vec::new();

    for rec in &records {
        let src_id = match rec.get(src_alias).and_then(value_to_node_id) {
            Some(id) => id,
            _ => continue,
        };
        let dst_id = match rec.get(dst_alias).and_then(value_to_node_id) {
            Some(id) => id,
            _ => continue,
        };

        if all_paths {
            let paths = edge::all_shortest_paths(conn, src_id, dst_id, label, direction, max_hops)?;
            if paths.is_empty() {
                // No path found — emit record with null path.
                let mut new_rec = rec.clone();
                new_rec.set(path_alias.to_string(), Value::Null);
                results.push(new_rec);
            } else {
                for path in paths {
                    let path_value = build_path_value(conn, path, label)?;
                    let mut new_rec = rec.clone();
                    new_rec.set(path_alias.to_string(), Value::Path(path_value));
                    results.push(new_rec);
                }
            }
        } else {
            let path = edge::shortest_path(conn, src_id, dst_id, label, direction, max_hops)?;
            let mut new_rec = rec.clone();
            match path {
                Some(p) => {
                    let path_value = build_path_value(conn, p, label)?;
                    new_rec.set(path_alias.to_string(), Value::Path(path_value));
                }
                None => new_rec.set(path_alias.to_string(), Value::Null),
            }
            results.push(new_rec);
        }
    }

    Ok(results)
}

/// Find a node that has ALL of `labels` and matches `properties`.
///
/// Uses the first label for index/scan and then filters by remaining labels.
fn find_merge_match_multi_label(
    conn: &Connection,
    labels: &[&str],
    properties: &Properties,
) -> Result<Option<crate::types::Node>> {
    let primary = labels.first().copied().unwrap_or("");
    let candidates = find_merge_matches_by_label_and_props(conn, primary, properties)?;
    // Require the node to carry every label in the pattern.
    Ok(candidates
        .into_iter()
        .find(|n| labels.iter().all(|req| n.labels.iter().any(|l| l == req))))
}

/// Collect all nodes matching a single label + property values (no multi-label filter).
///
/// When `label` is empty, performs a full node scan (handles label-less `MERGE (a)`).
fn find_merge_matches_by_label_and_props(
    conn: &Connection,
    label: &str,
    properties: &Properties,
) -> Result<Vec<crate::types::Node>> {
    // Try indexed lookup on the primary label (skipped when label is empty).
    let indexes = index::list_indexes_for_label(conn, label)?;
    let indexed_props: Vec<&str> = indexes.iter().map(|(_, p)| p.as_str()).collect();

    for (key, value) in properties {
        if indexed_props.contains(&key.as_str()) {
            let ids = index::index_lookup(conn, label, key, value)?;
            let mut matches = Vec::new();
            for id in ids {
                let n = node::get_node(conn, id)?;
                let all_match = properties
                    .iter()
                    .all(|(k, expected)| n.properties.get(k) == Some(expected));
                if all_match {
                    matches.push(n);
                }
            }
            return Ok(matches);
        }
    }

    // Fall back to label scan.
    let existing = node::find_nodes_by_label(conn, label)?;
    Ok(existing
        .into_iter()
        .filter(|n| {
            properties
                .iter()
                .all(|(key, expected)| n.properties.get(key) == Some(expected))
        })
        .collect())
}

/// Find a node matching label + pre-evaluated property values.
fn find_merge_match_evaluated(
    conn: &Connection,
    label: &str,
    properties: &Properties,
) -> Result<Option<crate::types::Node>> {
    // Try to find an indexed property.
    let indexes = index::list_indexes_for_label(conn, label)?;
    let indexed_props: Vec<&str> = indexes.iter().map(|(_, p)| p.as_str()).collect();

    for (key, value) in properties {
        if indexed_props.contains(&key.as_str()) {
            let ids = index::index_lookup(conn, label, key, value)?;
            // Filter candidates by remaining properties.
            for id in ids {
                let n = node::get_node(conn, id)?;
                let all_match = properties
                    .iter()
                    .all(|(k, expected)| n.properties.get(k) == Some(expected));
                if all_match {
                    return Ok(Some(n));
                }
            }
            return Ok(None);
        }
    }

    // No index available — fall back to label scan.
    let existing = node::find_nodes_by_label(conn, label)?;
    Ok(existing.into_iter().find(|n| {
        properties
            .iter()
            .all(|(key, expected)| n.properties.get(key) == Some(expected))
    }))
}

/// Find ALL nodes matching label + pre-evaluated property values.
fn find_merge_matches_evaluated(
    conn: &Connection,
    label: &str,
    properties: &Properties,
) -> Result<Vec<crate::types::Node>> {
    // Try indexed lookup first.
    let indexes = index::list_indexes_for_label(conn, label)?;
    let indexed_props: Vec<&str> = indexes.iter().map(|(_, p)| p.as_str()).collect();

    for (key, value) in properties {
        if indexed_props.contains(&key.as_str()) {
            let ids = index::index_lookup(conn, label, key, value)?;
            let mut matches = Vec::new();
            for id in ids {
                let n = node::get_node(conn, id)?;
                let all_match = properties
                    .iter()
                    .all(|(k, expected)| n.properties.get(k) == Some(expected));
                if all_match {
                    matches.push(n);
                }
            }
            return Ok(matches);
        }
    }

    // No index — label scan.
    let existing = node::find_nodes_by_label(conn, label)?;
    Ok(existing
        .into_iter()
        .filter(|n| {
            properties
                .iter()
                .all(|(key, expected)| n.properties.get(key) == Some(expected))
        })
        .collect())
}

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
fn type_rank(v: &Value) -> u8 {
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

fn compare_values_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
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
fn record_matches_bindings(rec: &NamedRecord, bindings: &[(String, Value)]) -> bool {
    bindings.iter().all(|(key, outer_val)| match rec.get(key) {
        Some(inner_val) => inner_val == outer_val,
        None => true,
    })
}

fn agg_fn_name(f: AggregateFunction) -> &'static str {
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
fn agg_col_name(agg: &AggregateExpr) -> String {
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
