use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use rusqlite::Connection;
use tracing::debug;

use crate::cypher::ir::*;
use crate::cypher::record::NamedRecord;
pub(crate) use crate::cypher::size_estimate::{
    approx_named_bytes, approx_slot_bytes, approx_value_bytes,
};
use crate::types::{GraphError, Result};

use crate::cypher::procedure::ProcedureRegistry;

mod accumulate;
mod correlated;
use correlated::{exec_correlated, exec_correlated_join, exec_left_outer_join, value_to_node_id};

/// Execution context carrying runtime limits.
pub struct ExecContext {
    /// Maximum rows any operator may produce. 0 = unlimited.
    pub max_result_rows: usize,
    /// Maximum hop count for any pattern traversal (`Expand` / `ShortestPath`).
    /// Enforced before execution. 0 = unlimited.
    pub max_traversal_depth: u32,
    /// Maximum total edge-visit budget for a single var-length traversal
    /// (enforced inside `traverse_paths`) and for all `ShortestPath`
    /// searches in a query combined. 0 = unlimited.
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
    /// Edge visits spent so far by `ShortestPath` searches in this query,
    /// checked against `max_traversal_work` across all of them. Shared
    /// (`Rc`) with the contexts of subqueries run inside the query.
    pub shortest_path_work: Rc<Cell<u64>>,
    /// Approximate budget, in bytes, for values the query keeps
    /// materialized (see `Config::max_materialized_bytes`). 0 = unlimited.
    pub max_materialized_bytes: u64,
    /// Bytes charged against `max_materialized_bytes` so far. Shared
    /// (`Rc`) with the contexts of subqueries run inside the query.
    pub materialized_used: Rc<Cell<u64>>,
    /// Maximum byte size of a single property value written by the query.
    /// 0 = unlimited. Mirrors `Config::max_property_value_bytes`.
    pub max_property_value_bytes: usize,
    /// Maximum byte length of label / relationship-type / property-key names
    /// written by the query. 0 = unlimited. Mirrors `Config::max_name_bytes`.
    pub max_name_bytes: usize,
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
            shortest_path_work: Rc::new(Cell::new(0)),
            max_materialized_bytes: 1 << 30,
            materialized_used: Rc::new(Cell::new(0)),
            max_property_value_bytes: 0,
            max_name_bytes: 0,
        }
    }
}

/// The read limits of the query executing on this thread, published by
/// [`ExecLimitsScope`]. Subqueries (EXISTS, pattern predicates, pattern
/// comprehensions) are reached through expression evaluation, which has no
/// `ExecContext`, so they read their limits from here.
struct ExecLimits {
    max_result_rows: usize,
    max_traversal_depth: u32,
    max_traversal_work: u64,
    shortest_path_work: Rc<Cell<u64>>,
    max_materialized_bytes: u64,
    materialized_used: Rc<Cell<u64>>,
    /// Whether UNWIND may stop at the row cap while it builds rows. True for
    /// read-only queries; false for writes, whose bulk `UNWIND ... CREATE`
    /// input is bounded by the materialization budget instead (the row cap
    /// limits what a query returns, and a write returns nothing).
    cap_unwind_rows: Cell<bool>,
}

thread_local! {
    static EXEC_LIMITS: RefCell<Option<ExecLimits>> = const { RefCell::new(None) };
}

/// RAII guard that publishes a query's limits for the duration of its
/// execution and restores the previous ones (supporting nested executions)
/// on drop. Installed by [`execute_with_ctx`]; mirrors
/// `write::WriteLimitsScope`.
pub(crate) struct ExecLimitsScope {
    prev: Option<ExecLimits>,
}

impl ExecLimitsScope {
    pub(crate) fn enter(ctx: &ExecContext, cap_unwind_rows: bool) -> Self {
        let limits = ExecLimits {
            max_result_rows: ctx.max_result_rows,
            max_traversal_depth: ctx.max_traversal_depth,
            max_traversal_work: ctx.max_traversal_work,
            shortest_path_work: Rc::clone(&ctx.shortest_path_work),
            max_materialized_bytes: ctx.max_materialized_bytes,
            materialized_used: Rc::clone(&ctx.materialized_used),
            cap_unwind_rows: Cell::new(cap_unwind_rows),
        };
        let prev = EXEC_LIMITS.with(|l| l.replace(Some(limits)));
        Self { prev }
    }
}

