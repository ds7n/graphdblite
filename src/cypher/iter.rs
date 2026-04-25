//! Pull-based record iterator model for the query executor.
//!
//! Each operator implements `RecordIter`, yielding one record at a time.
//! Pipeline operators (Filter, Limit, Project) stream without materializing.
//! Blocking operators (Sort, Aggregate) materialize their input then stream
//! the result.

use rusqlite::Connection;

use crate::cypher::ast::{Expr, ReturnItem};
use crate::cypher::eval::{eval_expr, eval_predicate, expr_to_column_name};
use crate::cypher::executor::{is_user_visible_field, literal_to_value, node_to_record};
use crate::cypher::ir::*;
use crate::cypher::record::Record;
use crate::types::{Direction, NodeId, Result, Value};
use crate::{edge, index, node};

/// Pull-based iterator that yields one record at a time.
///
/// Uses a fallible `next_record` method instead of `Iterator` because:
/// - Iteration can fail (SQLite errors, serialization errors)
/// - Iterators hold references to the connection, making standard `Iterator`
///   lifetime management complex
pub trait RecordIter {
    /// Return the next record, or `None` when exhausted.
    fn next_record(&mut self) -> Result<Option<Record>>;
}

/// Collect all records from an iterator into a Vec.
pub fn collect_all(iter: &mut dyn RecordIter) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    while let Some(rec) = iter.next_record()? {
        records.push(rec);
    }
    Ok(records)
}

// ── Leaf iterators ──────────────────────────────────────────────────────

/// Iterates over pre-materialized records (used for leaf nodes and blocking ops).
pub struct VecIter {
    records: std::vec::IntoIter<Record>,
}

impl VecIter {
    pub fn new(records: Vec<Record>) -> Self {
        Self {
            records: records.into_iter(),
        }
    }
}

impl RecordIter for VecIter {
    fn next_record(&mut self) -> Result<Option<Record>> {
        Ok(self.records.next())
    }
}

/// Yields a single empty record, then stops.
#[derive(Default)]
pub struct EmptyRowIter {
    done: bool,
}

impl EmptyRowIter {
    pub fn new() -> Self {
        Self::default()
    }
}

impl RecordIter for EmptyRowIter {
    fn next_record(&mut self) -> Result<Option<Record>> {
        if self.done {
            Ok(None)
        } else {
            self.done = true;
            Ok(Some(Record::new()))
        }
    }
}

// ── Pipeline iterators ──────────────────────────────────────────────────

/// Filters records from input, yielding only those matching the predicate.
pub struct FilterIter<'a> {
    input: Box<dyn RecordIter + 'a>,
    predicate: Expr,
    conn: &'a Connection,
}

impl<'a> RecordIter for FilterIter<'a> {
    fn next_record(&mut self) -> Result<Option<Record>> {
        while let Some(rec) = self.input.next_record()? {
            if eval_predicate(&self.predicate, &rec, self.conn)? {
                return Ok(Some(rec));
            }
        }
        Ok(None)
    }
}

/// Skips the first `count` records from input, then yields the rest.
pub struct SkipIter<'a> {
    input: Box<dyn RecordIter + 'a>,
    remaining_to_skip: u64,
}

impl<'a> RecordIter for SkipIter<'a> {
    fn next_record(&mut self) -> Result<Option<Record>> {
        while self.remaining_to_skip > 0 {
            if self.input.next_record()?.is_none() {
                return Ok(None);
            }
            self.remaining_to_skip -= 1;
        }
        self.input.next_record()
    }
}

/// Yields at most `count` records from input, then stops.
pub struct LimitIter<'a> {
    input: Box<dyn RecordIter + 'a>,
    remaining: u64,
}

impl<'a> RecordIter for LimitIter<'a> {
    fn next_record(&mut self) -> Result<Option<Record>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        if let Some(rec) = self.input.next_record()? {
            self.remaining -= 1;
            Ok(Some(rec))
        } else {
            Ok(None)
        }
    }
}

