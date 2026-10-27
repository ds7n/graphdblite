//! Full-text index over `(label, property)` pairs. Backed by SQLite FTS5
//! virtual tables using the `trigram` tokenizer with `case_sensitive 1`.
//! Accelerates `CONTAINS` / `STARTS WITH` / `ENDS WITH` predicates via the
//! `LogicalOp::FullTextLookup` planner rewrite.

use crate::storage::naming;
use crate::types::{validate_name, GraphError, NodeId, Properties, Result, Value};
use rusqlite::Connection;

/// Build the FTS table name for a single-property index on
/// `(label, property)` after validating both names.
///
/// See `storage::naming`: `node_fts$<label>$<property>$`, with uppercase
/// letters escaped (SQLite table names are case-insensitive) and a trailing
/// `$` so no index table can collide with an FTS5 shadow table
/// (`<table>_data`, ...) of another index. Databases created with older
/// names are migrated on open (`storage::migrate`).
pub fn fts_table_name(label: &str, property: &str) -> Result<String> {
    validate_name(label)?;
    validate_name(property)?;
    Ok(naming::fts_table_name(label, property))
}

/// Whether an FTS index table named `table` exists. Only `CREATE VIRTUAL
/// TABLE` entries count: FTS5 shadow tables (`<t>_data`, `<t>_idx`, ...)
/// are plain tables and must never be mistaken for an index (dropping one
/// would corrupt the index that owns it).
fn fts_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master \
         WHERE type='table' AND name=?1 AND sql LIKE 'CREATE VIRTUAL TABLE%'",
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
        properties: vec![property.to_string()],
        hint: Some("no fulltext index covers this (label, property)".to_string()),
    })
}

/// Parse a `CREATE VIRTUAL TABLE ... USING fts5(...)` DDL string and
/// recover which tokenizer it specified.
///
/// Order-of-checks invariant: a `unicode61` DDL never contains the
/// `case_sensitive` flag (that flag belongs to the `trigram` tokenizer),
/// so checking `unicode61` first is safe.
pub(crate) fn parse_tokenizer_kind(sql: &str) -> FtsTokenizerKind {
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

impl FtsIndexInfo {
    /// Whether this is a multi-property table (one FTS5 column per
    /// property) rather than a single-property table (one column named
    /// `content`).
    pub fn is_multi(&self) -> bool {
        naming::parse_fts_multi_table_name(&self.table_name).is_some()
    }

    /// FTS5 column holding `property` in this index's table.
    pub fn column_for<'a>(&self, property: &'a str) -> &'a str {
        if self.is_multi() {
            property
        } else {
            "content"
        }
    }
}

