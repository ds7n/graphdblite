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
use crate::types::{ErrorCode, GraphError, NodeId, QueryError, QueryPhase, Result, Value};

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
        "fts.search" => Some(BuiltinSig {
            inputs: vec![
                ProcParam {
                    name: "label".to_string(),
                    type_name: "STRING?".to_string(),
                },
                ProcParam {
                    name: "property".to_string(),
                    type_name: "STRING?".to_string(),
                },
                ProcParam {
                    name: "query".to_string(),
                    type_name: "STRING?".to_string(),
                },
            ],
            outputs: vec![
                ProcParam {
                    name: "node".to_string(),
                    type_name: "NODE?".to_string(),
                },
                ProcParam {
                    name: "score".to_string(),
                    type_name: "FLOAT?".to_string(),
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
        "fts.search" => exec_fts_search(args, conn),
        other => unreachable!("execute_builtin called with unknown name `{other}`"),
    }
}

fn exec_db_indexes(conn: &Connection) -> Result<Vec<HashMap<String, Value>>> {
    let secondary = index::list_all_indexes(conn)?;
    let fulltext = fts::list_all_fulltext_indexes(conn)?;
    let mut rows = Vec::with_capacity(secondary.len() + fulltext.len());
    for info in secondary {
        for prop in &info.properties {
            rows.push(row(&info.label, prop, info.kind));
        }
    }
    for info in fulltext {
        let kind_str = match info.kind {
            crate::storage::fts::FtsTokenizerKind::TrigramCaseSensitive => "fulltext",
            crate::storage::fts::FtsTokenizerKind::TrigramCaseInsensitive => "fulltext_ci",
            crate::storage::fts::FtsTokenizerKind::Word => "fulltext_word",
        };
        for property in &info.properties {
            rows.push(row(&info.label, property, kind_str));
        }
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

/// Build a runtime TypeError for `fts.search` arg validation.
fn fts_search_type_error(message: impl Into<String>) -> GraphError {
    GraphError::Query(QueryError::TypeError {
        phase: QueryPhase::Runtime,
        code: ErrorCode::Other,
        message: message.into(),
        hint: None,
        span: None,
    })
}

/// Resolve the `(label, property)` arg pair for `fts.search` to the
/// matching index + optional column scope.
///
/// Returns `(table_name, Some(column_name))` to scope the MATCH to one
/// FTS5 column, or `(table_name, None)` to search all columns.
///
/// `property == "*"` resolves to whatever single FTS index exists on
/// `label` (errors with a procedure error if none or more than one).
/// Otherwise the function finds the index whose column list covers
/// `property` and scopes to that column for multi-column tables; for
/// legacy single-column tables (whose FTS5 column is literally named
/// `content`), no column-scope is needed.
fn resolve_fts_target(
    conn: &Connection,
    label: &str,
    property: &str,
) -> Result<(String, Option<String>)> {
    let infos = fts::list_fulltext_indexes_for_label(conn, label)?;

    if property == "*" {
        match infos.len() {
            0 => Err(GraphError::IndexNotFound {
                label: label.to_string(),
                properties: vec!["*".to_string()],
                hint: Some(format!("no fulltext index on label `{label}`")),
            }),
            1 => Ok((infos[0].table_name.clone(), None)),
            _ => Err(GraphError::Query(QueryError::ProcedureError {
                phase: QueryPhase::Runtime,
                code: ErrorCode::Other,
                message: format!(
                    "fts.search('*'): label `{label}` has multiple FTS indexes; specify a property to disambiguate"
                ),
                hint: None,
                span: None,
            })),
        }
    } else {
        for info in &infos {
            if info.properties.iter().any(|p| p == property) {
                // For multi-column tables, scope to the column. For
                // legacy single-prop tables (one FTS5 column named
                // `content`), no scope needed — MATCH the bare query.
                let scope = if info.table_name.starts_with("node_fts_multi_") {
                    Some(property.to_string())
                } else {
                    None
                };
                return Ok((info.table_name.clone(), scope));
            }
        }
        Err(GraphError::IndexNotFound {
            label: label.to_string(),
            properties: vec![property.to_string()],
            hint: Some("no fulltext index covers this (label, property)".to_string()),
        })
    }
}

/// Execute `fts.search(label, property, query)` — yields `(node, score)`
/// rows ordered by descending BM25 relevance.
///
/// Requires an FTS index on `(label, property)`. The `unicode61` word
/// tokenizer is the intended pairing; trigram indexes also work but
/// treat the query as a literal substring search.
///
/// `property == "*"` searches all columns of the single FTS index on
/// `label` (errors if there are 0 or >1 indexes). For a specific
/// `property`, the match is scoped to that column on multi-prop tables.
///
/// FTS5's `bm25()` returns a negative-signed score where *lower is
/// better*; we negate so callers can use the conventional
/// `ORDER BY score DESC` ranking.
fn exec_fts_search(args: &[Value], conn: &Connection) -> Result<Vec<HashMap<String, Value>>> {
    let label = match args.first() {
        Some(Value::String(s)) => s.as_str(),
        _ => return Err(fts_search_type_error("fts.search: label must be a string")),
    };
    let property = match args.get(1) {
        Some(Value::String(s)) => s.as_str(),
        _ => {
            return Err(fts_search_type_error(
                "fts.search: property must be a string",
            ));
        }
    };
    let query = match args.get(2) {
        Some(Value::String(s)) => s.as_str(),
        _ => return Err(fts_search_type_error("fts.search: query must be a string")),
    };

    let (table, scope) = resolve_fts_target(conn, label, property)?;

    let match_term = match &scope {
        Some(col) => format!("\"{col}\":({query})"),
        None => query.to_string(),
    };

    // bm25(t) is FTS5's built-in ranking function. It returns a negative
    // score; lower (more negative) = more relevant. We negate below so
    // higher = better in the output.
    let sql = format!(
        "SELECT rowid, bm25(\"{table}\") AS rank \
         FROM \"{table}\" \
         WHERE \"{table}\" MATCH ?1 \
         ORDER BY rank ASC"
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        GraphError::Query(QueryError::ProcedureError {
            phase: QueryPhase::Runtime,
            code: ErrorCode::Other,
            message: format!("fts.search: failed to prepare fulltext query: {e}"),
            hint: Some(format!("query was: {query}")),
            span: None,
        })
    })?;

    let mapped = stmt
        .query_map([&match_term], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })
        .map_err(|e| {
            GraphError::Query(QueryError::ProcedureError {
                phase: QueryPhase::Runtime,
                code: ErrorCode::Other,
                message: format!("fts.search: invalid fulltext query syntax (fts5): {e}"),
                hint: Some(format!("query was: {query}")),
                span: None,
            })
        })?;

    let mut out = Vec::new();
    for r in mapped {
        let (rowid, rank) = r.map_err(|e| {
            GraphError::Query(QueryError::ProcedureError {
                phase: QueryPhase::Runtime,
                code: ErrorCode::Other,
                message: format!("fts.search: error iterating fulltext rows: {e}"),
                hint: Some(format!("query was: {query}")),
                span: None,
            })
        })?;
        let node_id = NodeId(rowid as u64);
        let node = match crate::storage::node::get_node(conn, node_id) {
            Ok(n) => n,
            // Row in FTS table but node row deleted — skip rather than fail.
            Err(GraphError::NodeNotFound { .. }) => continue,
            Err(e) => return Err(e),
        };
        let mut m = HashMap::with_capacity(2);
        m.insert("node".to_string(), Value::Node(node));
        m.insert("score".to_string(), Value::F64(-rank));
        out.push(m);
    }
    Ok(out)
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

    /// Build a `Properties` map from `(name, value)` pairs.
    fn props(pairs: &[(&str, Value)]) -> crate::types::Properties {
        let mut p = HashMap::new();
        for (k, v) in pairs {
            p.insert((*k).to_string(), v.clone());
        }
        p
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

    #[test]
    fn signature_for_fts_search() {
        let sig = builtin_signature("fts.search").unwrap();
        let in_names: Vec<&str> = sig.inputs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(in_names, vec!["label", "property", "query"]);
        let out_names: Vec<&str> = sig.outputs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(out_names, vec!["node", "score"]);
    }

    #[test]
    fn exec_fts_search_returns_nodes_and_scores() {
        let conn = fresh_conn();
        crate::storage::fts::create_fulltext_index_word(&conn, "Person", "bio").unwrap();
        let id1 = crate::storage::node::create_node(
            &conn,
            &["Person".to_string()],
            props(&[("bio", Value::String("rust systems programming".to_string()))]),
        )
        .unwrap();
        let _id2 = crate::storage::node::create_node(
            &conn,
            &["Person".to_string()],
            props(&[("bio", Value::String("python data science".to_string()))]),
        )
        .unwrap();
        // create_node doesn't trigger FTS sync — drop+recreate so backfill
        // picks up the rows we just inserted.
        crate::storage::fts::drop_fulltext_index(&conn, "Person", "bio").unwrap();
        crate::storage::fts::create_fulltext_index_word(&conn, "Person", "bio").unwrap();

        let rows = execute_builtin(
            "fts.search",
            &[
                Value::String("Person".to_string()),
                Value::String("bio".to_string()),
                Value::String("rust".to_string()),
            ],
            &conn,
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "only id1 matches `rust`");
        match rows[0].get("node").unwrap() {
            Value::Node(n) => assert_eq!(n.id, id1),
            v => panic!("node column was {v:?}"),
        }
        match rows[0].get("score").unwrap() {
            Value::F64(s) => {
                assert!(*s > 0.0, "score should be positive (negated bm25); got {s}")
            }
            v => panic!("score column was {v:?}"),
        }
    }

    #[test]
    fn exec_fts_search_orders_by_relevance_descending() {
        let conn = fresh_conn();
        crate::storage::node::create_node(
            &conn,
            &["Doc".to_string()],
            props(&[(
                "body",
                Value::String("rust rust rust everywhere".to_string()),
            )]),
        )
        .unwrap();
        crate::storage::node::create_node(
            &conn,
            &["Doc".to_string()],
            props(&[(
                "body",
                Value::String("rust occasionally appears here".to_string()),
            )]),
        )
        .unwrap();
        // Create the index after the nodes — the backfill on create
        // populates the FTS table.
        crate::storage::fts::create_fulltext_index_word(&conn, "Doc", "body").unwrap();

        let rows = execute_builtin(
            "fts.search",
            &[
                Value::String("Doc".to_string()),
                Value::String("body".to_string()),
                Value::String("rust".to_string()),
            ],
            &conn,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        let s0 = match rows[0].get("score").unwrap() {
            Value::F64(s) => *s,
            v => panic!("score was {v:?}"),
        };
        let s1 = match rows[1].get("score").unwrap() {
            Value::F64(s) => *s,
            v => panic!("score was {v:?}"),
        };
        assert!(
            s0 >= s1,
            "expected scores in descending order: {s0} vs {s1}"
        );
    }

    #[test]
    fn exec_fts_search_errors_on_missing_index() {
        let conn = fresh_conn();
        let err = execute_builtin(
            "fts.search",
            &[
                Value::String("Person".to_string()),
                Value::String("bio".to_string()),
                Value::String("anything".to_string()),
            ],
            &conn,
        )
        .unwrap_err();
        assert!(
            matches!(err, GraphError::IndexNotFound { .. }),
            "expected IndexNotFound, got {err:?}"
        );
    }

    #[test]
    fn exec_fts_search_invalid_query_returns_query_error() {
        let conn = fresh_conn();
        crate::storage::fts::create_fulltext_index_word(&conn, "Doc", "body").unwrap();
        // FTS5 errors on unmatched quote.
        let err = execute_builtin(
            "fts.search",
            &[
                Value::String("Doc".to_string()),
                Value::String("body".to_string()),
                Value::String("\"unmatched".to_string()),
            ],
            &conn,
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("fulltext") || msg.contains("syntax") || msg.contains("fts5"),
            "error message should mention the fulltext issue; got: {msg}"
        );
    }

    #[test]
    fn exec_fts_search_non_string_arg_errors() {
        let conn = fresh_conn();
        crate::storage::fts::create_fulltext_index_word(&conn, "Doc", "body").unwrap();
        let err = execute_builtin(
            "fts.search",
            &[
                Value::String("Doc".to_string()),
                Value::String("body".to_string()),
                Value::I64(42),
            ],
            &conn,
        )
        .unwrap_err();
        assert!(
            matches!(err, GraphError::Query(QueryError::TypeError { .. })),
            "expected TypeError, got {err:?}"
        );
    }

    #[test]
    fn exec_db_indexes_distinguishes_word_kind() {
        let conn = fresh_conn();
        crate::storage::fts::create_fulltext_index(&conn, "Person", "name").unwrap();
        crate::storage::fts::create_fulltext_index_word(&conn, "Article", "body").unwrap();
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
                    "fulltext_word".to_string()
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
    fn exec_fts_search_wildcard_searches_all_columns() {
        let conn = fresh_conn();
        crate::storage::fts::create_fulltext_index_word_multi(
            &conn,
            "Article",
            &["title".to_string(), "body".to_string()],
        )
        .unwrap();
        let id = crate::storage::node::create_node(
            &conn,
            &["Article".to_string()],
            props(&[
                (
                    "title",
                    Value::String("rust systems programming".to_string()),
                ),
                ("body", Value::String("python is also nice".to_string())),
            ]),
        )
        .unwrap();
        crate::storage::fts::update_fts_for_node(
            &conn,
            id,
            &["Article".to_string()],
            None,
            &props(&[
                (
                    "title",
                    Value::String("rust systems programming".to_string()),
                ),
                ("body", Value::String("python is also nice".to_string())),
            ]),
        )
        .unwrap();
        // Wildcard searches all columns; `python` is only in body.
        let rows = execute_builtin(
            "fts.search",
            &[
                Value::String("Article".to_string()),
                Value::String("*".to_string()),
                Value::String("python".to_string()),
            ],
            &conn,
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn exec_fts_search_column_scoped_isolates_one_property() {
        let conn = fresh_conn();
        crate::storage::fts::create_fulltext_index_word_multi(
            &conn,
            "Article",
            &["title".to_string(), "body".to_string()],
        )
        .unwrap();
        let id = crate::storage::node::create_node(
            &conn,
            &["Article".to_string()],
            props(&[
                (
                    "title",
                    Value::String("rust systems programming".to_string()),
                ),
                ("body", Value::String("python is also nice".to_string())),
            ]),
        )
        .unwrap();
        crate::storage::fts::update_fts_for_node(
            &conn,
            id,
            &["Article".to_string()],
            None,
            &props(&[
                (
                    "title",
                    Value::String("rust systems programming".to_string()),
                ),
                ("body", Value::String("python is also nice".to_string())),
            ]),
        )
        .unwrap();
        // Search title for `python` — should be empty since `python` is in body.
        let rows = execute_builtin(
            "fts.search",
            &[
                Value::String("Article".to_string()),
                Value::String("title".to_string()),
                Value::String("python".to_string()),
            ],
            &conn,
        )
        .unwrap();
        assert!(rows.is_empty());
        // Search title for `rust` — should hit.
        let rows = execute_builtin(
            "fts.search",
            &[
                Value::String("Article".to_string()),
                Value::String("title".to_string()),
                Value::String("rust".to_string()),
            ],
            &conn,
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn exec_fts_search_wildcard_errors_with_multiple_indexes() {
        let conn = fresh_conn();
        // Two disjoint indexes on the same label.
        crate::storage::fts::create_fulltext_index_word(&conn, "Article", "title").unwrap();
        crate::storage::fts::create_fulltext_index_word_multi(
            &conn,
            "Article",
            &["body".to_string(), "summary".to_string()],
        )
        .unwrap();
        let err = execute_builtin(
            "fts.search",
            &[
                Value::String("Article".to_string()),
                Value::String("*".to_string()),
                Value::String("anything".to_string()),
            ],
            &conn,
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("multiple") || msg.contains("ambiguous"),
            "expected multi-index error, got: {msg}"
        );
    }

    #[test]
    fn exec_db_indexes_emits_one_row_per_covered_property() {
        let conn = fresh_conn();
        crate::storage::fts::create_fulltext_index_word_multi(
            &conn,
            "Article",
            &[
                "title".to_string(),
                "body".to_string(),
                "summary".to_string(),
            ],
        )
        .unwrap();
        let rows = execute_builtin("db.indexes", &[], &conn).unwrap();
        assert_eq!(rows.len(), 3);
        let mut props: Vec<String> = rows
            .iter()
            .map(|r| match r.get("property").unwrap() {
                Value::String(s) => s.clone(),
                v => panic!("property was {v:?}"),
            })
            .collect();
        props.sort();
        assert_eq!(
            props,
            vec![
                "body".to_string(),
                "summary".to_string(),
                "title".to_string()
            ]
        );
        for r in &rows {
            assert_eq!(
                r.get("kind"),
                Some(&Value::String("fulltext_word".to_string()))
            );
        }
    }

    #[test]
    fn exec_db_indexes_emits_one_row_per_composite_btree_property() {
        let conn = fresh_conn();
        crate::storage::index::create_composite_index(&conn, "Person", &["tenant_id", "ext_id"])
            .unwrap();
        let rows = execute_builtin("db.indexes", &[], &conn).unwrap();
        assert_eq!(rows.len(), 2);
        let props: Vec<String> = rows
            .iter()
            .map(|r| match r.get("property").unwrap() {
                Value::String(s) => s.clone(),
                v => panic!("property was {v:?}"),
            })
            .collect();
        assert_eq!(
            props,
            vec!["tenant_id".to_string(), "ext_id".to_string()],
            "rows must preserve declared column order"
        );
        for r in &rows {
            assert_eq!(r.get("label"), Some(&Value::String("Person".to_string())));
            assert_eq!(r.get("kind"), Some(&Value::String("btree".to_string())));
        }
    }
}
