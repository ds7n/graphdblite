use rusqlite::Connection;

use crate::edge;
use crate::id::next_node_id;
use crate::stats;
use crate::storage::kv;
use crate::types::{
    validate_name, Direction, GraphError, Node, NodeId, NodeRecord, Properties, Result, Value,
};

/// Create a new node with the given label and properties.
pub fn create_node(
    conn: &Connection,
    label: &str,
    properties: Properties,
) -> Result<NodeId> {
    validate_name(label)?;
    for key in properties.keys() {
        validate_name(key)?;
    }
    let id = next_node_id(conn)?;
    let record = NodeRecord {
        label: label.to_string(),
        properties,
    };
    let data = rmp_serde::to_vec(&record)
        .map_err(|e| GraphError::Serialization(e.to_string()))?;
    kv::put(conn, kv::TABLE_NODES, &id.to_be_bytes(), &data)?;
    stats::increment_label_count(conn, label)?;
    Ok(id)
}

/// Get a node by ID.
pub fn get_node(conn: &Connection, id: NodeId) -> Result<Node> {
    let data = kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?
        .ok_or(GraphError::NodeNotFound(id))?;
    let record: NodeRecord = rmp_serde::from_slice(&data)
        .map_err(|e| GraphError::Serialization(e.to_string()))?;
    Ok(Node {
        id,
        label: record.label,
        properties: record.properties,
    })
}

/// Check if a node exists.
pub fn node_exists(conn: &Connection, id: NodeId) -> Result<bool> {
    Ok(kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?.is_some())
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
    stats::decrement_label_count(conn, &node.label)?;
    Ok(())
}

/// Set a property on an existing node (read-modify-write).
pub fn set_node_property(
    conn: &Connection,
    id: NodeId,
    key: &str,
    value: Value,
) -> Result<()> {
    validate_name(key)?;
    let data = kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?
        .ok_or(GraphError::NodeNotFound(id))?;
    let mut record: NodeRecord = rmp_serde::from_slice(&data)
        .map_err(|e| GraphError::Serialization(e.to_string()))?;
    record.properties.insert(key.to_string(), value);
    let new_data = rmp_serde::to_vec(&record)
        .map_err(|e| GraphError::Serialization(e.to_string()))?;
    kv::put(conn, kv::TABLE_NODES, &id.to_be_bytes(), &new_data)?;
    Ok(())
}

/// Remove a property from an existing node.
pub fn remove_node_property(
    conn: &Connection,
    id: NodeId,
    key: &str,
) -> Result<()> {
    let data = kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?
        .ok_or(GraphError::NodeNotFound(id))?;
    let mut record: NodeRecord = rmp_serde::from_slice(&data)
        .map_err(|e| GraphError::Serialization(e.to_string()))?;
    record.properties.remove(key);
    let new_data = rmp_serde::to_vec(&record)
        .map_err(|e| GraphError::Serialization(e.to_string()))?;
    kv::put(conn, kv::TABLE_NODES, &id.to_be_bytes(), &new_data)?;
    Ok(())
}

/// Scan all nodes with a given label.
pub fn find_nodes_by_label(
    conn: &Connection,
    label: &str,
) -> Result<Vec<Node>> {
    // Full scan of nodes table — filter by label after deserialization.
    // For indexed lookups, use index::index_lookup instead.
    let mut stmt = conn.prepare_cached(
        "SELECT key, value FROM nodes ORDER BY key",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;

    let mut nodes = Vec::new();
    for row in rows {
        let (key, data) = row?;
        let record: NodeRecord = rmp_serde::from_slice(&data)
            .map_err(|e| GraphError::Serialization(e.to_string()))?;
        if label.is_empty() || record.label == label {
            let id = NodeId::from_be_bytes(
                key.get(..8)
                    .and_then(|s| s.try_into().ok())
                    .ok_or_else(|| GraphError::Serialization("corrupt node key bytes".into()))?,
            );
            nodes.push(Node {
                id,
                label: record.label,
                properties: record.properties,
            });
        }
    }
    Ok(nodes)
}
