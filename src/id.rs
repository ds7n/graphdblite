use rusqlite::Connection;

use crate::types::{GraphError, NodeId, Result};

/// Allocate the next node ID. Must be called within a write transaction.
///
/// Uses compare-and-swap to detect concurrent-writer conflicts under WAL.
pub fn next_node_id(conn: &Connection) -> Result<NodeId> {
    let mut stmt = conn.prepare_cached("SELECT value FROM metadata WHERE key = 'next_node_id'")?;
    let raw: Vec<u8> = stmt.query_row([], |row| row.get(0))?;
    let current = u64::from_be_bytes(raw.get(..8).and_then(|s| s.try_into().ok()).ok_or_else(
        || GraphError::Serialization {
            context: String::new(),
            source: "corrupt node ID bytes".into(),
            hint: None,
        },
    )?);

    let next = current
        .checked_add(1)
        .ok_or_else(|| GraphError::Transaction {
            message: "node ID space exhausted".into(),
            hint: None,
        })?;

    // Compare-and-swap: only update if the current value hasn't changed (H6).
    let rows = conn.execute(
        "UPDATE metadata SET value = ?1 WHERE key = 'next_node_id' AND value = ?2",
        rusqlite::params![&next.to_be_bytes()[..], &raw],
    )?;
    if rows == 0 {
        return Err(GraphError::Transaction {
            message: "node ID conflict — concurrent writer".into(),
            hint: None,
        });
    }

    Ok(NodeId(current))
}

/// Allocate the next global edge sequence number. Must be called within a
/// write transaction.
///
/// Uses a monotonically increasing counter so edge storage keys are never
/// recycled, even after deletion and re-creation of an edge between the same
/// endpoints.
pub fn next_edge_seq(conn: &Connection) -> Result<u64> {
    let mut stmt = conn.prepare_cached("SELECT value FROM metadata WHERE key = 'next_edge_seq'")?;
    let raw: Vec<u8> = match stmt.query_row([], |row| row.get(0)) {
        Ok(v) => v,
        Err(_) => {
            // Counter missing (pre-existing DB) — initialize from 0.
            conn.execute(
                "INSERT INTO metadata (key, value) VALUES ('next_edge_seq', ?1)",
                [&0u64.to_be_bytes()[..]],
            )?;
            0u64.to_be_bytes().to_vec()
        }
    };
    let current = u64::from_be_bytes(raw.get(..8).and_then(|s| s.try_into().ok()).ok_or_else(
        || GraphError::Serialization {
            context: String::new(),
            source: "corrupt edge seq bytes".into(),
            hint: None,
        },
    )?);

    let next = current
        .checked_add(1)
        .ok_or_else(|| GraphError::Transaction {
            message: "edge sequence space exhausted".into(),
            hint: None,
        })?;

    conn.execute(
        "UPDATE metadata SET value = ?1 WHERE key = 'next_edge_seq' AND value = ?2",
        rusqlite::params![&next.to_be_bytes()[..], &raw],
    )?;

    Ok(current)
}
