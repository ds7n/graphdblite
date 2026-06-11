//! Slot-indexed pull-based iterator stack for the record-v2 migration.
//!
//! Mirrors `cypher::iter::RecordIter` but yields [`record_v2::Record`]s
//! addressed by [`SlotId`]. Phase 3b lands the first three operators
//! (`Scan`, `IndexLookup`, `Project`) plus the `EmptyRow`/`SingleRow`
//! leaves; everything else falls back to the named path via
//! [`is_slot_supported`].
//!
//! Each iterator carries the [`RecordSchema`] of the rows it produces.
//! Schemas are computed by [`schema_infer`] off the plan subtree at
//! `build_slot_iter` time and threaded through downstream operators.
//! At the API boundary, [`collect_to_named`] converts the slot records
//! back to `NamedRecord` using the final schema.

use rusqlite::Connection;

use crate::cypher::ast::{Expr, ExprKind, ReturnItem};
use crate::cypher::eval::{eval_expr, eval_predicate, expr_to_column_name};
use crate::cypher::executor::{literal_to_value, ExecContext};
use crate::cypher::ir::LogicalOp;
use crate::cypher::record::NamedRecord;
use crate::cypher::record_v2::{Record as SlotRecord, RecordSchema, SlotId};
use crate::cypher::record_view::SlotView;
use crate::cypher::schema_infer::{collect_property_refs, infer_with_props, PropertyRefs};
use crate::node;
use crate::types::{NodeId, Result, Value};

/// Pull-based iterator that yields slot-indexed records.
///
/// Each implementor knows its output [`RecordSchema`]; downstream
/// operators query it to wire slot lookups.
pub trait SlotRecordIter {
    /// Return the next record, or `None` when exhausted.
    fn next_slot(&mut self) -> Result<Option<SlotRecord>>;

    /// Schema of the records this iterator yields.
    fn schema(&self) -> &RecordSchema;
}

/// Drain a slot iterator, converting each row into a [`NamedRecord`] using
/// the iterator's output schema. Used at the API boundary in
/// [`super::executor::execute_with_ctx_slot`].
pub fn collect_to_named(iter: &mut dyn SlotRecordIter) -> Result<Vec<NamedRecord>> {
    let schema = iter.schema().clone();
    let mut out = Vec::new();
    while let Some(rec) = iter.next_slot()? {
        out.push(slot_to_named(&schema, &rec));
    }
    Ok(out)
}

fn slot_to_named(schema: &RecordSchema, rec: &SlotRecord) -> NamedRecord {
    let mut nr = NamedRecord::new();
    for (slot, name) in schema.iter() {
        let v = rec.get(slot).clone();
        nr.set(name.to_string(), v);
    }
    nr
}

/// True iff every operator in `plan` has a slot-path implementation in
/// Phase 3c. Anything else routes to the named path. Phase 3d–3g extend
/// this set as operators land.
///
/// **Phase 3c coverage.** Adds `Filter` (any predicate via materialize +
/// `eval_predicate`) and widens `Project` to allow any expression except
/// `Variable` and `Star` — those still need compound binding which lands
/// in Phase 3d. `Property` is schema-aware on the fast path: when the
/// input schema has the slot we read it directly; otherwise we fall
/// through to materialize + `eval_expr` so subfield access on non-node
/// values (`d.year` on a temporal etc.) still works.
pub fn is_slot_supported(plan: &LogicalOp) -> bool {
    match plan {
        LogicalOp::EmptyRow | LogicalOp::SingleRow => true,
        LogicalOp::Scan { .. } => true,
        // FullTextLookup is not slot-supported in v1 (Task 17 codifies this).
        LogicalOp::FullTextLookup { .. } => false,
        LogicalOp::IndexLookup {
            remaining_filters, ..
        } => match remaining_filters {
            // Multi-property pattern predicates where one prop is indexed
            // and the rest are residuals. The builder wraps the index hits
            // in a slot-aware FilterSlotIter — same shape as a top-level
            // Filter, so the same constraints apply.
            Some(f) => !expr_has_bare_variable(f),
            None => true,
        },
        LogicalOp::Filter { input, predicate } => {
            is_slot_supported(input) && !expr_has_bare_variable(predicate)
        }
        LogicalOp::Project { input, items, .. } => {
            is_slot_supported(input) && items.iter().all(is_project_item_supported)
        }
        LogicalOp::Aggregate {
            input,
            group_keys,
            aggregates,
        } => {
            // Aggregate runs the named `aggregate_named_records` helper on
            // input materialized to NamedRecords, so anything `eval_expr`
            // handles works — including bare Variable group keys (resolved
            // via `build_compound_binding` from flat metadata + properties)
            // and `count(*)` (Star input is never evaluated). Pattern
            // subqueries open scopes the slot path doesn't track; gate them.
            is_slot_supported(input)
                && group_keys.iter().all(|e| !expr_has_pattern_subquery(e))
                && aggregates
                    .iter()
                    .all(|a| !expr_has_pattern_subquery(&a.input))
        }
        // Phase 3f pass-through ops. Each preserves the input schema and
        // applies a row-level transformation that is independent of which
        // backing record shape carries the row — the slot impls below do
        // the work natively.
        LogicalOp::Skip { input, .. } | LogicalOp::Limit { input, .. } => is_slot_supported(input),
        LogicalOp::Distinct { input } => is_slot_supported(input),
        LogicalOp::Sort { input, items } => {
            // Sort keys may reference bare variables (compound binding for
            // node/edge ordering); fall back when they do.
            is_slot_supported(input) && items.iter().all(|s| !expr_has_bare_variable(&s.expr))
        }
        LogicalOp::Unwind { input, expr, .. } => {
            // Unwind expression evaluates against a materialized NamedRecord
            // (same trade-off as 3c–3e). Pattern subqueries open scopes the
            // slot path doesn't track; reject those. Bare Variable in the
            // unwind expr is fine — it resolves through `build_compound_binding`
            // on the materialized view.
            is_slot_supported(input) && !expr_has_pattern_subquery(expr)
        }
        LogicalOp::Expand {
            input,
            var_length_prop_filters,
            ..
        } => {
            // ExpandIter now delegates to `executor::read::expand_record`,
            // which is `exec_expand`'s per-row body — same semantics for
            // every shape (labeled / untyped, any direction, parallel
            // edges). The only remaining slot gate: var-length predicates
            // that reference the per-hop bare variable still need the
            // named correlated path.
            is_slot_supported(input)
                && var_length_prop_filters
                    .values()
                    .all(|e| !expr_has_bare_variable(e))
        }
        // Phase 3g.1 — correlated. Real slot-native join logic is open
        // question #4 in the plan and warrants its own design pass; for
        // now we bridge: route the entire join subtree through `exec()`
        // (the named materialized path) and present the result as a
        // slot iter so upstream Project/Aggregate/Sort can stay on the
        // slot path. No perf win at the join itself, but no regression
        // either — and it widens dual-run coverage.
        LogicalOp::CrossProduct { .. }
        | LogicalOp::CorrelatedJoin { .. }
        | LogicalOp::LeftOuterJoin { .. } => true,
        // Phase 4.1 — write-path bridge. Mirrors the 3g.1 join bridge:
        // route the entire write subtree through the named `exec()` and
        // present the resulting records as a slot iter via
        // `MaterializedSlotIter`. The write itself happens during iter
        // construction (named exec materializes), so a bare-write plan
        // (no RETURN) still mutates exactly once and `is_bare_write` in
        // `execute_with_ctx_slot` continues to discard the result rows.
        // No perf win at the write, no regression — extends dual_run
        // coverage to every TCK scenario per Phase 4.2. Slot-native
        // writes land in a follow-up.
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
        | LogicalOp::DropIndex { .. } => true,
        _ => false,
    }
}

