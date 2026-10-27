//! Named-record pull iterators used by the slot engine (`iter_slot`).
//!
//! `RecordIter` yields one `NamedRecord` at a time; `ExpandIter` drives
//! `executor::read::expand_record` over such a stream so the slot path shares
//! the materialized executor's Expand semantics.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::cypher::record::NamedRecord;
use crate::types::{Direction, Result, Value};

/// Pull-based iterator that yields one record at a time.
///
/// Uses a fallible `next_record` method instead of `Iterator` because:
/// - Iteration can fail (SQLite errors, serialization errors)
/// - Iterators hold references to the connection, making standard `Iterator`
///   lifetime management complex
pub trait RecordIter {
    /// Return the next record, or `None` when exhausted.
    fn next_record(&mut self) -> Result<Option<NamedRecord>>;
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
    var_length: bool,
    /// Relationship property constraints of a var-length pattern, evaluated
    /// per input row (they may reference parameters and row variables).
    var_length_prop_filters: HashMap<String, crate::cypher::ast::Expr>,
    max_traversal_work: u64,
    /// Buffer of expanded records from the current input record.
    buffer: std::vec::IntoIter<NamedRecord>,
}

impl<'a> ExpandIter<'a> {
    /// Construct an ExpandIter from an arbitrary upstream `RecordIter`. Used
    /// by `cypher::iter_slot` to wrap a slot-input adapter so the slot path
    /// reuses Expand's dense traversal logic instead of duplicating it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn RecordIter + 'a>,
        conn: &'a Connection,
        src_alias: String,
        dst_alias: String,
        rel_alias: Option<String>,
        edge_types: Vec<String>,
        direction: Direction,
        min_hops: u32,
        max_hops: u32,
        var_length: bool,
        var_length_prop_filters: HashMap<String, crate::cypher::ast::Expr>,
        max_traversal_work: u64,
    ) -> Self {
        Self {
            input,
            conn,
            src_alias,
            dst_alias,
            rel_alias,
            edge_types,
            direction,
            min_hops,
            max_hops,
            var_length,
            var_length_prop_filters,
            max_traversal_work,
            buffer: Vec::new().into_iter(),
        }
    }
}

impl<'a> RecordIter for ExpandIter<'a> {
    fn next_record(&mut self) -> Result<Option<NamedRecord>> {
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

            // Delegate to `executor::read::expand_record` so this iterator
            // matches `exec_expand`'s semantics on every shape — including
            // untyped relationships, `Direction::Both` parallel edges, and
            // the `__edge_seq` binding for relationship uniqueness.
            let prop_filter_values = crate::cypher::executor::eval_var_length_prop_filters(
                self.conn,
                &self.var_length_prop_filters,
                &rec,
            )?;
            let expanded = crate::cypher::executor::expand_record(
                self.conn,
                &rec,
                &NamedRecord::new(),
                &self.src_alias,
                &self.dst_alias,
                self.rel_alias.as_deref(),
                &self.edge_types,
                self.direction,
                self.min_hops,
                self.max_hops,
                self.var_length,
                &prop_filter_values,
                None,
                self.max_traversal_work,
            )?;
            self.buffer = expanded.into_iter();
        }
    }
}

/// Crate-internal accessor so `iter_slot::SortSlotIter` reuses the single
/// canonical comparator in `executor::util`. Keeping one source ensures the
/// materialized, named-iter, and slot-iter Sort paths order rows identically
/// across every value type (Bool, List, temporal, Duration, NaN-last).
pub(crate) fn compare_values_for_sort_pub(a: &Value, b: &Value) -> std::cmp::Ordering {
    crate::cypher::executor::compare_values_for_sort(a, b)
}
