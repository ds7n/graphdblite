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
use crate::stats;
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
        "db.counts" => Some(BuiltinSig {
            inputs: Vec::new(),
            outputs: vec![
                ProcParam {
                    name: "kind".to_string(),
                    type_name: "STRING?".to_string(),
                },
                ProcParam {
                    name: "name".to_string(),
                    type_name: "STRING?".to_string(),
                },
                ProcParam {
                    name: "count".to_string(),
                    type_name: "INTEGER?".to_string(),
                },
            ],
        }),
        _ => None,
    }
}

/// Run a built-in by name. Caller must have validated the name via
/// [`builtin_signature`] first.
///
/// `args` carries the evaluated procedure arguments for the current
/// outer record. Built-ins that take no arguments (e.g. `db.indexes`,
/// `db.counts`) ignore the slice; arg-parameterized built-ins read it.
pub fn execute_builtin(
    name: &str,
    args: &[Value],
    conn: &Connection,
) -> Result<Vec<HashMap<String, Value>>> {
    match name {
        "db.indexes" => {
            debug_assert!(args.is_empty(), "db.indexes takes no args");
            exec_db_indexes(conn)
        }
        "db.counts" => {
            debug_assert!(args.is_empty(), "db.counts takes no args");
            exec_db_counts(conn)
        }
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
    for (label, property, kind) in fulltext {
        let kind_str = match kind {
            crate::storage::fts::FtsTokenizerKind::TrigramCaseSensitive => "fulltext",
            crate::storage::fts::FtsTokenizerKind::TrigramCaseInsensitive => "fulltext_ci",
            crate::storage::fts::FtsTokenizerKind::Word => "fulltext_word",
        };
        rows.push(row(&label, &property, kind_str));
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

fn exec_db_counts(conn: &Connection) -> Result<Vec<HashMap<String, Value>>> {
    let labels = stats::get_all_label_counts(conn)?;
    let edge_types = stats::get_all_edge_type_counts(conn)?;
    let mut rows = Vec::with_capacity(labels.len() + edge_types.len());
    for (name, count) in labels {
        rows.push(count_row("label", &name, count));
    }
    for (name, count) in edge_types {
        rows.push(count_row("edge_type", &name, count));
    }
    Ok(rows)
}

fn count_row(kind: &str, name: &str, count: u64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(3);
    m.insert("kind".to_string(), Value::String(kind.to_string()));
    m.insert("name".to_string(), Value::String(name.to_string()));
    m.insert("count".to_string(), Value::I64(count as i64));
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
        let rows = execute_builtin("db.indexes", &[], &conn).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn exec_db_indexes_returns_btree_and_fulltext() {
        let conn = fresh_conn();
        index::create_index(&conn, "Person", "name").unwrap();
        fts::create_fulltext_index(&conn, "Doc", "body").unwrap();
        let rows = execute_builtin("db.indexes", &[], &conn).unwrap();
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
        let rows = execute_builtin("db.indexes", &[], &conn).unwrap();
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

    #[test]
    fn exec_db_indexes_distinguishes_ci_and_cs() {
        let conn = fresh_conn();
        crate::storage::fts::create_fulltext_index(&conn, "Person", "name").unwrap();
        crate::storage::fts::create_fulltext_index_ci(&conn, "Article", "body").unwrap();
        let rows = execute_builtin("db.indexes", &[], &conn).unwrap();
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
                    "Article".to_string(),
                    "body".to_string(),
                    "fulltext_ci".to_string()
                ),
                (
                    "Person".to_string(),
                    "name".to_string(),
                    "fulltext".to_string()
                ),
            ]
        );
    }

    #[test]
    fn signature_for_db_counts() {
        let sig = builtin_signature("db.counts").unwrap();
        assert!(sig.inputs.is_empty());
        let names: Vec<&str> = sig.outputs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["kind", "name", "count"]);
        let types: Vec<&str> = sig.outputs.iter().map(|p| p.type_name.as_str()).collect();
        assert_eq!(types, vec!["STRING?", "STRING?", "INTEGER?"]);
    }

    #[test]
    fn exec_db_counts_empty_on_fresh_db() {
        let conn = fresh_conn();
        let rows = execute_builtin("db.counts", &[], &conn).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn exec_db_counts_returns_labels_and_edge_types() {
        let conn = fresh_conn();
        let a =
            crate::storage::node::create_node(&conn, &["Person".to_string()], Default::default())
                .unwrap();
        let _b =
            crate::storage::node::create_node(&conn, &["Person".to_string()], Default::default())
                .unwrap();
        let c =
            crate::storage::node::create_node(&conn, &["Company".to_string()], Default::default())
                .unwrap();
        crate::storage::edge::create_edge(&conn, a, c, "WORKS_AT", Default::default()).unwrap();
        crate::storage::edge::create_edge(&conn, a, c, "KNOWS", Default::default()).unwrap();
        crate::storage::edge::create_edge(&conn, a, c, "KNOWS", Default::default()).unwrap();

        let rows = execute_builtin("db.counts", &[], &conn).unwrap();
        let mut tuples: Vec<(String, String, i64)> = rows
            .into_iter()
            .map(|r| {
                let kind = match r.get("kind").unwrap() {
                    Value::String(s) => s.clone(),
                    v => panic!("kind was {v:?}"),
                };
                let name = match r.get("name").unwrap() {
                    Value::String(s) => s.clone(),
                    v => panic!("name was {v:?}"),
                };
                let count = match r.get("count").unwrap() {
                    Value::I64(n) => *n,
                    v => panic!("count was {v:?}"),
                };
                (kind, name, count)
            })
            .collect();
        tuples.sort();
        assert_eq!(
            tuples,
            vec![
                ("edge_type".to_string(), "KNOWS".to_string(), 2),
                ("edge_type".to_string(), "WORKS_AT".to_string(), 1),
                ("label".to_string(), "Company".to_string(), 1),
                ("label".to_string(), "Person".to_string(), 2),
            ]
        );
    }
}