/// Phase 3c: a Project item qualifies for the slot path only if its
/// expression has no embedded bare `Variable` or `Star`. The named-path
/// `Variable` handler builds compound `Value::Node` / `Value::Edge`
/// values out of `alias.__id` plus every `alias.<prop>` key in the
/// record — slot records only carry properties referenced as
/// `Property(alias, prop)` somewhere in the plan, so dynamic property
/// access through a Variable (e.g. `[123, n]` followed by `(list[1]).x`,
/// or `properties(p)`) sees a node with empty properties on the slot
/// path. Phase 3d adds compound binding for slots and lifts this.
fn is_project_item_supported(item: &ReturnItem) -> bool {
    !expr_has_bare_variable(&item.expr)
}

/// True iff `e` contains a bare `Variable`, `Star`, or any sub-expression
/// whose `Variable` reference can resolve to a compound node/edge value.
/// Conservatively pessimistic — false positives just mean falling back to
/// the named path, which is correct.
fn expr_has_bare_variable(e: &Expr) -> bool {
    use ExprKind::*;
    match &e.kind {
        Variable(_) | Star => true,
        Literal(_) | Parameter(_) | HasLabel(_, _) => false,
        // `Property(v, p)` is a flat-key read, not a compound dereference,
        // so we can keep it on the slot path — `is_project_item_supported`
        // (and the runtime fallback in `eval_project_item`) handle missing
        // slots safely.
        Property(_, _) => false,
        BinaryOp { left, right, .. } => {
            expr_has_bare_variable(left) || expr_has_bare_variable(right)
        }
        Not(x) | IsNull(x) | IsNotNull(x) => expr_has_bare_variable(x),
        FunctionCall { args, .. } => args.iter().any(expr_has_bare_variable),
        Case {
            operand,
            alternatives,
            default,
        } => {
            operand.as_deref().is_some_and(expr_has_bare_variable)
                || alternatives
                    .iter()
                    .any(|(c, r)| expr_has_bare_variable(c) || expr_has_bare_variable(r))
                || default.as_deref().is_some_and(expr_has_bare_variable)
        }
        List(xs) => xs.iter().any(expr_has_bare_variable),
        ListComprehension {
            list_expr,
            filter,
            map_expr,
            ..
        } => {
            expr_has_bare_variable(list_expr)
                || filter.as_deref().is_some_and(expr_has_bare_variable)
                || map_expr.as_deref().is_some_and(expr_has_bare_variable)
        }
        // Pattern comprehensions / EXISTS / pattern predicates introduce
        // new scopes that may bind aliases the slot path doesn't track —
        // be conservative.
        PatternComprehension { .. } | Exists { .. } | ExistsSubquery(_) | PatternPredicate(_) => {
            true
        }
        MapLiteral(entries) => entries.iter().any(|(_, v)| expr_has_bare_variable(v)),
        Index { expr, index } => expr_has_bare_variable(expr) || expr_has_bare_variable(index),
        DotAccess { expr, .. } => expr_has_bare_variable(expr),
        Slice { expr, start, end } => {
            expr_has_bare_variable(expr)
                || start.as_deref().is_some_and(expr_has_bare_variable)
                || end.as_deref().is_some_and(expr_has_bare_variable)
        }
        Quantifier {
            list_expr,
            predicate,
            ..
        } => expr_has_bare_variable(list_expr) || expr_has_bare_variable(predicate),
    }
}

