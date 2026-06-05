//! Full-text index over `(label, property)` pairs. Backed by SQLite FTS5
//! virtual tables using the `trigram` tokenizer with `case_sensitive 1`.
//! Accelerates `CONTAINS` / `STARTS WITH` / `ENDS WITH` predicates via the
//! `LogicalOp::FullTextLookup` planner rewrite.

use crate::types::{validate_name, GraphError, NodeId, Properties, Result, Value};
use rusqlite::Connection;

/// Build the FTS table name for a (label, property) pair.
///
/// Validates both components so the returned name is safe to interpolate
/// into raw SQL identifiers. The `fts_` infix prevents collision with the
/// regular `node_idx_*` naming used by `storage::index`.
pub fn fts_table_name(label: &str, property: &str) -> Result<String> {
    validate_name(label)?;
    validate_name(property)?;
    Ok(format!("node_fts_{label}_{property}"))
}

fn fts_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(exists)
}

/// Return `true` if the FTS index on `(label, property)` was created
/// with the case-insensitive trigram tokenizer.
///
/// Reads back the `CREATE VIRTUAL TABLE` DDL from `sqlite_master.sql`
/// and looks for the `case_sensitive 0` token we wrote at create time.
/// Errors with `IndexNotFound` when no FTS index exists for the pair.
pub fn is_case_insensitive(conn: &Connection, label: &str, property: &str) -> Result<bool> {
    let table = fts_table_name(label, property)?;
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1 AND sql LIKE 'CREATE VIRTUAL TABLE%'",
            [&table],
            |r| r.get(0),
        )
        .ok();
    let sql = sql.ok_or_else(|| GraphError::IndexNotFound {
        label: label.to_string(),
        property: property.to_string(),
        hint: Some("no fulltext index on this (label, property)".to_string()),
    })?;
    Ok(sql.contains("case_sensitive 0"))
}

/// Create a fulltext index on `(label, property)`. The underlying
/// FTS5 virtual table uses the `trigram` tokenizer with
/// `case_sensitive 1` to preserve openCypher's case-sensitive
/// substring semantics for `CONTAINS` / `STARTS WITH` / `ENDS WITH`.
///
/// Backfills from all existing nodes with the label — only string-valued
/// properties are indexed (non-strings are silently skipped, matching
/// the steady-state write path).
pub fn create_fulltext_index(conn: &Connection, label: &str, property: &str) -> Result<()> {
    create_fulltext_index_impl(conn, label, property, true)
}

/// Create a case-insensitive fulltext index on `(label, property)`.
///
/// The underlying FTS5 virtual table uses the `trigram` tokenizer with
/// `case_sensitive 0`, so MATCH queries case-fold both the indexed
/// content and the query phrase. Plain `CONTAINS` / `STARTS WITH` /
/// `ENDS WITH` against this property is then case-insensitive.
pub fn create_fulltext_index_ci(conn: &Connection, label: &str, property: &str) -> Result<()> {
    create_fulltext_index_impl(conn, label, property, false)
}

fn create_fulltext_index_impl(
    conn: &Connection,
    label: &str,
    property: &str,
    case_sensitive: bool,
) -> Result<()> {
    let table = fts_table_name(label, property)?;
    if fts_table_exists(conn, &table)? {
        return Err(GraphError::IndexAlreadyExists {
            label: label.to_string(),
            property: property.to_string(),
            hint: Some("a fulltext index on this (label, property) already exists".to_string()),
        });
    }
    let cs_flag = if case_sensitive { 1 } else { 0 };
    conn.execute(
        &format!(
            "CREATE VIRTUAL TABLE \"{table}\" USING fts5(content, tokenize='trigram case_sensitive {cs_flag}')"
        ),
        [],
    )?;

    // Backfill from existing nodes with this label. Only string-valued
    // properties are indexed.
    let nodes = crate::node::find_nodes_by_label(conn, label)?;
    for n in &nodes {
        if let Some(Value::String(s)) = n.properties.get(property) {
            conn.execute(
                &format!("INSERT INTO \"{table}\" (rowid, content) VALUES (?1, ?2)"),
                rusqlite::params![n.id.0 as i64, s],
            )?;
        }
    }
    Ok(())
}

