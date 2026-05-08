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

#![cfg_attr(not(feature = "record-v2"), allow(dead_code))]

use rusqlite::Connection;

use crate::cypher::ast::{Expr, ExprKind, ReturnItem};
use crate::cypher::eval::{eval_expr, eval_predicate, expr_to_column_name};
use crate::cypher::executor::{literal_to_value, ExecContext};
use crate::cypher::ir::LogicalOp;
use crate::cypher::record::NamedRecord;
use crate::cypher::record_v2::{Record as SlotRecord, RecordSchema, SlotId};
use crate::cypher::schema_infer::{collect_property_refs, infer_with_props, PropertyRefs};
use crate::types::{NodeId, Result, Value};
use crate::{index, node};

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
    let mut refs = PropertyRefs::new();
    collect_property_refs(plan, &mut refs);
    is_slot_supported_inner(plan, &refs)
}

fn is_slot_supported_inner(plan: &LogicalOp, refs: &PropertyRefs) -> bool {
    match plan {
        LogicalOp::EmptyRow | LogicalOp::SingleRow => true,
        LogicalOp::Scan { .. } => true,
        LogicalOp::IndexLookup {
            remaining_filters, ..
        } => match remaining_filters {
            Some(f) => !expr_has_bare_variable(f),
            None => true,
        },
        LogicalOp::Filter { input, predicate } => {
            is_slot_supported_inner(input, refs) && !expr_has_bare_variable(predicate)
        }
        LogicalOp::Project { input, items, .. } => {
            is_slot_supported_inner(input, refs) && items.iter().all(is_project_item_supported)
        }
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
            property,
            value,
            remaining_filters: _,
        } => {
            let schema = infer_with_props(plan, refs);
            let lookup_value = literal_to_value(value);
            let node_ids = index::index_lookup(conn, label, property, &lookup_value)?;
            let mut records = Vec::with_capacity(node_ids.len());
            for id in node_ids {
                let n = node::get_node(conn, id)?;
                records.push(node_to_slot_record(&n, alias, &schema));
            }
            Ok(Box::new(VecSlotIter::new(records, schema)))
        }

        LogicalOp::Filter { input, predicate } => {
            let input_iter = build_slot_iter_inner(conn, input, ctx, refs)?;
            Ok(Box::new(FilterSlotIter {
                input: input_iter,
                predicate: predicate.clone(),
                conn,
            }))
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

/// Slot-aware `Filter`. Materializes each input slot record into a
/// `NamedRecord` so the existing `eval_predicate` runs unchanged; the
/// original slot record is forwarded on match. Per-row allocation is the
/// 3c trade-off — eval becomes natively slot-aware in a later sub-phase.
pub struct FilterSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    predicate: Expr,
    conn: &'a Connection,
}

impl<'a> SlotRecordIter for FilterSlotIter<'a> {
    fn next_slot(&mut self) -> Result<Option<SlotRecord>> {
        while let Some(rec) = self.input.next_slot()? {
            let view = materialize_named(self.input.schema(), &rec);
            if eval_predicate(&self.predicate, &view, self.conn)? {
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
        // Lazily materialize a NamedRecord the first time an item needs it,
        // so all-fast-path projections (RETURN p.name) skip the allocation.
        let mut materialized: Option<NamedRecord> = None;
        let mut out = SlotRecord::with_capacity(self.output_schema.len());

        for item in &self.items {
            let col = column_name_for_item(item);
            let Some(out_slot) = self.output_schema.slot(&col) else {
                continue;
            };
            let value = eval_project_item(
                &item.expr,
                input_schema,
                &input_rec,
                &mut materialized,
                self.conn,
            )?;
            out.set(out_slot, value);
        }
        Ok(Some(out))
    }

    fn schema(&self) -> &RecordSchema {
        &self.output_schema
    }
}

fn column_name_for_item(item: &ReturnItem) -> String {
    item.alias
        .clone()
        .unwrap_or_else(|| expr_to_column_name(&item.expr))
}

/// Evaluate a `Project` item against `(input_schema, input_rec)`.
///
/// `materialized` is a lazily-built named-record cache shared across all
/// items in one row — building it costs O(schema.len()), so we only do it
/// once even if multiple items hit the fallback.
fn eval_project_item(
    e: &Expr,
    input_schema: &RecordSchema,
    input_rec: &SlotRecord,
    materialized: &mut Option<NamedRecord>,
    conn: &Connection,
) -> Result<Value> {
    match &e.kind {
        ExprKind::Literal(lit) => Ok(literal_to_value_local(lit)),
        ExprKind::Property(var, prop) => {
            let key = format!("{var}.{prop}");
            if let Some(slot) = input_schema.slot(&key) {
                return Ok(input_rec.get(slot).clone());
            }
            // Not slotted — could be subfield access on a non-node value.
            // Fall through to named eval.
            let view =
                materialized.get_or_insert_with(|| materialize_named(input_schema, input_rec));
            eval_expr(e, view, conn)
        }
        _ => {
            let view =
                materialized.get_or_insert_with(|| materialize_named(input_schema, input_rec));
            eval_expr(e, view, conn)
        }
    }
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

// Suppress unused-import warnings for type aliases we'll need in 3d+.
#[allow(dead_code)]
fn _kept_for_future_use(_: NodeId, _: SlotId) {}