/// True iff `e` contains a pattern-based subquery construct that opens a
/// scope the slot path doesn't track (`PatternComprehension`, `Exists`,
/// `ExistsSubquery`, `PatternPredicate`). Used by the `Aggregate` gate
/// where bare `Variable` is fine (the named eval rebuilds compound nodes
/// from the materialized record) but pattern scopes are not.
fn expr_has_pattern_subquery(e: &Expr) -> bool {
    use ExprKind::*;
    match &e.kind {
        Variable(_) | Star | Literal(_) | Parameter(_) | HasLabel(_, _) | Property(_, _) => false,
        PatternComprehension { .. } | Exists { .. } | ExistsSubquery(_) | PatternPredicate(_) => {
            true
        }
        BinaryOp { left, right, .. } => {
            expr_has_pattern_subquery(left) || expr_has_pattern_subquery(right)
        }
        Not(x) | IsNull(x) | IsNotNull(x) => expr_has_pattern_subquery(x),
        FunctionCall { args, .. } => args.iter().any(expr_has_pattern_subquery),
        Case {
            operand,
            alternatives,
            default,
        } => {
            operand.as_deref().is_some_and(expr_has_pattern_subquery)
                || alternatives
                    .iter()
                    .any(|(c, r)| expr_has_pattern_subquery(c) || expr_has_pattern_subquery(r))
                || default.as_deref().is_some_and(expr_has_pattern_subquery)
        }
        List(xs) => xs.iter().any(expr_has_pattern_subquery),
        ListComprehension {
            list_expr,
            filter,
            map_expr,
            ..
        } => {
            expr_has_pattern_subquery(list_expr)
                || filter.as_deref().is_some_and(expr_has_pattern_subquery)
                || map_expr.as_deref().is_some_and(expr_has_pattern_subquery)
        }
        MapLiteral(entries) => entries.iter().any(|(_, v)| expr_has_pattern_subquery(v)),
        Index { expr, index } => {
            expr_has_pattern_subquery(expr) || expr_has_pattern_subquery(index)
        }
        DotAccess { expr, .. } => expr_has_pattern_subquery(expr),
        Slice { expr, start, end } => {
            expr_has_pattern_subquery(expr)
                || start.as_deref().is_some_and(expr_has_pattern_subquery)
                || end.as_deref().is_some_and(expr_has_pattern_subquery)
        }
        Quantifier {
            list_expr,
            predicate,
            ..
        } => expr_has_pattern_subquery(list_expr) || expr_has_pattern_subquery(predicate),
    }
}

// ---------------------------------------------------------------------------
// Builder

/// Build a slot iterator tree for `plan`. Caller must have first verified
/// [`is_slot_supported`].
///
/// Property references are collected from the **whole** plan in a single
/// pre-pass so every subtree's schema agrees on which prop slots to
/// reserve — without this, a `Scan` deep in the tree wouldn't see that an
/// upstream `Project` reads `p.name`.
pub fn build_slot_iter<'a>(
    conn: &'a Connection,
    plan: &'a LogicalOp,
    ctx: &'a ExecContext,
) -> Result<Box<dyn SlotRecordIter + 'a>> {
    let mut refs = PropertyRefs::new();
    collect_property_refs(plan, &mut refs);
    build_slot_iter_inner(conn, plan, ctx, &refs)
}

fn build_slot_iter_inner<'a>(
    conn: &'a Connection,
    plan: &'a LogicalOp,
    ctx: &'a ExecContext,
    refs: &PropertyRefs,
) -> Result<Box<dyn SlotRecordIter + 'a>> {
    match plan {
        LogicalOp::EmptyRow | LogicalOp::SingleRow => Ok(Box::new(EmptyRowSlotIter::new())),

        LogicalOp::Scan { label, alias } => {
            let schema = infer_with_props(plan, refs);
            let nodes = node::find_nodes_by_label(conn, label)?;
            let records: Vec<SlotRecord> = nodes
                .iter()
                .map(|n| node_to_slot_record(n, alias, &schema))
                .collect();
            Ok(Box::new(VecSlotIter::new(records, schema)))
        }

        LogicalOp::IndexLookup {
            label,
            alias,
            index_properties,
            lookups,
            remaining_filters,
        } => {
            let schema = infer_with_props(plan, refs);
            let node_ids =
                crate::cypher::executor::index_lookup_ids(conn, label, index_properties, lookups)?;
            let mut records = Vec::with_capacity(node_ids.len());
            for id in node_ids {
                let n = node::get_node(conn, id)?;
                records.push(node_to_slot_record(&n, alias, &schema));
            }
            let base: Box<dyn SlotRecordIter + 'a> = Box::new(VecSlotIter::new(records, schema));
            // When the planner attached a residual predicate (multi-prop
            // pattern where only one prop is indexed), wrap with a slot
            // FilterSlotIter — same materialize-and-eval pattern as a
            // top-level Filter, no schema change.
            match remaining_filters {
                Some(f) => Ok(Box::new(FilterSlotIter {
                    input: base,
                    predicate: f.clone(),
                    conn,
                })),
                None => Ok(base),
            }
        }

        LogicalOp::Filter { input, predicate } => {
            let input_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            Ok(Box::new(FilterSlotIter {
                input: input_iter,
                predicate: predicate.clone(),
                conn,
            }))
        }

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
            result_cap: _,
        } => {
            // Reuse the named ExpandIter via slot↔named adapters: in 3d
            // we trade per-row materialize/convert for not duplicating
            // ~180 lines of dense traversal logic. Phase 5 re-baseline
            // measures away the conversion overhead.
            let input_slot_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            let output_schema = infer_with_props(plan, refs);
            let named_input: Box<dyn crate::cypher::iter::RecordIter + 'a> =
                Box::new(SlotToNamedAdapter::new(input_slot_iter));
            let prop_filter_values: std::collections::HashMap<String, Value> =
                var_length_prop_filters
                    .iter()
                    .filter_map(|(k, e)| match &e.kind {
                        ExprKind::Literal(lit) => Some((k.clone(), literal_to_value(lit))),
                        _ => None,
                    })
                    .collect();
            let named_expand = crate::cypher::iter::ExpandIter::new(
                named_input,
                conn,
                src_alias.clone(),
                dst_alias.clone(),
                rel_alias.clone(),
                edge_types.clone(),
                *direction,
                *min_hops,
                *max_hops,
                *var_length,
                prop_filter_values,
                ctx.max_traversal_work,
            );
            Ok(Box::new(NamedToSlotAdapter::new(
                Box::new(named_expand),
                output_schema,
            )))
        }

        LogicalOp::Aggregate {
            input,
            group_keys,
            aggregates,
        } => {
            let input_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            let output_schema = infer_with_props(plan, refs);
            Ok(Box::new(AggregateSlotIter::new(
                input_iter,
                group_keys.clone(),
                aggregates.clone(),
                output_schema,
                conn,
            )))
        }

        LogicalOp::Skip { input, count } => {
            let input_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            Ok(Box::new(SkipSlotIter {
                input: input_iter,
                remaining_to_skip: *count,
            }))
        }

        LogicalOp::Limit { input, count } => {
            let input_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            Ok(Box::new(LimitSlotIter {
                input: input_iter,
                remaining: *count,
            }))
        }

        LogicalOp::Distinct { input } => {
            let input_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            Ok(Box::new(DistinctSlotIter::new(input_iter)))
        }

        LogicalOp::Sort { input, items } => {
            let input_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            Ok(Box::new(SortSlotIter::new(input_iter, items.clone(), conn)))
        }

        LogicalOp::CrossProduct { .. }
        | LogicalOp::CorrelatedJoin { .. }
        | LogicalOp::LeftOuterJoin { .. }
        // Phase 4.1 — same bridge shape for write ops. The named `exec()`
        // performs the mutation and produces the result records (matched
        // bindings plus newly-created entities); we adapt to slots so any
        // upstream Project/Sort/Limit can stay on the slot path.
        | LogicalOp::CreateNode { .. }
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
        | LogicalOp::DropIndex { .. } => {
            // Phase 3g.1 / 4.1 bridge — see is_slot_supported comment.
            // Run the whole subtree through `exec()` and adapt to slots.
            let output_schema = infer_with_props(plan, refs);
            let records = crate::cypher::executor::exec_pub(conn, plan, ctx)?;
            Ok(Box::new(MaterializedSlotIter::new(records, output_schema)))
        }

        LogicalOp::Unwind { input, expr, alias } => {
            let input_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            let output_schema = infer_with_props(plan, refs);
            Ok(Box::new(UnwindSlotIter::new(
                input_iter,
                expr.clone(),
                alias.clone(),
                output_schema,
                conn,
            )))
        }

        LogicalOp::Project {
            input,
            items,
            emit_compound,
        } => {
            let input_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            let output_schema = infer_with_props(plan, refs);
            Ok(Box::new(ProjectSlotIter::new(
                input_iter,
                items.clone(),
                *emit_compound,
                output_schema,
                conn,
            )))
        }

        // Caller must check `is_slot_supported` first; reaching here is a bug.
        _ => unreachable!(
            "build_slot_iter called on unsupported op `{}` — caller must \
             gate with is_slot_supported()",
            plan.op_name()
        ),
    }
}

