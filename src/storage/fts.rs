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

/// Which tokenizer the FTS5 virtual table was created with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FtsTokenizerKind {
    /// `trigram case_sensitive 1` — default, preserves Cypher substring
    /// semantics for `CONTAINS` / `STARTS WITH` / `ENDS WITH`.
    TrigramCaseSensitive,
    /// `trigram case_sensitive 0` — case-folded trigram matching.
    TrigramCaseInsensitive,
    /// `unicode61` — word-tokenized matching for `fts.search`.
    Word,
}

/// Resolve which FTS5 tokenizer covers `(label, property)`.
///
/// Scans every fulltext index on `label` (single- and multi-prop) and
/// returns the tokenizer kind of the one whose column list contains
/// `property`. Errors with `IndexNotFound` when no FTS index covers
/// the pair. Multi-prop tables share their tokenizer across every
/// covered property by construction.
pub fn fts_tokenizer_kind(
    conn: &Connection,
    label: &str,
    property: &str,
) -> Result<FtsTokenizerKind> {
    let infos = list_all_fulltext_indexes(conn)?;
    for info in &infos {
        if info.label == label && info.properties.iter().any(|p| p == property) {
            return Ok(info.kind);
        }
    }
    Err(GraphError::IndexNotFound {
        label: label.to_string(),
        property: property.to_string(),
        hint: Some("no fulltext index covers this (label, property)".to_string()),
    })
}

/// Parse a `CREATE VIRTUAL TABLE ... USING fts5(...)` DDL string and
/// recover which tokenizer it specified.
///
/// Order-of-checks invariant: a `unicode61` DDL never contains the
/// `case_sensitive` flag (that flag belongs to the `trigram` tokenizer),
/// so checking `unicode61` first is safe.
fn parse_tokenizer_kind(sql: &str) -> FtsTokenizerKind {
    if sql.contains("unicode61") {
        FtsTokenizerKind::Word
    } else if sql.contains("case_sensitive 0") {
        FtsTokenizerKind::TrigramCaseInsensitive
    } else {
        FtsTokenizerKind::TrigramCaseSensitive
    }
}

/// Structured description of a fulltext index.
///
/// `properties.len() == 1` for single-property indexes (the historical
/// shape); `properties.len() > 1` for multi-property indexes whose
/// underlying FTS5 virtual table has multiple content columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsIndexInfo {
    /// Label this index is attached to.
    pub label: String,
    /// Property names covered by this index. Length 1 for single-prop
    /// (legacy) indexes; length > 1 for multi-prop indexes.
    pub properties: Vec<String>,
    /// Which FTS5 tokenizer was used at create time.
    pub kind: FtsTokenizerKind,
    /// Underlying SQLite virtual table name.
    pub table_name: String,
}

