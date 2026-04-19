use indexmap::IndexMap;

use crate::types::Value;

/// A single result record — a row of named values produced by the executor.
///
/// Uses `IndexMap` to preserve insertion order, giving deterministic column
/// ordering in query results.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub fields: IndexMap<String, Value>,
}

impl Record {
    pub fn new() -> Self {
        Self {
            fields: IndexMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    pub fn set(&mut self, key: String, value: Value) {
        self.fields.insert(key, value);
    }

    /// Remove a key from the record.
    pub fn remove(&mut self, key: &str) {
        self.fields.swap_remove(key);
    }
}

impl Default for Record {
    fn default() -> Self {
        Self::new()
    }
}