/// Build a [`NamedRecord`] from a slot record using `schema`. Used as a
/// shim when the slot path needs to call into the named-path `eval` for
/// expressions richer than slot-fast-path leaves (Phase 3c) — the
/// resulting NamedRecord carries every flat key the named eval may
/// expect (`alias.__id`, `alias.__label`, etc.), so semantics match
/// exactly what the named path would have computed.
///
/// Per-row allocation costs the immediate slot perf win for these calls;
/// the win returns when `eval` itself becomes slot-aware. Phase 5
/// re-baselines benchmarks once the migration is complete.
pub fn materialize_named(schema: &RecordSchema, rec: &SlotRecord) -> NamedRecord {
    let mut nr = NamedRecord::new();
    for (slot, name) in schema.iter() {
        nr.set(name.to_string(), rec.get(slot).clone());
    }
    nr
}

/// Materialize a `Node` into a slot record using `schema`. Bindings absent
/// from the schema are silently dropped — the schema declares which keys
/// downstream operators may read, so unreferenced fields cost nothing.
pub fn node_to_slot_record(
    n: &crate::types::Node,
    alias: &str,
    schema: &RecordSchema,
) -> SlotRecord {
    debug_assert!(n.id.0 <= i64::MAX as u64, "NodeId exceeds i64::MAX");
    let mut rec = SlotRecord::with_capacity(schema.len());

    if let Some(s) = schema.slot(alias) {
        rec.set(s, Value::I64(n.id.0 as i64));
    }
    let id_key = format!("{alias}.__id");
    if let Some(s) = schema.slot(&id_key) {
        rec.set(s, Value::I64(n.id.0 as i64));
    }
    let label_key = format!("{alias}.__label");
    if let Some(s) = schema.slot(&label_key) {
        rec.set(s, Value::String(n.labels.join(":")));
    }
    let labels_key = format!("{alias}.__labels");
    if let Some(s) = schema.slot(&labels_key) {
        rec.set(
            s,
            Value::List(n.labels.iter().map(|l| Value::String(l.clone())).collect()),
        );
    }
    for (key, val) in &n.properties {
        let prop_key = format!("{alias}.{key}");
        if let Some(s) = schema.slot(&prop_key) {
            rec.set(s, val.clone());
        }
    }
    rec
}

// ---------------------------------------------------------------------------
// Leaf iterators

/// Yields a single empty record once, then stops.
pub struct EmptyRowSlotIter {
    schema: RecordSchema,
    done: bool,
}

impl EmptyRowSlotIter {
    pub fn new() -> Self {
        Self {
            schema: RecordSchema::new(),
            done: false,
        }
    }
}

impl Default for EmptyRowSlotIter {
    fn default() -> Self {
        Self::new()
    }
}

impl SlotRecordIter for EmptyRowSlotIter {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        if self.done {
            Ok(None)
        } else {
            self.done = true;
            Ok(Some(SlotRecord::new()))
        }
    }
    fn schema(&self) -> &RecordSchema {
        &self.schema
    }
}

/// Wraps a pre-materialized `Vec<NamedRecord>` (typically from running a
/// subtree through the named `exec()`) and presents it as a slot iter
/// against a fixed output schema. Used in Phase 3g.1 to bridge correlated
/// operators — keeping upstream Project / Aggregate / Sort on the slot
/// path even when the join itself doesn't have a slot-native impl yet.
pub struct MaterializedSlotIter {
    records: std::vec::IntoIter<NamedRecord>,
    schema: RecordSchema,
}