/// Drop a fulltext index on `(label, property)`.
pub fn drop_fulltext_index(conn: &Connection, label: &str, property: &str) -> Result<()> {
    let table = fts_table_name(label, property)?;
    if !fts_table_exists(conn, &table)? {
        return Err(GraphError::IndexNotFound {
            label: label.to_string(),
            property: property.to_string(),
            hint: Some("no fulltext index on this (label, property)".to_string()),
        });
    }
    conn.execute(&format!("DROP TABLE \"{table}\""), [])?;
    Ok(())
}

/// List all fulltext indexes that exist for a given label.
/// Returns `Vec<(label, property)>`.
///
/// FTS5 creates internal shadow tables (`<name>_data`, `<name>_idx`,
/// `<name>_content`, `<name>_docsize`, `<name>_config`) alongside each
/// virtual table. We distinguish user-created FTS indexes from shadow
/// tables by looking at `sqlite_master.sql`: virtual tables are created
/// with `CREATE VIRTUAL TABLE`, shadow tables with plain `CREATE TABLE`.
pub fn list_fulltext_indexes_for_label(
    conn: &Connection,
    label: &str,
) -> Result<Vec<(String, String)>> {
    let prefix = format!("node_fts_{label}_");
    let mut stmt = conn.prepare_cached(
        "SELECT name FROM sqlite_master \
         WHERE type='table' \
         AND name LIKE ?1 \
         AND sql LIKE 'CREATE VIRTUAL TABLE%'",
    )?;
    let rows = stmt.query_map([format!("{prefix}%")], |row| row.get::<_, String>(0))?;

    let mut result = Vec::new();
    for name in rows {
        let name = name?;
        if let Some(property) = name.strip_prefix(&prefix) {
            result.push((label.to_string(), property.to_string()));
        }
    }
    Ok(result)
}

/// List every fulltext index in the database as `(label, property)`
/// pairs. Filters out FTS5 shadow tables (`_data`, `_idx`, `_content`,
/// `_docsize`, `_config`) via the same `CREATE VIRTUAL TABLE` check
/// that `list_fulltext_indexes_for_label` uses.
///
/// Note: same label/property underscore ambiguity as
/// `index::list_all_indexes` — see that function's doc comment. Split
/// is on the FIRST underscore after the `node_fts_` prefix so that
/// properties with underscores (e.g. `cache_data`) round-trip when
/// labels do not contain underscores.
pub fn list_all_fulltext_indexes(conn: &Connection) -> Result<Vec<(String, String, bool)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, sql FROM sqlite_master \
         WHERE type='table' \
         AND name LIKE 'node_fts_%' \
         AND sql LIKE 'CREATE VIRTUAL TABLE%'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut result = Vec::new();
    for r in rows {
        let (name, sql) = r?;
        let Some(rest) = name.strip_prefix("node_fts_") else {
            continue;
        };
        let Some(split) = rest.find('_') else {
            continue;
        };
        let (label, property) = rest.split_at(split);
        let case_insensitive = sql.contains("case_sensitive 0");
        result.push((
            label.to_string(),
            property[1..].to_string(),
            case_insensitive,
        ));
    }
    Ok(result)
}