/// Projects each input record through a RETURN / WITH clause.
///
/// `emit_compound` matches the semantics of `LogicalOp::Project`: the terminal
/// RETURN yields compound `Value::Node` / `Value::Edge` values for bare
/// variable references, while intermediate WITH clauses preserve the flat
/// binding shape.
pub struct ProjectIter<'a> {
    input: Box<dyn RecordIter + 'a>,
    items: Vec<ReturnItem>,
    emit_compound: bool,
    conn: &'a Connection,
}

impl<'a> RecordIter for ProjectIter<'a> {
    fn next_record(&mut self) -> Result<Option<Record>> {
        let rec = match self.input.next_record()? {
            Some(r) => r,
            None => return Ok(None),
        };

        let mut projected = Record::new();
        for item in &self.items {
            match &item.expr {
                Expr::Star => {
                    if self.emit_compound {
                        let bound_vars = crate::cypher::executor::compound_binding_vars(&rec);
                        for var in &bound_vars {
                            if let Some(compound) =
                                crate::cypher::executor::build_compound_binding(&rec, var)
                            {
                                projected.set(var.clone(), compound);
                            }
                        }
                        for (key, val) in &rec.fields {
                            if !is_user_visible_field(key) {
                                continue;
                            }
                            let owner = key.split_once('.').map(|(v, _)| v);
                            if let Some(owner) = owner {
                                if bound_vars.iter().any(|v| v == owner) {
                                    continue;
                                }
                            }
                            projected.set(key.clone(), val.clone());
                        }
                    } else {
                        for (key, val) in &rec.fields {
                            projected.set(key.clone(), val.clone());
                        }
                    }
                }
                Expr::Variable(var) => {
                    let col_name = item.alias.clone().unwrap_or_else(|| var.clone());
                    if self.emit_compound {
                        if let Some(compound) =
                            crate::cypher::executor::build_compound_binding(&rec, var)
                        {
                            projected.set(col_name, compound);
                        } else if let Some(existing) = rec.get(&col_name) {
                            projected.set(col_name, existing.clone());
                        } else {
                            let val = eval_expr(&item.expr, &rec, self.conn)?;
                            projected.set(col_name, val);
                        }
                    } else {
                        // Preserve flat shape for downstream consumers.
                        let src_prefix = format!("{var}.");
                        let dst_prefix = format!("{col_name}.");
                        let mut propagated_any = false;
                        for (key, val) in &rec.fields {
                            if let Some(rest) = key.strip_prefix(&src_prefix) {
                                projected.set(format!("{dst_prefix}{rest}"), val.clone());
                                propagated_any = true;
                            }
                        }
                        if let Some(existing) = rec.get(var) {
                            projected.set(col_name.clone(), existing.clone());
                            propagated_any = true;
                        }
                        if !propagated_any {
                            let val = eval_expr(&item.expr, &rec, self.conn)?;
                            projected.set(col_name, val);
                        }
                    }
                }
                _ => {
                    let col_name = item
                        .alias
                        .clone()
                        .unwrap_or_else(|| expr_to_column_name(&item.expr));
                    let val = if let Some(existing) = rec.get(&col_name) {
                        existing.clone()
                    } else {
                        eval_expr(&item.expr, &rec, self.conn)?
                    };
                    projected.set(col_name, val);
                }
            }
        }
        Ok(Some(projected))
    }
}

/// Expand along edges: for each input record, yield one record per neighbor.
pub struct ExpandIter<'a> {
    input: Box<dyn RecordIter + 'a>,
    conn: &'a Connection,
    src_alias: String,
    dst_alias: String,
    rel_alias: Option<String>,
    edge_types: Vec<String>,
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    /// Buffer of expanded records from the current input record.
    buffer: std::vec::IntoIter<Record>,
}