impl MaterializedSlotIter {
    pub fn new(records: Vec<NamedRecord>, schema: RecordSchema) -> Self {
        Self {
            records: records.into_iter(),
            schema,
        }
    }
}

impl SlotRecordIter for MaterializedSlotIter {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        Ok(self.records.next().map(|r| named_to_slot(&self.schema, &r)))
    }
    fn schema(&self) -> &RecordSchema {
        &self.schema
    }
}

/// Iterates over pre-materialized slot records (for leaf scans).
pub struct VecSlotIter {
    records: std::vec::IntoIter<SlotRecord>,
    schema: RecordSchema,
}

impl VecSlotIter {
    pub fn new(records: Vec<SlotRecord>, schema: RecordSchema) -> Self {
        Self {
            records: records.into_iter(),
            schema,
        }
    }
}

impl SlotRecordIter for VecSlotIter {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        Ok(self.records.next())
    }
    fn schema(&self) -> &RecordSchema {
        &self.schema
    }
}

// ---------------------------------------------------------------------------
// Pipeline iterators

/// Slot-aware `Filter`. Evaluates the predicate against the slot record
/// via [`SlotView`], so no per-row `NamedRecord` materialization is
/// needed; the original slot record is forwarded on match.
pub struct FilterSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    predicate: Expr,
    conn: &'a Connection,
}

impl<'a> SlotRecordIter for FilterSlotIter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        while let Some(rec) = self.input.next_slot()? {
            let view = SlotView::new(self.input.schema(), &rec);
            if eval_predicate(
                &self.predicate,
                &view,
                crate::cypher::eval::EvalCx::new(self.conn),
            )? {
                return Ok(Some(rec));
            }
        }
        Ok(None)
    }

    fn schema(&self) -> &RecordSchema {
        self.input.schema()
    }
}

/// Slot-aware `Project`. For each item:
///
/// - **Literals** evaluate inline to a `Value`.
/// - **`Property(v, p)`** with the slot present in the input schema reads
///   directly via slot lookup (the fast path Phase 3b set up).
/// - Everything else materializes the slot record into a `NamedRecord`
///   and calls into `eval_expr` for full named-path semantics. This keeps
///   correctness exact while we incrementally migrate eval; the per-row
///   allocation cost is what Phase 5's re-baseline measures away.
pub struct ProjectSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    items: Vec<ReturnItem>,
    /// Reserved for Phase 3d — terminal RETURN of bare variables needs to
    /// emit `Value::Node` / `Value::Edge`. `is_slot_supported` excludes
    /// `Variable` / `Star` items today, so this never triggers.
    _emit_compound: bool,
    output_schema: RecordSchema,
    conn: &'a Connection,
}

impl<'a> ProjectSlotIter<'a> {
    pub fn new(
        input: Box<dyn SlotRecordIter + 'a>,
        items: Vec<ReturnItem>,
        emit_compound: bool,
        output_schema: RecordSchema,
        conn: &'a Connection,
    ) -> Self {
        Self {
            input,
            items,
            _emit_compound: emit_compound,
            output_schema,
            conn,
        }
    }
}

impl<'a> SlotRecordIter for ProjectSlotIter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        let input_rec = match self.input.next_slot()? {
            Some(r) => r,
            None => return Ok(None),
        };
        let input_schema = self.input.schema();
        let mut out = SlotRecord::with_capacity(self.output_schema.len());

        for item in &self.items {
            let col = column_name_for_item(item);
            let Some(out_slot) = self.output_schema.slot(&col) else {
                continue;
            };
            // Mirror exec_project's precomputed-value lookup. Two safe
            // cache hits:
            //   1. The expression's natural column name (`expr_to_column_name`)
            //      is in the input schema. Always safe — it can't shadow an
            //      upstream variable.
            //   2. The output column name (alias) is in the input schema AND
            //      the expression is an aggregate function call. After
            //      `Aggregate`, the alias holds the precomputed result;
            //      re-evaluating it on a single materialized row would be
            //      wrong. Without the aggregate guard we'd shadow ordinary
            //      bindings (e.g. `RETURN a.id AS a` would pick up the
            //      `a → I64(node_id)` slot instead of `a.id`).
            let expr_col = expr_to_column_name(&item.expr);
            // Skip the precomputed-value fast path when `item.expr` reads a
            // property of a variable an upstream `Delete` has tagged — that
            // must raise `EntityNotFound` via eval, not return the
            // pre-delete cached value. (Mirrors eval_project_item's check.)
            let deleted_var = property_deleted_var(&item.expr, input_schema, &input_rec);
            let value = if deleted_var {
                eval_project_item(&item.expr, input_schema, &input_rec, self.conn)?
            } else if let Some(in_slot) = input_schema.slot(&expr_col) {
                input_rec.get(in_slot).clone()
            } else if col != expr_col && is_aggregate_call(&item.expr) {
                if let Some(in_slot) = input_schema.slot(&col) {
                    input_rec.get(in_slot).clone()
                } else {
                    eval_project_item(&item.expr, input_schema, &input_rec, self.conn)?
                }
            } else {
                eval_project_item(&item.expr, input_schema, &input_rec, self.conn)?
            };
            out.set(out_slot, value);
        }
        Ok(Some(out))
    }

    fn schema(&self) -> &RecordSchema {
        &self.output_schema
    }
}

/// True iff `e` is a `Property(var, _)` (or wraps such) where
/// `<var>.__deleted` is bound to `Bool(true)` in `(input_schema, input_rec)`.
/// Conservatively only recognizes the common shape (top-level Property);
/// anything more complex falls through to the materialize+eval path which
/// raises the error itself.
fn property_deleted_var(e: &Expr, input_schema: &RecordSchema, input_rec: &SlotRecord) -> bool {
    if let ExprKind::Property(var, _) = &e.kind {
        let key = format!("{var}.__deleted");
        if let Some(slot) = input_schema.slot(&key) {
            return matches!(input_rec.get(slot), Value::Bool(true));
        }
    }
    false
}

