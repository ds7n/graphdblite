use std::cell::RefCell;
use std::collections::HashMap;

use rusqlite::Connection;
use tracing::{debug, instrument};

use crate::cypher::ir::*;
use crate::cypher::record::NamedRecord;
use crate::types::{GraphError, Result};

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
    /// Per-query bounded LRU of compiled regexes for the `=~` operator.
    pub regex_cache: RefCell<crate::cypher::eval::regex_cache::RegexCache>,
}

impl Default for ExecContext {
    fn default() -> Self {
        Self {
            max_result_rows: 0,
            max_traversal_depth: 0,
            max_traversal_work: 10_000_000,
            procedures: ProcedureRegistry::default(),
            require_read_only: false,
            regex_cache: RefCell::new(crate::cypher::eval::regex_cache::RegexCache::default()),
        }
    }
}

/// Walk the plan tree and reject any traversal whose hop count exceeds the
/// configured `max_traversal_depth`. Returns early when the cap is 0
/// (unlimited).
pub(in crate::cypher::executor) fn validate_traversal_depth(
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<()> {
    if ctx.max_traversal_depth == 0 {
        return Ok(());
    }
    check_depth_recursive(plan, ctx.max_traversal_depth)
}

pub(in crate::cypher::executor) fn check_depth_recursive(op: &LogicalOp, cap: u32) -> Result<()> {
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
        | LogicalOp::IdLookup { .. }
        | LogicalOp::FullTextLookup { .. }
        | LogicalOp::CreateNode { .. }
        | LogicalOp::CreateEdge { .. }
        | LogicalOp::Merge { .. }
        | LogicalOp::CreateIndex { .. }
        | LogicalOp::DropIndex { .. }
        | LogicalOp::EmptyRow => Ok(()),
    }
}

/// Check that a result set hasn't exceeded the row cap.
pub(in crate::cypher::executor) fn check_row_limit(
    results: &[NamedRecord],
    ctx: &ExecContext,
) -> Result<()> {
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
pub(in crate::cypher::executor) fn is_bare_write(plan: &LogicalOp) -> bool {
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
            | LogicalOp::CreateIndex { .. }
            | LogicalOp::DropIndex { .. }
    )
}