/// Parse the comma-separated column list out of an FTS5
/// `CREATE VIRTUAL TABLE ... USING fts5(...)` DDL string.
///
/// Handles bare identifiers (`content`) and quoted ones (`"title"`).
/// Stops at the `tokenize=` clause if present, otherwise at the
/// closing `)`. Returns an empty vec if the DDL shape is unexpected.
fn parse_column_list(sql: &str) -> Vec<String> {
    let Some(open) = sql.find('(') else {
        return Vec::new();
    };
    let rest = &sql[open + 1..];
    // Stop at "tokenize=" or the matching close paren.
    let end = rest
        .find("tokenize=")
        .unwrap_or_else(|| rest.rfind(')').unwrap_or(rest.len()));
    let columns_str = rest[..end].trim().trim_end_matches(',').trim();
    columns_str
        .split(',')
        .map(|c| {
            let c = c.trim();
            c.trim_matches('"').trim_matches('\'').to_string()
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// Internal spec for the shared creator. Determines the FTS5 tokenize clause.
enum FtsTokenizerSpec {
    TrigramCaseSensitive,
    TrigramCaseInsensitive,
    Word,
}

impl FtsTokenizerSpec {
    fn tokenize_clause(&self) -> &'static str {
        match self {
            Self::TrigramCaseSensitive => "trigram case_sensitive 1",
            Self::TrigramCaseInsensitive => "trigram case_sensitive 0",
            Self::Word => "unicode61",
        }
    }
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
    create_fulltext_index_impl(
        conn,
        label,
        property,
        FtsTokenizerSpec::TrigramCaseSensitive,
    )
}

/// Create a case-insensitive fulltext index on `(label, property)`.
///
/// The underlying FTS5 virtual table uses the `trigram` tokenizer with
/// `case_sensitive 0`, so MATCH queries case-fold both the indexed
/// content and the query phrase. Plain `CONTAINS` / `STARTS WITH` /
/// `ENDS WITH` against this property is then case-insensitive.
pub fn create_fulltext_index_ci(conn: &Connection, label: &str, property: &str) -> Result<()> {
    create_fulltext_index_impl(
        conn,
        label,
        property,
        FtsTokenizerSpec::TrigramCaseInsensitive,
    )
}

/// Create a word-tokenized fulltext index on `(label, property)`,
/// suitable for the `fts.search` procedure.
///
/// FTS5 `unicode61` tokenizer — lowercases and folds diacritics, splits
/// on word boundaries. Matches whole tokens (not arbitrary substrings),
/// supports phrase / boolean / prefix queries via FTS5 MATCH syntax.
/// `CONTAINS` / `STARTS WITH` / `ENDS WITH` are not accelerated against
/// this index and fall back to label scan with per-row eval.
pub fn create_fulltext_index_word(conn: &Connection, label: &str, property: &str) -> Result<()> {
    create_fulltext_index_impl(conn, label, property, FtsTokenizerSpec::Word)
}

/// Build a unique multi-prop FTS table name for `label`.
///
/// Scans `sqlite_master` for existing `node_fts_multi_{label}_*`
/// tables and picks the smallest unused positive integer suffix.
/// Deterministic, doesn't depend on the property list (so the same
/// label can host several disjoint multi-prop indexes).
fn fts_multi_table_name(conn: &Connection, label: &str) -> Result<String> {
    validate_name(label)?;
    let prefix = format!("node_fts_multi_{label}_");
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE ?1")?;
    let rows = stmt.query_map([format!("{prefix}%")], |row| row.get::<_, String>(0))?;

    let mut used: Vec<u64> = Vec::new();
    for r in rows {
        let name = r?;
        if let Some(rest) = name.strip_prefix(&prefix) {
            if let Ok(n) = rest.parse::<u64>() {
                used.push(n);
            }
        }
    }
    used.sort();
    let mut n: u64 = 1;
    for u in used {
        if u == n {
            n += 1;
        } else if u > n {
            break;
        }
    }
    Ok(format!("{prefix}{n}"))
}

/// Create a multi-property word-tokenized fulltext index on `label`.
///
/// One FTS5 virtual table (`node_fts_multi_{label}_{N}`) covers all
/// listed properties as separate columns. `fts.search` can search
/// across all of them via `'*'` or scope to one column by name.
///
/// Strict mutex: errors with `IndexAlreadyExists` if any existing FTS
/// index (single or multi) already covers any of the listed properties
/// on this label. `properties` must be non-empty and contain no
/// duplicates.
pub fn create_fulltext_index_word_multi(
    conn: &Connection,
    label: &str,
    properties: &[String],
) -> Result<()> {
    if properties.is_empty() {
        return Err(GraphError::IndexAlreadyExists {
            label: label.to_string(),
            property: String::new(),
            hint: Some(
                "create_fulltext_index_word_multi requires at least one property".to_string(),
            ),
        });
    }
    // Reject duplicate property names in the request.
    let mut seen = std::collections::HashSet::new();
    for p in properties {
        if !seen.insert(p.as_str()) {
            return Err(GraphError::IndexAlreadyExists {
                label: label.to_string(),
                property: p.clone(),
                hint: Some("duplicate property in multi-prop index list".to_string()),
            });
        }
    }
    // Mutex: no existing FTS index can cover any of these properties.
    for p in properties {
        if fts_tokenizer_kind(conn, label, p).is_ok() {
            return Err(GraphError::IndexAlreadyExists {
                label: label.to_string(),
                property: p.clone(),
                hint: Some("another fulltext index already covers (label, property)".to_string()),
            });
        }
    }
    // Validate every property name as a safe identifier (we interpolate
    // them into SQL column lists below).
    for p in properties {
        validate_name(p)?;
    }
    create_fulltext_index_multi_impl(conn, label, properties, FtsTokenizerSpec::Word)
}

fn create_fulltext_index_multi_impl(
    conn: &Connection,
    label: &str,
    properties: &[String],
    spec: FtsTokenizerSpec,
) -> Result<()> {
    let table = fts_multi_table_name(conn, label)?;
    let cols_quoted: String = properties
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let tokenize = spec.tokenize_clause();
    conn.execute(
        &format!(
            "CREATE VIRTUAL TABLE \"{table}\" USING fts5({cols_quoted}, tokenize='{tokenize}')"
        ),
        [],
    )?;

    // Backfill from existing nodes with this label.
    let nodes = crate::node::find_nodes_by_label(conn, label)?;
    if nodes.is_empty() {
        return Ok(());
    }
    let placeholders: String = (2..=properties.len() + 1)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let insert_sql =
        format!("INSERT INTO \"{table}\" (rowid, {cols_quoted}) VALUES (?1, {placeholders})");
    for n in &nodes {
        // Skip nodes that have no string value on any covered property —
        // matches the steady-state write path in `update_fts_for_node`.
        let any_string = properties
            .iter()
            .any(|p| matches!(n.properties.get(p), Some(Value::String(_))));
        if !any_string {
            continue;
        }
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(properties.len() + 1);
        params.push(Box::new(n.id.0 as i64));
        for prop in properties {
            match n.properties.get(prop) {
                Some(Value::String(s)) => params.push(Box::new(s.clone())),
                _ => params.push(Box::new(rusqlite::types::Null)),
            }
        }
        conn.execute(&insert_sql, rusqlite::params_from_iter(params.iter()))?;
    }
    Ok(())
}

fn create_fulltext_index_impl(
    conn: &Connection,
    label: &str,
    property: &str,
    spec: FtsTokenizerSpec,
) -> Result<()> {
    let table = fts_table_name(label, property)?;
    if fts_table_exists(conn, &table)? {
        return Err(GraphError::IndexAlreadyExists {
            label: label.to_string(),
            property: property.to_string(),
            hint: Some("a fulltext index on this (label, property) already exists".to_string()),
        });
    }
    // Mutex against multi-prop indexes covering the same (label, property).
    if fts_tokenizer_kind(conn, label, property).is_ok() {
        return Err(GraphError::IndexAlreadyExists {
            label: label.to_string(),
            property: property.to_string(),
            hint: Some(
                "a multi-property fulltext index already covers this (label, property)".to_string(),
            ),
        });
    }
    conn.execute(
        &format!(
            "CREATE VIRTUAL TABLE \"{table}\" USING fts5(content, tokenize='{}')",
            spec.tokenize_clause()
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
) -> Result<Vec<FtsIndexInfo>> {
    let all = list_all_fulltext_indexes(conn)?;
    Ok(all.into_iter().filter(|info| info.label == label).collect())
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
pub fn list_all_fulltext_indexes(conn: &Connection) -> Result<Vec<FtsIndexInfo>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, sql FROM sqlite_master \
         WHERE type='table' \
         AND (name LIKE 'node_fts_%') \
         AND sql LIKE 'CREATE VIRTUAL TABLE%'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut result = Vec::new();
    for r in rows {
        let (name, sql) = r?;
        let kind = parse_tokenizer_kind(&sql);
        let properties = parse_column_list(&sql);

        // Determine label from table name. Two naming schemes:
        //   node_fts_{label}_{property}        (single-prop, legacy)
        //   node_fts_multi_{label}_{N}         (multi-prop)
        if let Some(rest) = name.strip_prefix("node_fts_multi_") {
            // Suffix is {label}_{N}; split on the LAST underscore.
            let Some(split) = rest.rfind('_') else {
                continue;
            };
            let label = &rest[..split];
            result.push(FtsIndexInfo {
                label: label.to_string(),
                properties,
                kind,
                table_name: name,
            });
        } else if let Some(rest) = name.strip_prefix("node_fts_") {
            // Single-prop: split on the FIRST underscore to separate
            // {label}_{property}. The `properties` parsed from the DDL
            // is `["content"]`, which is the FTS5 column name — for
            // legacy single-prop tables we want the PROPERTY name from
            // the table name, not the column name.
            let Some(split) = rest.find('_') else {
                continue;
            };
            let (label, property) = rest.split_at(split);
            result.push(FtsIndexInfo {
                label: label.to_string(),
                properties: vec![property[1..].to_string()],
                kind,
                table_name: name,
            });
        }
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
    let infos = list_fulltext_indexes_for_label(conn, label)?;
    for info in &infos {
        // Single-prop only at this task. Multi-prop write path lands in Task 3.
        debug_assert_eq!(
            info.properties.len(),
            1,
            "multi-prop write path not yet implemented; expected after Task 3"
        );
        let property = &info.properties[0];
        let table = &info.table_name;

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
    let infos = list_fulltext_indexes_for_label(conn, label)?;
    for info in &infos {
        debug_assert_eq!(info.properties.len(), 1);
        let property = &info.properties[0];
        if let Some(Value::String(_)) = properties.get(property.as_str()) {
            conn.execute(
                &format!("DELETE FROM \"{}\" WHERE rowid=?1", info.table_name),
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
        got.sort_by(|a, b| a.properties[0].cmp(&b.properties[0]));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].label, "Person");
        assert_eq!(got[0].properties, vec!["bio".to_string()]);
        assert_eq!(got[1].label, "Person");
        assert_eq!(got[1].properties, vec!["name".to_string()]);
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
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].label, "Foo");
        assert_eq!(got[0].properties, vec!["cache_data".to_string()]);
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
        got.sort_by(|a, b| a.table_name.cmp(&b.table_name));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].label, "Doc");
        assert_eq!(got[0].properties, vec!["body".to_string()]);
        assert_eq!(got[0].kind, FtsTokenizerKind::TrigramCaseSensitive);
        assert_eq!(got[1].label, "Doc");
        assert_eq!(got[1].properties, vec!["title".to_string()]);
        assert_eq!(got[1].kind, FtsTokenizerKind::TrigramCaseSensitive);
        assert_eq!(got[2].label, "Note");
        assert_eq!(got[2].properties, vec!["text".to_string()]);
        assert_eq!(got[2].kind, FtsTokenizerKind::TrigramCaseSensitive);
    }

    #[test]
    fn list_all_fulltext_indexes_returns_tokenizer_kind() {
        let c = conn();
        create_fulltext_index(&c, "Person", "name").unwrap();
        create_fulltext_index_ci(&c, "Person", "bio").unwrap();
        create_fulltext_index_ci(&c, "Article", "body").unwrap();
        let mut got = list_all_fulltext_indexes(&c).unwrap();
        got.sort_by(|a, b| a.table_name.cmp(&b.table_name));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].label, "Article");
        assert_eq!(got[0].properties, vec!["body".to_string()]);
        assert_eq!(got[0].kind, FtsTokenizerKind::TrigramCaseInsensitive);
        assert_eq!(got[1].label, "Person");
        assert_eq!(got[1].properties, vec!["bio".to_string()]);
        assert_eq!(got[1].kind, FtsTokenizerKind::TrigramCaseInsensitive);
        assert_eq!(got[2].label, "Person");
        assert_eq!(got[2].properties, vec!["name".to_string()]);
        assert_eq!(got[2].kind, FtsTokenizerKind::TrigramCaseSensitive);
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
    fn fts_tokenizer_kind_returns_trigram_cs_for_default_index() {
        let c = conn();
        create_fulltext_index(&c, "Doc", "body").unwrap();
        assert_eq!(
            fts_tokenizer_kind(&c, "Doc", "body").unwrap(),
            FtsTokenizerKind::TrigramCaseSensitive
        );
    }

    #[test]
    fn fts_tokenizer_kind_returns_trigram_ci_for_ci_index() {
        let c = conn();
        create_fulltext_index_ci(&c, "Doc", "body").unwrap();
        assert_eq!(
            fts_tokenizer_kind(&c, "Doc", "body").unwrap(),
            FtsTokenizerKind::TrigramCaseInsensitive
        );
    }

    #[test]
    fn fts_tokenizer_kind_errors_when_index_missing() {
        let c = conn();
        match fts_tokenizer_kind(&c, "Doc", "body") {
            Err(GraphError::IndexNotFound {
                label, property, ..
            }) => {
                assert_eq!(label, "Doc");
                assert_eq!(property, "body");
            }
            other => panic!("expected IndexNotFound, got {other:?}"),
        }
    }

    #[test]
    fn create_fulltext_index_word_creates_unicode61_table() {
        let c = conn();
        create_fulltext_index_word(&c, "Doc", "body").unwrap();
        let sql: String = c
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                ["node_fts_Doc_body"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("unicode61"), "sql was: {sql}");
    }

    #[test]
    fn fts_tokenizer_kind_returns_word_for_unicode61_index() {
        let c = conn();
        create_fulltext_index_word(&c, "Doc", "body").unwrap();
        assert_eq!(
            fts_tokenizer_kind(&c, "Doc", "body").unwrap(),
            FtsTokenizerKind::Word
        );
    }

    #[test]
    fn create_fulltext_index_word_errors_when_trigram_exists_on_same_pair() {
        let c = conn();
        create_fulltext_index(&c, "Doc", "body").unwrap();
        match create_fulltext_index_word(&c, "Doc", "body") {
            Err(GraphError::IndexAlreadyExists {
                label, property, ..
            }) => {
                assert_eq!(label, "Doc");
                assert_eq!(property, "body");
            }
            other => panic!("expected IndexAlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn word_index_round_trip_does_not_match_substring() {
        // unicode61 tokenizes on word boundaries, so 'lic' (a substring
        // of 'Alice') must NOT match a word-tokenized index. This is
        // the key behavior distinguishing it from trigram.
        let c = conn();
        let id = crate::node::create_node(
            &c,
            &["Person".to_string()],
            props(&[("bio", Value::String("Alice Smith".to_string()))]),
        )
        .unwrap();
        create_fulltext_index_word(&c, "Person", "bio").unwrap();
        let hits: Vec<i64> = c
            .prepare(
                "SELECT rowid FROM \"node_fts_Person_bio\" WHERE \"node_fts_Person_bio\" MATCH ?1",
            )
            .unwrap()
            .query_map(["lic"], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            hits.is_empty(),
            "word index must not match substring 'lic'; got {hits:?}"
        );
        let _ = id;
    }

    #[test]
    fn word_index_matches_whole_token_case_insensitively() {
        let c = conn();
        let id = crate::node::create_node(
            &c,
            &["Person".to_string()],
            props(&[("bio", Value::String("Alice Smith".to_string()))]),
        )
        .unwrap();
        create_fulltext_index_word(&c, "Person", "bio").unwrap();
        let hits: Vec<i64> = c
            .prepare(
                "SELECT rowid FROM \"node_fts_Person_bio\" WHERE \"node_fts_Person_bio\" MATCH ?1",
            )
            .unwrap()
            .query_map(["alice"], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(hits, vec![id.0 as i64]);
    }

    #[test]
    fn parse_column_list_extracts_quoted_identifiers() {
        let sql = "CREATE VIRTUAL TABLE \"node_fts_multi_Article_1\" USING fts5(\"title\", \"body\", \"summary\", tokenize='unicode61')";
        assert_eq!(
            parse_column_list(sql),
            vec![
                "title".to_string(),
                "body".to_string(),
                "summary".to_string()
            ]
        );
    }

    #[test]
    fn parse_column_list_handles_single_content_column() {
        let sql = "CREATE VIRTUAL TABLE \"node_fts_Person_bio\" USING fts5(content, tokenize='trigram case_sensitive 1')";
        assert_eq!(parse_column_list(sql), vec!["content".to_string()]);
    }

    #[test]
    fn parse_column_list_handles_no_tokenize_clause() {
        let sql = "CREATE VIRTUAL TABLE \"t\" USING fts5(\"a\", \"b\")";
        assert_eq!(
            parse_column_list(sql),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn list_all_fulltext_indexes_returns_index_info_for_single_prop() {
        let c = conn();
        create_fulltext_index(&c, "Person", "name").unwrap();
        create_fulltext_index_ci(&c, "Person", "bio").unwrap();
        let mut got = list_all_fulltext_indexes(&c).unwrap();
        got.sort_by(|a, b| a.table_name.cmp(&b.table_name));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].label, "Person");
        assert_eq!(got[0].properties, vec!["bio".to_string()]);
        assert_eq!(got[0].kind, FtsTokenizerKind::TrigramCaseInsensitive);
        assert_eq!(got[0].table_name, "node_fts_Person_bio");
        assert_eq!(got[1].properties, vec!["name".to_string()]);
        assert_eq!(got[1].kind, FtsTokenizerKind::TrigramCaseSensitive);
    }

    #[test]
    fn create_fulltext_index_word_multi_creates_multi_column_table() {
        let c = conn();
        create_fulltext_index_word_multi(
            &c,
            "Article",
            &[
                "title".to_string(),
                "body".to_string(),
                "summary".to_string(),
            ],
        )
        .unwrap();
        let sql: String = c
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                ["node_fts_multi_Article_1"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("unicode61"), "sql was: {sql}");
        assert!(sql.contains("\"title\""), "sql was: {sql}");
        assert!(sql.contains("\"body\""), "sql was: {sql}");
        assert!(sql.contains("\"summary\""), "sql was: {sql}");
    }

    #[test]
    fn create_fulltext_index_word_multi_picks_smallest_unused_n() {
        let c = conn();
        create_fulltext_index_word_multi(&c, "Article", &["title".to_string(), "body".to_string()])
            .unwrap();
        create_fulltext_index_word_multi(
            &c,
            "Article",
            &["summary".to_string(), "abstract".to_string()],
        )
        .unwrap();
        let names: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'node_fts_multi_Article_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let virtual_tables: Vec<&String> = names
            .iter()
            .filter(|n| {
                !n.contains("_data")
                    && !n.contains("_idx")
                    && !n.contains("_content")
                    && !n.contains("_docsize")
                    && !n.contains("_config")
            })
            .collect();
        assert_eq!(virtual_tables.len(), 2);
        assert_eq!(virtual_tables[0], "node_fts_multi_Article_1");
        assert_eq!(virtual_tables[1], "node_fts_multi_Article_2");
    }

    #[test]
    fn create_fulltext_index_word_multi_errors_on_empty_properties() {
        let c = conn();
        match create_fulltext_index_word_multi(&c, "Article", &[]) {
            Err(GraphError::Query(_)) | Err(GraphError::IndexAlreadyExists { .. }) => {}
            other => panic!("expected error on empty properties, got {other:?}"),
        }
    }

    #[test]
    fn create_fulltext_index_word_multi_errors_on_duplicate_properties() {
        let c = conn();
        match create_fulltext_index_word_multi(
            &c,
            "Article",
            &["title".to_string(), "title".to_string()],
        ) {
            Err(GraphError::IndexAlreadyExists { .. }) | Err(GraphError::Query(_)) => {}
            other => panic!("expected error on duplicate properties, got {other:?}"),
        }
    }

    #[test]
    fn create_fulltext_index_word_multi_errors_when_single_prop_overlaps() {
        let c = conn();
        create_fulltext_index_word(&c, "Article", "title").unwrap();
        match create_fulltext_index_word_multi(
            &c,
            "Article",
            &["title".to_string(), "body".to_string()],
        ) {
            Err(GraphError::IndexAlreadyExists {
                label, property, ..
            }) => {
                assert_eq!(label, "Article");
                assert_eq!(property, "title");
            }
            other => panic!("expected IndexAlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn create_single_prop_errors_when_multi_covers_pair() {
        let c = conn();
        create_fulltext_index_word_multi(&c, "Article", &["title".to_string(), "body".to_string()])
            .unwrap();
        match create_fulltext_index_word(&c, "Article", "title") {
            Err(GraphError::IndexAlreadyExists {
                label, property, ..
            }) => {
                assert_eq!(label, "Article");
                assert_eq!(property, "title");
            }
            other => panic!("expected IndexAlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn fts_tokenizer_kind_resolves_through_multi_prop_index() {
        let c = conn();
        create_fulltext_index_word_multi(&c, "Article", &["title".to_string(), "body".to_string()])
            .unwrap();
        assert_eq!(
            fts_tokenizer_kind(&c, "Article", "title").unwrap(),
            FtsTokenizerKind::Word
        );
        assert_eq!(
            fts_tokenizer_kind(&c, "Article", "body").unwrap(),
            FtsTokenizerKind::Word
        );
        match fts_tokenizer_kind(&c, "Article", "summary") {
            Err(GraphError::IndexNotFound { .. }) => {}
            other => panic!("expected IndexNotFound, got {other:?}"),
        }
    }

    #[test]
    fn list_all_fulltext_indexes_returns_multi_prop_info() {
        let c = conn();
        create_fulltext_index_word_multi(&c, "Article", &["title".to_string(), "body".to_string()])
            .unwrap();
        let got = list_all_fulltext_indexes(&c).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].label, "Article");
        assert_eq!(
            got[0].properties,
            vec!["title".to_string(), "body".to_string()]
        );
        assert_eq!(got[0].kind, FtsTokenizerKind::Word);
        assert_eq!(got[0].table_name, "node_fts_multi_Article_1");
    }

    #[test]
    fn list_all_fulltext_indexes_skips_shadow_tables() {
        // Regression: a user property literally named "cache_data" or other
        // FTS5 shadow-suffix names must not confuse the listing.
        let c = conn();
        create_fulltext_index(&c, "Foo", "cache_data").unwrap();
        let got = list_all_fulltext_indexes(&c).unwrap();
        // Exactly one entry — the virtual table itself — not the shadow tables.
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].label, "Foo");
        assert_eq!(got[0].properties, vec!["cache_data".to_string()]);
        assert_eq!(got[0].kind, FtsTokenizerKind::TrigramCaseSensitive);
    }
}
