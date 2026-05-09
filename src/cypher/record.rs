use indexmap::IndexMap;

use crate::types::Value;

/// A single result record — a row of named values produced by the executor.
///
/// Uses `IndexMap` to preserve insertion order, giving deterministic column
/// ordering in query results.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)]
pub struct NamedRecord {
    pub fields: IndexMap<String, Value>,
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
}

impl Default for NamedRecord {
    fn default() -> Self {
        Self::new()
    }
}
