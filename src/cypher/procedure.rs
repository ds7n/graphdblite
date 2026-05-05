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

    /// Register a procedure definition.
    ///
    /// The name must match the grammar's `procedure_name` rule:
    /// `ident ("." ident)+` — at least one dot, each segment a valid
    /// identifier (`[A-Za-z_][A-Za-z0-9_]*`). A name that fails this rule
    /// is unreachable from any parseable `CALL` query. Duplicate names
    /// overwrite the prior registration.
    pub fn register(&mut self, proc_def: ProcedureDef) {
        debug_assert!(
            is_valid_procedure_name(&proc_def.name),
            "procedure name {:?} is not callable from any parseable CALL query",
            proc_def.name
        );
        self.procs.insert(proc_def.name.clone(), proc_def);
    }

    pub fn get(&self, name: &str) -> Option<&ProcedureDef> {
        self.procs.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }
}

fn is_valid_procedure_name(name: &str) -> bool {
    let mut segments = name.split('.');
    let Some(first) = segments.next() else {
        return false;
    };
    if !is_valid_ident(first) {
        return false;
    }
    let mut had_more = false;
    for seg in segments {
        had_more = true;
        if !is_valid_ident(seg) {
            return false;
        }
    }
    had_more
}

fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}