impl Drop for ExecLimitsScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        EXEC_LIMITS.with(|l| *l.borrow_mut() = prev);
    }
}

impl ExecContext {
    /// Context for a subquery run from expression evaluation: the enclosing
    /// query's row cap, traversal depth and work limits, and its shared
    /// `shortestPath` budget. Falls back to [`ExecContext::default`] when no
    /// query is executing on this thread.
    pub(crate) fn for_subquery() -> Self {
        EXEC_LIMITS.with(|l| match &*l.borrow() {
            Some(limits) => ExecContext {
                max_result_rows: limits.max_result_rows,
                max_traversal_depth: limits.max_traversal_depth,
                max_traversal_work: limits.max_traversal_work,
                shortest_path_work: Rc::clone(&limits.shortest_path_work),
                max_materialized_bytes: limits.max_materialized_bytes,
                materialized_used: Rc::clone(&limits.materialized_used),
                ..Default::default()
            },
            None => ExecContext::default(),
        })
    }
}

/// Charge `bytes` of newly materialized data against the running query's
/// budget (`Config::max_materialized_bytes`), failing with
/// `GraphError::SizeLimit` once the cumulative total exceeds it. A no-op
/// outside a query or when the budget is 0 (unlimited). Called by every
/// operator that buffers rows or groups, so a query cannot hold more than
/// about the budget no matter how wide its rows are.
pub(crate) fn charge_materialized(bytes: u64) -> Result<()> {
    EXEC_LIMITS.with(|l| match &*l.borrow() {
        Some(limits) if limits.max_materialized_bytes > 0 => {
            let used = limits.materialized_used.get().saturating_add(bytes);
            limits.materialized_used.set(used);
            materialized_within_budget(used, limits.max_materialized_bytes)
        }
        _ => Ok(()),
    })
}

/// Fail unless `bytes` more could be materialized without exceeding the
/// budget; charges nothing. Used before allocating a large list in one go
/// (`range()`, `+`, comprehensions), whose contents are charged when they
/// land in a buffered row.
pub(crate) fn check_materialized_headroom(bytes: u64) -> Result<()> {
    EXEC_LIMITS.with(|l| match &*l.borrow() {
        Some(limits) if limits.max_materialized_bytes > 0 => materialized_within_budget(
            limits.materialized_used.get().saturating_add(bytes),
            limits.max_materialized_bytes,
        ),
        _ => Ok(()),
    })
}

/// Current materialized-bytes counter, to restore with
/// [`reset_materialized`] once an operator has dropped its transient data.
pub(crate) fn materialized_mark() -> u64 {
    EXEC_LIMITS.with(|l| l.borrow().as_ref().map_or(0, |l| l.materialized_used.get()))
}

/// Set the counter back to `mark`. Charges are cumulative while an operator
/// runs; once it returns, only its output is still alive, so callers reset
/// to the mark taken before it ran and charge the output alone.
pub(crate) fn reset_materialized(mark: u64) {
    EXEC_LIMITS.with(|l| {
        if let Some(limits) = &*l.borrow() {
            limits.materialized_used.set(mark);
        }
    });
}

/// Give back bytes charged for data that has been dropped.
pub(crate) fn release_materialized(bytes: u64) {
    EXEC_LIMITS.with(|l| {
        if let Some(limits) = &*l.borrow() {
            let used = limits.materialized_used.get();
            limits.materialized_used.set(used.saturating_sub(bytes));
        }
    });
}

/// Row cap for the rows an UNWIND builds: the result row cap for read-only
/// queries, 0 (none) for writes. See [`ExecLimits::cap_unwind_rows`].
pub(in crate::cypher::executor) fn unwind_row_cap(ctx: &ExecContext) -> usize {
    let capped = EXEC_LIMITS.with(|l| l.borrow().as_ref().is_none_or(|l| l.cap_unwind_rows.get()));
    if capped {
        ctx.max_result_rows
    } else {
        0
    }
}

