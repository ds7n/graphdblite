use rusqlite::Connection;

use crate::node;
use crate::storage::kv;
use crate::types::{validate_name, GraphError, NodeId, Properties, Result, Value};

/// Metadata for one secondary index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    pub label: String,
    /// Properties in declared (column) order. Length 1 for legacy
    /// single-prop indexes; length >= 2 for composite.
    pub properties: Vec<String>,
    /// "btree" today. Reserved for future kinds.
    pub kind: &'static str,
}

/// Build the table name for a composite (or single-prop) index.
///
/// - `N == 1` → legacy `node_idx_<label>_<prop>` (same as `index_table_name`).
/// - `N >= 2` → `node_idx$<label>$<prop1>$…$<propN>`.
///
/// `$` is forbidden by `validate_name`, so the separator is unambiguous.
fn composite_index_table_name(label: &str, properties: &[&str]) -> Result<String> {
    if properties.is_empty() {
        return Err(GraphError::InvalidIndexDefinition {
            reason: "property list cannot be empty".to_string(),
            hint: None,
        });
    }
    validate_name(label)?;
    let mut seen = std::collections::HashSet::with_capacity(properties.len());
    for p in properties {
        validate_name(p)?;
        if !seen.insert(*p) {
            return Err(GraphError::InvalidIndexDefinition {
                reason: format!("duplicate property in composite index: {p}"),
                hint: None,
            });
        }
    }
    if properties.len() == 1 {
        Ok(format!("node_idx_{label}_{}", properties[0]))
    } else {
        let joined = properties.join("$");
        Ok(format!("node_idx${label}${joined}"))
    }
}

/// Build the composite index key: [msgpack(v1)]…[msgpack(vN)][node_id: 8 bytes BE].
fn composite_index_key(values: &[Value], node_id: NodeId) -> Result<Vec<u8>> {
    debug_assert!(
        !values.is_empty(),
        "composite_index_key requires at least one value; \
         every caller goes through composite_index_table_name which already rejects empty"
    );
    let mut key = Vec::with_capacity(values.len() * 16 + 8);
    for v in values {
        let frame = rmp_serde::to_vec(v).map_err(|e| GraphError::Serialization {
            context: String::new(),
            source: e.to_string(),
            hint: None,
        })?;
        key.extend_from_slice(&frame);
    }
    key.extend_from_slice(&node_id.to_be_bytes());
    Ok(key)
}

/// Build the index table name for a (label, property) pair.
///
/// Validates both components so that the returned name is safe to interpolate
/// into raw SQL identifiers (`"{table}"`). Defense-in-depth: callers (the
/// planner, the public `create_index` API) already validate their inputs, but
/// re-validating here ensures any future caller can't accidentally smuggle a
/// `"` through into the SQL identifier and corrupt the query. See security
/// finding M1.
fn index_table_name(label: &str, property: &str) -> Result<String> {
    validate_name(label)?;
    validate_name(property)?;
    Ok(format!("node_idx_{label}_{property}"))
}

/// Create a composite (multi-column) secondary index.
///
/// `properties.len() == 1` is permitted and resolves to the same on-disk
/// table as `create_index(label, prop)` — both APIs are interchangeable
/// for the single-prop case.
pub fn create_composite_index(conn: &Connection, label: &str, properties: &[&str]) -> Result<()> {
    let table = composite_index_table_name(label, properties)?;

    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        [&table],
        |row| row.get(0),
    )?;
    if exists {
        return Err(GraphError::IndexAlreadyExists {
            label: label.to_string(),
            properties: properties.iter().map(|s| s.to_string()).collect(),
            hint: None,
        });
    }

    conn.execute(
        &format!(
            "CREATE TABLE \"{table}\" (key BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID"
        ),
        [],
    )?;

    // Backfill — skip nodes missing any covered property.
    let nodes = node::find_nodes_by_label(conn, label)?;
    for n in &nodes {
        let mut values: Vec<Value> = Vec::with_capacity(properties.len());
        let mut complete = true;
        for p in properties {
            match n.properties.get(*p) {
                Some(v) => values.push(v.clone()),
                None => {
                    complete = false;
                    break;
                }
            }
        }
        if !complete {
            continue;
        }
        let key = composite_index_key(&values, n.id)?;
        kv::put(conn, &table, &key, &[])?;
    }

    Ok(())
}

