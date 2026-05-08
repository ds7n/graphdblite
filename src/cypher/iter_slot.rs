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
use crate::cypher::eval::expr_to_column_name;
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
/// Phase 3b. Anything else routes to the named path. Phase 3c–3g extend
/// this set as operators land.
///
/// `Project` items are checked schema-aware: a `Property(v, p)` only
/// qualifies for the slot path if the input schema actually contains a
/// slot for `v.p`. This excludes subfield access on non-node values
/// (e.g. `WITH v.date AS d ... RETURN d.year` — `d.year` is a temporal
/// subfield, not a node-property slot — falls back to the named path so
/// `eval_property`'s subfield logic runs).
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
        } => remaining_filters.is_none(),
        LogicalOp::Project { input, items, .. } => {
            if !is_slot_supported_inner(input, refs) {
                return false;
            }
            let input_schema = infer_with_props(input, refs);
            items
                .iter()
                .all(|i| is_project_item_slot_evaluable(i, &input_schema))
        }
        _ => false,
    }
}

/// True iff `item` projects an expression we can evaluate against a slot
/// record without invoking the full named-path `eval` machinery.
///
/// Phase 3b supports literals and property reads — but only when the
/// referenced slot exists in `input_schema`. **Bare variable references
/// are excluded:** for the terminal RETURN, the named path builds a
/// compound `Value::Node` / `Value::Edge` from the flat metadata cluster,
/// and we don't have that machinery on the slot side yet. Phase 3d adds
/// compound binding for slots; until then any plan with a bare-variable
/// RETURN falls back.
fn is_project_item_slot_evaluable(item: &ReturnItem, input_schema: &RecordSchema) -> bool {
    match &item.expr.kind {
        ExprKind::Literal(_) => true,
        ExprKind::Property(v, p) => input_schema.slot(&format!("{v}.{p}")).is_some(),
        _ => false,
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

/// Slot-aware `Project`. Reads each input record by slot, evaluates the
/// (slot-evaluable) RETURN items, and writes them into the output record
/// at the slot dictated by the output schema.
pub struct ProjectSlotIter<'a> {
    input: Box<dyn SlotRecordIter + 'a>,
    items: Vec<ReturnItem>,
    /// Reserved for Phase 3d when bare-variable projections need to emit
    /// `Value::Node` / `Value::Edge` for the terminal RETURN. Today the
    /// 3b set of slot-evaluable expressions never triggers this path.
    _emit_compound: bool,
    output_schema: RecordSchema,
}

impl<'a> ProjectSlotIter<'a> {
    pub fn new(
        input: Box<dyn SlotRecordIter + 'a>,
        items: Vec<ReturnItem>,
        emit_compound: bool,
        output_schema: RecordSchema,
    ) -> Self {
        Self {
            input,
            items,
            _emit_compound: emit_compound,
            output_schema,
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
                // Schema dropped this item — should not happen given
                // infer_schema mirrors column_name_for_item, but be defensive.
                continue;
            };
            let value = eval_slot_expr(&item.expr, input_schema, &input_rec);
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

/// Slot-aware evaluator covering Phase 3b's leaf set: literals and
/// property reads. The named-path `eval` covers the rest;
/// `is_slot_evaluable_expr` rejects anything we'd reach here that isn't
/// in this match.
fn eval_slot_expr(e: &Expr, schema: &RecordSchema, rec: &SlotRecord) -> Value {
    match &e.kind {
        ExprKind::Literal(lit) => literal_to_value_local(lit),
        ExprKind::Property(var, prop) => slot_or_null(schema, rec, &format!("{var}.{prop}")),
        _ => unreachable!(
            "eval_slot_expr received non-slot-evaluable expression — \
             is_slot_evaluable_expr should have rejected this earlier"
        ),
    }
}

fn slot_or_null(schema: &RecordSchema, rec: &SlotRecord, name: &str) -> Value {
    schema
        .slot(name)
        .map(|s| rec.get(s).clone())
        .unwrap_or(Value::Null)
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

// Suppress unused-import warnings for type aliases we'll need in 3c+.
#[allow(dead_code)]
fn _kept_for_future_use(_: NodeId, _: SlotId) {}