/// Update fulltext indexes for a node after create / SET / REMOVE.
///
/// Pass `old_properties = None` for newly-created nodes. For each
/// fulltext index on `label`:
/// - if the old value was a string and the new value differs, delete
///   the old row;
/// - if the new value is a string and differs from the old, insert the
///   new row.
///
/// Non-string values are skipped silently (the FTS index only covers
/// string properties).
pub fn update_fts_for_node(
    conn: &Connection,
    node_id: NodeId,
    label: &str,
    old_properties: Option<&Properties>,
    new_properties: &Properties,
) -> Result<()> {
    let tables = list_fulltext_indexes_for_label(conn, label)?;
    for (_, property) in &tables {
        let table = fts_table_name(label, property)?;
        let old_val = old_properties
            .and_then(|p| p.get(property.as_str()))
            .and_then(|v| match v {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            });
        let new_val = new_properties.get(property.as_str()).and_then(|v| match v {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        });

        if old_val == new_val {
            continue;
        }
        if old_val.is_some() {
            conn.execute(
                &format!("DELETE FROM \"{table}\" WHERE rowid=?1"),
                rusqlite::params![node_id.0 as i64],
            )?;
        }
        if let Some(s) = new_val {
            conn.execute(
                &format!("INSERT INTO \"{table}\" (rowid, content) VALUES (?1, ?2)"),
                rusqlite::params![node_id.0 as i64, s],
            )?;
        }
    }
    Ok(())
}

/// Remove all fulltext index entries for a node being deleted.
pub fn remove_fts_for_node(
    conn: &Connection,
    node_id: NodeId,
    label: &str,
    properties: &Properties,
) -> Result<()> {
    let tables = list_fulltext_indexes_for_label(conn, label)?;
    for (_, property) in &tables {
        if let Some(Value::String(_)) = properties.get(property.as_str()) {
            let table = fts_table_name(label, property)?;
            conn.execute(
                &format!("DELETE FROM \"{table}\" WHERE rowid=?1"),
                rusqlite::params![node_id.0 as i64],
            )?;
        }
    }
    Ok(())
}