fn column_name_for_item(item: &ReturnItem) -> String {
    item.alias
        .clone()
        .unwrap_or_else(|| expr_to_column_name(&item.expr))
}

/// Evaluate a `Project` item against `(input_schema, input_rec)`.
///
fn eval_project_item(
    e: &Expr,
    input_schema: &RecordSchema,
    input_rec: &SlotRecord,
    conn: &Connection,
) -> Result<Value> {
    match &e.kind {
        ExprKind::Literal(lit) => Ok(literal_to_value_local(lit)),
        ExprKind::Property(var, prop) => {
            // Mirror `eval_property`'s deletion check: an upstream `Delete`
            // sets `<var>.__deleted = true` and any property read on a
            // deleted entity must raise `EntityNotFound`. The slot bridge
            // picks this up via the `<var>.__deleted` slot reserved by
            // `schema_infer` for `Delete`. When the flag is set, defer to
            // named `eval_expr` (which raises) rather than returning the
            // pre-delete cached value via the fast path.
            let deleted_key = format!("{var}.__deleted");
            let is_deleted = input_schema
                .slot(&deleted_key)
                .map(|s| matches!(input_rec.get(s), Value::Bool(true)))
                .unwrap_or(false);
            if !is_deleted {
                let key = format!("{var}.{prop}");
                if let Some(slot) = input_schema.slot(&key) {
                    return Ok(input_rec.get(slot).clone());
                }
            }
            let view = SlotView::new(input_schema, input_rec);
            eval_expr(e, &view, crate::cypher::eval::EvalCx::new(conn))
        }
        _ => {
            let view = SlotView::new(input_schema, input_rec);
            eval_expr(e, &view, crate::cypher::eval::EvalCx::new(conn))
        }
    }
}

/// True iff `e` is a top-level call to a recognized aggregate function. Same
/// allow-list as `exec_project`'s aggregate-cache heuristic so the slot
/// path's lookup behavior matches the named path exactly.
fn is_aggregate_call(e: &Expr) -> bool {
    matches!(
        &e.kind,
        ExprKind::FunctionCall { name, .. }
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "count" | "sum" | "avg" | "min" | "max" | "collect"
                    | "percentiledisc" | "percentilecont" | "stdev" | "stdevp"
            )
    )
}

/// Local copy of the executor's literal-to-value conversion to keep the
/// slot path independent of the named-path module's visibility.
fn literal_to_value_local(lit: &crate::cypher::ast::LiteralValue) -> Value {
    use crate::cypher::ast::LiteralValue as L;
    match lit {
        L::Null => Value::Null,
        L::Bool(b) => Value::Bool(*b),
        L::I64(n) => Value::I64(*n),
        L::F64(n) => Value::F64(*n),
        L::String(s) => Value::String(s.clone()),
    }
}

/// Slot-aware `Aggregate` (blocking). Drains its input, materializes each
/// row into a `NamedRecord` (so the existing
/// `executor::aggregate_named_records` runs unchanged — same column-naming,
/// same compound-binding semantics for Variable group keys), then re-shapes
/// the produced named results back into slot records via
/// [`named_to_slot`]. Per-row materialize/convert cost is the 3e trade-off,
/// matching 3c–3d's pattern.
pub struct AggregateSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    group_keys: Vec<Expr>,
    aggregates: Vec<crate::cypher::ir::AggregateExpr>,
    output_schema: RecordSchema,
    conn: &'a Connection,
    /// Lazily computed on first `next_slot` call.
    results: Option<std::vec::IntoIter<SlotRecord>>,
}

impl<'a> AggregateSlotIter<'a> {
    pub fn new(
        input: Box<dyn SlotRecordIter + 'a>,
        group_keys: Vec<Expr>,
        aggregates: Vec<crate::cypher::ir::AggregateExpr>,
        output_schema: RecordSchema,
        conn: &'a Connection,
    ) -> Self {
        Self {
            input,
            group_keys,
            aggregates,
            output_schema,
            conn,
            results: None,
        }
    }

    fn materialize(&mut self) -> Result<()> {
        // Drain input slot records — no NamedRecord materialization.
        let input_schema = self.input.schema().clone();
        let mut slot_inputs: Vec<SlotRecord> = Vec::new();
        while let Some(rec) = self.input.next_slot()? {
            slot_inputs.push(rec);
        }
        let agg_results = crate::cypher::executor::aggregate_slot_records(
            self.conn,
            &input_schema,
            &slot_inputs,
            &self.output_schema,
            &self.group_keys,
            &self.aggregates,
        )?;
        self.results = Some(agg_results.into_iter());
        Ok(())
    }
}

impl<'a> SlotRecordIter for AggregateSlotIter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        if self.results.is_none() {
            self.materialize()?;
        }
        Ok(self.results.as_mut().and_then(|it| it.next()))
    }
    fn schema(&self) -> &RecordSchema {
        &self.output_schema
    }
}

// ---------------------------------------------------------------------------
// Phase 3f pass-through ops

/// Slot-aware `Skip`. Drops the first `count` rows from `input`, then
/// streams the rest unchanged.
pub struct SkipSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    remaining_to_skip: u64,
}

impl<'a> SlotRecordIter for SkipSlotIter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        while self.remaining_to_skip > 0 {
            match self.input.next_slot()? {
                Some(_) => self.remaining_to_skip -= 1,
                None => return Ok(None),
            }
        }
        self.input.next_slot()
    }
    fn schema(&self) -> &RecordSchema {
        self.input.schema()
    }
}

/// Slot-aware `Limit`. Streams up to `count` rows from `input`, then
/// reports exhaustion.
pub struct LimitSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    remaining: u64,
}

