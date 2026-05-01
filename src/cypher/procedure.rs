use crate::types::Value;
use std::collections::HashMap;

/// A parameter in a procedure signature (input or output).
#[derive(Debug, Clone)]
pub struct ProcParam {
    pub name: String,
    /// Cypher type name: "STRING?", "INTEGER?", "FLOAT?", "NUMBER?", "BOOLEAN?", "ANY?"
    pub type_name: String,
}

/// A registered test procedure: signature + canned data rows.
#[derive(Debug, Clone)]
pub struct ProcedureDef {
    pub name: String,
    pub inputs: Vec<ProcParam>,
    pub outputs: Vec<ProcParam>,
    /// Each row is a map from column name (inputs + outputs) → Value.
    pub rows: Vec<HashMap<String, Value>>,
}

/// Registry of mock procedures for test use.
#[derive(Debug, Clone, Default)]
pub struct ProcedureRegistry {
    procs: HashMap<String, ProcedureDef>,
}

impl ProcedureRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, proc_def: ProcedureDef) {
        self.procs.insert(proc_def.name.clone(), proc_def);
    }

    pub fn get(&self, name: &str) -> Option<&ProcedureDef> {
        self.procs.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }
}
