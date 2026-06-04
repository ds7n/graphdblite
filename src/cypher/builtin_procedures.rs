//! Built-in procedures (`db.*`) dispatched directly by the planner +
//! executor, separate from the test-only `ProcedureRegistry`.
//!
//! Adding a new built-in:
//! 1. Add a `name => BuiltinSig { ... }` arm in [`builtin_signature`].
//! 2. Add a `name => ...` arm in [`execute_builtin`].
//! 3. Add unit tests in this module.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::cypher::procedure::ProcParam;
use crate::storage::{fts, index};
use crate::types::{Result, Value};

/// Signature of a built-in procedure. Mirrors the fields of
/// `ProcedureDef` that the planner needs for arg/yield validation.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct BuiltinSig {
    pub inputs: Vec<ProcParam>,
    pub outputs: Vec<ProcParam>,
}

/// Return a signature for a known built-in, or `None`.
pub fn builtin_signature(name: &str) -> Option<BuiltinSig> {
    match name {
        "db.indexes" => Some(BuiltinSig {
            inputs: Vec::new(),
            outputs: vec![
                ProcParam {
                    name: "label".to_string(),
                    type_name: "STRING?".to_string(),
                },
                ProcParam {
                    name: "property".to_string(),
                    type_name: "STRING?".to_string(),
                },
                ProcParam {
                    name: "kind".to_string(),
                    type_name: "STRING?".to_string(),
                },
            ],
        }),
        _ => None,
    }
}

/// Run a built-in by name. Caller must have validated the name via
/// [`builtin_signature`] first.
pub fn execute_builtin(name: &str, conn: &Connection) -> Result<Vec<HashMap<String, Value>>> {
    match name {
        "db.indexes" => exec_db_indexes(conn),
        other => unreachable!("execute_builtin called with unknown name `{other}`"),
    }
}

fn exec_db_indexes(conn: &Connection) -> Result<Vec<HashMap<String, Value>>> {
    let secondary = index::list_all_indexes(conn)?;
    let fulltext = fts::list_all_fulltext_indexes(conn)?;
    let mut rows = Vec::with_capacity(secondary.len() + fulltext.len());
    for (label, property) in secondary {
        rows.push(row(&label, &property, "btree"));
    }
    for (label, property) in fulltext {
        rows.push(row(&label, &property, "fulltext"));
    }
    Ok(rows)
}

fn row(label: &str, property: &str, kind: &str) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(3);
    m.insert("label".to_string(), Value::String(label.to_string()));
    m.insert("property".to_string(), Value::String(property.to_string()));
    m.insert("kind".to_string(), Value::String(kind.to_string()));
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{fts, index};

    fn fresh_conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&c).unwrap();
        c
    }

    #[test]
    fn signature_for_db_indexes() {
        let sig = builtin_signature("db.indexes").unwrap();
        assert!(sig.inputs.is_empty());
        let names: Vec<&str> = sig.outputs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["label", "property", "kind"]);
    }

    #[test]
    fn signature_for_unknown_returns_none() {
        assert!(builtin_signature("db.nonsense").is_none());
        assert!(builtin_signature("not.a.builtin").is_none());
    }

    #[test]
    fn exec_db_indexes_empty_on_fresh_db() {
        let conn = fresh_conn();
        let rows = execute_builtin("db.indexes", &conn).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn exec_db_indexes_returns_btree_and_fulltext() {
        let conn = fresh_conn();
        index::create_index(&conn, "Person", "name").unwrap();
        fts::create_fulltext_index(&conn, "Doc", "body").unwrap();
        let rows = execute_builtin("db.indexes", &conn).unwrap();
        assert_eq!(rows.len(), 2);

        let mut tuples: Vec<(String, String, String)> = rows
            .into_iter()
            .map(|r| {
                let lab = match r.get("label").unwrap() {
                    Value::String(s) => s.clone(),
                    v => panic!("label was {v:?}"),
                };
                let prop = match r.get("property").unwrap() {
                    Value::String(s) => s.clone(),
                    v => panic!("property was {v:?}"),
                };
                let kind = match r.get("kind").unwrap() {
                    Value::String(s) => s.clone(),
                    v => panic!("kind was {v:?}"),
                };
                (lab, prop, kind)
            })
            .collect();
        tuples.sort();
        assert_eq!(
            tuples,
            vec![
                (
                    "Doc".to_string(),
                    "body".to_string(),
                    "fulltext".to_string()
                ),
                (
                    "Person".to_string(),
                    "name".to_string(),
                    "btree".to_string()
                ),
            ]
        );
    }

    #[test]
    fn exec_db_indexes_yields_two_rows_for_btree_plus_fulltext_on_same_pair() {
        let conn = fresh_conn();
        index::create_index(&conn, "Doc", "body").unwrap();
        fts::create_fulltext_index(&conn, "Doc", "body").unwrap();
        let rows = execute_builtin("db.indexes", &conn).unwrap();
        assert_eq!(rows.len(), 2);
        let kinds: std::collections::HashSet<String> = rows
            .iter()
            .map(|r| match r.get("kind").unwrap() {
                Value::String(s) => s.clone(),
                v => panic!("kind was {v:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            ["btree".to_string(), "fulltext".to_string()]
                .into_iter()
                .collect()
        );
    }
}