/// Parse the comma-separated column list out of an FTS5
/// `CREATE VIRTUAL TABLE ... USING fts5(...)` DDL string.
///
/// Handles bare identifiers (`content`) and quoted ones (`"title"`).
/// Stops at the `tokenize=` clause if present, otherwise at the
/// closing `)`. Returns an empty vec if the DDL shape is unexpected.
pub(crate) fn parse_column_list(sql: &str) -> Vec<String> {
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
/// content and the query phrase. The planner uses it for the
/// `toLower(n.prop) <op> toLower(term)` idiom; plain predicates may use it
/// too, but every candidate is re-checked case-sensitively.
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
/// Picks the smallest positive `n` such that `node_fts_multi$<label>$<n>`
/// is unused. Deterministic, doesn't depend on the property list (so the
/// same label can host several disjoint multi-prop indexes).
fn fts_multi_table_name(conn: &Connection, label: &str) -> Result<String> {
    validate_name(label)?;
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE ?1")?;
    let rows = stmt.query_map([format!("{}%", naming::FTS_MULTI_PREFIX)], |row| {
        row.get::<_, String>(0)
    })?;

    let mut used: Vec<u64> = Vec::new();
    for r in rows {
        if let Some((l, n)) = naming::parse_fts_multi_table_name(&r?) {
            if l == label {
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
    Ok(naming::fts_multi_table_name(label, n))
}

/// Create a multi-property word-tokenized fulltext index on `label`.
///
/// One FTS5 virtual table (`node_fts_multi$<label>$<N>`) covers all
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
            properties: vec![],
            hint: Some(
                "create_fulltext_index_word_multi requires at least one property".to_string(),
            ),
        });
    }
    // Reject duplicate property names in the request. FTS5 column names are
    // case-insensitive, so `t` and `T` cannot both be columns of one table.
    let mut seen = std::collections::HashSet::new();
    for p in properties {
        if !seen.insert(p.to_ascii_lowercase()) {
            return Err(GraphError::IndexAlreadyExists {
                label: label.to_string(),
                properties: vec![p.clone()],
                hint: Some(
                    "duplicate property (names are compared case-insensitively) in multi-prop index list"
                        .to_string(),
                ),
            });
        }
    }
    // Mutex: no existing FTS index can cover any of these properties.
    for p in properties {
        if fts_tokenizer_kind(conn, label, p).is_ok() {
            return Err(GraphError::IndexAlreadyExists {
                label: label.to_string(),
                properties: vec![p.clone()],
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
    let qt = naming::quote_ident(&table);
    let cols_quoted: String = properties
        .iter()
        .map(|p| naming::quote_ident(p))
        .collect::<Vec<_>>()
        .join(", ");
    let tokenize = spec.tokenize_clause();
    conn.execute(
        &format!("CREATE VIRTUAL TABLE {qt} USING fts5({cols_quoted}, tokenize='{tokenize}')"),
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
    let insert_sql = format!("INSERT INTO {qt} (rowid, {cols_quoted}) VALUES (?1, {placeholders})");
    for n in &nodes {
        // Skip nodes that have no string value on any covered property,
        // matching the steady-state write path in `update_fts_for_node`.
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
            properties: vec![property.to_string()],
            hint: Some("a fulltext index on this (label, property) already exists".to_string()),
        });
    }
    // Mutex against multi-prop indexes covering the same (label, property).
    if fts_tokenizer_kind(conn, label, property).is_ok() {
        return Err(GraphError::IndexAlreadyExists {
            label: label.to_string(),
            properties: vec![property.to_string()],
            hint: Some(
                "a multi-property fulltext index already covers this (label, property)".to_string(),
            ),
        });
    }
    let qt = naming::quote_ident(&table);
    conn.execute(
        &format!(
            "CREATE VIRTUAL TABLE {qt} USING fts5(content, tokenize='{}')",
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
                &format!("INSERT INTO {qt} (rowid, content) VALUES (?1, ?2)"),
                rusqlite::params![n.id.0 as i64, s],
            )?;
        }
    }
    Ok(())
}

/// Recreate an index from its listed definition (same label, properties and
/// tokenizer) under the current naming scheme, backfilling from node data.
/// Used by the schema migration; the caller drops the old table first.
pub(crate) fn recreate_fulltext_index(
    conn: &Connection,
    label: &str,
    properties: &[String],
    kind: FtsTokenizerKind,
    multi: bool,
) -> Result<()> {
    let spec = match kind {
        FtsTokenizerKind::TrigramCaseSensitive => FtsTokenizerSpec::TrigramCaseSensitive,
        FtsTokenizerKind::TrigramCaseInsensitive => FtsTokenizerSpec::TrigramCaseInsensitive,
        FtsTokenizerKind::Word => FtsTokenizerSpec::Word,
    };
    if multi {
        for p in properties {
            validate_name(p)?;
        }
        create_fulltext_index_multi_impl(conn, label, properties, spec)
    } else {
        create_fulltext_index_impl(conn, label, &properties[0], spec)
    }
}

/// Drop a fulltext index on `(label, property)`.
pub fn drop_fulltext_index(conn: &Connection, label: &str, property: &str) -> Result<()> {
    let table = fts_table_name(label, property)?;
    if !fts_table_exists(conn, &table)? {
        return Err(GraphError::IndexNotFound {
            label: label.to_string(),
            properties: vec![property.to_string()],
            hint: Some("no fulltext index on this (label, property)".to_string()),
        });
    }
    conn.execute(&format!("DROP TABLE {}", naming::quote_ident(&table)), [])?;
    Ok(())
}

/// List all fulltext indexes that exist for a given label.
pub fn list_fulltext_indexes_for_label(
    conn: &Connection,
    label: &str,
) -> Result<Vec<FtsIndexInfo>> {
    let all = list_all_fulltext_indexes(conn)?;
    Ok(all.into_iter().filter(|info| info.label == label).collect())
}

/// List every fulltext index in the database.
///
/// FTS5 creates internal shadow tables (`<name>_data`, `<name>_idx`,
/// `<name>_content`, `<name>_docsize`, `<name>_config`) alongside each
/// virtual table. We distinguish user-created FTS indexes from shadow
/// tables by looking at `sqlite_master.sql`: virtual tables are created
/// with `CREATE VIRTUAL TABLE`, shadow tables with plain `CREATE TABLE`.
///
/// Table names and column lists come from the database file, which may be
/// crafted: a table is only listed when its name parses as a canonical index
/// name (`storage::naming`) and every column is a valid property name, so
/// nothing unvalidated is ever interpolated into SQL.
pub fn list_all_fulltext_indexes(conn: &Connection) -> Result<Vec<FtsIndexInfo>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, sql FROM sqlite_master \
         WHERE type='table' \
         AND name LIKE 'node_fts%' \
         AND sql LIKE 'CREATE VIRTUAL TABLE%'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut result = Vec::new();
    for r in rows {
        let (name, sql) = r?;
        let kind = parse_tokenizer_kind(&sql);
        if let Some((label, property)) = naming::parse_fts_table_name(&name) {
            // Single-prop: the property comes from the table name; the FTS5
            // column is always literally `content`.
            result.push(FtsIndexInfo {
                label,
                properties: vec![property],
                kind,
                table_name: name,
            });
        } else if let Some((label, _)) = naming::parse_fts_multi_table_name(&name) {
            let properties = parse_column_list(&sql);
            if properties.is_empty() || properties.iter().any(|p| validate_name(p).is_err()) {
                continue;
            }
            result.push(FtsIndexInfo {
                label,
                properties,
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
/// Sync the fulltext indexes owned by a single `label` for a node write.
pub fn update_fts_for_label(
    conn: &Connection,
    node_id: NodeId,
    label: &str,
    old_properties: Option<&Properties>,
    new_properties: &Properties,
) -> Result<()> {
    let infos = list_fulltext_indexes_for_label(conn, label)?;
    for info in &infos {
        update_fts_for_node_one_index(conn, node_id, info, old_properties, new_properties)?;
    }
    Ok(())
}

/// Sync fulltext indexes for a node write across ALL of the node's labels.
///
/// Like btree indexes, an FTS index is served for any node carrying its
/// declared label, so maintenance must run for every label the node has — not
/// just the sorted-first primary. Call with `old_properties = None` for new
/// nodes.
pub fn update_fts_for_node(
    conn: &Connection,
    node_id: NodeId,
    labels: &[String],
    old_properties: Option<&Properties>,
    new_properties: &Properties,
) -> Result<()> {
    for label in labels {
        update_fts_for_label(conn, node_id, label, old_properties, new_properties)?;
    }
    Ok(())
}

/// Sync one FTS index for a node write.
///
/// Handles both single-property tables (one FTS5 column literally named
/// `content`) and multi-property tables (one FTS5 column per property).
///
/// Idempotent: the node's row is deleted before the new one is inserted, so
/// a re-sync with `old_properties = None` for a node that is already indexed
/// (e.g. `SET n:Label` re-syncing every label) replaces the row instead of
/// failing on the duplicate rowid.
fn update_fts_for_node_one_index(
    conn: &Connection,
    node_id: NodeId,
    info: &FtsIndexInfo,
    old_properties: Option<&Properties>,
    new_properties: &Properties,
) -> Result<()> {
    let old_vals: Vec<Option<String>> = info
        .properties
        .iter()
        .map(|p| {
            old_properties
                .and_then(|props| props.get(p.as_str()))
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
        })
        .collect();
    let new_vals: Vec<Option<String>> = info
        .properties
        .iter()
        .map(|p| {
            new_properties.get(p.as_str()).and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
        })
        .collect();

    if old_vals == new_vals && old_properties.is_some() {
        return Ok(());
    }

    let qt = naming::quote_ident(&info.table_name);
    let has_any_new = new_vals.iter().any(|v| v.is_some());

    conn.execute(
        &format!("DELETE FROM {qt} WHERE rowid=?1"),
        rusqlite::params![node_id.0 as i64],
    )?;
    if !has_any_new {
        return Ok(());
    }
    let cols_quoted: String = info
        .properties
        .iter()
        .map(|p| naming::quote_ident(info.column_for(p)))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders: String = (2..=info.properties.len() + 1)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("INSERT INTO {qt} (rowid, {cols_quoted}) VALUES (?1, {placeholders})");
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(info.properties.len() + 1);
    params.push(Box::new(node_id.0 as i64));
    for v in &new_vals {
        match v {
            Some(s) => params.push(Box::new(s.clone())),
            None => params.push(Box::new(rusqlite::types::Null)),
        }
    }
    conn.execute(&sql, rusqlite::params_from_iter(params.iter()))?;
    Ok(())
}

/// Remove the fulltext rows owned by a single `label` for a node.
pub fn remove_fts_for_label(
    conn: &Connection,
    node_id: NodeId,
    label: &str,
    properties: &Properties,
) -> Result<()> {
    let infos = list_fulltext_indexes_for_label(conn, label)?;
    for info in &infos {
        // Delete the row if any covered property had a string value (the
        // only condition under which a row would have been inserted).
        let any_indexed = info
            .properties
            .iter()
            .any(|p| matches!(properties.get(p.as_str()), Some(Value::String(_))));
        if any_indexed {
            conn.execute(
                &format!(
                    "DELETE FROM {} WHERE rowid=?1",
                    naming::quote_ident(&info.table_name)
                ),
                rusqlite::params![node_id.0 as i64],
            )?;
        }
    }
    Ok(())
}

/// Remove all fulltext index entries for a node being deleted, across ALL its
/// labels.
pub fn remove_fts_for_node(
    conn: &Connection,
    node_id: NodeId,
    labels: &[String],
    properties: &Properties,
) -> Result<()> {
    for label in labels {
        remove_fts_for_label(conn, node_id, label, properties)?;
    }
    Ok(())
}

/// Look up node ids whose property contains the term as a substring.
///
/// Returns `Ok(Some(ids))` when the FTS index handled the lookup, and
/// `Ok(None)` when the term is too short for the trigram tokenizer
/// (`<3` Unicode codepoints); the caller must then fall back to a scan.
///
/// Errors when no fulltext index exists for `(label, property)`.
pub fn fulltext_lookup(
    conn: &Connection,
    label: &str,
    property: &str,
    term: &str,
) -> Result<Option<Vec<(NodeId, f64)>>> {
    if term.chars().count() < 3 {
        return Ok(None);
    }
    let table = fts_table_name(label, property)?;
    if !fts_table_exists(conn, &table)? {
        return Err(GraphError::IndexNotFound {
            label: label.to_string(),
            properties: vec![property.to_string()],
            hint: Some("no fulltext index on this (label, property)".to_string()),
        });
    }

    // FTS5 phrase: wrap in double-quotes; embedded double-quotes are
    // escaped by doubling them.
    let escaped = term.replace('"', "\"\"");
    let phrase = format!("\"{escaped}\"");

    let qt = naming::quote_ident(&table);
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT rowid, bm25({qt}) AS rank FROM {qt} WHERE content MATCH ?1"
    ))?;
    let rows = stmt.query_map([&phrase], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id, rank) = row?;
        out.push((NodeId(id as u64), rank));
    }
    Ok(Some(out))
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
            "node_fts$^article$body$"
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
                ["node_fts$^person$bio$"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exists, "create must produce an FTS table");

        drop_fulltext_index(&c, "Person", "bio").unwrap();
        let exists_after: bool = c
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                ["node_fts$^person$bio$"],
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
                label, properties, ..
            }) => {
                assert_eq!(label, "Person");
                assert_eq!(properties, vec!["bio".to_string()]);
            }
            other => panic!("expected IndexAlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn drop_fulltext_index_errors_when_absent() {
        let c = conn();
        match drop_fulltext_index(&c, "Person", "bio") {
            Err(GraphError::IndexNotFound {
                label, properties, ..
            }) => {
                assert_eq!(label, "Person");
                assert_eq!(properties, vec!["bio".to_string()]);
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
        update_fts_for_label(&c, NodeId(7), "Person", None, &p).unwrap();
        assert_eq!(fts_rowids(&c, "Person", "bio"), vec![7]);
    }

    #[test]
    fn update_fts_skips_non_string_values() {
        let c = conn();
        create_fulltext_index(&c, "Person", "age").unwrap();
        let p = props(&[("age", Value::I64(42))]);
        update_fts_for_label(&c, NodeId(7), "Person", None, &p).unwrap();
        assert_eq!(fts_rowids(&c, "Person", "age"), Vec::<i64>::new());
    }

    #[test]
    fn update_fts_replaces_on_value_change() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        let old = props(&[("bio", Value::String("hello".into()))]);
        let new = props(&[("bio", Value::String("goodbye".into()))]);
        update_fts_for_label(&c, NodeId(7), "Person", None, &old).unwrap();
        update_fts_for_label(&c, NodeId(7), "Person", Some(&old), &new).unwrap();
        assert_eq!(fts_rowids(&c, "Person", "bio"), vec![7]);
        let content: String = c
            .query_row(
                "SELECT content FROM \"node_fts$^person$bio$\" WHERE rowid=7",
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
        update_fts_for_label(&c, NodeId(7), "Person", None, &old).unwrap();
        update_fts_for_label(&c, NodeId(7), "Person", Some(&old), &new).unwrap();
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
        update_fts_for_label(&c, NodeId(7), "Person", None, &p).unwrap();
        remove_fts_for_label(&c, NodeId(7), "Person", &p).unwrap();
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
            update_fts_for_label(c, NodeId(id as u64), "Doc", None, &p).unwrap();
        }
    }

    #[test]
    fn fulltext_lookup_returns_matches_for_normal_term() {
        let c = conn();
        setup_with_nodes(&c);
        let mut ids: Vec<NodeId> = fulltext_lookup(&c, "Doc", "text", "brown")
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        ids.sort_by_key(|n| n.0);
        assert_eq!(ids, vec![NodeId(1), NodeId(3)]);
    }

    #[test]
    fn fulltext_lookup_is_case_sensitive() {
        let c = conn();
        setup_with_nodes(&c);
        let ids: Vec<NodeId> = fulltext_lookup(&c, "Doc", "text", "foobar")
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(
            ids.is_empty(),
            "case-sensitive: lowercase must not match uppercase"
        );

        let ids: Vec<NodeId> = fulltext_lookup(&c, "Doc", "text", "FOOBAR")
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
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
        update_fts_for_label(&c, NodeId(1), "Doc", None, &p).unwrap();
        let ids: Vec<NodeId> = fulltext_lookup(&c, "Doc", "text", "\"hello\"")
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![NodeId(1)]);
    }

    #[test]
    fn fulltext_lookup_returns_rowid_and_rank_tuples() {
        let c = conn();
        let id1 = crate::node::create_node(
            &c,
            &["Doc".to_string()],
            props(&[("body", Value::String("rust rust rust".to_string()))]),
        )
        .unwrap();
        let id2 = crate::node::create_node(
            &c,
            &["Doc".to_string()],
            props(&[("body", Value::String("rust occasionally".to_string()))]),
        )
        .unwrap();
        create_fulltext_index(&c, "Doc", "body").unwrap();
        let hits = fulltext_lookup(&c, "Doc", "body", "rust").unwrap().unwrap();
        // Two hits; FTS5 bm25() returns negative floats (lower = better match).
        assert_eq!(hits.len(), 2);
        let id_set: std::collections::HashSet<NodeId> = hits.iter().map(|(id, _)| *id).collect();
        assert!(id_set.contains(&id1));
        assert!(id_set.contains(&id2));
        // All scores must be finite floats.
        for (_, s) in &hits {
            assert!(s.is_finite(), "score not finite: {s}");
        }
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
                ["node_fts$^person$bio$"],
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
        let hits: Vec<NodeId> = fulltext_lookup(&c, "Person", "bio", "alice")
            .unwrap()
            .expect("ci index should serve the query")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(hits, vec![id]);
    }

    #[test]
    fn create_fulltext_index_ci_errors_when_cs_exists_on_same_pair() {
        let c = conn();
        create_fulltext_index(&c, "Person", "bio").unwrap();
        match create_fulltext_index_ci(&c, "Person", "bio") {
            Err(GraphError::IndexAlreadyExists {
                label, properties, ..
            }) => {
                assert_eq!(label, "Person");
                assert_eq!(properties, vec!["bio".to_string()]);
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
                label, properties, ..
            }) => {
                assert_eq!(label, "Doc");
                assert_eq!(properties, vec!["body".to_string()]);
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
                ["node_fts$^doc$body$"],
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
                label, properties, ..
            }) => {
                assert_eq!(label, "Doc");
                assert_eq!(properties, vec!["body".to_string()]);
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
                "SELECT rowid FROM \"node_fts$^person$bio$\" WHERE \"node_fts$^person$bio$\" MATCH ?1",
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
                "SELECT rowid FROM \"node_fts$^person$bio$\" WHERE \"node_fts$^person$bio$\" MATCH ?1",
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
        let sql = "CREATE VIRTUAL TABLE \"node_fts_multi$^article$1\" USING fts5(\"title\", \"body\", \"summary\", tokenize='unicode61')";
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
        let sql = "CREATE VIRTUAL TABLE \"node_fts$^person$bio$\" USING fts5(content, tokenize='trigram case_sensitive 1')";
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
        assert_eq!(got[0].table_name, "node_fts$^person$bio$");
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
                ["node_fts_multi$^article$1"],
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
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'node_fts_multi$^article$%' ORDER BY name")
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
        assert_eq!(virtual_tables[0], "node_fts_multi$^article$1");
        assert_eq!(virtual_tables[1], "node_fts_multi$^article$2");
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
                label, properties, ..
            }) => {
                assert_eq!(label, "Article");
                assert_eq!(properties, vec!["title".to_string()]);
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
                label, properties, ..
            }) => {
                assert_eq!(label, "Article");
                assert_eq!(properties, vec!["title".to_string()]);
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
        assert_eq!(got[0].table_name, "node_fts_multi$^article$1");
    }

    #[test]
    fn update_fts_for_node_writes_multi_column_row() {
        let c = conn();
        create_fulltext_index_word_multi(&c, "Article", &["title".to_string(), "body".to_string()])
            .unwrap();
        let id = NodeId(42);
        let p = props(&[
            ("title", Value::String("rust systems".to_string())),
            ("body", Value::String("memory safety is key".to_string())),
        ]);
        update_fts_for_label(&c, id, "Article", None, &p).unwrap();

        let title_hits: Vec<i64> = c
            .prepare("SELECT rowid FROM \"node_fts_multi$^article$1\" WHERE \"node_fts_multi$^article$1\" MATCH ?1")
            .unwrap()
            .query_map(["systems"], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(title_hits, vec![42]);
        let body_hits: Vec<i64> = c
            .prepare("SELECT rowid FROM \"node_fts_multi$^article$1\" WHERE \"node_fts_multi$^article$1\" MATCH ?1")
            .unwrap()
            .query_map(["safety"], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(body_hits, vec![42]);
    }

    #[test]
    fn update_fts_for_node_handles_partial_multi_column_node() {
        let c = conn();
        create_fulltext_index_word_multi(&c, "Article", &["title".to_string(), "body".to_string()])
            .unwrap();
        let p = props(&[("title", Value::String("only title".to_string()))]);
        update_fts_for_label(&c, NodeId(7), "Article", None, &p).unwrap();
        let hits: Vec<i64> = c
            .prepare("SELECT rowid FROM \"node_fts_multi$^article$1\" WHERE \"node_fts_multi$^article$1\" MATCH ?1")
            .unwrap()
            .query_map(["title"], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(hits, vec![7]);
    }

    #[test]
    fn remove_fts_for_node_clears_multi_column_row() {
        let c = conn();
        create_fulltext_index_word_multi(&c, "Article", &["title".to_string(), "body".to_string()])
            .unwrap();
        let p = props(&[
            ("title", Value::String("hello".to_string())),
            ("body", Value::String("world".to_string())),
        ]);
        update_fts_for_label(&c, NodeId(1), "Article", None, &p).unwrap();
        remove_fts_for_label(&c, NodeId(1), "Article", &p).unwrap();
        let hits: Vec<i64> = c
            .prepare("SELECT rowid FROM \"node_fts_multi$^article$1\" WHERE \"node_fts_multi$^article$1\" MATCH ?1")
            .unwrap()
            .query_map(["hello"], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn update_fts_for_node_with_property_change_overwrites_multi_column_row() {
        let c = conn();
        create_fulltext_index_word_multi(&c, "Article", &["title".to_string(), "body".to_string()])
            .unwrap();
        let old = props(&[
            ("title", Value::String("old".to_string())),
            ("body", Value::String("text".to_string())),
        ]);
        update_fts_for_label(&c, NodeId(1), "Article", None, &old).unwrap();
        let new = props(&[
            ("title", Value::String("new".to_string())),
            ("body", Value::String("text".to_string())),
        ]);
        update_fts_for_label(&c, NodeId(1), "Article", Some(&old), &new).unwrap();
        let old_hits: Vec<i64> = c
            .prepare("SELECT rowid FROM \"node_fts_multi$^article$1\" WHERE \"node_fts_multi$^article$1\" MATCH ?1")
            .unwrap()
            .query_map(["old"], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(old_hits.is_empty(), "old title should no longer match");
        let new_hits: Vec<i64> = c
            .prepare("SELECT rowid FROM \"node_fts_multi$^article$1\" WHERE \"node_fts_multi$^article$1\" MATCH ?1")
            .unwrap()
            .query_map(["new"], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(new_hits, vec![1]);
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