/// Execute with an explicit context carrying runtime limits.
///
/// Runs the slot-indexed iterator stack in [`crate::cypher::iter_slot`] for
/// any plan it supports, falling back transparently to the materialized
/// named executor for the remaining ops (`MaterializePath`, `ShortestPath`,
/// `Call`).
pub fn execute_with_ctx(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    use crate::cypher::iter_slot;

    validate_traversal_depth(plan, ctx)?;
    if !iter_slot::is_slot_supported(plan) {
        let result = exec(conn, plan, ctx)?;
        return if is_bare_write(plan) {
            Ok(vec![])
        } else {
            Ok(result)
        };
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
        | LogicalOp::IdLookup { .. }
        | LogicalOp::FullTextLookup { .. }
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

        // Write operations (including DDL).
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
        | LogicalOp::CreateIndex { .. }
        | LogicalOp::DropIndex { .. } => false,
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

pub(in crate::cypher::executor) fn exec(
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
            index_properties,
            lookups,
            remaining_filters,
        } => exec_index_lookup(
            conn,
            label,
            alias,
            index_properties,
            lookups,
            remaining_filters.as_ref(),
        ),

        LogicalOp::IdLookup { alias, value_expr } => {
            // Top-level (non-correlated) — evaluate against an empty record.
            // Constant exprs (literal/param/etc.) work fine; anything that
            // references row state at this level would be a planner bug.
            exec_id_lookup(conn, alias, value_expr, &NamedRecord::new())
        }

        LogicalOp::FullTextLookup {
            label,
            alias,
            property,
            op,
            term,
            remaining_filters,
        } => read::exec_fulltext_lookup(
            conn,
            label,
            alias,
            property,
            *op,
            term,
            remaining_filters.as_ref(),
            &NamedRecord::new(),
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

        LogicalOp::CreateIndex { label, properties } => exec_create_index(conn, label, properties),

        LogicalOp::DropIndex { label, properties } => exec_drop_index(conn, label, properties),

        LogicalOp::Union { inputs, all } => {
            let mut results = Vec::new();
            for input in inputs {
                results.extend(exec(conn, input, ctx)?);
            }
            if !all {
                // Deduplicate for plain UNION. Compare rows ignoring the
                // synthetic per-branch `__fts_score` key: an OR-chain rewritten
                // to `Union(FullTextLookup, …)` gives the same node a different
                // BM25 score in each disjunct, so including it here would defeat
                // dedup and return the node once per matching branch.
                let mut seen: Vec<NamedRecord> = Vec::new();
                results.retain(|rec| {
                    if seen.iter().any(|s| union_rows_equal(s, rec)) {
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

/// Whether two records are equal for UNION dedup purposes.
///
/// Identical to `a.fields == b.fields` except that the synthetic
/// `<alias>.__fts_score` binding is ignored. That key holds a per-branch BM25
/// score set by FTS-driven scans; the same node reached through two disjuncts of
/// an OR-chain rewritten to `Union(FullTextLookup, …)` carries a different score
/// per branch, and those rows must still dedup to one.
fn union_rows_equal(a: &NamedRecord, b: &NamedRecord) -> bool {
    fn is_fts_score(key: &str) -> bool {
        key.ends_with(".__fts_score")
    }
    let a_len = a.fields.iter().filter(|(k, _)| !is_fts_score(k)).count();
    let b_len = b.fields.iter().filter(|(k, _)| !is_fts_score(k)).count();
    if a_len != b_len {
        return false;
    }
    for (key, val) in &a.fields {
        if is_fts_score(key) {
            continue;
        }
        if b.get(key) != Some(val) {
            return false;
        }
    }
    true
}

// === executor split: submodule declarations ===

mod aggregate;
mod call;
mod merge;
mod path;
mod read;
mod util;
mod write;

// Sibling submodule imports for the dispatcher. Globs of `pub(in
// crate::cypher::executor)` items don't propagate cleanly through a
// `use foo::*;` glob, so the dispatcher lists what it actually calls.
// Items also re-exported (`pub use` / `pub(crate) use` below) need not
// be imported a second time — the re-export brings them into scope.
use aggregate::{
    exec_aggregate, exec_aggregate_over_records, exec_distinct, exec_limit, exec_skip, exec_sort,
};
use call::exec_call;
use merge::{exec_match_merge, exec_merge};
use path::{exec_materialize_path, exec_shortest_path};
use read::{
    collect_flat_edge_ids, exec_cross_product, exec_expand, exec_filter, exec_id_lookup,
    exec_index_lookup, exec_project, exec_scan, exec_unwind, has_duplicate_relationships,
};
use write::{
    exec_create_edge, exec_create_index, exec_create_node, exec_create_sequence, exec_delete,
    exec_drop_index, exec_match_create, exec_remove, exec_set_label, exec_set_properties,
    exec_set_property,
};

// Public surface — anything previously `pub`/`pub(crate)` on mod.rs that
// moved to a submodule. External callers reach these as
// `cypher::executor::<name>` exactly as before.
pub(crate) use aggregate::aggregate_slot_records;
pub(crate) use read::{
    build_compound_binding, compound_binding_vars, exec_fulltext_lookup, expand_record,
    index_lookup_ids, is_user_visible_field,
};
pub(in crate::cypher) use util::compare_values_for_sort;
pub use util::{exec_correlated_exists, exec_correlated_subquery, execute_first_match};
pub(crate) use util::{
    fetch_and_populate, literal_to_value, node_to_record, node_to_record_pub, resolve_lookup_key,
};
