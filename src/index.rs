use rusqlite::Connection;

use crate::node;
use crate::storage::kv;
use crate::types::{validate_name, GraphError, NodeId, Properties, Result, Value};

/// Build the index table name for a (label, property) pair.
fn index_table_name(label: &str, property: &str) -> String {
    format!("node_idx_{label}_{property}")
}

/// Build the index key: [msgpack(value)][node_id: 8 bytes BE].
fn index_key(value: &Value, node_id: NodeId) -> Result<Vec<u8>> {
    let mut key = rmp_serde::to_vec(value)
        .map_err(|e| GraphError::Serialization(e.to_string()))?;
    key.extend_from_slice(&node_id.to_be_bytes());
    Ok(key)
}

/// Create a secondary index on a (label, property) pair.
/// Backfills the index with all existing matching nodes.
pub fn create_index(
    conn: &Connection,
    label: &str,
    property: &str,
) -> Result<()> {
    validate_name(label)?;
    validate_name(property)?;
    let table = index_table_name(label, property);

    // Check if index table already exists.
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        [&table],
        |row| row.get(0),
    )?;
    if exists {
        return Err(GraphError::IndexAlreadyExists(
            label.to_string(),
            property.to_string(),
        ));
    }

    // Create the index table (key-only, empty value).
    conn.execute(
        &format!(
            "CREATE TABLE \"{table}\" (key BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID"
        ),
        [],
    )?;

    // Backfill from existing nodes.
    let nodes = node::find_nodes_by_label(conn, label)?;
    for n in &nodes {
        if let Some(val) = n.properties.get(property) {
            let key = index_key(val, n.id)?;
            kv::put(conn, &table, &key, &[])?;
        }
    }

    Ok(())
}

/// Drop a secondary index.
pub fn drop_index(
    conn: &Connection,
    label: &str,
    property: &str,
) -> Result<()> {
    let table = index_table_name(label, property);
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        [&table],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(GraphError::IndexNotFound(
            label.to_string(),
            property.to_string(),
        ));
    }
    conn.execute(&format!("DROP TABLE \"{table}\""), [])?;
    Ok(())
}

/// Lookup nodes by indexed property value.
pub fn index_lookup(
    conn: &Connection,
    label: &str,
    property: &str,
    value: &Value,
) -> Result<Vec<NodeId>> {
    let table = index_table_name(label, property);

    // Build prefix from the serialized value.
    let prefix = rmp_serde::to_vec(value)
        .map_err(|e| GraphError::Serialization(e.to_string()))?;
    let entries = match kv::scan_prefix(conn, &table, &prefix) {
        Ok(e) => e,
        Err(GraphError::Storage(ref e)) if e.to_string().contains("no such table") => {
            return Err(GraphError::IndexNotFound(label.to_string(), property.to_string()));
        }
        Err(e) => return Err(e),
    };

    let mut ids = Vec::new();
    for (key, _) in entries {
        // Key = [msgpack(value)][node_id: 8 BE]
        // Extract the last 8 bytes as node_id.
        if key.len() >= 8 {
            let id_bytes: [u8; 8] = key[key.len() - 8..]
                .try_into()
                .map_err(|_| GraphError::Serialization("corrupt index key bytes".into()))?;
            ids.push(NodeId::from_be_bytes(id_bytes));
        }
    }
    Ok(ids)
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
    // Find all index tables for this label.
    let tables = list_indexes_for_label(conn, label)?;

    for (_, property) in &tables {
        let table = index_table_name(label, property);
        let old_val = old_properties.and_then(|p| p.get(property.as_str()));
        let new_val = new_properties.get(property.as_str());

        // Remove old index entry if value changed or was removed.
        if let Some(old) = old_val {
            if new_val != Some(old) {
                let key = index_key(old, node_id)?;
                kv::delete(conn, &table, &key)?;
            }
        }

        // Add new index entry if value exists and changed.
        if let Some(new) = new_val {
            if old_val != Some(new) {
                let key = index_key(new, node_id)?;
                kv::put(conn, &table, &key, &[])?;
            }
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
    let tables = list_indexes_for_label(conn, label)?;
    for (_, property) in &tables {
        if let Some(val) = properties.get(property.as_str()) {
            let table = index_table_name(label, &property);
            let key = index_key(val, node_id)?;
            kv::delete(conn, &table, &key)?;
        }
    }
    Ok(())
}

/// Count the number of index entries matching a given value.
///
/// Used by the planner to pick the most selective index when multiple
/// indexed properties are available.
pub fn index_count_for_value(
    conn: &Connection,
    label: &str,
    property: &str,
    value: &Value,
) -> Result<usize> {
    let table = index_table_name(label, property);
    let prefix = rmp_serde::to_vec(value)
        .map_err(|e| GraphError::Serialization(e.to_string()))?;
    let entries = match kv::scan_prefix(conn, &table, &prefix) {
        Ok(e) => e,
        Err(GraphError::Storage(ref e)) if e.to_string().contains("no such table") => {
            return Ok(0);
        }
        Err(e) => return Err(e),
    };
    Ok(entries.len())
}

/// List all indexes that exist for a given label.
/// Returns Vec<(label, property)>.
pub fn list_indexes_for_label(
    conn: &Connection,
    label: &str,
) -> Result<Vec<(String, String)>> {
    let prefix = format!("node_idx_{label}_");
    let mut stmt = conn.prepare_cached(
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE ?1",
    )?;
    let rows = stmt.query_map([format!("{prefix}%")], |row| {
        row.get::<_, String>(0)
    })?;

    let mut result = Vec::new();
    for name in rows {
        let name = name?;
        if let Some(property) = name.strip_prefix(&prefix) {
            result.push((label.to_string(), property.to_string()));
        }
    }
    Ok(result)
}
