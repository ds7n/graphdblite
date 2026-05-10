//! `RecordView` — read-only abstraction over both record shapes.
//!
//! Eval (`cypher::eval`) and the compound-binding helpers
//! (`executor::read::build_compound_binding`, `compound_binding_vars`)
//! used to take `&NamedRecord` and access fields via a `String`-keyed
//! lookup. To avoid materializing a `NamedRecord` per row in the
//! slot-indexed iterator stack (`iter_slot`), they now take
//! `&dyn RecordView`, and a [`SlotView`] wraps `(&RecordSchema,
//! &SlotRecord)` so name lookups resolve through the schema's
//! slot table.

use crate::cypher::record::NamedRecord;
use crate::cypher::record_v2::{Record as SlotRecord, RecordSchema};
use crate::types::Value;

/// Read-only field accessor used by eval and compound-binding helpers.
///
/// Implementors must:
/// - return `None` from `get` for unknown names (don't panic),
/// - visit every name/value pair via `for_each_field` exactly once,
/// - keep both methods consistent with each other.
pub trait RecordView {
    /// Look up a field by name. Returns `None` if absent.
    fn get(&self, name: &str) -> Option<&Value>;

    /// Visit every (name, value) pair. Used by helpers that prefix-scan
    /// for compound bindings (`alias.__id`, `alias.prop`, …).
    fn for_each_field(&self, f: &mut dyn FnMut(&str, &Value));
}

impl RecordView for NamedRecord {
    fn get(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    fn for_each_field(&self, f: &mut dyn FnMut(&str, &Value)) {
        for (k, v) in &self.fields {
            f(k.as_str(), v);
        }
    }
}

/// Borrowed view over a slot record paired with its schema. Constructed
/// per row at the point eval needs to read fields by name.
#[derive(Clone, Copy)]
pub struct SlotView<'a> {
    schema: &'a RecordSchema,
    rec: &'a SlotRecord,
}

impl<'a> SlotView<'a> {
    /// Wrap a schema + record for read-only name access.
    pub fn new(schema: &'a RecordSchema, rec: &'a SlotRecord) -> Self {
        Self { schema, rec }
    }
}

impl<'a> RecordView for SlotView<'a> {
    fn get(&self, name: &str) -> Option<&Value> {
        self.schema.slot(name).map(|s| self.rec.get(s))
    }

    fn for_each_field(&self, f: &mut dyn FnMut(&str, &Value)) {
        for (slot, name) in self.schema.iter() {
            f(name, self.rec.get(slot));
        }
    }
}

/// Materialize a `RecordView` back into an owned `NamedRecord`. Used at
/// the few eval → executor boundaries that still expect the named shape
/// (correlated EXISTS / pattern comprehensions). Per-row hot paths
/// (Filter, Project, Sort, Unwind) avoid this by passing the view
/// through directly.
pub fn view_to_named(view: &dyn RecordView) -> NamedRecord {
    let mut out = NamedRecord::new();
    view.for_each_field(&mut |k, v| {
        out.set(k.to_string(), v.clone());
    });
    out
}