impl<'a> RecordIter for ExpandIter<'a> {
    fn next_record(&mut self) -> Result<Option<Record>> {
        loop {
            // Drain buffered expansions first.
            if let Some(rec) = self.buffer.next() {
                return Ok(Some(rec));
            }

            // Pull next input record.
            let rec = match self.input.next_record()? {
                Some(r) => r,
                None => return Ok(None),
            };

            let src_id = match rec.get(&self.src_alias) {
                Some(Value::I64(id)) => NodeId(*id as u64),
                _ => continue,
            };

            let label = self.edge_types.first().map(|s| s.as_str()).unwrap_or("");
            let dst_ids = if self.min_hops == 1 && self.max_hops == 1 {
                edge::get_neighbors(self.conn, src_id, label, self.direction)?
            } else {
                edge::traverse(
                    self.conn,
                    src_id,
                    label,
                    self.direction,
                    self.min_hops,
                    self.max_hops,
                )?
            };

            // If the destination alias is already bound (cyclic pattern),
            // only keep expansions matching the bound node.
            let bound_dst = rec.get(&self.dst_alias).and_then(|v| match v {
                Value::I64(id) => Some(NodeId(*id as u64)),
                _ => None,
            });

            let mut expanded = Vec::with_capacity(dst_ids.len());
            for dst_id in dst_ids {
                if let Some(required) = bound_dst {
                    if dst_id != required {
                        continue;
                    }
                }
                let dst_node = node::get_node(self.conn, dst_id)?;
                let mut new_rec = rec.clone();
                new_rec.set(self.dst_alias.clone(), Value::I64(dst_id.0 as i64));
                for (key, val) in &dst_node.properties {
                    new_rec.set(format!("{}.{key}", self.dst_alias), val.clone());
                }
                new_rec.set(
                    format!("{}.__label", self.dst_alias),
                    Value::String(dst_node.labels.join(":")),
                );
                new_rec.set(
                    format!("{}.__labels", self.dst_alias),
                    Value::List(
                        dst_node
                            .labels
                            .iter()
                            .map(|l| Value::String(l.clone()))
                            .collect(),
                    ),
                );
                new_rec.set(
                    format!("{}.__id", self.dst_alias),
                    Value::I64(dst_id.0 as i64),
                );
                if let Some(ref r_alias) = self.rel_alias {
                    let (edge_src, edge_dst) = match self.direction {
                        Direction::Incoming => (dst_id, src_id),
                        _ => (src_id, dst_id),
                    };

                    // Relationship uniqueness: skip if another named rel in
                    // this record already uses the same edge. Normalize to
                    // (min, max, type) for direction-independent comparison.
                    let (es, ed) = (edge_src.0 as i64, edge_dst.0 as i64);
                    let ek = (es.min(ed), es.max(ed), label);
                    let mut dup = false;
                    for (key, _) in &new_rec.fields {
                        if key.ends_with(".__src") && !key.starts_with(&format!("{r_alias}.")) {
                            let oa = &key[..key.len() - 6];
                            if let (Some(Value::I64(os)), Some(Value::I64(od)), Some(Value::String(ot))) = (
                                new_rec.get(key),
                                new_rec.get(&format!("{oa}.__dst")),
                                new_rec.get(&format!("{oa}.__type")),
                            ) {
                                let ok = ((*os).min(*od), (*os).max(*od), ot.as_str());
                                if ok == ek {
                                    dup = true;
                                    break;
                                }
                            }
                        }
                    }
                    if dup {
                        continue;
                    }

                    new_rec.set(format!("{r_alias}.__src"), Value::I64(edge_src.0 as i64));
                    new_rec.set(format!("{r_alias}.__dst"), Value::I64(edge_dst.0 as i64));
                    new_rec.set(
                        format!("{r_alias}.__type"),
                        Value::String(label.to_string()),
                    );
                    if let Ok(props) =
                        edge::get_edge_properties(self.conn, edge_src, edge_dst, label)
                    {
                        for (key, val) in &props {
                            new_rec.set(format!("{r_alias}.{key}"), val.clone());
                        }
                    }
                }
                expanded.push(new_rec);
            }
            self.buffer = expanded.into_iter();
        }
    }
}