/// Drop a composite (or N=1) secondary index.
pub fn drop_composite_index(conn: &Connection, label: &str, properties: &[&str]) -> Result<()> {
    let table = composite_index_table_name(label, properties)?;
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        [&table],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(GraphError::IndexNotFound {
            label: label.to_string(),
            properties: properties.iter().map(|s| s.to_string()).collect(),
            hint: None,
        });
    }
    conn.execute(&format!("DROP TABLE \"{table}\""), [])?;
    Ok(())
}

/// Create a secondary index on a (label, property) pair.
/// Backfills the index with all existing matching nodes.
pub fn create_index(conn: &Connection, label: &str, property: &str) -> Result<()> {
    create_composite_index(conn, label, &[property])
}

/// Drop a secondary index.
pub fn drop_index(conn: &Connection, label: &str, property: &str) -> Result<()> {
    drop_composite_index(conn, label, &[property])
}

/// Lookup nodes by indexed property value.
pub fn index_lookup(
    conn: &Connection,
    label: &str,
    property: &str,
    value: &Value,
) -> Result<Vec<NodeId>> {
    let table = index_table_name(label, property)?;

    // Build prefix from the serialized value.
    let prefix = rmp_serde::to_vec(value).map_err(|e| GraphError::Serialization {
        context: String::new(),
        source: e.to_string(),
        hint: None,
    })?;
    let entries = match kv::scan_prefix(conn, &table, &prefix) {
        Ok(e) => e,
        Err(GraphError::Storage { source: ref e, .. })
            if e.to_string().contains("no such table") =>
        {
            return Err(GraphError::IndexNotFound {
                label: label.to_string(),
                properties: vec![property.to_string()],
                hint: None,
            });
        }
        Err(e) => return Err(e),
    };

    let mut ids = Vec::new();
    for (key, _) in entries {
        // Key = [msgpack(value)][node_id: 8 BE]
        // Extract the last 8 bytes as node_id.
        if key.len() >= 8 {
            let id_bytes: [u8; 8] =
                key[key.len() - 8..]
                    .try_into()
                    .map_err(|_| GraphError::Serialization {
                        context: String::new(),
                        source: "corrupt index key bytes".into(),
                        hint: None,
                    })?;
            ids.push(NodeId::from_be_bytes(id_bytes));
        }
    }
    Ok(ids)
}

