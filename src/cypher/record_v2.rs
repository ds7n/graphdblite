//! Slot-indexed Record (record-v2).
//!
//! See `plans/record-v2.md`. A row is a `Vec<Value>` indexed by `SlotId`;
//! a per-query [`RecordSchema`] translates between binding names and slot
//! indices. Built alongside the legacy [`crate::cypher::record::NamedRecord`]
//! during the migration; nothing wires it into the executor yet.

#![allow(dead_code)]

use std::collections::HashMap;

use crate::types::Value;

/// Slot index into a [`Record`]. Newtype around `u32` so we never confuse
/// "slot" (Record index) with "column" (RETURN order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotId(pub u32);

impl SlotId {
    /// Construct a slot from a `usize` index. Panics if the index does not
    /// fit in `u32` — schemas with >4G slots are not supported.
    #[inline]
    pub fn from_usize(i: usize) -> Self {
        Self(u32::try_from(i).expect("SlotId overflow"))
    }

    /// Index as `usize`, ready for `Vec` access.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A row in the result stream. Slot indices are assigned by the planner;
/// names are recovered via [`RecordSchema`].
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    values: Vec<Value>,
}

impl Record {
    /// Empty row — no slots.
    pub fn new() -> Self {
        Self { values: Vec::new() }
    }

    /// Row of `n` slots, each initialized to [`Value::Null`].
    pub fn with_capacity(n: usize) -> Self {
        Self {
            values: vec![Value::Null; n],
        }
    }

    /// Read the value at `slot`. Panics if `slot` is out of range.
    #[inline]
    pub fn get(&self, slot: SlotId) -> &Value {
        &self.values[slot.index()]
    }

    /// Write `v` into `slot`. Panics if `slot` is out of range.
    #[inline]
    pub fn set(&mut self, slot: SlotId, v: Value) {
        self.values[slot.index()] = v;
    }

    /// Number of slots in the row.
    #[inline]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// True iff [`Self::len`] is zero.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Append a value, growing the row by one slot. Returns the new slot's id.
    pub fn push(&mut self, v: Value) -> SlotId {
        let slot = SlotId::from_usize(self.values.len());
        self.values.push(v);
        slot
    }

    /// Iterate values in slot order.
    pub fn iter(&self) -> std::slice::Iter<'_, Value> {
        self.values.iter()
    }
}

impl Default for Record {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps binding names to slot indices for a single query plan. Built by the
/// planner during plan construction; immutable thereafter. Carried on every
/// `LogicalOp` so the executor and `eval` resolve names without per-row
/// allocation.
///
/// Names look like `"p"`, `"p.__id"`, `"p.name"`, `"r.__type"`, or generated
/// aliases like `"_anon_42"`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordSchema {
    by_name: HashMap<String, SlotId>,
    names: Vec<String>,
}

impl RecordSchema {
    /// Empty schema — no slots assigned.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve capacity for `n` slots.
    pub fn with_capacity(n: usize) -> Self {
        Self {
            by_name: HashMap::with_capacity(n),
            names: Vec::with_capacity(n),
        }
    }

    /// Resolve a binding name to its slot, if present.
    pub fn slot(&self, name: &str) -> Option<SlotId> {
        self.by_name.get(name).copied()
    }

    /// Recover the name of a slot. Panics if `slot` is out of range.
    pub fn name(&self, slot: SlotId) -> &str {
        &self.names[slot.index()]
    }

    /// Number of slots in the schema.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// True iff the schema has no slots.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Iterate `(slot, name)` pairs in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (SlotId, &str)> {
        self.names
            .iter()
            .enumerate()
            .map(|(i, n)| (SlotId::from_usize(i), n.as_str()))
    }

    /// Append a binding to the schema and return its newly-assigned slot.
    /// If `name` already exists, the existing slot is returned and no new
    /// slot is allocated — duplicate names collapse onto a single slot.
    pub fn add(&mut self, name: impl Into<String>) -> SlotId {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name) {
            return slot;
        }
        let slot = SlotId::from_usize(self.names.len());
        self.by_name.insert(name.clone(), slot);
        self.names.push(name);
        slot
    }

    /// Slice of binding names in slot order.
    pub fn names(&self) -> &[String] {
        &self.names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_roundtrip() {
        let s = SlotId::from_usize(42);
        assert_eq!(s.0, 42);
        assert_eq!(s.index(), 42);
    }

    #[test]
    fn record_with_capacity_initializes_to_null() {
        let r = Record::with_capacity(3);
        assert_eq!(r.len(), 3);
        for v in r.iter() {
            assert_eq!(v, &Value::Null);
        }
    }

    #[test]
    fn record_get_set() {
        let mut r = Record::with_capacity(2);
        let s0 = SlotId(0);
        let s1 = SlotId(1);
        r.set(s0, Value::I64(7));
        r.set(s1, Value::String("hi".into()));
        assert_eq!(r.get(s0), &Value::I64(7));
        assert_eq!(r.get(s1), &Value::String("hi".into()));
    }

    #[test]
    fn record_push_returns_new_slot() {
        let mut r = Record::new();
        assert!(r.is_empty());
        let s = r.push(Value::I64(1));
        assert_eq!(s, SlotId(0));
        let s2 = r.push(Value::I64(2));
        assert_eq!(s2, SlotId(1));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn schema_add_assigns_contiguous_slots() {
        let mut sc = RecordSchema::new();
        let a = sc.add("p");
        let b = sc.add("p.__id");
        let c = sc.add("p.name");
        assert_eq!(a, SlotId(0));
        assert_eq!(b, SlotId(1));
        assert_eq!(c, SlotId(2));
        assert_eq!(sc.len(), 3);
    }

    #[test]
    fn schema_add_duplicate_returns_existing_slot() {
        let mut sc = RecordSchema::new();
        let a = sc.add("p");
        let a2 = sc.add("p");
        assert_eq!(a, a2);
        assert_eq!(sc.len(), 1);
    }

    #[test]
    fn schema_slot_and_name_are_inverses() {
        let mut sc = RecordSchema::new();
        sc.add("a");
        sc.add("b.c");
        sc.add("_anon_0");
        for (slot, name) in sc
            .iter()
            .map(|(s, n)| (s, n.to_string()))
            .collect::<Vec<_>>()
        {
            assert_eq!(sc.slot(&name), Some(slot));
            assert_eq!(sc.name(slot), &name);
        }
    }

    #[test]
    fn schema_unknown_name_returns_none() {
        let sc = RecordSchema::new();
        assert!(sc.slot("missing").is_none());
    }

    #[test]
    fn schema_iter_in_slot_order() {
        let mut sc = RecordSchema::new();
        sc.add("z");
        sc.add("a");
        sc.add("m");
        let names: Vec<&str> = sc.iter().map(|(_, n)| n).collect();
        assert_eq!(names, vec!["z", "a", "m"]);
    }
}
