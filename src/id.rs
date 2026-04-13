use rusqlite::Connection;

use crate::types::{GraphError, NodeId, Result};

/// Allocate the next node ID. Must be called within a write transaction.
///
/// Uses compare-and-swap to detect concurrent-writer conflicts under WAL.
pub fn next_node_id(conn: &Connection) -> Result<NodeId> {
    let mut stmt = conn.prepare_cached("SELECT value FROM metadata WHERE key = 'next_node_id'")?;
    let raw: Vec<u8> = stmt.query_row([], |row| row.get(0))?;
    let current = u64::from_be_bytes(
        raw.get(..8)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| GraphError::Serialization("corrupt node ID bytes".into()))?,
    );

    let next = current
        .checked_add(1)
        .ok_or_else(|| GraphError::Transaction("node ID space exhausted".into()))?;

    // Compare-and-swap: only update if the current value hasn't changed (H6).
    let rows = conn.execute(
        "UPDATE metadata SET value = ?1 WHERE key = 'next_node_id' AND value = ?2",
        rusqlite::params![&next.to_be_bytes()[..], &raw],
    )?;
    if rows == 0 {
        return Err(GraphError::Transaction(
            "node ID conflict — concurrent writer".into(),
        ));
    }

    Ok(NodeId(current))
}
