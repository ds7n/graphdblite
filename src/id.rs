use rusqlite::Connection;

use crate::types::{NodeId, Result};

/// Allocate the next node ID. Must be called within a write transaction.
pub fn next_node_id(conn: &Connection) -> Result<NodeId> {
    let mut stmt = conn.prepare_cached(
        "SELECT value FROM metadata WHERE key = 'next_node_id'",
    )?;
    let raw: Vec<u8> = stmt.query_row([], |row| row.get(0))?;
    let current = u64::from_be_bytes(raw[..8].try_into().unwrap());

    let next = current + 1;
    conn.execute(
        "UPDATE metadata SET value = ?1 WHERE key = 'next_node_id'",
        [&next.to_be_bytes()[..]],
    )?;

    Ok(NodeId(current))
}