/// Look up node ids whose property contains the term as a substring.
///
/// Returns `Ok(Some(ids))` when the FTS index handled the lookup, and
/// `Ok(None)` when the term is too short for the trigram tokenizer
/// (`<3` Unicode codepoints) — the caller must fall back to a scan.
///
/// Errors when no fulltext index exists for `(label, property)`.
pub fn fulltext_lookup(
    conn: &Connection,
    label: &str,
    property: &str,
    term: &str,
) -> Result<Option<Vec<NodeId>>> {
    if term.chars().count() < 3 {
        return Ok(None);
    }
    let table = fts_table_name(label, property)?;
    if !fts_table_exists(conn, &table)? {
        return Err(GraphError::IndexNotFound {
            label: label.to_string(),
            property: property.to_string(),
            hint: Some("no fulltext index on this (label, property)".to_string()),
        });
    }

    // FTS5 phrase: wrap in double-quotes; embedded double-quotes are
    // escaped by doubling them.
    let escaped = term.replace('"', "\"\"");
    let phrase = format!("\"{escaped}\"");

    let mut stmt = conn.prepare_cached(&format!(
        "SELECT rowid FROM \"{table}\" WHERE content MATCH ?1"
    ))?;
    let rows = stmt.query_map([&phrase], |r| r.get::<_, i64>(0))?;

    let mut ids = Vec::new();
    for row in rows {
        let id = row?;
        ids.push(NodeId(id as u64));
    }
    Ok(Some(ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::collections::HashMap;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&c).unwrap();
        c
    }

    fn props(pairs: &[(&str, Value)]) -> Properties {
        let mut p = HashMap::new();
        for (k, v) in pairs {
            p.insert(k.to_string(), v.clone());
        }
        p
    }

    fn fts_rowids(conn: &Connection, label: &str, property: &str) -> Vec<i64> {
        let table = fts_table_name(label, property).unwrap();
        let mut stmt = conn
            .prepare(&format!("SELECT rowid FROM \"{table}\" ORDER BY rowid"))
            .unwrap();
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn fts_table_name_uses_fts_infix() {
        assert_eq!(
            fts_table_name("Article", "body").unwrap(),
            "node_fts_Article_body"
        );
    }

    #[test]
    fn fts_table_name_rejects_invalid_label() {
        assert!(fts_table_name("Bad\"Name", "body").is_err());
    }

    #[test]
    fn fts_table_name_rejects_invalid_property() {
        assert!(fts_table_name("Article", "bad\"prop").is_err());
    }

    #[test]
    fn create_and_drop_fulltext_index_roundtrip() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        let exists: bool = c
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                ["node_fts_Person_bio"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exists, "create must produce an FTS table");

        drop_fulltext_index(&c, "Person", "bio").unwrap();
        let exists_after: bool = c
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                ["node_fts_Person_bio"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists_after, "drop must remove the FTS table");
    }

    #[test]
    fn create_fulltext_index_errors_when_exists() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        match create_fulltext_index(&c, "Person", "bio") {
            Err(GraphError::IndexAlreadyExists {
                label, property, ..
            }) => {
                assert_eq!(label, "Person");
                assert_eq!(property, "bio");
            }
            other => panic!("expected IndexAlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn drop_fulltext_index_errors_when_absent() {
        let c = conn();
        match drop_fulltext_index(&c, "Person", "bio") {
            Err(GraphError::IndexNotFound {
                label, property, ..
            }) => {
                assert_eq!(label, "Person");
                assert_eq!(property, "bio");
            }
            other => panic!("expected IndexNotFound, got {other:?}"),
        }
    }

    #[test]
    fn list_fulltext_indexes_for_label_returns_only_matching_label() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        create_fulltext_index(&c, "Person", "name").unwrap();
        create_fulltext_index(&c, "Article", "body").unwrap();

        let mut got = list_fulltext_indexes_for_label(&c, "Person").unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("Person".to_string(), "bio".to_string()),
                ("Person".to_string(), "name".to_string()),
            ]
        );
    }

    #[test]
    fn list_fulltext_indexes_for_label_returns_empty_when_none() {
        let c = conn();
        assert!(list_fulltext_indexes_for_label(&c, "Nothing")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn list_fulltext_indexes_for_label_handles_shadow_suffix_property_names() {
        let c = conn();
        // A property literally named "cache_data" must be returned even
        // though it shares a suffix with FTS5's "_data" shadow tables.
        create_fulltext_index(&c, "Foo", "cache_data").unwrap();
        let got = list_fulltext_indexes_for_label(&c, "Foo").unwrap();
        assert_eq!(got, vec![("Foo".to_string(), "cache_data".to_string())]);
    }

    #[test]
    fn update_fts_inserts_on_new_node_with_string_value() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        let p = props(&[("bio", Value::String("hello world".into()))]);
        update_fts_for_node(&c, NodeId(7), "Person", None, &p).unwrap();
        assert_eq!(fts_rowids(&c, "Person", "bio"), vec![7]);
    }

    #[test]
    fn update_fts_skips_non_string_values() {
        let c = conn();
        create_fulltext_index(&c, "Person", "age").unwrap();
        let p = props(&[("age", Value::I64(42))]);
        update_fts_for_node(&c, NodeId(7), "Person", None, &p).unwrap();
        assert_eq!(fts_rowids(&c, "Person", "age"), Vec::<i64>::new());
    }

    #[test]
    fn update_fts_replaces_on_value_change() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        let old = props(&[("bio", Value::String("hello".into()))]);
        let new = props(&[("bio", Value::String("goodbye".into()))]);
        update_fts_for_node(&c, NodeId(7), "Person", None, &old).unwrap();
        update_fts_for_node(&c, NodeId(7), "Person", Some(&old), &new).unwrap();
        assert_eq!(fts_rowids(&c, "Person", "bio"), vec![7]);
        let content: String = c
            .query_row(
                "SELECT content FROM node_fts_Person_bio WHERE rowid=7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content, "goodbye");
    }

    #[test]
    fn update_fts_removes_when_property_drops() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        let old = props(&[("bio", Value::String("hello".into()))]);
        let new = props(&[]);
        update_fts_for_node(&c, NodeId(7), "Person", None, &old).unwrap();
        update_fts_for_node(&c, NodeId(7), "Person", Some(&old), &new).unwrap();
        assert_eq!(fts_rowids(&c, "Person", "bio"), Vec::<i64>::new());
    }

    #[test]
    fn remove_fts_for_node_deletes_all_indexed_props() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        create_fulltext_index(&c, "Person", "name").unwrap();
        let p = props(&[
            ("bio", Value::String("hello".into())),
            ("name", Value::String("Alice".into())),
        ]);
        update_fts_for_node(&c, NodeId(7), "Person", None, &p).unwrap();
        remove_fts_for_node(&c, NodeId(7), "Person", &p).unwrap();
        assert_eq!(fts_rowids(&c, "Person", "bio"), Vec::<i64>::new());
        assert_eq!(fts_rowids(&c, "Person", "name"), Vec::<i64>::new());
    }

    fn setup_with_nodes(c: &Connection) {
        create_fulltext_index(c, "Doc", "text").unwrap();
        for (id, s) in [
            (1i64, "the quick brown fox"),
            (2, "lazy dogs sleep"),
            (3, "quickly brown"),
            (4, "FOOBAR matches case"),
        ] {
            let p = props(&[("text", Value::String(s.into()))]);
            update_fts_for_node(c, NodeId(id as u64), "Doc", None, &p).unwrap();
        }
    }

    #[test]
    fn fulltext_lookup_returns_matches_for_normal_term() {
        let c = conn();
        setup_with_nodes(&c);
        let mut ids = fulltext_lookup(&c, "Doc", "text", "brown")
            .unwrap()
            .unwrap();
        ids.sort_by_key(|n| n.0);
        assert_eq!(ids, vec![NodeId(1), NodeId(3)]);
    }

    #[test]
    fn fulltext_lookup_is_case_sensitive() {
        let c = conn();
        setup_with_nodes(&c);
        let ids = fulltext_lookup(&c, "Doc", "text", "foobar")
            .unwrap()
            .unwrap();
        assert!(
            ids.is_empty(),
            "case-sensitive: lowercase must not match uppercase"
        );

        let ids = fulltext_lookup(&c, "Doc", "text", "FOOBAR")
            .unwrap()
            .unwrap();
        assert_eq!(ids, vec![NodeId(4)]);
    }

    #[test]
    fn fulltext_lookup_short_term_returns_none_sentinel() {
        let c = conn();
        setup_with_nodes(&c);
        assert!(fulltext_lookup(&c, "Doc", "text", "fo").unwrap().is_none());
        assert!(fulltext_lookup(&c, "Doc", "text", "").unwrap().is_none());
    }

    #[test]
    fn fulltext_lookup_escapes_embedded_double_quotes() {
        let c = conn();
        create_fulltext_index(&c, "Doc", "text").unwrap();
        let p = props(&[("text", Value::String("say \"hello\" loudly".into()))]);
        update_fts_for_node(&c, NodeId(1), "Doc", None, &p).unwrap();
        let ids = fulltext_lookup(&c, "Doc", "text", "\"hello\"")
            .unwrap()
            .unwrap();
        assert_eq!(ids, vec![NodeId(1)]);
    }

    #[test]
    fn fulltext_lookup_missing_index_errors() {
        let c = conn();
        match fulltext_lookup(&c, "Nope", "x", "foo") {
            Err(GraphError::IndexNotFound { .. }) => {}
            other => panic!("expected IndexNotFound, got {other:?}"),
        }
    }

    #[test]
    fn create_fulltext_index_backfills_existing_nodes() {
        let c = conn();

        // Create nodes BEFORE the index exists.
        crate::node::create_node(
            &c,
            &["Doc".to_string()],
            props(&[("text", Value::String("hello world".into()))]),
        )
        .unwrap();
        crate::node::create_node(
            &c,
            &["Doc".to_string()],
            props(&[("text", Value::String("goodbye now".into()))]),
        )
        .unwrap();

        // Now create the fulltext index.
        create_fulltext_index(&c, "Doc", "text").unwrap();

        // The new index must contain both pre-existing nodes.
        let ids = fulltext_lookup(&c, "Doc", "text", "hello")
            .unwrap()
            .unwrap();
        assert_eq!(ids.len(), 1);
        let ids = fulltext_lookup(&c, "Doc", "text", "goodbye")
            .unwrap()
            .unwrap();
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn list_all_fulltext_indexes_returns_empty_on_fresh_db() {
        let c = conn();
        let got = list_all_fulltext_indexes(&c).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn list_all_fulltext_indexes_returns_one_per_index_across_labels() {
        let c = conn();
        create_fulltext_index(&c, "Doc", "body").unwrap();
        create_fulltext_index(&c, "Doc", "title").unwrap();
        create_fulltext_index(&c, "Note", "text").unwrap();
        let mut got = list_all_fulltext_indexes(&c).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("Doc".to_string(), "body".to_string(), false),
                ("Doc".to_string(), "title".to_string(), false),
                ("Note".to_string(), "text".to_string(), false),
            ]
        );
    }

    #[test]
    fn list_all_fulltext_indexes_returns_case_insensitive_flag() {
        let c = conn();
        create_fulltext_index(&c, "Person", "name").unwrap();
        create_fulltext_index_ci(&c, "Person", "bio").unwrap();
        create_fulltext_index_ci(&c, "Article", "body").unwrap();
        let mut got = list_all_fulltext_indexes(&c).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("Article".to_string(), "body".to_string(), true),
                ("Person".to_string(), "bio".to_string(), true),
                ("Person".to_string(), "name".to_string(), false),
            ]
        );
    }

    #[test]
    fn create_fulltext_index_ci_creates_case_insensitive_table() {
        let c = conn();
        create_fulltext_index_ci(&c, "Person", "bio").unwrap();
        let sql: String = c
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                ["node_fts_Person_bio"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("case_sensitive 0"), "sql was: {sql}");
    }

    #[test]
    fn create_fulltext_index_ci_round_trip_finds_mismatched_case() {
        let c = conn();
        // Create node first so the CI index backfills it on create —
        // `storage::node::create_node` does not call `update_fts_for_node`
        // (that's done at the transaction/executor layer).
        let id = crate::node::create_node(
            &c,
            &["Person".to_string()],
            props(&[("bio", Value::String("Alice Smith".to_string()))]),
        )
        .unwrap();
        create_fulltext_index_ci(&c, "Person", "bio").unwrap();
        let hits = fulltext_lookup(&c, "Person", "bio", "alice")
            .unwrap()
            .expect("ci index should serve the query");
        assert_eq!(hits, vec![id]);
    }

    #[test]
    fn create_fulltext_index_ci_errors_when_cs_exists_on_same_pair() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        match create_fulltext_index_ci(&c, "Person", "bio") {
            Err(GraphError::IndexAlreadyExists {
                label, property, ..
            }) => {
                assert_eq!(label, "Person");
                assert_eq!(property, "bio");
            }
            other => panic!("expected IndexAlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn is_case_insensitive_returns_false_for_cs_index() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        assert!(!is_case_insensitive(&c, "Person", "bio").unwrap());
    }

    #[test]
    fn is_case_insensitive_returns_true_for_ci_index() {
        let c = conn();
        create_fulltext_index_ci(&c, "Person", "bio").unwrap();
        assert!(is_case_insensitive(&c, "Person", "bio").unwrap());
    }

    #[test]
    fn is_case_insensitive_errors_when_index_missing() {
        let c = conn();
        match is_case_insensitive(&c, "Person", "bio") {
            Err(GraphError::IndexNotFound {
                label, property, ..
            }) => {
                assert_eq!(label, "Person");
                assert_eq!(property, "bio");
            }
            other => panic!("expected IndexNotFound, got {other:?}"),
        }
    }

    #[test]
    fn list_all_fulltext_indexes_skips_shadow_tables() {
        // Regression: a user property literally named "cache_data" or other
        // FTS5 shadow-suffix names must not confuse the listing.
        let c = conn();
        create_fulltext_index(&c, "Foo", "cache_data").unwrap();
        let got = list_all_fulltext_indexes(&c).unwrap();
        // Exactly one entry — the virtual table itself — not the shadow tables.
        assert_eq!(
            got,
            vec![("Foo".to_string(), "cache_data".to_string(), false)]
        );
    }
}