/// While alive, UNWIND stops enforcing the row cap on the rows it builds.
/// Held around the input of an aggregation: a long UNWIND consumed by
/// `sum`/`count`/`collect` produces a small result, so only the
/// materialization budget bounds it.
pub(in crate::cypher::executor) struct UnwindCapSuspended(bool);

impl UnwindCapSuspended {
    pub(in crate::cypher::executor) fn new() -> Self {
        let prev = EXEC_LIMITS.with(|l| {
            l.borrow()
                .as_ref()
                .is_none_or(|l| l.cap_unwind_rows.replace(false))
        });
        Self(prev)
    }
}

impl Drop for UnwindCapSuspended {
    fn drop(&mut self) {
        let prev = self.0;
        EXEC_LIMITS.with(|l| {
            if let Some(l) = &*l.borrow() {
                l.cap_unwind_rows.set(prev);
            }
        });
    }
}

/// A row set that is dropped at the end of a loop iteration: releases its
/// budget charge when dropped, so re-running the right side of a join per
/// left row does not accumulate charges.
pub(in crate::cypher::executor) struct TransientRows(u64);

impl TransientRows {
    pub(in crate::cypher::executor) fn of(rows: &[NamedRecord]) -> Self {
        Self(rows.iter().map(approx_named_bytes).sum())
    }
}

impl Drop for TransientRows {
    fn drop(&mut self) {
        release_materialized(self.0);
    }
}

fn materialized_within_budget(total: u64, budget: u64) -> Result<()> {
    if total <= budget {
        return Ok(());
    }
    Err(GraphError::SizeLimit {
        what: "materialized query data in bytes".to_string(),
        limit: usize::try_from(budget).unwrap_or(usize::MAX),
        actual: usize::try_from(total).unwrap_or(usize::MAX),
        hint: Some(
            "add LIMIT or a more selective MATCH, aggregate instead of collecting, \
             or raise Config::max_materialized_bytes"
                .to_string(),
        ),
    })
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
    row_count_within_cap(results.len(), ctx.max_result_rows)
}

/// Error once `rows` exceeds `max_rows` (0 = unlimited). Shared by every
/// row-cap check so the error is the same on all execution paths.
pub(crate) fn row_count_within_cap(rows: usize, max_rows: usize) -> Result<()> {
    if max_rows > 0 && rows > max_rows {
        return Err(GraphError::constraint(format!(
            "result set exceeded maximum of {max_rows} rows"
        )));
    }
    Ok(())
}

/// Append a row produced by a materializing operator, failing as soon as
/// the output exceeds the row cap rather than after it is fully built.
pub(in crate::cypher::executor) fn push_capped(
    results: &mut Vec<NamedRecord>,
    rec: NamedRecord,
    ctx: &ExecContext,
) -> Result<()> {
    charge_materialized(approx_named_bytes(&rec))?;
    results.push(rec);
    check_row_limit(results, ctx)
}

