use std::collections::HashMap;

use crate::types::Value;

/// A single result record — a row of named values produced by the executor.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub fields: HashMap<String, Value>,
}

impl Record {
    pub fn new() -> Self {
        Self {
            fields: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    pub fn set(&mut self, key: String, value: Value) {
        self.fields.insert(key, value);
    }
}

impl Default for Record {
    fn default() -> Self {
        Self::new()
    }
}