impl<'a> SlotRecordIter for LimitSlotIter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        match self.input.next_slot()? {
            Some(rec) => {
                self.remaining -= 1;
                Ok(Some(rec))
            }
            None => Ok(None),
        }
    }
    fn schema(&self) -> &RecordSchema {
        self.input.schema()
    }
}

/// Slot-aware `Distinct` (blocking). Materializes input rows once and
/// emits each unique slot record. Equality is on full slot-row contents
/// using the input schema (ordered by slot index, identical to
/// schema-name iteration order).
pub struct DistinctSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    schema: RecordSchema,
    /// `None` until first `next_slot` triggers materialization.
    results: Option<std::vec::IntoIter<SlotRecord>>,
}

impl<'a> DistinctSlotIter<'a> {
    pub fn new(input: Box<dyn SlotRecordIter + 'a>) -> Self {
        let schema = input.schema().clone();
        Self {
            input,
            schema,
            results: None,
        }
    }

    fn materialize(&mut self) -> Result<()> {
        let mut deduped: Vec<SlotRecord> = Vec::new();
        while let Some(rec) = self.input.next_slot()? {
            // O(n²) dedup using slot-row equality. Matches the named path's
            // strategy in `iter::Distinct`; both rely on small result sets.
            if !deduped.iter().any(|s| slot_rows_equal(s, &rec)) {
                deduped.push(rec);
            }
        }
        self.results = Some(deduped.into_iter());
        Ok(())
    }
}

impl<'a> SlotRecordIter for DistinctSlotIter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        if self.results.is_none() {
            self.materialize()?;
        }
        Ok(self.results.as_mut().and_then(|it| it.next()))
    }
    fn schema(&self) -> &RecordSchema {
        &self.schema
    }
}

fn slot_rows_equal(a: &SlotRecord, b: &SlotRecord) -> bool {
    if a.len() != b.len() {
        return false;
    }
    (0..a.len()).all(|i| {
        let s = SlotId(i as u32);
        a.get(s) == b.get(s)
    })
}

/// Slot-aware `Sort` (blocking). Drains input, materializes each row to
/// a NamedRecord on the fly to evaluate sort keys via `eval_expr`
/// (matching `iter::Sort`'s null-last + descending semantics), then
/// re-emits the underlying slot records in the resolved order. The
/// per-row materialize is the 3f trade-off; same shape as 3c/3d/3e.
pub struct SortSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    items: Vec<crate::cypher::ast::SortItem>,
    conn: &'a Connection,
    results: Option<std::vec::IntoIter<SlotRecord>>,
}

impl<'a> SortSlotIter<'a> {
    pub fn new(
        input: Box<dyn SlotRecordIter + 'a>,
        items: Vec<crate::cypher::ast::SortItem>,
        conn: &'a Connection,
    ) -> Self {
        Self {
            input,
            items,
            conn,
            results: None,
        }
    }

    fn materialize(&mut self) -> Result<()> {
        let schema = self.input.schema().clone();
        // Pre-compute sort keys per row so eval runs once even when the
        // comparator visits a row multiple times.
        let mut tagged: Vec<(Vec<Value>, SlotRecord)> = Vec::new();
        while let Some(rec) = self.input.next_slot()? {
            let view = SlotView::new(&schema, &rec);
            let keys: Vec<Value> = self
                .items
                .iter()
                .map(|si| {
                    eval_expr(&si.expr, &view, crate::cypher::eval::EvalCx::new(self.conn))
                        .unwrap_or(Value::Null)
                })
                .collect();
            tagged.push((keys, rec));
        }
        let descending: Vec<bool> = self.items.iter().map(|si| si.descending).collect();
        tagged.sort_by(|a, b| {
            for (i, desc) in descending.iter().enumerate() {
                let ord = compare_values_for_sort(&a.0[i], &b.0[i]);
                let ord = if *desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        let sorted: Vec<SlotRecord> = tagged.into_iter().map(|(_, r)| r).collect();
        self.results = Some(sorted.into_iter());
        Ok(())
    }
}

impl<'a> SlotRecordIter for SortSlotIter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        if self.results.is_none() {
            self.materialize()?;
        }
        Ok(self.results.as_mut().and_then(|it| it.next()))
    }
    fn schema(&self) -> &RecordSchema {
        self.input.schema()
    }
}

/// Reuses `iter`'s null-last comparator so slot-path Sort orders rows
/// identically to the named path on every TCK scenario.
fn compare_values_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    crate::cypher::iter::compare_values_for_sort_pub(a, b)
}

/// Slot-aware `Unwind`. For each input row, materialize to a NamedRecord
/// to evaluate the unwind expression, then emit one output slot record
/// per list item — populating `alias.<…>` keys in the output schema with
/// the same flat-binding rules `exec_unwind` uses (node metadata cluster
/// for `Value::Node`, edge metadata for `Value::Edge`, otherwise just
/// `alias` itself). `Value::Null` produces zero rows; non-list, non-null
/// values raise a semantic error to match named-path behavior.
pub struct UnwindSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    expr: Expr,
    alias: String,
    output_schema: RecordSchema,
    conn: &'a Connection,
    /// Buffered output rows for the current input row. Drained before
    /// pulling the next input.
    pending: std::vec::IntoIter<SlotRecord>,
}

impl<'a> UnwindSlotIter<'a> {
    pub fn new(
        input: Box<dyn SlotRecordIter + 'a>,
        expr: Expr,
        alias: String,
        output_schema: RecordSchema,
        conn: &'a Connection,
    ) -> Self {
        Self {
            input,
            expr,
            alias,
            output_schema,
            conn,
            pending: Vec::new().into_iter(),
        }
    }