/// Range-scan a composite index for the prefix `prefix_values`.
/// `prefix_values.len() <= properties.len()` (the prefix may be shorter
/// than the full key). Returns the matching node IDs in scan order.
pub fn composite_index_prefix_lookup(
    conn: &Connection,
    label: &str,
    properties: &[&str],
    prefix_values: &[Value],
) -> Result<Vec<NodeId>> {
    assert!(
        prefix_values.len() <= properties.len(),
        "prefix length exceeds index width"
    );
    let table = composite_index_table_name(label, properties)?;

    // Encode the prefix: msgpack-concat of the prefix values, no node id.
    let mut prefix_bytes = Vec::new();
    for v in prefix_values {
        let frame = rmp_serde::to_vec(v).map_err(|e| GraphError::Serialization {
            context: String::new(),
            source: e.to_string(),
            hint: None,
        })?;
        prefix_bytes.extend_from_slice(&frame);
    }

    // `next_prefix` returns `None` when the prefix has no lex-successor
    // (all bytes 0xFF). In that case we drop the upper bound entirely —
    // there's nothing greater than the prefix in lex order, so
    // `key >= prefix` alone covers exactly the matching rows.
    let upper = next_prefix(&prefix_bytes);

    let rows: Vec<Vec<u8>> = if let Some(upper) = upper {
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT key FROM \"{table}\" WHERE key >= ?1 AND key < ?2"
        ))?;
        let mapped = stmt.query_map(rusqlite::params![&prefix_bytes, &upper], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt =
            conn.prepare_cached(&format!("SELECT key FROM \"{table}\" WHERE key >= ?1"))?;
        let mapped = stmt.query_map(rusqlite::params![&prefix_bytes], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut out = Vec::new();
    for key in rows {
        if key.len() < 8 {
            continue;
        }
        let id_bytes: [u8; 8] = key[key.len() - 8..].try_into().unwrap();
        out.push(NodeId::from_be_bytes(id_bytes));
    }
    Ok(out)
}

/// Compute the lex-next byte string after `prefix` for an exclusive
/// range-scan upper bound. Returns `None` when `prefix` is all-`0xFF`
/// (no lex successor exists); callers must then omit the upper bound
/// from the range query entirely. Any fixed-length sentinel would be
/// wrong — composite keys can be arbitrarily long, so e.g.
/// `[0xFF, 0xFF, 0x00]` is smaller than a real key
/// `[0xFF, 0xFF, 0x01, ...]`, which would silently drop matches.
fn next_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    for i in (0..out.len()).rev() {
        if out[i] < 0xFF {
            out[i] += 1;
            out.truncate(i + 1);
            return Some(out);
        }
    }
    None
}

#[cfg(test)]
mod next_prefix_tests {
    use super::next_prefix;

    #[test]
    fn increments_last_byte_when_under_ff() {
        assert_eq!(next_prefix(&[0x01]), Some(vec![0x02]));
        assert_eq!(next_prefix(&[0x00]), Some(vec![0x01]));
    }

    #[test]
    fn carries_through_trailing_ff_bytes() {
        assert_eq!(next_prefix(&[0x01, 0xFF]), Some(vec![0x02]));
        assert_eq!(next_prefix(&[0x01, 0xFF, 0xFF]), Some(vec![0x02]));
    }

    #[test]
    fn all_ff_returns_none() {
        assert_eq!(next_prefix(&[0xFF]), None);
        assert_eq!(next_prefix(&[0xFF, 0xFF, 0xFF]), None);
    }

    #[test]
    fn empty_returns_none() {
        // An empty prefix has no successor; callers shouldn't ask, but
        // we shouldn't panic either.
        assert_eq!(next_prefix(&[]), None);
    }
}

/// Return `Some(values)` if every property in `properties` has a value in
/// `props`; `None` if any is missing.
fn collect_values(props: &Properties, properties: &[String]) -> Option<Vec<Value>> {
    let mut out = Vec::with_capacity(properties.len());
    for p in properties {
        out.push(props.get(p)?.clone());
    }
    Some(out)
}

/// Update indexes after a node is created or its properties change.
/// Call with old_properties = None for new nodes.
pub fn update_indexes_for_node(
    conn: &Connection,
    node_id: NodeId,
    label: &str,
    old_properties: Option<&Properties>,
    new_properties: &Properties,
) -> Result<()> {
    for info in list_indexes_for_label(conn, label)? {
        let props_refs: Vec<&str> = info.properties.iter().map(String::as_str).collect();
        let table = composite_index_table_name(label, &props_refs)?;

        // Remove old entry, if any.
        if let Some(old_props) = old_properties {
            if let Some(old_values) = collect_values(old_props, &info.properties) {
                let old_key = composite_index_key(&old_values, node_id)?;
                kv::delete(conn, &table, &old_key)?;
            }
        }
        // Insert new entry if all covered properties are present.
        if let Some(new_values) = collect_values(new_properties, &info.properties) {
            let new_key = composite_index_key(&new_values, node_id)?;
            kv::put(conn, &table, &new_key, &[])?;
        }
    }
    Ok(())
}

