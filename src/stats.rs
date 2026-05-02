use rusqlite::Connection;

use crate::types::{GraphError, Result};

/// Metadata key prefix for label node counts.
const LABEL_COUNT_PREFIX: &str = "stats:label_count:";

/// Get the node count for a label. Returns 0 if no stats available.
pub fn get_label_count(conn: &Connection, label: &str) -> Result<u64> {
    let key = format!("{LABEL_COUNT_PREFIX}{label}");
    let mut stmt = conn.prepare_cached("SELECT value FROM metadata WHERE key = ?1")?;
    let result = stmt.query_row([&key], |row| row.get::<_, Vec<u8>>(0));
    match result {
        Ok(data) if data.len() == 8 => {
            Ok(u64::from_be_bytes(data[..8].try_into().map_err(|_| {
                GraphError::Serialization("corrupt label count bytes".into())
            })?))
        }
        _ => Ok(0),
    }
}

/// Increment the node count for a label by 1.
///
/// Safe: always called within a BEGIN IMMEDIATE write transaction,
/// which serializes all writers (even cross-process under WAL).
pub fn increment_label_count(conn: &Connection, label: &str) -> Result<()> {
    let count = get_label_count(conn, label)?;
    set_label_count(conn, label, count + 1)
}

/// Decrement the node count for a label by 1.
pub fn decrement_label_count(conn: &Connection, label: &str) -> Result<()> {
    let count = get_label_count(conn, label)?;
    let new_count = count.saturating_sub(1);
    if new_count == 0 {
        let key = format!("{LABEL_COUNT_PREFIX}{label}");
        conn.execute(
            "DELETE FROM metadata WHERE key = ?1",
            rusqlite::params![key],
        )?;
        Ok(())
    } else {
        set_label_count(conn, label, new_count)
    }
}

/// Set the node count for a label.
fn set_label_count(conn: &Connection, label: &str, count: u64) -> Result<()> {
    let key = format!("{LABEL_COUNT_PREFIX}{label}");
    conn.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, count.to_be_bytes().as_slice()],
    )?;
    Ok(())
}

