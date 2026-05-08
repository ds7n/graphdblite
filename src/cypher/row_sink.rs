//! `RowSink` — write-by-name abstraction over the legacy [`NamedRecord`] and
//! the slot-indexed [`record_v2::Record`].
//!
//! Lives only during the record-v2 migration. Phase 3/4 executor and `iter`
//! code is generic over `RowSink`; once the slot shape becomes the only
//! shape (Phase 6), this trait is deleted.
//!
//! [`NamedRecord`]: crate::cypher::record::NamedRecord
//! [`record_v2::Record`]: crate::cypher::record_v2::Record

#![allow(dead_code)]

use crate::cypher::record::NamedRecord;
use crate::cypher::record_v2::{Record as SlotRecord, RecordSchema};
use crate::types::Value;

/// Row builder with name-keyed writes. Both record shapes implement this;
/// the slot-backed impl needs a [`RecordSchema`] to translate names to
/// slots, the named impl ignores it.
pub trait RowSink {
    /// Write `value` under `name`.
    fn set_by_name(&mut self, schema: &RecordSchema, name: &str, value: Value);

    /// Read the value at `name`, if present. Returns `None` for unknown
    /// names (legacy shape: missing key; slot shape: name not in schema).
    fn get_by_name(&self, schema: &RecordSchema, name: &str) -> Option<&Value>;
}

impl RowSink for NamedRecord {
    fn set_by_name(&mut self, _schema: &RecordSchema, name: &str, value: Value) {
        self.fields.insert(name.to_string(), value);
    }

    fn get_by_name(&self, _schema: &RecordSchema, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }
}

impl RowSink for SlotRecord {
    fn set_by_name(&mut self, schema: &RecordSchema, name: &str, value: Value) {
        if let Some(slot) = schema.slot(name) {
            self.set(slot, value);
        }
    }

    fn get_by_name(&self, schema: &RecordSchema, name: &str) -> Option<&Value> {
        schema.slot(name).map(|s| self.get(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_with(names: &[&str]) -> RecordSchema {
        let mut s = RecordSchema::new();
        for n in names {
            s.add(*n);
        }
        s
    }

    #[test]
    fn named_record_sink_ignores_schema() {
        let schema = RecordSchema::new();
        let mut r = NamedRecord::new();
        r.set_by_name(&schema, "p.name", Value::String("Alice".into()));
        assert_eq!(
            r.get_by_name(&schema, "p.name"),
            Some(&Value::String("Alice".into()))
        );
    }

    #[test]
    fn slot_record_sink_uses_schema() {
        let schema = schema_with(&["p", "p.name"]);
        let mut r = SlotRecord::with_capacity(schema.len());
        r.set_by_name(&schema, "p.name", Value::String("Alice".into()));
        assert_eq!(
            r.get_by_name(&schema, "p.name"),
            Some(&Value::String("Alice".into()))
        );
        assert_eq!(r.get_by_name(&schema, "p"), Some(&Value::Null));
    }

    #[test]
    fn slot_record_sink_unknown_name_is_noop() {
        let schema = schema_with(&["p"]);
        let mut r = SlotRecord::with_capacity(schema.len());
        r.set_by_name(&schema, "missing", Value::I64(1));
        assert!(r.get_by_name(&schema, "missing").is_none());
        assert_eq!(r.get_by_name(&schema, "p"), Some(&Value::Null));
    }
}
