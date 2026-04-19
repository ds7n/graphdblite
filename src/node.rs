use rusqlite::Connection;

use crate::edge;
use crate::id::next_node_id;
use crate::stats;
use crate::storage::kv;
use crate::types::{
    validate_name, Direction, GraphError, Node, NodeId, NodeRecord, Properties, Result, Value,
};

/// Create a new node with the given labels and properties.
///
/// An empty `labels` slice creates an unlabeled node (valid in openCypher).
pub fn create_node(conn: &Connection, labels: &[String], properties: Properties) -> Result<NodeId> {
    for label in labels {
        validate_name(label)?;
    }
    for key in properties.keys() {
        validate_name(key)?;
    }
    let id = next_node_id(conn)?;
    let mut sorted_labels = labels.to_vec();
    sorted_labels.sort();
    let record = NodeRecord {
        labels: sorted_labels.clone(),
        properties,
    };
    let data = rmp_serde::to_vec(&record).map_err(|e| GraphError::Serialization(e.to_string()))?;
    let label_col = sorted_labels.join(":");
    put_node(conn, &id.to_be_bytes(), &label_col, &data)?;
    for label in &sorted_labels {
        stats::increment_label_count(conn, label)?;
    }
    Ok(id)
}

/// Get a node by ID.
pub fn get_node(conn: &Connection, id: NodeId) -> Result<Node> {
    let data =
        kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?.ok_or(GraphError::NodeNotFound(id))?;
    let record: NodeRecord =
        rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization(e.to_string()))?;
    Ok(Node {
        id,
        labels: record.labels,
        properties: record.properties,
    })
}

/// Check if a node exists (without fetching the full BLOB).
pub fn node_exists(conn: &Connection, id: NodeId) -> Result<bool> {
    let mut stmt = conn.prepare_cached("SELECT 1 FROM nodes WHERE key = ?1 LIMIT 1")?;
    let exists = stmt.query_row([&id.to_be_bytes()[..]], |_| Ok(())).is_ok();
    Ok(exists)
}

/// Check if a node has any edges (incoming or outgoing).
pub fn node_has_edges(conn: &Connection, id: NodeId) -> Result<bool> {
    let out = edge::get_all_edge_labels(conn, id, Direction::Outgoing)?;
    if !out.is_empty() {
        return Ok(true);
    }
    let inc = edge::get_all_edge_labels(conn, id, Direction::Incoming)?;
    Ok(!inc.is_empty())
}

/// Delete a node and all its edges (cascading).
pub fn delete_node(conn: &Connection, id: NodeId) -> Result<()> {
    // Read the node to get its label for stats tracking.
    let node = get_node(conn, id)?;

    // Delete all outgoing edges.
    let out_edges = edge::get_all_edge_labels(conn, id, Direction::Outgoing)?;
    for (label, neighbors) in &out_edges {
        for &dst in neighbors {
            edge::delete_edge(conn, id, NodeId(dst), label)?;
        }
    }

    // Delete all incoming edges.
    let in_edges = edge::get_all_edge_labels(conn, id, Direction::Incoming)?;
    for (label, neighbors) in &in_edges {
        for &src in neighbors {
            edge::delete_edge(conn, NodeId(src), id, label)?;
        }
    }

    // Delete the node record.
    kv::delete(conn, kv::TABLE_NODES, &id.to_be_bytes())?;
    for label in &node.labels {
        stats::decrement_label_count(conn, label)?;
    }
    Ok(())
}

/// Set a property on an existing node (read-modify-write).
pub fn set_node_property(conn: &Connection, id: NodeId, key: &str, value: Value) -> Result<()> {
    validate_name(key)?;
    let data =
        kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?.ok_or(GraphError::NodeNotFound(id))?;
    let mut record: NodeRecord =
        rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization(e.to_string()))?;
    record.properties.insert(key.to_string(), value);
    let new_data =
        rmp_serde::to_vec(&record).map_err(|e| GraphError::Serialization(e.to_string()))?;
    let label_col = record.labels.join(":");
    put_node(conn, &id.to_be_bytes(), &label_col, &new_data)?;
    Ok(())
}