// ── Builder ─────────────────────────────────────────────────────────────

/// Build an iterator tree from a logical plan.
///
/// Read-only operators get streaming iterators. Write operators and complex
/// ops that aren't yet migrated fall back to the old `exec()` path and
/// wrap the results in a `VecIter`.
pub fn build_iter<'a>(
    conn: &'a Connection,
    plan: &'a LogicalOp,
) -> Result<Box<dyn RecordIter + 'a>> {
    match plan {
        LogicalOp::EmptyRow => Ok(Box::new(EmptyRowIter::new())),

        LogicalOp::Scan { label, alias } => {
            // Materialize the scan (SQLite rows) but the pipeline above streams.
            let nodes = node::find_nodes_by_label(conn, label)?;
            let records: Vec<Record> = nodes.iter().map(|n| node_to_record(n, alias)).collect();
            Ok(Box::new(VecIter::new(records)))
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
            Ok(Box::new(VecIter::new(records)))
        }

        LogicalOp::Filter { input, predicate } => {
            let input_iter = build_iter(conn, input)?;
            Ok(Box::new(FilterIter {
                input: input_iter,
                predicate: predicate.clone(),
                conn,
            }))
        }

        LogicalOp::Skip { input, count } => {
            let input_iter = build_iter(conn, input)?;
            Ok(Box::new(SkipIter {
                input: input_iter,
                remaining_to_skip: *count,
            }))
        }

        LogicalOp::Limit { input, count } => {
            let input_iter = build_iter(conn, input)?;
            Ok(Box::new(LimitIter {
                input: input_iter,
                remaining: *count,
            }))
        }

        LogicalOp::Project {
            input,
            items,
            emit_compound,
        } => {
            let input_iter = build_iter(conn, input)?;
            Ok(Box::new(ProjectIter {
                input: input_iter,
                items: items.clone(),
                emit_compound: *emit_compound,
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
            ..
        } => {
            let input_iter = build_iter(conn, input)?;
            Ok(Box::new(ExpandIter {
                input: input_iter,
                conn,
                src_alias: src_alias.clone(),
                dst_alias: dst_alias.clone(),
                rel_alias: rel_alias.clone(),
                edge_types: edge_types.clone(),
                direction: *direction,
                min_hops: *min_hops,
                max_hops: *max_hops,
                buffer: Vec::new().into_iter(),
            }))
        }

        LogicalOp::Distinct { input } => {
            // Blocking: must materialize to deduplicate.
            let mut input_iter = build_iter(conn, input)?;
            let records = collect_all(&mut *input_iter)?;
            let mut seen = Vec::new();
            let mut deduped = Vec::new();
            for rec in records {
                if !seen.iter().any(|s: &Record| s.fields == rec.fields) {
                    seen.push(rec.clone());
                    deduped.push(rec);
                }
            }
            Ok(Box::new(VecIter::new(deduped)))
        }

        LogicalOp::Sort { input, items } => {
            // Blocking: must materialize all input before sorting.
            let mut input_iter = build_iter(conn, input)?;
            let mut records = collect_all(&mut *input_iter)?;
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
            Ok(Box::new(VecIter::new(records)))
        }

        // For all other operators (Aggregate, write ops, etc.), fall back to
        // the existing `exec()` and wrap in VecIter.
        _ => {
            use crate::cypher::executor::execute;
            let records = execute(conn, plan)?;
            Ok(Box::new(VecIter::new(records)))
        }
    }
}

/// Compare two values for sorting (null-last semantics).
fn compare_values_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::I64(a), Value::I64(b)) => a.cmp(b),
        (Value::F64(a), Value::F64(b)) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
        (Value::I64(a), Value::F64(b)) => (*a as f64)
            .partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::F64(a), Value::I64(b)) => a
            .partial_cmp(&(*b as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        _ => std::cmp::Ordering::Equal,
    }
}