/// Remove all index entries for a node being deleted.
pub fn remove_indexes_for_node(
    conn: &Connection,
    node_id: NodeId,
    label: &str,
    properties: &Properties,
) -> Result<()> {
    for info in list_indexes_for_label(conn, label)? {
        let props_refs: Vec<&str> = info.properties.iter().map(String::as_str).collect();
        let table = composite_index_table_name(label, &props_refs)?;
        if let Some(values) = collect_values(properties, &info.properties) {
            let key = composite_index_key(&values, node_id)?;
            kv::delete(conn, &table, &key)?;
        }
    }
    Ok(())
}

/// List all indexes that exist for a given label.
///
/// Returns an empty vec for labels that don't satisfy `validate_name`
/// (rather than erroring): the planner calls this for every label that
/// appears in a Cypher query, including ones that can never have been
/// indexed. Errors here would break read paths for any label outside
/// the validate_name charset. Safety: `label` is only used to build a
/// `LIKE` pattern bound as a `?` parameter, so there's no SQL
/// injection surface.
pub fn list_indexes_for_label(conn: &Connection, label: &str) -> Result<Vec<IndexInfo>> {
    if validate_name(label).is_err() {
        return Ok(Vec::new());
    }
    let single_prefix = format!("node_idx_{label}_");
    let composite_prefix = format!("node_idx${label}$");

    let mut stmt = conn.prepare_cached(
        "SELECT name FROM sqlite_master \
         WHERE type='table' \
           AND (name LIKE ?1 OR name LIKE ?2)",
    )?;
    let rows = stmt.query_map(
        [format!("{single_prefix}%"), format!("{composite_prefix}%")],
        |row| row.get::<_, String>(0),
    )?;

    let mut result = Vec::new();
    for name in rows {
        let name = name?;
        if let Some(props) = parse_composite_table_name(&name, label) {
            result.push(IndexInfo {
                label: label.to_string(),
                properties: props,
                kind: "btree",
            });
        } else if let Some(property) = name.strip_prefix(&single_prefix) {
            result.push(IndexInfo {
                label: label.to_string(),
                properties: vec![property.to_string()],
                kind: "btree",
            });
        }
    }
    Ok(result)
}

/// List every secondary index in the database.
///
/// Order is unspecified — callers should sort if they need determinism.
///
/// Note: the single-prop table name is `node_idx_<label>_<property>`.
/// Both labels and properties may contain underscores
/// (`validate_name` allows `[A-Za-z0-9_]`), so the split between label
/// and property is ambiguous in principle. We split on the **first**
/// underscore after the `node_idx_` prefix, which round-trips correctly
/// when labels do not contain underscores (the common case). Labels
/// with underscores will be misparsed by this label-agnostic listing;
/// callers needing exact label scoping should use
/// `list_indexes_for_label`.
pub fn list_all_indexes(conn: &Connection) -> Result<Vec<IndexInfo>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name FROM sqlite_master \
         WHERE type='table' \
           AND (name LIKE 'node_idx\\_%' ESCAPE '\\' OR name LIKE 'node_idx$%')",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

    let mut result = Vec::new();
    for name in rows {
        let name = name?;
        if let Some(rest) = name.strip_prefix("node_idx$") {
            // Composite: "node_idx$<label>$<prop1>$<prop2>$..."
            let parts: Vec<&str> = rest.split('$').collect();
            if parts.len() < 3 {
                continue; // malformed (need at least label + 2 props)
            }
            let label = parts[0].to_string();
            let properties: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
            result.push(IndexInfo {
                label,
                properties,
                kind: "btree",
            });
        } else if let Some(rest) = name.strip_prefix("node_idx_") {
            // Single-prop legacy: split on first underscore (existing behavior).
            let Some(split) = rest.find('_') else {
                continue;
            };
            let (label, property) = rest.split_at(split);
            result.push(IndexInfo {
                label: label.to_string(),
                properties: vec![property[1..].to_string()],
                kind: "btree",
            });
        }
    }
    Ok(result)
}

