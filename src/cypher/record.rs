use indexmap::IndexMap;

use crate::types::Value;

/// A single result record — a row of named values produced by the executor.
///
/// Uses `IndexMap` to preserve insertion order, giving deterministic column
/// ordering in query results.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedRecord {
    pub(crate) fields: IndexMap<String, Value>,
}

impl NamedRecord {
    /// Create an empty record.
    pub fn new() -> Self {
        Self {
            fields: IndexMap::new(),
        }
    }

    /// Look up a field by name.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// Insert or replace a field.
    pub fn set(&mut self, key: String, value: Value) {
        self.fields.insert(key, value);
    }

    /// Remove a key from the record.
    pub fn remove(&mut self, key: &str) {
        self.fields.swap_remove(key);
    }

    /// Number of fields in the record.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether the record has no fields.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Iterate `(name, value)` pairs in column (insertion) order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.fields.iter()
    }

    /// Iterate column names in column order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.fields.keys()
    }

    /// Iterate values in column order.
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.fields.values()
    }

    /// Column name at the given positional index.
    pub fn name_at(&self, index: usize) -> Option<&String> {
        self.fields.get_index(index).map(|(k, _)| k)
    }

    /// Value at the given positional index.
    pub fn value_at(&self, index: usize) -> Option<&Value> {
        self.fields.get_index(index).map(|(_, v)| v)
    }
}

impl Default for NamedRecord {
    fn default() -> Self {
        Self::new()
    }
}
