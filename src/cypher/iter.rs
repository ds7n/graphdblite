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
        Self { records: records.into_iter() }
    }
}

impl RecordIter for VecIter {
    fn next_record(&mut self) -> Result<Option<Record>> {
        Ok(self.records.next())
    }
}

/// Yields a single empty record, then stops.
pub struct EmptyRowIter {
    done: bool,
}

impl EmptyRowIter {
    pub fn new() -> Self {
        Self { done: false }
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

/// Projects each input record through the RETURN clause.
pub struct ProjectIter<'a> {
    input: Box<dyn RecordIter + 'a>,
    items: Vec<ReturnItem>,
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
                    for (key, val) in &rec.fields {
                        if is_user_visible_field(key) {
                            projected.set(key.clone(), val.clone());
                        }
                    }
                }
                Expr::Variable(var) => {
                    let prefix = format!("{var}.");
                    let mut found_props = false;
                    for (key, val) in &rec.fields {
                        if let Some(prop) = key.strip_prefix(&prefix) {
                            if !prop.starts_with("__") {
                                if let Some(alias) = &item.alias {
                                    projected.set(format!("{alias}.{prop}"), val.clone());
                                } else {
                                    projected.set(key.clone(), val.clone());
                                }
                                found_props = true;
                            }
                        }
                    }
                    if !found_props {
                        let col_name = item.alias.clone().unwrap_or_else(|| var.clone());
                        let val = if let Some(existing) = rec.get(&col_name) {
                            existing.clone()
                        } else {
                            eval_expr(&item.expr, &rec, self.conn)?
                        };
                        projected.set(col_name, val);
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
    edge_type: Option<String>,
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

            let label = self.edge_type.as_deref().unwrap_or("");
            let dst_ids = if self.min_hops == 1 && self.max_hops == 1 {
                edge::get_neighbors(self.conn, src_id, label, self.direction)?
            } else {
                edge::traverse(self.conn, src_id, label, self.direction, self.min_hops, self.max_hops)?
            };

            let mut expanded = Vec::with_capacity(dst_ids.len());
            for dst_id in dst_ids {
                let dst_node = node::get_node(self.conn, dst_id)?;
                let mut new_rec = rec.clone();
                new_rec.set(self.dst_alias.clone(), Value::I64(dst_id.0 as i64));
                for (key, val) in &dst_node.properties {
                    new_rec.set(format!("{}.{key}", self.dst_alias), val.clone());
                }
                new_rec.set(
                    format!("{}.__label", self.dst_alias),
                    Value::String(dst_node.label.clone()),
                );
                new_rec.set(format!("{}.__id", self.dst_alias), Value::I64(dst_id.0 as i64));
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

        LogicalOp::IndexLookup { label, alias, property, value, remaining_filters } => {
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

        LogicalOp::Limit { input, count } => {
            let input_iter = build_iter(conn, input)?;
            Ok(Box::new(LimitIter {
                input: input_iter,
                remaining: *count,
            }))
        }

        LogicalOp::Project { input, items } => {
            let input_iter = build_iter(conn, input)?;
            Ok(Box::new(ProjectIter {
                input: input_iter,
                items: items.clone(),
                conn,
            }))
        }

        LogicalOp::Expand {
            input, src_alias, dst_alias, edge_type, direction, min_hops, max_hops,
        } => {
            let input_iter = build_iter(conn, input)?;
            Ok(Box::new(ExpandIter {
                input: input_iter,
                conn,
                src_alias: src_alias.clone(),
                dst_alias: dst_alias.clone(),
                edge_type: edge_type.clone(),
                direction: *direction,
                min_hops: *min_hops,
                max_hops: *max_hops,
                buffer: Vec::new().into_iter(),
            }))
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