/// Parse `node_idx$<label>$<prop1>$…$<propN>` and return the property list
/// when the label matches; `None` for non-composite or wrong-label names.
fn parse_composite_table_name(name: &str, expected_label: &str) -> Option<Vec<String>> {
    let rest = name.strip_prefix("node_idx$")?;
    let prefix = format!("{expected_label}$");
    let after_label = rest.strip_prefix(&prefix)?;
    let props: Vec<String> = after_label.split('$').map(|s| s.to_string()).collect();
    if props.len() < 2 {
        return None;
    }
    Some(props)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&c).unwrap();
        c
    }

    #[test]
    fn list_all_indexes_returns_empty_on_fresh_db() {
        let conn = fresh_conn();
        let got = list_all_indexes(&conn).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn list_all_indexes_returns_one_per_index_across_labels() {
        let conn = fresh_conn();
        create_index(&conn, "Person", "name").unwrap();
        create_index(&conn, "Person", "age").unwrap();
        create_index(&conn, "City", "name").unwrap();
        let mut got = list_all_indexes(&conn).unwrap();
        got.sort_by(|a, b| a.label.cmp(&b.label).then(a.properties.cmp(&b.properties)));
        assert_eq!(
            got,
            vec![
                IndexInfo {
                    label: "City".to_string(),
                    properties: vec!["name".to_string()],
                    kind: "btree"
                },
                IndexInfo {
                    label: "Person".to_string(),
                    properties: vec!["age".to_string()],
                    kind: "btree"
                },
                IndexInfo {
                    label: "Person".to_string(),
                    properties: vec!["name".to_string()],
                    kind: "btree"
                },
            ]
        );
    }

    #[test]
    fn list_helpers_return_index_info_with_properties() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        // Use the legacy single-prop create for now; composite create lands in Task 4.
        create_index(&conn, "Person", "name").unwrap();
        create_index(&conn, "Person", "age").unwrap();

        let label_scoped = list_indexes_for_label(&conn, "Person").unwrap();
        let mut sorted: Vec<_> = label_scoped.into_iter().collect();
        sorted.sort_by(|a, b| a.properties.cmp(&b.properties));
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted[0].label, "Person");
        assert_eq!(sorted[0].properties, vec!["age".to_string()]);
        assert_eq!(sorted[0].kind, "btree");
        assert_eq!(sorted[1].properties, vec!["name".to_string()]);

        let all = list_all_indexes(&conn).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|i| i.kind == "btree"));
    }

    #[test]
    fn composite_table_name_uses_dollar_separator() {
        // Single-prop preserves legacy naming.
        assert_eq!(
            composite_index_table_name("Person", &["name"]).unwrap(),
            "node_idx_Person_name"
        );
        // Multi-prop uses $-separated form.
        assert_eq!(
            composite_index_table_name("Person", &["tenant_id", "external_id"]).unwrap(),
            "node_idx$Person$tenant_id$external_id"
        );
        assert_eq!(
            composite_index_table_name("Person", &["a", "b", "c"]).unwrap(),
            "node_idx$Person$a$b$c"
        );
    }

    #[test]
    fn composite_table_name_rejects_empty_property_list() {
        let err = composite_index_table_name("Person", &[]).unwrap_err();
        assert!(matches!(err, GraphError::InvalidIndexDefinition { .. }));
    }

    #[test]
    fn composite_table_name_rejects_duplicates() {
        let err = composite_index_table_name("Person", &["a", "b", "a"]).unwrap_err();
        match err {
            GraphError::InvalidIndexDefinition { reason, .. } => {
                assert!(reason.contains("duplicate"), "got reason: {reason}");
            }
            other => panic!("expected InvalidIndexDefinition, got {other:?}"),
        }
    }

    #[test]
    fn composite_table_name_validates_each_name() {
        // bad label
        assert!(composite_index_table_name("bad-label", &["a"]).is_err());
        // bad property
        assert!(composite_index_table_name("Person", &["bad-prop"]).is_err());
    }

    #[test]
    fn index_key_n_round_trip() {
        use crate::types::{NodeId, Value};
        let vals = vec![Value::I64(7), Value::String("alice".into())];
        let key = composite_index_key(&vals, NodeId(42)).unwrap();
        // Trailing 8 bytes are the node id big-endian.
        let id_bytes: [u8; 8] = key[key.len() - 8..].try_into().unwrap();
        assert_eq!(u64::from_be_bytes(id_bytes), 42);
    }

    #[test]
    fn duplicate_create_returns_index_already_exists_with_property_list() {
        let conn = fresh_conn();
        create_index(&conn, "Person", "name").unwrap();
        let err = create_index(&conn, "Person", "name").unwrap_err();
        match err {
            GraphError::IndexAlreadyExists {
                label, properties, ..
            } => {
                assert_eq!(label, "Person");
                assert_eq!(properties, vec!["name".to_string()]);
            }
            other => panic!("expected IndexAlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn create_composite_index_round_trip() {
        let conn = fresh_conn();
        create_composite_index(&conn, "Person", &["tenant_id", "external_id"]).unwrap();

        let indexes = list_indexes_for_label(&conn, "Person").unwrap();
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].properties, vec!["tenant_id", "external_id"]);
    }

    #[test]
    fn create_composite_rejects_exact_duplicate() {
        let conn = fresh_conn();
        create_composite_index(&conn, "Person", &["a", "b"]).unwrap();
        let err = create_composite_index(&conn, "Person", &["a", "b"]).unwrap_err();
        assert!(matches!(err, GraphError::IndexAlreadyExists { .. }));
    }

    #[test]
    fn create_composite_allows_different_column_order() {
        let conn = fresh_conn();
        create_composite_index(&conn, "Person", &["a", "b"]).unwrap();
        // Different order → different index → allowed.
        create_composite_index(&conn, "Person", &["b", "a"]).unwrap();
        assert_eq!(list_indexes_for_label(&conn, "Person").unwrap().len(), 2);
    }

    #[test]
    fn create_composite_n1_collides_with_legacy_create_index() {
        let conn = fresh_conn();
        create_index(&conn, "Person", "name").unwrap();
        // N=1 composite resolves to same table → duplicate error.
        let err = create_composite_index(&conn, "Person", &["name"]).unwrap_err();
        assert!(matches!(err, GraphError::IndexAlreadyExists { .. }));
    }

    #[test]
    fn drop_composite_index_round_trip() {
        let conn = fresh_conn();
        create_composite_index(&conn, "Person", &["a", "b", "c"]).unwrap();
        drop_composite_index(&conn, "Person", &["a", "b", "c"]).unwrap();
        assert!(list_indexes_for_label(&conn, "Person").unwrap().is_empty());
    }

    #[test]
    fn drop_composite_wrong_order_errors() {
        let conn = fresh_conn();
        create_composite_index(&conn, "Person", &["a", "b"]).unwrap();
        let err = drop_composite_index(&conn, "Person", &["b", "a"]).unwrap_err();
        assert!(matches!(err, GraphError::IndexNotFound { .. }));
    }

    #[test]
    fn update_indexes_writes_composite_entry() {
        use crate::types::{Properties, Value};
        let conn = fresh_conn();
        create_composite_index(&conn, "Person", &["a", "b"]).unwrap();

        let props = Properties::from_iter([
            ("a".to_string(), Value::I64(1)),
            ("b".to_string(), Value::String("x".into())),
        ]);
        let id = node::create_node(&conn, &["Person".to_string()], props.clone()).unwrap();
        update_indexes_for_node(&conn, id, "Person", None, &props).unwrap();

        let table = composite_index_table_name("Person", &["a", "b"]).unwrap();
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 1,
            "update_indexes_for_node should populate composite index for id={id:?}"
        );
    }

    #[test]
    fn update_indexes_skips_when_any_prop_missing() {
        use crate::types::{Properties, Value};
        let conn = fresh_conn();
        create_composite_index(&conn, "Person", &["a", "b"]).unwrap();

        let props = Properties::from_iter([("a".to_string(), Value::I64(1))]);
        let id = node::create_node(&conn, &["Person".to_string()], props.clone()).unwrap();
        update_indexes_for_node(&conn, id, "Person", None, &props).unwrap();

        let table = composite_index_table_name("Person", &["a", "b"]).unwrap();
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn remove_indexes_clears_composite_entry() {
        use crate::types::{Properties, Value};
        let conn = fresh_conn();
        create_composite_index(&conn, "Person", &["a", "b"]).unwrap();

        let props = Properties::from_iter([
            ("a".to_string(), Value::I64(1)),
            ("b".to_string(), Value::String("x".into())),
        ]);
        let id = node::create_node(&conn, &["Person".to_string()], props.clone()).unwrap();
        update_indexes_for_node(&conn, id, "Person", None, &props).unwrap();
        remove_indexes_for_node(&conn, id, "Person", &props).unwrap();

        let table = composite_index_table_name("Person", &["a", "b"]).unwrap();
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn composite_index_backfills_existing_nodes_with_all_props() {
        use crate::types::{Properties, Value};
        let conn = fresh_conn();
        node::create_node(
            &conn,
            &[String::from("Person")],
            Properties::from_iter([
                ("tenant_id".to_string(), Value::I64(1)),
                ("external_id".to_string(), Value::String("alice".into())),
            ]),
        )
        .unwrap();
        node::create_node(
            &conn,
            &[String::from("Person")],
            Properties::from_iter([
                ("tenant_id".to_string(), Value::I64(1)),
                // missing external_id — should NOT appear in the composite.
            ]),
        )
        .unwrap();

        create_composite_index(&conn, "Person", &["tenant_id", "external_id"]).unwrap();

        let table = composite_index_table_name("Person", &["tenant_id", "external_id"]).unwrap();
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1, "only the fully-propertied node should be indexed");
    }

    #[test]
    fn composite_prefix_scan_returns_matching_node_ids() {
        use crate::types::{Properties, Value};
        let conn = fresh_conn();
        create_composite_index(&conn, "Person", &["a", "b"]).unwrap();

        let id1 = node::create_node(
            &conn,
            &["Person".to_string()],
            Properties::from_iter([
                ("a".to_string(), Value::I64(1)),
                ("b".to_string(), Value::String("x".into())),
            ]),
        )
        .unwrap();
        let id2 = node::create_node(
            &conn,
            &["Person".to_string()],
            Properties::from_iter([
                ("a".to_string(), Value::I64(2)),
                ("b".to_string(), Value::String("x".into())),
            ]),
        )
        .unwrap();
        let id3 = node::create_node(
            &conn,
            &["Person".to_string()],
            Properties::from_iter([
                ("a".to_string(), Value::I64(1)),
                ("b".to_string(), Value::String("y".into())),
            ]),
        )
        .unwrap();

        // Backfill composite index for each node.
        for id in [id1, id2, id3] {
            let n = node::get_node(&conn, id).unwrap();
            update_indexes_for_node(&conn, id, &n.labels[0], None, &n.properties).unwrap();
        }

        // Prefix match on a=1 → id1 and id3.
        let mut ids =
            composite_index_prefix_lookup(&conn, "Person", &["a", "b"], &[Value::I64(1)]).unwrap();
        ids.sort_by_key(|id| id.0);
        let mut expected = vec![id1, id3];
        expected.sort_by_key(|id| id.0);
        assert_eq!(ids, expected, "prefix a=1 should return 2 nodes");

        // Full match on a=1, b="x" → id1 only.
        let ids = composite_index_prefix_lookup(
            &conn,
            "Person",
            &["a", "b"],
            &[Value::I64(1), Value::String("x".into())],
        )
        .unwrap();
        assert_eq!(ids, vec![id1]);
    }
}