/// Charge rows that an operator built in bulk (e.g. a label scan) against
/// the materialization budget.
pub(in crate::cypher::executor) fn charge_named_rows(rows: &[NamedRecord]) -> Result<()> {
    for rec in rows {
        charge_materialized(approx_named_bytes(rec))?;
    }
    Ok(())
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
    let _write_limits =
        write::WriteLimitsScope::enter(ctx.max_property_value_bytes, ctx.max_name_bytes);
    let _exec_limits = ExecLimitsScope::enter(ctx, is_read_only(plan));
    if !iter_slot::is_slot_supported(plan) {
        let result = exec(conn, plan, ctx)?;
        if is_bare_write(plan) {
            return Ok(vec![]);
        }
        // Operators that can explode check the cap while producing rows;
        // this covers the rest (e.g. UNWIND, which is bounded only by the
        // list cap) at the result boundary, as the slot path does.
        check_row_limit(&result, ctx)?;
        return Ok(result);
    }
    let mut iter = iter_slot::build_slot_iter(conn, plan, ctx)?;
    if is_bare_write(plan) {
        // A write without RETURN has no result rows: drain the iterator to
        // perform the writes, without collecting (or row-capping) its
        // output, so a bulk `UNWIND ... CREATE` is not limited by the cap.
        while iter.next_slot()?.is_some() {}
        return Ok(vec![]);
    }
    iter_slot::collect_to_named(&mut *iter, ctx.max_result_rows)
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

/// Run one operator of the materialized executor and account for its output
/// in the query's materialization budget. While the operator runs its
/// charges are cumulative (inputs, join output, ...); when it returns, only
/// the output remains alive, so the counter is reset to its value before the
/// call plus the output's size.
pub(in crate::cypher::executor) fn exec(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mark = materialized_mark();
    let rows = exec_op(conn, plan, ctx)?;
    reset_materialized(mark);
    charge_named_rows(&rows)?;
    Ok(rows)
}

fn exec_op(conn: &Connection, plan: &LogicalOp, ctx: &ExecContext) -> Result<Vec<NamedRecord>> {
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
            &NamedRecord::new(),
            None,
        ),

        LogicalOp::IdLookup { alias, value_expr } => {
            // Top-level (non-correlated) — evaluate against an empty record.
            // Constant exprs (literal/param/etc.) work fine; anything that
            // references row state at this level would be a planner bug.
            exec_id_lookup(conn, alias, value_expr, &NamedRecord::new(), None)
        }

        LogicalOp::FullTextLookup {
            label,
            alias,
            property,
            op,
            term,
            case_insensitive,
            remaining_filters,
        } => read::exec_fulltext_lookup(
            conn,
            label,
            alias,
            property,
            *op,
            *case_insensitive,
            term,
            remaining_filters.as_ref(),
            &NamedRecord::new(),
            None,
        ),

        LogicalOp::Expand { .. } => exec_expand(conn, plan, ctx),

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
        } => exec_merge(conn, pattern, on_create, on_match, ctx),

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
        } => {
            let records = exec(conn, input, ctx)?;
            materialize_paths(conn, records, path_alias, node_aliases, rel_aliases)
        }

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

        LogicalOp::ShortestPath { .. } => exec_shortest_path(conn, plan, ctx),

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
                dedup_union_rows(&mut results);
            }
            Ok(results)
        }
    }
}

/// Deduplicate the rows of a plain (non-ALL) UNION, keeping first
/// occurrences in order. Compare rows ignoring the synthetic per-branch
/// `__fts_score` key: an OR-chain rewritten to `Union(FullTextLookup, ...)`
/// gives the same node a different BM25 score in each disjunct, so including
/// it here would defeat dedup and return the node once per matching branch.
fn dedup_union_rows(results: &mut Vec<NamedRecord>) {
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
        if !b
            .get(key)
            .is_some_and(|bv| crate::cypher::eval::grouping_eq(val, bv))
        {
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
    distinct_records, exec_aggregate, exec_aggregate_over_records, exec_distinct, exec_limit,
    exec_skip, exec_sort, sort_records,
};
use call::exec_call;
use merge::{exec_match_merge, exec_merge};
use path::{exec_shortest_path, extend_with_shortest_paths, materialize_paths};
use read::{
    exec_cross_product, exec_expand, exec_filter, exec_id_lookup, exec_index_lookup, exec_project,
    exec_scan, exec_unwind, expand_all, has_duplicate_relationships, unwind_records,
};
use util::{merge_outer, AliasBinding};
use write::{
    exec_create_edge, exec_create_index, exec_create_node, exec_create_sequence, exec_delete,
    exec_drop_index, exec_match_create, exec_remove, exec_set_label, exec_set_properties,
    exec_set_property,
};

// Public surface — anything previously `pub`/`pub(crate)` on mod.rs that
// moved to a submodule. External callers reach these as
// `cypher::executor::<name>` exactly as before.
pub(crate) use accumulate::StreamingAggregate;
pub(crate) use read::{
    build_compound_binding, compound_binding_vars, eval_var_length_prop_filters, expand_record,
    index_lookup_ids,
};
pub(crate) use util::agg_col_name as aggregate_column_name;
pub(in crate::cypher) use util::compare_values_for_sort;
pub use util::{exec_correlated_exists, exec_correlated_subquery};
pub(crate) use util::{
    node_to_record, node_to_record_pub, populate_edge_bindings, resolve_lookup_key,
};