/// Remove a property from an existing node.
pub fn remove_node_property(conn: &Connection, id: NodeId, key: &str) -> Result<()> {
    let data =
        kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?.ok_or(GraphError::NodeNotFound(id))?;
    let mut record: NodeRecord =
        rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization(e.to_string()))?;
    record.properties.remove(key);
    let new_data =
        rmp_serde::to_vec(&record).map_err(|e| GraphError::Serialization(e.to_string()))?;
    let label_col = record.labels.join(":");
    put_node(conn, &id.to_be_bytes(), &label_col, &new_data)?;
    Ok(())
}

/// Remove a label from an existing node.
pub fn remove_node_label(conn: &Connection, id: NodeId, label: &str) -> Result<()> {
    let data =
        kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?.ok_or(GraphError::NodeNotFound(id))?;
    let mut record: NodeRecord =
        rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization(e.to_string()))?;
    if !record.labels.contains(&label.to_string()) {
        return Ok(());
    }
    record.labels.retain(|l| l != label);
    let new_data =
        rmp_serde::to_vec(&record).map_err(|e| GraphError::Serialization(e.to_string()))?;
    let label_col = record.labels.join(":");
    put_node(conn, &id.to_be_bytes(), &label_col, &new_data)?;
    stats::decrement_label_count(conn, label)?;
    Ok(())
}

/// Add a label to an existing node. No-op if already present.
pub fn add_node_label(conn: &Connection, id: NodeId, label: &str) -> Result<()> {
    let data =
        kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?.ok_or(GraphError::NodeNotFound(id))?;
    let mut record: NodeRecord =
        rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization(e.to_string()))?;
    if record.labels.contains(&label.to_string()) {
        return Ok(());
    }
    record.labels.push(label.to_string());
    record.labels.sort();
    let new_data =
        rmp_serde::to_vec(&record).map_err(|e| GraphError::Serialization(e.to_string()))?;
    let label_col = record.labels.join(":");
    put_node(conn, &id.to_be_bytes(), &label_col, &new_data)?;
    stats::increment_label_count(conn, label)?;
    Ok(())
}

/// Replace all properties on an existing node.
pub fn set_all_node_properties(
    conn: &Connection,
    id: NodeId,
    properties: Properties,
) -> Result<()> {
    let data =
        kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?.ok_or(GraphError::NodeNotFound(id))?;
    let mut record: NodeRecord =
        rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization(e.to_string()))?;
    record.properties = properties;
    let new_data =
        rmp_serde::to_vec(&record).map_err(|e| GraphError::Serialization(e.to_string()))?;
    let label_col = record.labels.join(":");
    put_node(conn, &id.to_be_bytes(), &label_col, &new_data)?;
    Ok(())
}

/// Scan nodes by label using the indexed label column.
///
/// When `label` is empty, returns all nodes.
pub fn find_nodes_by_label(conn: &Connection, label: &str) -> Result<Vec<Node>> {
    let (sql, use_param) = if label.is_empty() {
        (
            "SELECT key, value FROM nodes ORDER BY key".to_string(),
            false,
        )
    } else {
        (
            "SELECT key, value FROM nodes WHERE label = ?1 OR ':' || label || ':' LIKE '%:' || ?1 || ':%' ORDER BY key".to_string(),
            true,
        )
    };

    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = if use_param {
        stmt.query_map(rusqlite::params![label], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        stmt.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };

    let mut nodes = Vec::new();
    for (key, data) in rows {
        let record: NodeRecord =
            rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization(e.to_string()))?;
        let id = NodeId::from_be_bytes(
            key.get(..8)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(|| GraphError::Serialization("corrupt node key bytes".into()))?,
        );
        nodes.push(Node {
            id,
            labels: record.labels,
            properties: record.properties,
        });
    }
    Ok(nodes)
}

/// Insert or replace a node row in the v2 schema (key + label + value).
fn put_node(conn: &Connection, key: &[u8], label: &str, value: &[u8]) -> Result<()> {
    let mut stmt = conn
        .prepare_cached("INSERT OR REPLACE INTO nodes (key, label, value) VALUES (?1, ?2, ?3)")?;
    stmt.execute(rusqlite::params![key, label, value])?;
    Ok(())
}