    /// Convert one input slot row + its expanded NamedRecord (alias
    /// already populated with flat keys) into a slot record laid out
    /// against `output_schema`. Carries forward every input-schema slot
    /// and overlays the alias bindings.
    fn project(
        &self,
        input_schema: &RecordSchema,
        base: &SlotRecord,
        named: &NamedRecord,
    ) -> SlotRecord {
        let mut out = SlotRecord::with_capacity(self.output_schema.len());
        // Carry forward every slot the input row already had.
        for (in_slot, name) in input_schema.iter() {
            if let Some(out_slot) = self.output_schema.slot(name) {
                out.set(out_slot, base.get(in_slot).clone());
            }
        }
        // Overlay alias-related bindings the named expansion just wrote.
        let prefix = format!("{}.", self.alias);
        for (k, v) in &named.fields {
            if k == &self.alias || k.starts_with(&prefix) {
                if let Some(out_slot) = self.output_schema.slot(k) {
                    out.set(out_slot, v.clone());
                }
            }
        }
        out
    }
}

impl<'a> SlotRecordIter for UnwindSlotIter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        loop {
            if let Some(rec) = self.pending.next() {
                return Ok(Some(rec));
            }
            let Some(input_rec) = self.input.next_slot()? else {
                return Ok(None);
            };
            let input_schema = self.input.schema();
            let val = {
                let view = SlotView::new(input_schema, &input_rec);
                eval_expr(
                    &self.expr,
                    &view,
                    crate::cypher::eval::EvalCx::new(self.conn),
                )?
            };
            let items = match val {
                Value::List(items) => items,
                Value::Null => continue,
                other => {
                    return Err(crate::types::GraphError::semantic(format!(
                        "UNWIND requires a list, got: {other}"
                    )));
                }
            };
            if items.is_empty() {
                continue;
            }
            // Materialize once after we know there's at least one item to
            // emit; each output row clones from this base + adds the
            // alias bindings (mirrors `exec_unwind`).
            let view = materialize_named(input_schema, &input_rec);
            let mut produced = Vec::with_capacity(items.len());
            for item in items {
                let mut named = view.clone();
                match &item {
                    Value::Node(n) => {
                        let node_rec = crate::cypher::executor::node_to_record_pub(n, &self.alias);
                        for (k, v) in &node_rec.fields {
                            named.set(k.clone(), v.clone());
                        }
                    }
                    Value::Edge(e) => {
                        named.set(self.alias.clone(), Value::Edge(e.clone()));
                        named.set(format!("{}.__src", self.alias), Value::I64(e.src.0 as i64));
                        named.set(format!("{}.__dst", self.alias), Value::I64(e.dst.0 as i64));
                        named.set(
                            format!("{}.__type", self.alias),
                            Value::String(e.label.clone()),
                        );
                        for (k, v) in &e.properties {
                            named.set(format!("{}.{}", self.alias, k), v.clone());
                        }
                    }
                    Value::Map(entries) => {
                        // Mirror Node/Edge: bind the compound value AND
                        // flatten entries to `alias.<key>` so the slot
                        // path's Property fast-path doesn't read a
                        // reserved-but-unwritten Null slot. (The named
                        // path doesn't bother because its `eval_property`
                        // falls back to a runtime DotAccess on the Map;
                        // the slot path's eager slot reads can't.)
                        named.set(self.alias.clone(), Value::Map(entries.clone()));
                        for (k, v) in entries {
                            named.set(format!("{}.{}", self.alias, k), v.clone());
                        }
                    }
                    _ => {
                        named.set(self.alias.clone(), item.clone());
                    }
                }
                produced.push(self.project(input_schema, &input_rec, &named));
            }
            self.pending = produced.into_iter();
        }
    }

    fn schema(&self) -> &RecordSchema {
        &self.output_schema
    }
}

// ---------------------------------------------------------------------------
// Slot ↔ Named adapters
//
// The named iterator stack (`cypher::iter`) carries a lot of dense logic
// — Expand's relationship-uniqueness check, var-length traversal, bound
// destination filtering — that we'd rather not duplicate at slot level.
// These adapters let a slot iter consume / produce named records at one
// boundary so the named operator runs unchanged in between.

/// Wraps a slot iter so it presents as a named [`RecordIter`]. Each input
/// slot record is materialized into a `NamedRecord` using its schema.
pub struct SlotToNamedAdapter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
}

impl<'a> SlotToNamedAdapter<'a> {
    pub fn new(input: Box<dyn SlotRecordIter + 'a>) -> Self {
        Self { input }
    }
}

impl<'a> crate::cypher::iter::RecordIter for SlotToNamedAdapter<'a> {
    fn next_record(&mut self) -> Result<Option<NamedRecord>> {
        Ok(self
            .input
            .next_slot()?
            .map(|rec| materialize_named(self.input.schema(), &rec)))
    }
}

/// Wraps a named iter so it presents as a [`SlotRecordIter`] over a fixed
/// output schema. Each named record is decanted into a slot record by
/// looking up each slot name in the named record.
pub struct NamedToSlotAdapter<'a> {
    input: Box<dyn crate::cypher::iter::RecordIter + 'a>,
    schema: RecordSchema,
}

impl<'a> NamedToSlotAdapter<'a> {
    pub fn new(input: Box<dyn crate::cypher::iter::RecordIter + 'a>, schema: RecordSchema) -> Self {
        Self { input, schema }
    }
}

impl<'a> SlotRecordIter for NamedToSlotAdapter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        match self.input.next_record()? {
            Some(named) => Ok(Some(named_to_slot(&self.schema, &named))),
            None => Ok(None),
        }
    }
    fn schema(&self) -> &RecordSchema {
        &self.schema
    }
}

/// Project a [`NamedRecord`] into the shape declared by `schema`. Keys
/// missing from the named record become `Value::Null` slots.
fn named_to_slot(schema: &RecordSchema, named: &NamedRecord) -> SlotRecord {
    let mut rec = SlotRecord::with_capacity(schema.len());
    for (slot, name) in schema.iter() {
        if let Some(v) = named.get(name) {
            rec.set(slot, v.clone());
        }
    }
    rec
}

// Suppress unused-import warnings for type aliases we'll need in 3e+.
#[allow(dead_code)]
fn _kept_for_future_use(_: NodeId, _: SlotId) {}
